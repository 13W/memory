//! `local-rag index`/`reindex`/`watch`'s stderr dual-indexing advisory
//! (spec 11 §6, T20-09), driving the real compiled binary — mirrors
//! `tests/cli_index.rs`'s own `open_layout`/`run_cli`/`seed_active_worktree`
//! shapes and `tests/cli_service.rs`'s/`tests/cli_project.rs`'s
//! `spawn_serve`/`wait_until_ready` for the daemon-alive half of the matrix
//! (duplicated here per this crate's established per-file-fixture
//! convention).
//!
//! None of these tests need a real installed model: `finish_index_ctx`
//! (`indexing/mod.rs:374-386`) returns a clean `Err("... is not installed;
//! run \`local-rag init --download-models\` first")` when the default model
//! is absent — which every test `LOCAL_RAG_HOME` is, unconditionally — and
//! the advisory itself is printed *before* `finish_index_ctx` is ever
//! called. This means every combination in the matrix below fails the same
//! way for the same reason, which is exactly what proves the advisory never
//! changes the command's own exit code (the card's own requirement).

#![cfg(unix)]

use std::path::Path;
use std::process::{Child, Output, Stdio};
use std::time::{Duration, Instant};

use local_rag::daemon::gitroot;
use local_rag_core::identity::{SystemUuidV7, Uuid, UuidSource};
use local_rag_core::paths::StoreLayout;
use local_rag_store::{
    StateDb, create_repository, create_worktree, insert_projection_state, observe_repository_path,
    observe_worktree_path, register_managed_worktree, unregister_managed_worktree,
};
use local_rag_test_support::TempHome;

const ADVISORY_SNIPPET: &str = "local-rag project reindex";

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

fn spawn_serve(home: &TempHome) -> Child {
    let mut cmd = home.command(env!("CARGO_BIN_EXE_local-rag"));
    cmd.arg("serve");
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.spawn().expect("spawn local-rag serve")
}

