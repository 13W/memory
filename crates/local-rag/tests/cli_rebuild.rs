//! `local-rag rebuild --worktree <id> [--fts] [--dense]` acceptance tests
//! (spec 11 §6, spec 05 §7), driving the real compiled binary — mirrors
//! `tests/cli_index.rs`'s own `open_layout`/`run_cli` helpers (duplicated
//! here per this crate's established per-file-fixture convention).
//!
//! Argument parsing and the "no active generation yet" refusals need no
//! model at all (`rebuild`'s own module doc: neither leg opens an
//! embedder). The real end-to-end rebuild — actually re-deriving FTS/dense
//! content that only a prior real `index` run can have produced — is
//! env-gated, following `tests/cli_init.rs`'s established precedent.

#![cfg(unix)]

use std::path::PathBuf;
use std::process::{Output, Stdio};

use local_rag_core::paths::StoreLayout;
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

#[test]
fn rebuild_without_a_worktree_flag_is_a_usage_error() {
    let (home, _layout) = open_layout();
    let output = run_cli(&home, &["rebuild", "--fts"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
}

#[test]
fn rebuild_without_fts_or_dense_is_a_usage_error() {
    let (home, _layout) = open_layout();
    let output = run_cli(
        &home,
        &[
            "rebuild",
            "--worktree",
            "00000000-0000-7000-8000-000000000001",
        ],
    );
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--fts or --dense"),
        "{:?}",
        output.stderr
    );
}

#[test]
fn rebuild_rejects_an_unknown_argument() {
    let (home, _layout) = open_layout();
    let output = run_cli(&home, &["rebuild", "--bogus"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
}

#[test]
fn rebuild_rejects_a_worktree_flag_missing_its_value() {
    let (home, _layout) = open_layout();
    let output = run_cli(&home, &["rebuild", "--worktree"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
}

#[test]
fn rebuild_with_an_invalid_worktree_id_is_reported() {
    let (home, _layout) = open_layout();
    let output = run_cli(&home, &["rebuild", "--worktree", "not-a-uuid", "--fts"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("not a valid worktree id"),
        "{:?}",
        output.stderr
    );
}

#[test]
fn rebuild_of_an_unknown_worktree_reports_no_active_generation() {
    let (home, _layout) = open_layout();
    // A well-formed but never-registered worktree id: both legs read
    // `worktree_projection_state`/`worktree` and find nothing, which is the
    // same observable outcome as "registered but never indexed" — neither
    // leg needs the worktree to actually exist to report this.
    let output = run_cli(
        &home,
        &[
            "rebuild",
            "--worktree",
            "00000000-0000-7000-8000-000000000001",
            "--fts",
            "--dense",
        ],
    );
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("local-rag index"),
        "{:?}",
        output.stderr
    );
}

/// Real end-to-end runs through the compiled binary with the real default
/// model — see this file's own module doc for why this is env-gated (the
/// *setup*, a real `index` run, needs ONNX; `rebuild` itself never does).
mod with_real_model {
    use super::*;

    fn require_env() -> Option<(String, String)> {
        let dylib = std::env::var("ORT_DYLIB_PATH").ok();
        let model_home = std::env::var("LOCAL_RAG_TEST_MODEL_HOME").ok();
        match (dylib, model_home) {
            (Some(d), Some(m)) => Some((d, m)),
            _ => {
                eprintln!(
                    "SKIP: ORT_DYLIB_PATH and/or LOCAL_RAG_TEST_MODEL_HOME are unset — \
                     set both to run the real-model rebuild test."
                );
                None
            }
        }
    }

    fn install_real_model(layout: &StoreLayout, model_home: &str) {
        let src = PathBuf::from(model_home)
            .join("models")
            .join(local_rag_models::DEFAULT_MODEL_ID);
        assert!(
            src.join(".ok").is_file(),
            "{}: LOCAL_RAG_TEST_MODEL_HOME must already have the default model installed",
            src.display()
        );
        let dst = layout.model_dir(local_rag_models::DEFAULT_MODEL_ID);
        std::fs::create_dir_all(dst.parent().expect("models dir has a parent"))
            .expect("create models/ parent");
        std::os::unix::fs::symlink(&src, &dst).expect("symlink installed model");
    }

    fn run_cli_with_ort(home: &TempHome, dylib: &str, args: &[&str]) -> Output {
        let mut cmd = home.command(env!("CARGO_BIN_EXE_local-rag"));
        cmd.args(args);
        cmd.env("ORT_DYLIB_PATH", dylib);
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.output().expect("run local-rag")
    }

    fn worktree_id(layout: &StoreLayout) -> String {
        let conn = rusqlite::Connection::open_with_flags(
            layout.state_db(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .expect("open state.sqlite read-only");
        conn.query_row("SELECT worktree_id FROM worktree LIMIT 1", [], |r| r.get(0))
            .expect("one worktree exists")
    }

    #[test]
    fn rebuild_fts_and_dense_together_report_both() {
        let Some((dylib, model_home)) = require_env() else {
            return;
        };
        let (home, layout) = open_layout();
        install_real_model(&layout, &model_home);
        let init = run_cli_with_ort(&home, &dylib, &["init"]);
        assert_eq!(init.status.code(), Some(0), "{init:?}");

        let target = home.join("project");
        std::fs::create_dir_all(&target).expect("create target dir");
        std::fs::write(target.join("main.rs"), "fn one() {}\nfn two() {}").expect("seed file");
        let index = run_cli_with_ort(&home, &dylib, &["index", target.to_str().unwrap()]);
        assert_eq!(index.status.code(), Some(0), "{index:?}");

        let worktree_id = worktree_id(&layout);
        let output = run_cli_with_ort(
            &home,
            &dylib,
            &["rebuild", "--worktree", &worktree_id, "--fts", "--dense"],
        );
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("fts:"), "{stdout}");
        assert!(stdout.contains("dense:"), "{stdout}");
    }

    #[test]
    fn rebuild_dense_forces_a_rebuild_even_when_the_shard_is_valid() {
        let Some((dylib, model_home)) = require_env() else {
            return;
        };
        let (home, layout) = open_layout();
        install_real_model(&layout, &model_home);
        let init = run_cli_with_ort(&home, &dylib, &["init"]);
        assert_eq!(init.status.code(), Some(0), "{init:?}");

        let target = home.join("project");
        std::fs::create_dir_all(&target).expect("create target dir");
        std::fs::write(target.join("main.rs"), "fn one() {}").expect("seed file");
        let index = run_cli_with_ort(&home, &dylib, &["index", target.to_str().unwrap()]);
        assert_eq!(index.status.code(), Some(0), "{index:?}");

        let worktree_id = worktree_id(&layout);
        // Nothing changed since `index` just built a valid shard — `--dense`
        // must still rebuild it (T15-07's own point, force_rebuild skips
        // `validate` by construction), not report "nothing to do".
        let output = run_cli_with_ort(
            &home,
            &dylib,
            &["rebuild", "--worktree", &worktree_id, "--dense"],
        );
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("dense:"),
            "{:?}",
            output.stdout
        );
    }
}
