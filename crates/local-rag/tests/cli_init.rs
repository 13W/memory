//! `local-rag init [--download-models]` acceptance tests (spec 11 §6, D-013,
//! D-045), driving the real compiled binary — mirrors `tests/cli_service.rs`'s
//! own `open_layout`/`run_cli` helpers (duplicated here per this crate's
//! established per-file-fixture convention).
//!
//! Registration itself (the transaction, idempotency, the effect on
//! `params_for_model_space`) is unit-tested against a fixture key in
//! `src/cli/init.rs` — no ONNX runtime or real weights required there. What
//! is only testable here, end to end, is the *disk-state gate*: what `init`
//! prints and does before any model is installed. Actually opening the
//! installed model and registering its real key needs the ONNX Runtime
//! shared library and ~295 MB of weights, neither of which may be a CI
//! prerequisite (spec 14 §1) — that path is env-gated below, following
//! `local-rag-models`'s own `real_inference_when_the_runtime_and_weights_are_present`
//! precedent exactly: skip loudly when the environment does not supply both.
//!
//! T22-16: the ONNX Runtime is a **third** artifact `--download-models`
//! installs, and it gets the same disk-state-gate coverage for the same
//! reason it works for the generator — `ort_is_installed` is a marker check,
//! so a fixture `.ok` is enough to exercise "already installed" without a
//! real 38 MB library or the network.
//!
//! D-045: the generative model gets the same disk-state-gate coverage as the
//! embedder. Unlike the embedder, `is_installed` is all its install status
//! ever needs (no ONNX/database registration step), so a fixture `.ok`
//! marker is enough to exercise "already installed" — no llama.cpp runtime
//! or multi-gigabyte weights required for that path either.

#![cfg(unix)]

use std::path::PathBuf;
use std::process::{Output, Stdio};

use local_rag_core::paths::StoreLayout;
use local_rag_generate::DEFAULT_MODEL_ID as GENERATOR_MODEL_ID;
use local_rag_models::DEFAULT_MODEL_ID;
use local_rag_test_support::TempHome;

fn open_layout() -> (TempHome, StoreLayout) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    (home, layout)
}

fn run_cli(home: &TempHome, args: &[&str]) -> Output {
    let mut cmd = home.command(env!("CARGO_BIN_EXE_local-rag"));
    cmd.args(args);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.output().expect("run local-rag")
}

