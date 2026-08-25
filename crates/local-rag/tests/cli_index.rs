//! `local-rag index <path>` / `local-rag reindex` acceptance tests (spec 11
//! §6, spec 06 §1), driving the real compiled binary — mirrors
//! `tests/cli_service.rs`/`tests/cli_init.rs`'s own `open_layout`/`run_cli`
//! helpers (duplicated here per this crate's established per-file-fixture
//! convention).
//!
//! Most of these need no installed model at all: `run_index`/`run_reindex`
//! resolve worktree identity *before* opening the embedder
//! (`local_rag::indexing`'s own `open_state`/`resolve_facts`/
//! `finish_index_ctx` split), so the `Ambiguous`/`GlobalOnly`-refusal/"not
//! installed" paths are all reachable — and asserted here — without ONNX.
//! The pipeline's own correctness (files indexed, vectors embedded,
//! generation searchable) is unit-tested against a fixture `HashingEmbedder`
//! directly in `src/indexing/mod.rs` (T20-02); what
//! is only testable here, end to end through the real binary with the real
//! default model, is env-gated below, following
//! `local-rag-models`'s own `real_inference_when_the_runtime_and_weights_are_present`
//! precedent: skip loudly when the environment does not supply both.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};

use local_rag::daemon::gitroot;
use local_rag_core::paths::StoreLayout;
use local_rag_models::DEFAULT_MODEL_ID;
use local_rag_store::{
    StateDb, WorktreeState, create_repository, create_worktree, insert_projection_state,
    observe_repository_path, observe_worktree_path, transition_worktree_state,
};
use local_rag_test_support::TempHome;

fn open_layout() -> (TempHome, StoreLayout) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    (home, layout)
}

fn run_cli(home: &TempHome, dir: &Path, args: &[&str]) -> Output {
    let mut cmd = home.command(env!("CARGO_BIN_EXE_local-rag"));
    cmd.args(args);
    cmd.current_dir(dir);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.output().expect("run local-rag")
}

/// Seed one active `{repo, worktree}` whose *current* path is `path` — a
/// test-only shortcut for the "already indexed, just not initialized"
/// scenario `reindex` itself never creates. Uses the real `gitroot::probe`
/// to derive the canonical path/fingerprint, so the seeded row matches byte
/// for byte what the CLI's own probe of the same directory computes (a
/// hand-rolled canonicalization here previously drifted from `probe`'s own
/// symlink resolution on macOS's `/var` → `/private/var`).
async fn seed_active_worktree(layout: &StoreLayout, path: &Path) {
    let facts = gitroot::probe(path).expect("probe the seeded path");
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    let repo_id = "10000000-0000-7000-8000-000000000001".to_string();
    let worktree_id = "10000000-0000-7000-8000-000000000002".to_string();
    db.writer()
        .transaction(move |tx| {
            create_repository(tx, &repo_id, None, 1_000)?;
            create_worktree(tx, &worktree_id, &repo_id, facts.kind, 1_000)?;
            observe_worktree_path(
                tx,
                &worktree_id,
                &facts.observed_canonical_path,
                &facts.display_path,
                &facts.path_fingerprint,
                1_000,
            )?;
            observe_repository_path(tx, &repo_id, &facts.observed_canonical_path, 1_000)?;
            insert_projection_state(tx, &worktree_id, 1_000)
        })
        .await
        .expect("seed active worktree");
}

/// Seed a `detached` reattach candidate whose *historical* path is
/// `original`: register the worktree there, observe it moving to `elsewhere`
/// (which clears `original`'s `is_current` flag but keeps the row, spec 04
/// §7), then mark it `detached` — the same "worktree moved away, old path
/// recreated" scenario `local_rag_store::registry::resolve`'s own tests
/// build (`crates/store/tests/resolve.rs::linked_ambiguity_requires_id`).
/// Probing `original` again afterwards must **not** auto-resolve
/// (`find_worktree_by_current_path` no longer matches it — `elsewhere` does)
/// but must surface it as an advisory [`Resolution::Ambiguous`] candidate via
/// its still-recorded path fingerprint.
async fn seed_detached_reattach_candidate(layout: &StoreLayout, original: &Path, elsewhere: &Path) {
    std::fs::create_dir_all(elsewhere).expect("create the 'moved to' directory");
    let from = gitroot::probe(original).expect("probe the original path");
    let to = gitroot::probe(elsewhere).expect("probe the elsewhere path");
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    let repo_id = "20000000-0000-7000-8000-000000000001".to_string();
    let worktree_id = "20000000-0000-7000-8000-000000000002".to_string();

    db.writer()
        .transaction({
            let (repo_id, worktree_id, from) = (repo_id.clone(), worktree_id.clone(), from.clone());
            move |tx| {
                create_repository(tx, &repo_id, None, 1_000)?;
                create_worktree(tx, &worktree_id, &repo_id, from.kind, 1_000)?;
                observe_worktree_path(
                    tx,
                    &worktree_id,
                    &from.observed_canonical_path,
                    &from.display_path,
                    &from.path_fingerprint,
                    1_000,
                )?;
                observe_repository_path(tx, &repo_id, &from.observed_canonical_path, 1_000)?;
                insert_projection_state(tx, &worktree_id, 1_000)
            }
        })
        .await
        .expect("seed the original registration");

    db.writer()
        .transaction({
            let (repo_id, worktree_id) = (repo_id.clone(), worktree_id.clone());
            move |tx| {
                observe_worktree_path(
                    tx,
                    &worktree_id,
                    &to.observed_canonical_path,
                    &to.display_path,
                    &to.path_fingerprint,
                    2_000,
                )?;
                observe_repository_path(tx, &repo_id, &to.observed_canonical_path, 2_000)
            }
        })
        .await
        .expect("observe the move");

    db.writer()
        .transaction(move |tx| {
            transition_worktree_state(tx, &worktree_id, WorktreeState::Detached, 3_000)
        })
        .await
        .expect("transition worktree state")
        .expect("legal transition");
}

