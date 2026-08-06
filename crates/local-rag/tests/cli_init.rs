//! `local-rag init [--download-models]` acceptance tests (spec 11 §6, D-013),
//! driving the real compiled binary — mirrors `tests/cli_service.rs`'s own
//! `open_layout`/`run_cli` helpers (duplicated here per this crate's
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

#![cfg(unix)]

use std::path::PathBuf;
use std::process::{Output, Stdio};

use local_rag_core::paths::StoreLayout;
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
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("is not installed yet"),
        "{:?}",
        output.stdout
    );
    // `init` never even opens `state.sqlite` on this path — nothing to
    // register against an uninstalled model — so the file must not exist.
    assert!(
        !layout.state_db().exists(),
        "a bare init on an uninstalled model must not touch state.sqlite"
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