fn representation_row_count(layout: &StoreLayout) -> i64 {
    let conn = rusqlite::Connection::open_with_flags(
        layout.state_db(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("open state.sqlite read-only");
    conn.query_row("SELECT count(*) FROM representation", [], |r| r.get(0))
        .expect("count representation rows")
}

#[test]
fn bare_init_without_download_models_is_a_light_no_op() {
    let (home, layout) = open_layout();
    let output = run_cli(&home, &["init"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    // D-045: both catalogued default models must get their own "not
    // installed yet" hint by name, not just a single coincidental substring
    // match — the generator's hint is independent of the embedder's.
    assert!(
        stdout.contains(&format!("{DEFAULT_MODEL_ID} is not installed yet")),
        "{stdout:?}"
    );
    assert!(
        stdout.contains(&format!("{GENERATOR_MODEL_ID} is not installed yet")),
        "{stdout:?}"
    );
    // `init` never even opens `state.sqlite` on this path — nothing to
    // register against an uninstalled model — so the file must not exist.
    assert!(
        !layout.state_db().exists(),
        "a bare init on an uninstalled model must not touch state.sqlite"
    );
}

/// D-045: a fixture `.ok` marker is enough to prove the generator's hint is
/// gated on its own disk state, independent of the embedder — no llama.cpp
/// runtime or real weights needed, since `is_installed` only checks for the
/// marker file.
#[test]
fn generator_install_marker_suppresses_only_the_generators_hint() {
    let (home, layout) = open_layout();
    let generator_dir = layout.model_dir(GENERATOR_MODEL_ID);
    std::fs::create_dir_all(&generator_dir).expect("create generator model dir");
    std::fs::write(generator_dir.join(".ok"), b"").expect("write fixture .ok marker");

    let output = run_cli(&home, &["init"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains(&format!("{GENERATOR_MODEL_ID} is not installed yet")),
        "the generator hint must not print once its marker is on disk: {stdout:?}"
    );
    // The embedder is still uninstalled in this fixture — its own hint (and
    // early return before any registration) must be unaffected.
    assert!(
        stdout.contains(&format!("{DEFAULT_MODEL_ID} is not installed yet")),
        "{stdout:?}"
    );
    assert!(
        !layout.state_db().exists(),
        "the embedder's own early return must still hold — nothing to register yet"
    );
}

/// T22-16: the runtime's own hint, on the same disk-state gate the two models
/// use. The fixture is a marker plus a stand-in file — `init` never loads the
/// library on this path, and a test that supplied a real one would be
/// exercising `ort`, not this gate.
#[test]
fn the_runtime_hint_is_gated_on_its_own_disk_state() {
    let Some(asset) = local_rag_models::for_current_platform() else {
        // A platform with no pinned runtime gets a different line entirely,
        // and asserting the installed-state one there would be asserting
        // something this build cannot do.
        let (home, _layout) = open_layout();
        let stdout = String::from_utf8_lossy(&run_cli(&home, &["init"]).stdout).into_owned();
        assert!(
            stdout.contains("no pinned ONNX Runtime for this platform"),
            "{stdout:?}"
        );
        return;
    };

    let (home, layout) = open_layout();
    let stdout = String::from_utf8_lossy(&run_cli(&home, &["init"]).stdout).into_owned();
    assert!(
        stdout.contains("the ONNX Runtime is not installed yet"),
        "{stdout:?}"
    );
    // The consequence, not just the fact: without it the embedder cannot be
    // opened at all, which is why the line says what it says.
    assert!(stdout.contains("lexical-only"), "{stdout:?}");
    assert!(
        !layout.state_db().exists(),
        "a bare init must still not touch state.sqlite"
    );

    // Now put one where the store expects it, marker last — `install_ort`'s
    // own ordering, so `ort_is_installed` agrees.
    let dylib = local_rag_models::ort_dylib_path(&layout, asset);
    let dir = dylib.parent().expect("version dir");
    std::fs::create_dir_all(dir).expect("create runtime dir");
    std::fs::write(&dylib, b"not a real library").expect("write stand-in");
    std::fs::write(dir.join(".ok"), b"").expect("write fixture .ok marker");

    let stdout = String::from_utf8_lossy(&run_cli(&home, &["init"]).stdout).into_owned();
    assert!(
        !stdout.contains("the ONNX Runtime is not installed yet"),
        "the runtime hint must not print once its marker is on disk: {stdout:?}"
    );
    // And it is independent of the two models, exactly as they are of each
    // other: both are still uninstalled here and must still say so.
    assert!(
        stdout.contains(&format!("{DEFAULT_MODEL_ID} is not installed yet")),
        "{stdout:?}"
    );
    assert!(
        stdout.contains(&format!("{GENERATOR_MODEL_ID} is not installed yet")),
        "{stdout:?}"
    );
}

/// T22-16: a bare `init` still downloads nothing, runtime included.
///
/// Worth its own assertion rather than trusting the branch: the runtime block
/// sits *before* the embedder's early return, so a `download` flag read wrongly
/// there would fetch 30 MB on a command whose whole contract is that it does
/// not touch the network.
#[test]
fn bare_init_installs_no_runtime() {
    let Some(asset) = local_rag_models::for_current_platform() else {
        return;
    };
    let (home, layout) = open_layout();
    let output = run_cli(&home, &["init"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(
        !local_rag_models::ort_dylib_path(&layout, asset).exists(),
        "a bare init must not fetch the runtime"
    );
    assert!(
        !layout.models_dir().join("onnxruntime").exists(),
        "not even the directory"
    );
}

#[test]
fn init_rejects_an_unknown_argument() {
    let (home, _layout) = open_layout();
    let output = run_cli(&home, &["init", "--bogus"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
}

/// Real registration through the compiled binary, when the host supplies
/// both an ONNX Runtime and a store already holding the installed default
/// model — see this file's own module doc for why the policy-only path above
/// is exercised unconditionally while this one is not.
#[test]
fn bare_init_registers_code_raw_when_the_model_is_already_installed() {
    let Ok(dylib) = std::env::var("ORT_DYLIB_PATH") else {
        eprintln!(
            "SKIP: ORT_DYLIB_PATH is unset — no ONNX Runtime to load. \
             Set it to libonnxruntime.{{so,dylib,dll}} to run this test."
        );
        return;
    };
    let Ok(model_home) = std::env::var("LOCAL_RAG_TEST_MODEL_HOME") else {
        eprintln!(
            "SKIP: LOCAL_RAG_TEST_MODEL_HOME is unset — no installed weights. \
             Point it at a store root containing models/{DEFAULT_MODEL_ID}/.ok."
        );
        return;
    };
    let src = PathBuf::from(&model_home)
        .join("models")
        .join(DEFAULT_MODEL_ID);
    assert!(
        src.join(".ok").is_file(),
        "{}: LOCAL_RAG_TEST_MODEL_HOME must already have {DEFAULT_MODEL_ID} installed",
        src.display()
    );

    let (home, layout) = open_layout();
    let models_dir = layout.model_dir(DEFAULT_MODEL_ID);
    std::fs::create_dir_all(models_dir.parent().expect("models dir has a parent"))
        .expect("create models/ parent");
    // Symlink rather than copy: the fixture weights are ~295 MB and this test
    // only runs when explicitly opted into locally.
    std::os::unix::fs::symlink(&src, &models_dir)
        .expect("symlink installed model into the temp store");

    let mut cmd = home.command(env!("CARGO_BIN_EXE_local-rag"));
    cmd.arg("init");
    cmd.env("ORT_DYLIB_PATH", &dylib);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let output = cmd.output().expect("run local-rag init");

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("registered code_raw representation"),
        "{:?}",
        output.stdout
    );
    // D-036: `init` also registers the `memory` representation, same model,
    // second `RepresentationKey` (`kind: Memory`).
    assert!(
        stdout.contains("registered memory representation"),
        "{:?}",
        output.stdout
    );
    assert_eq!(
        representation_row_count(&layout),
        2,
        "exactly two representation rows (code_raw + memory) after a fresh init"
    );

    // Idempotency: running it again must not create a second pair of rows.
    let second = run_cli(&home, &["init"]);
    assert_eq!(second.status.code(), Some(0), "{second:?}");
    assert_eq!(
        representation_row_count(&layout),
        2,
        "a repeated init must converge on the same two rows, not add more"
    );
}