#[test]
fn index_without_a_path_argument_is_a_usage_error() {
    let (home, _layout) = open_layout();
    let output = run_cli(&home, home.path(), &["index"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
}

#[test]
fn index_rejects_a_second_positional_argument() {
    let (home, _layout) = open_layout();
    let output = run_cli(&home, home.path(), &["index", ".", "extra"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
}

#[test]
fn reindex_rejects_any_argument() {
    let (home, _layout) = open_layout();
    let output = run_cli(&home, home.path(), &["reindex", "extra"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
}

#[test]
fn index_of_a_nonexistent_path_is_reported() {
    let (home, _layout) = open_layout();
    let output = run_cli(
        &home,
        home.path(),
        &["index", "/definitely/does/not/exist/xyz-123"],
    );
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("not an accessible directory"),
        "{:?}",
        output.stderr
    );
}

#[test]
fn index_of_a_fresh_directory_registers_it_then_reports_the_model_is_not_installed() {
    let (home, _layout) = open_layout();
    let target = home.join("project");
    std::fs::create_dir_all(&target).expect("create target dir");

    let output = run_cli(&home, home.path(), &["index", target.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("is not installed"),
        "{:?}",
        output.stderr
    );
}

#[test]
fn reindex_in_an_unindexed_cwd_reports_not_indexed() {
    let (home, _layout) = open_layout();
    let target = home.join("project");
    std::fs::create_dir_all(&target).expect("create target dir");

    let output = run_cli(&home, &target, &["reindex"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("run `local-rag index"),
        "{:?}",
        output.stderr
    );
}

#[test]
fn reindex_on_a_resolved_worktree_reports_the_model_is_not_installed() {
    let (home, layout) = open_layout();
    let target = home.join("project");
    std::fs::create_dir_all(&target).expect("create target dir");

    tokio_test_block_on(seed_active_worktree(&layout, &target));

    let output = run_cli(&home, &target, &["reindex"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("is not installed"),
        "{:?}",
        output.stderr
    );
}

#[test]
fn index_ambiguous_path_is_refused_and_suggests_repo_attach() {
    let (home, layout) = open_layout();
    let target = home.join("project");
    let elsewhere = home.join("project-moved");
    std::fs::create_dir_all(&target).expect("create target dir");

    // A worktree that once lived at `target`, moved to `elsewhere`, and is
    // now `detached` is an advisory reattach candidate the next time
    // `target`'s path fingerprint is seen — `index`/`reindex` never pass a
    // `repo_hint`, so even one candidate is `Resolution::Ambiguous` (spec 02
    // §3.3).
    tokio_test_block_on(seed_detached_reattach_candidate(
        &layout, &target, &elsewhere,
    ));

    let output = run_cli(&home, home.path(), &["index", target.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("repo attach"),
        "{:?}",
        output.stderr
    );
}

#[test]
fn reindex_ambiguous_cwd_is_refused_and_suggests_repo_attach() {
    let (home, layout) = open_layout();
    let target = home.join("project");
    let elsewhere = home.join("project-moved");
    std::fs::create_dir_all(&target).expect("create target dir");
    tokio_test_block_on(seed_detached_reattach_candidate(
        &layout, &target, &elsewhere,
    ));

    let output = run_cli(&home, &target, &["reindex"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("repo attach"),
        "{:?}",
        output.stderr
    );
}

/// A tiny, dependency-free `block_on` for the handful of async setup calls
/// this file's own tests need — the tests themselves are synchronous
/// (`#[test]`, driving a real subprocess), so pulling in `#[tokio::test]`
/// for one setup call per test would be more ceremony than the one-off
/// runtime it replaces.
fn tokio_test_block_on<F: std::future::Future>(fut: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(fut)
}

/// Real end-to-end runs through the compiled binary with the real default
/// model — see this file's own module doc for why these are env-gated while
/// everything above runs unconditionally.
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
                     set both to run the real-model index/reindex tests."
                );
                None
            }
        }
    }

    /// Install the real default model into `layout` by symlinking it out of
    /// `LOCAL_RAG_TEST_MODEL_HOME` (never copied: the fixture weights are
    /// ~295 MB and this only runs when explicitly opted into locally).
    fn install_real_model(layout: &StoreLayout, model_home: &str) {
        let src = PathBuf::from(model_home)
            .join("models")
            .join(DEFAULT_MODEL_ID);
        assert!(
            src.join(".ok").is_file(),
            "{}: LOCAL_RAG_TEST_MODEL_HOME must already have {DEFAULT_MODEL_ID} installed",
            src.display()
        );
        let dst = layout.model_dir(DEFAULT_MODEL_ID);
        std::fs::create_dir_all(dst.parent().expect("models dir has a parent"))
            .expect("create models/ parent");
        std::os::unix::fs::symlink(&src, &dst).expect("symlink installed model");
    }

    fn run_cli_with_ort(home: &TempHome, dir: &Path, dylib: &str, args: &[&str]) -> Output {
        let mut cmd = home.command(env!("CARGO_BIN_EXE_local-rag"));
        cmd.args(args);
        cmd.current_dir(dir);
        cmd.env("ORT_DYLIB_PATH", dylib);
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.output().expect("run local-rag")
    }

    #[test]
    fn index_new_path_creates_a_queryable_worktree_then_reuses_it_on_a_second_run() {
        let Some((dylib, model_home)) = require_env() else {
            return;
        };
        let (home, layout) = open_layout();
        install_real_model(&layout, &model_home);
        let init = run_cli_with_ort(&home, home.path(), &dylib, &["init"]);
        assert_eq!(init.status.code(), Some(0), "{init:?}");

        let target = home.join("project");
        std::fs::create_dir_all(&target).expect("create target dir");
        std::fs::write(
            target.join("main.rs"),
            "fn parse_config(path: &str) -> String { path.to_string() }",
        )
        .expect("seed file");
        // D-096: one file of each fate, so the summary line has to name all
        // three. Before D-096 this tree printed the same "indexed 1 files" as a
        // tree with nothing else in it.
        std::fs::write(target.join("notes.md"), "# notes\n").expect("seed a deferred file");
        std::fs::write(target.join("blob.rs"), b"fn a() {}\0x").expect("seed a skipped file");

        let first = run_cli_with_ort(
            &home,
            home.path(),
            &dylib,
            &["index", target.to_str().unwrap()],
        );
        assert_eq!(first.status.code(), Some(0), "{first:?}");
        let summary = String::from_utf8_lossy(&first.stdout).to_string();
        assert!(summary.contains("indexed 1 files"), "{summary}");
        assert!(
            summary.contains("skipped 1 (1 binary)"),
            "the skip and its reason are named: {summary}"
        );
        assert!(
            summary.contains("deferred 1 (no v0 language)"),
            "the file no report used to mention is named: {summary}"
        );

        // A second `index` of the same path must resolve to the *same*
        // worktree rather than registering a duplicate.
        let second = run_cli_with_ort(
            &home,
            home.path(),
            &dylib,
            &["index", target.to_str().unwrap()],
        );
        assert_eq!(second.status.code(), Some(0), "{second:?}");

        let conn = rusqlite::Connection::open_with_flags(
            layout.state_db(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .expect("open state.sqlite read-only");
        let worktrees: i64 = conn
            .query_row("SELECT count(*) FROM worktree", [], |r| r.get(0))
            .expect("count worktrees");
        assert_eq!(
            worktrees, 1,
            "indexing the same path twice must not duplicate identity"
        );
        let generations: i64 = conn
            .query_row("SELECT count(*) FROM generation", [], |r| r.get(0))
            .expect("count generations");
        assert_eq!(generations, 2, "each index run builds its own generation");
    }

    #[test]
    fn reindex_picks_up_a_changed_file() {
        let Some((dylib, model_home)) = require_env() else {
            return;
        };
        let (home, layout) = open_layout();
        install_real_model(&layout, &model_home);
        let init = run_cli_with_ort(&home, home.path(), &dylib, &["init"]);
        assert_eq!(init.status.code(), Some(0), "{init:?}");

        let target = home.join("project");
        std::fs::create_dir_all(&target).expect("create target dir");
        let file = target.join("lib.rs");
        std::fs::write(&file, "fn one() {}").expect("seed file");

        let first = run_cli_with_ort(
            &home,
            home.path(),
            &dylib,
            &["index", target.to_str().unwrap()],
        );
        assert_eq!(first.status.code(), Some(0), "{first:?}");

        std::fs::write(&file, "fn one() {}\nfn two() {}\nfn three() {}").expect("modify file");

        let second = run_cli_with_ort(&home, &target, &dylib, &["reindex"]);
        assert_eq!(second.status.code(), Some(0), "{second:?}");

        let conn = rusqlite::Connection::open_with_flags(
            layout.state_db(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .expect("open state.sqlite read-only");
        let generations: i64 = conn
            .query_row("SELECT count(*) FROM generation", [], |r| r.get(0))
            .expect("count generations");
        assert_eq!(
            generations, 2,
            "reindex after a real change builds a new generation"
        );
    }
}
