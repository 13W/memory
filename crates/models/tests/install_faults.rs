//! Interrupted download: the card's resumability row, driven by a named crash
//! point rather than a timer (spec 14 §3) — T11-06.
//!
//! `models.install.between_files` fires after a file has been verified and
//! renamed into place and before the next one starts — the exact instant where a
//! `kill -9` is most awkward: some assets are durable, the manifest is not, the
//! marker is not. The test asserts the three properties that make that instant
//! safe:
//!
//! 1. the model is **not** usable (no `.ok`, so `require_model_assets` still
//!    reports `ModelAssetsMissing`);
//! 2. a rerun **resumes** — it refetches only what is missing, proven by the
//!    fixture server's request log, not by a report field the installer could
//!    have miscomputed;
//! 3. a third run is a no-op, i.e. the operation is idempotent rather than
//!    merely repeatable.

#![cfg(feature = "failpoints")]

mod support;

use std::sync::Mutex;

use local_rag_core::paths::StoreLayout;
use local_rag_embed::{EmbedError, require_model_assets};
use local_rag_models::{
    HttpFetcher, InstallError, LocalFetcher, MANIFEST_FILE, OK_MARKER, install_model, install_ort,
    is_installed, ort_is_installed,
};
use local_rag_test_support::TempHome;
use local_rag_test_support::failpoint::{Action, global};
use support::{FIXTURE_FILES, FIXTURE_MODEL_ID, FixtureServer};

const FAILPOINT: &str = "models.install.between_files";
/// Fires between a durable ONNX Runtime library and the marker (T22-15).
const ORT_FAILPOINT: &str = "models.install.ort_before_marker";

/// The failpoint registry is process-global, so an arming in one test would be
/// visible to a concurrently running one. Serializing the whole file is the same
/// remedy `crates/embed/tests/backfill_resume.rs` uses.
static SERIAL: Mutex<()> = Mutex::new(());

/// Take the serialization lock, ignoring poisoning: a panicking test must not
/// cascade into "every later test fails to acquire the lock".
fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// Arm the crash point, guaranteeing it is disarmed when the guard drops — a
/// leaked arming would silently break every later test in the binary.
struct Armed(&'static str);

impl Armed {
    fn new() -> Self {
        Self::at(FAILPOINT)
    }

    fn at(name: &'static str) -> Self {
        global().register(name);
        global().arm(name, Action::Error).expect("arm");
        Armed(name)
    }
}

impl Drop for Armed {
    fn drop(&mut self) {
        let _ = global().disarm(self.0);
    }
}

#[test]
fn an_interrupted_install_resumes_from_what_is_already_durable() {
    let _serial = serial();
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let server = FixtureServer::start();
    let entry = server.entry();
    let dir = layout.model_dir(FIXTURE_MODEL_ID);

    // ---- run 1: killed after the first file lands -------------------------
    let armed = Armed::new();
    let err = install_model(&layout, &entry, &HttpFetcher::new(), &mut std::io::sink())
        .expect_err("the crash point fires");
    assert!(matches!(err, InstallError::Interrupted), "{err:?}");
    drop(armed);

    assert_eq!(server.request_count(), 1, "{:?}", server.requests());
    assert_eq!(
        std::fs::read(dir.join("weights.onnx")).expect("the first file survived"),
        support::fixture_bytes("weights.onnx"),
        "a file that was renamed into place stays durable across the crash"
    );
    assert!(!dir.join("weights.onnx_data").exists(), "nothing beyond it");
    assert!(!dir.join(MANIFEST_FILE).exists(), "no manifest yet");
    assert!(!dir.join(OK_MARKER).exists(), "no marker yet");
    assert!(!is_installed(&layout, FIXTURE_MODEL_ID));
    assert!(
        matches!(
            require_model_assets(&layout, FIXTURE_MODEL_ID),
            Err(EmbedError::ModelAssetsMissing { .. })
        ),
        "an interrupted install is indistinguishable from 'not installed'"
    );

    // ---- run 2: resumes, refetching only the missing files -----------------
    let report = install_model(&layout, &entry, &HttpFetcher::new(), &mut std::io::sink())
        .expect("the rerun completes");

    assert_eq!(report.reused, vec!["weights.onnx"]);
    assert_eq!(
        report.downloaded,
        vec!["weights.onnx_data", "tokenizer.json"]
    );
    assert!(report.marked_ready);
    assert_eq!(
        server.requests(),
        vec![
            "/repo/resolve/rev-0/onnx/weights.onnx".to_string(),
            "/repo/resolve/rev-0/onnx/weights.onnx_data".to_string(),
            "/repo/resolve/rev-0/tokenizer.json".to_string(),
        ],
        "the already-durable file is never fetched twice"
    );
    assert!(is_installed(&layout, FIXTURE_MODEL_ID));
    for (relative, _, bytes) in FIXTURE_FILES {
        assert_eq!(
            std::fs::read(dir.join(relative)).expect("installed file"),
            *bytes
        );
    }

    // ---- run 3: idempotent -------------------------------------------------
    let after = server.request_count();
    let repeat = install_model(
        &layout,
        &entry,
        &support::ForbiddenFetcher,
        &mut std::io::sink(),
    )
    .expect("a completed install repeats cleanly");
    assert!(repeat.is_noop(), "{repeat:?}");
    assert_eq!(server.request_count(), after, "no further requests");
}

#[test]
fn a_crash_leaves_no_part_file_masquerading_as_an_asset() {
    let _serial = serial();
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let server = FixtureServer::start();
    let entry = server.entry();
    let dir = layout.model_dir(FIXTURE_MODEL_ID);

    let armed = Armed::new();
    install_model(&layout, &entry, &HttpFetcher::new(), &mut std::io::sink())
        .expect_err("the crash point fires");
    drop(armed);

    // Whatever is on disk after the crash is either a verified asset or an
    // obvious `.part`; a rerun must not be able to mistake one for the other.
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .expect("read model dir")
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, vec!["weights.onnx".to_string()], "{names:?}");
}