fn wait_until_ready(layout: &StoreLayout, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(bytes) = std::fs::read(layout.store_lock())
            && let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes)
            && json.get("ready").and_then(|v| v.as_bool()) == Some(true)
        {
            break;
        }
        if Instant::now() >= deadline {
            panic!("store.lock did not become ready within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    // `store.lock`'s own `ready:true` (spec 02 §4.1 step 4) only proves the
    // listener has bound and the readiness marker was written — under this
    // machine's own well-documented shared-load contention, the accept loop
    // can still take a moment longer to actually start servicing new
    // connections. `advise_if_daemon_managed` (T20-09) performs this exact
    // `fetch_welcome` round trip internally; waiting for it here too, using
    // the same production call, closes that gap rather than racing it.
    let deadline = Instant::now() + timeout;
    loop {
        if local_rag::daemon::fetch_welcome(&layout.socket_path(), Duration::from_millis(500))
            .is_some()
        {
            return;
        }
        if Instant::now() >= deadline {
            panic!("daemon did not answer a WELCOME handshake within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn tokio_test_block_on<F: std::future::Future>(fut: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(fut)
}

/// Register one active `{repo, worktree}` whose *current* path is `path` —
/// the same seeding `tests/cli_index.rs::seed_active_worktree` does, so
/// `reindex`/`watch` resolve it as `Resolution::Resolved` without needing
/// `index <path>` to run first (which would need a real embedder). Returns
/// the worktree id so callers can toggle its `managed_worktree` row.
async fn seed_active_worktree(layout: &StoreLayout, path: &Path) -> Uuid {
    let facts = gitroot::probe(path).expect("probe the seeded path");
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    let repo_id = SystemUuidV7.next_uuid();
    let worktree_id = SystemUuidV7.next_uuid();
    let (repo_id_s, worktree_id_s) = (repo_id.to_string(), worktree_id.to_string());
    db.writer()
        .transaction(move |tx| {
            create_repository(tx, &repo_id_s, None, 1_000)?;
            create_worktree(tx, &worktree_id_s, &repo_id_s, facts.kind, 1_000)?;
            observe_worktree_path(
                tx,
                &worktree_id_s,
                &facts.observed_canonical_path,
                &facts.display_path,
                &facts.path_fingerprint,
                1_000,
            )?;
            observe_repository_path(tx, &repo_id_s, &facts.observed_canonical_path, 1_000)?;
            insert_projection_state(tx, &worktree_id_s, 1_000)
        })
        .await
        .expect("seed active worktree");
    worktree_id
}

async fn set_managed(layout: &StoreLayout, worktree_id: Uuid, managed: bool) {
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    let id = worktree_id.to_string();
    if managed {
        db.writer()
            .transaction(move |tx| register_managed_worktree(tx, &id, 2_000))
            .await
            .expect("mark managed");
    } else {
        db.writer()
            .transaction(move |tx| unregister_managed_worktree(tx, &id).map(|_| ()))
            .await
            .expect("unmark managed");
    }
}

/// The card's own required matrix, walked as one scenario (one seeded
/// worktree, one daemon spawned partway through) rather than four separate
/// tests — cheaper, and it directly proves the exit code is identical
/// across every combination, not just asserted per-test by coincidence.
#[test]
fn reindex_advisory_appears_exactly_when_managed_and_daemon_alive() {
    let (home, layout) = open_layout();
    let target = home.join("repo");
    std::fs::create_dir_all(&target).expect("create target dir");
    let worktree_id = tokio_test_block_on(seed_active_worktree(&layout, &target));

    // 1. not managed, no daemon.
    let out = run_cli(&home, &target, &["reindex"]);
    assert!(!stderr(&out).contains(ADVISORY_SNIPPET), "{:?}", out);
    let baseline_exit = out.status.code();
    assert_ne!(baseline_exit, Some(0), "{out:?}");

    // 2. managed, no daemon.
    tokio_test_block_on(set_managed(&layout, worktree_id, true));
    let out = run_cli(&home, &target, &["reindex"]);
    assert!(!stderr(&out).contains(ADVISORY_SNIPPET), "{:?}", out);
    assert_eq!(out.status.code(), baseline_exit, "{out:?}");

    // 3. managed, daemon alive.
    let mut daemon = spawn_serve(&home);
    wait_until_ready(&layout, Duration::from_secs(20));
    let out = run_cli(&home, &target, &["reindex"]);
    assert!(stderr(&out).contains(ADVISORY_SNIPPET), "{:?}", out);
    assert_eq!(out.status.code(), baseline_exit, "{out:?}");

    // 4. not managed, daemon alive.
    tokio_test_block_on(set_managed(&layout, worktree_id, false));
    let out = run_cli(&home, &target, &["reindex"]);
    assert!(!stderr(&out).contains(ADVISORY_SNIPPET), "{:?}", out);
    assert_eq!(out.status.code(), baseline_exit, "{out:?}");

    let _ = daemon.kill();
    let _ = daemon.wait();
}

#[test]
fn index_prints_the_advisory_for_a_managed_path_with_a_live_daemon() {
    let (home, layout) = open_layout();
    let target = home.join("repo");
    std::fs::create_dir_all(&target).expect("create target dir");
    let worktree_id = tokio_test_block_on(seed_active_worktree(&layout, &target));
    tokio_test_block_on(set_managed(&layout, worktree_id, true));

    let mut daemon = spawn_serve(&home);
    wait_until_ready(&layout, Duration::from_secs(20));

    let out = run_cli(&home, home.path(), &["index", target.to_str().unwrap()]);
    assert!(stderr(&out).contains(ADVISORY_SNIPPET), "{:?}", out);

    let _ = daemon.kill();
    let _ = daemon.wait();
}

#[test]
fn watch_prints_the_advisory_for_a_managed_path_with_a_live_daemon() {
    let (home, layout) = open_layout();
    let target = home.join("repo");
    std::fs::create_dir_all(&target).expect("create target dir");
    let worktree_id = tokio_test_block_on(seed_active_worktree(&layout, &target));
    tokio_test_block_on(set_managed(&layout, worktree_id, true));

    let mut daemon = spawn_serve(&home);
    wait_until_ready(&layout, Duration::from_secs(20));

    // `watch` fails fast on the same missing-model error before it ever
    // enters its continuous loop, so this is a plain one-shot `.output()`
    // call — no process to kill afterward.
    let out = run_cli(&home, &target, &["watch"]);
    assert!(stderr(&out).contains(ADVISORY_SNIPPET), "{:?}", out);

    let _ = daemon.kill();
    let _ = daemon.wait();
}
