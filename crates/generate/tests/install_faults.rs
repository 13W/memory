//! Interrupted download (spec 14 §3's named crash point, not a timer) —
//! T14-07, mirroring `local_rag_models/tests/install_faults.rs`.
//!
//! `generate.install.between_files` fires after the (single) GGUF file has
//! been verified and renamed into place and before the manifest/marker are
//! written — the exact instant where the asset is durable but the model is
//! not yet usable.

#![cfg(feature = "failpoints")]

mod support;

use std::sync::Mutex;

use local_rag_core::paths::StoreLayout;
use local_rag_generate::{
    HttpFetcher, InstallError, MANIFEST_FILE, OK_MARKER, install_model, is_installed,
};
use local_rag_test_support::TempHome;
use local_rag_test_support::failpoint::{Action, global};
use support::{FIXTURE_MODEL_ID, FixtureServer, ForbiddenFetcher, fixture_bytes};

const FAILPOINT: &str = "generate.install.between_files";

static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

struct Armed;

impl Armed {
    fn new() -> Self {
        global().register(FAILPOINT);
        global().arm(FAILPOINT, Action::Error).expect("arm");
        Armed
    }
}

impl Drop for Armed {
    fn drop(&mut self) {
        let _ = global().disarm(FAILPOINT);
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

    let armed = Armed::new();
    let err = install_model(&layout, &entry, &HttpFetcher::new(), &mut std::io::sink())
        .expect_err("the crash point fires");
    assert!(matches!(err, InstallError::Interrupted), "{err:?}");
    drop(armed);

    assert_eq!(server.request_count(), 1);
    assert_eq!(
        std::fs::read(dir.join("model.gguf")).expect("the file survived"),
        fixture_bytes(),
        "the file that was renamed into place stays durable across the crash"
    );
    assert!(!dir.join(MANIFEST_FILE).exists(), "no manifest yet");
    assert!(!dir.join(OK_MARKER).exists(), "no marker yet");
    assert!(!is_installed(&layout, FIXTURE_MODEL_ID));

    // ---- run 2: resumes, refetching nothing (the only file already landed) ----
    let report = install_model(&layout, &entry, &ForbiddenFetcher, &mut std::io::sink())
        .expect("the rerun completes without touching the network again");
    assert_eq!(report.reused, vec!["model.gguf".to_string()]);
    assert!(report.downloaded.is_empty());
    assert!(report.marked_ready);
    assert!(is_installed(&layout, FIXTURE_MODEL_ID));

    // ---- run 3: idempotent -----------------------------------------------
    let repeat = install_model(&layout, &entry, &ForbiddenFetcher, &mut std::io::sink())
        .expect("a completed install repeats cleanly");
    assert!(repeat.is_noop(), "{repeat:?}");
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

    std::fs::write(dir.join("model.gguf.part"), b"stale prefix").expect("stale part");

    install_model(&layout, &entry, &HttpFetcher::new(), &mut std::io::sink()).expect("install");

    assert!(is_installed(&layout, FIXTURE_MODEL_ID));
    assert_eq!(
        std::fs::read(dir.join("model.gguf")).expect("installed"),
        fixture_bytes(),
        "the stale prefix was discarded, not appended to"
    );
    assert!(!dir.join("model.gguf.part").exists());
}