#[test]
fn a_stale_part_file_is_overwritten_rather_than_resumed_blindly() {
    let _serial = serial();
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let server = FixtureServer::start();
    let entry = server.entry();
    let dir = layout.model_dir(FIXTURE_MODEL_ID);
    std::fs::create_dir_all(&dir).expect("model dir");

    // A `.part` left by a killed process, holding a prefix of the real bytes.
    // Nothing recorded how far it got, so appending to it would corrupt the
    // asset; the installer must start it over.
    std::fs::write(dir.join("weights.onnx.part"), b"fixture wei").expect("stale part");

    install_model(&layout, &entry, &HttpFetcher::new(), &mut std::io::sink()).expect("install");

    assert!(is_installed(&layout, FIXTURE_MODEL_ID));
    assert_eq!(
        std::fs::read(dir.join("weights.onnx")).expect("installed"),
        support::fixture_bytes("weights.onnx"),
        "the stale prefix was discarded, not appended to"
    );
    assert!(!dir.join("weights.onnx.part").exists());
}

#[test]
fn a_runtime_install_killed_before_the_marker_is_not_a_runtime_install() {
    // The ordering property `install_ort` rests on, made observable. Without
    // this the marker could be written before the library was durable and every
    // test stayed green — found by a mutation that did exactly that, not by
    // reading the code.
    //
    // The three things asserted are the same three the weights' own crash test
    // asserts, in the same order: the library is durable, nothing that makes it
    // usable exists, and a rerun completes without refetching.
    let _serial = serial();
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");

    let payload = vec![7u8; 4096];
    let f = support::ort_fixture(&home, &payload);
    let dir = layout
        .models_dir()
        .join("onnxruntime")
        .join(f.asset.version);

    let armed = Armed::at(ORT_FAILPOINT);
    let err = install_ort(&layout, &f.asset, &LocalFetcher::new(&f.served))
        .expect_err("the crash point fires");
    assert!(matches!(err, InstallError::Interrupted), "{err:?}");
    drop(armed);

    assert_eq!(
        std::fs::read(dir.join(f.asset.dylib_name)).expect("the library survived"),
        payload,
        "a file renamed into place stays durable across the crash"
    );
    assert!(!dir.join(MANIFEST_FILE).exists(), "no manifest yet");
    assert!(!dir.join(OK_MARKER).exists(), "no marker yet");
    assert!(
        !ort_is_installed(&layout, &f.asset),
        "an interrupted install must be indistinguishable from 'not installed'"
    );

    // The archive is still there, which is the whole reason it is kept until
    // the marker: the rerun finishes without going back to the network.
    let report = install_ort(&layout, &f.asset, &support::ForbiddenFetcher).expect("resume");
    assert!(report.marked_ready);
    assert_eq!(report.bytes_downloaded, 0);
    assert!(ort_is_installed(&layout, &f.asset));
}
