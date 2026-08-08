//! `local-rag watch` acceptance tests (spec 11 §6, spec 06 §1), driving the
//! real compiled binary — mirrors `tests/cli_index.rs`'s own `open_layout`/
//! `run_cli`/seeding helpers (duplicated here per this crate's established
//! per-file-fixture convention).
//!
//! `watch` shares `local_rag::indexing`'s own `open_state`/`resolve_facts`
//! split (T20-02), so
//! everything except the actual watch loop (`GlobalOnly`/`Ambiguous`
//! refusal, the "model not installed" gate) is reachable and asserted here
//! without ONNX. The live reconcile → embed → activate → materialize loop
//! itself needs the real default model and a real ONNX Runtime, so it is
//! env-gated below — see `tests/cli_init.rs`'s own module doc for the
//! precedent this follows.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Child, Output, Stdio};
use std::time::{Duration, Instant};

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

/// Seed one active `{repo, worktree}` whose *current* path is `path` — see
/// `tests/cli_index.rs::seed_active_worktree` for why `gitroot::probe` (not
/// hand-rolled canonicalization) is the source of truth here.
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

/// Seed a `detached` reattach candidate — see
/// `tests/cli_index.rs::seed_detached_reattach_candidate` for the full
/// rationale (register at `original`, observe a move to `elsewhere`, mark
/// `detached`; probing `original` again then surfaces it as an advisory
/// candidate rather than auto-resolving).
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

fn tokio_test_block_on<F: std::future::Future>(fut: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(fut)
}

#[test]
fn watch_rejects_any_argument() {
    let (home, _layout) = open_layout();
    let output = run_cli(&home, home.path(), &["watch", "extra"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
}

#[test]
fn watch_in_an_unindexed_cwd_reports_not_indexed() {
    let (home, _layout) = open_layout();
    let target = home.join("project");
    std::fs::create_dir_all(&target).expect("create target dir");

    let output = run_cli(&home, &target, &["watch"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("run `local-rag index"),
        "{:?}",
        output.stderr
    );
}

#[test]
fn watch_on_a_resolved_worktree_reports_the_model_is_not_installed() {
    let (home, layout) = open_layout();
    let target = home.join("project");
    std::fs::create_dir_all(&target).expect("create target dir");
    tokio_test_block_on(seed_active_worktree(&layout, &target));

    let output = run_cli(&home, &target, &["watch"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("is not installed"),
        "{:?}",
        output.stderr
    );
}

#[test]
fn watch_ambiguous_cwd_is_refused_and_suggests_repo_attach() {
    let (home, layout) = open_layout();
    let target = home.join("project");
    let elsewhere = home.join("project-moved");
    std::fs::create_dir_all(&target).expect("create target dir");
    tokio_test_block_on(seed_detached_reattach_candidate(
        &layout, &target, &elsewhere,
    ));

    let output = run_cli(&home, &target, &["watch"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("repo attach"),
        "{:?}",
        output.stderr
    );
}

/// Real end-to-end runs through the compiled binary with the real default
/// model — see this file's own module doc for why this is env-gated.
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
                     set both to run the real-model watch test."
                );
                None
            }
        }
    }

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

    fn spawn_watch(home: &TempHome, dir: &Path, dylib: &str) -> Child {
        let mut cmd = home.command(env!("CARGO_BIN_EXE_local-rag"));
        cmd.arg("watch");
        cmd.current_dir(dir);
        cmd.env("ORT_DYLIB_PATH", dylib);
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.spawn().expect("spawn local-rag watch")
    }

    fn generation_count(layout: &StoreLayout) -> i64 {
        let conn = rusqlite::Connection::open_with_flags(
            layout.state_db(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .expect("open state.sqlite read-only");
        conn.query_row("SELECT count(*) FROM generation", [], |r| r.get(0))
            .expect("count generations")
    }

    fn wait_for_generation_count(layout: &StoreLayout, at_least: i64, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            if generation_count(layout) >= at_least {
                return;
            }
            if Instant::now() >= deadline {
                panic!(
                    "generation count did not reach {at_least} within {timeout:?} (currently {})",
                    generation_count(layout)
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn watch_reconciles_on_startup_and_on_a_file_change_then_stops_on_sigterm() {
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

        let index = run_cli_with_ort(
            &home,
            home.path(),
            &dylib,
            &["index", target.to_str().unwrap()],
        );
        assert_eq!(index.status.code(), Some(0), "{index:?}");
        assert_eq!(generation_count(&layout), 1);

        let mut watch = spawn_watch(&home, &target, &dylib);

        // Cold-start (`TriggerKind::Startup`) reconcile: the tree has not
        // changed, so `build_generation`'s structural sharing means this may
        // or may not mint a *new* generation row depending on dedup — what
        // matters here is only that the process comes up and starts serving
        // triggers, asserted by the file-change reconcile below actually
        // landing.
        std::thread::sleep(Duration::from_millis(300));

        std::fs::write(&file, "fn one() {}\nfn two() {}").expect("modify file");
        wait_for_generation_count(&layout, 2, Duration::from_secs(15));

        // SAFETY: `kill` with a valid pid and signal number is a plain,
        // side-effect-documented syscall; no memory is read or written.
        #[allow(unsafe_code)]
        let rc = unsafe { libc::kill(watch.id() as libc::pid_t, libc::SIGTERM) };
        assert_eq!(rc, 0, "SIGTERM delivery failed");

        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = watch.try_wait().expect("try_wait") {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = watch.kill();
                panic!("watch did not exit within 10s of SIGTERM");
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        assert!(status.success(), "{status:?}");
    }
}
