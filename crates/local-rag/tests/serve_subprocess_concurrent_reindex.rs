//! G20's own mandatory acceptance scenario (`groups/20-daemon-managed-
//! indexing.md`, gate checklist): a live daemon managing a worktree **and**
//! a concurrently-run external `local-rag reindex` of that same worktree —
//! confirmed by the gate's own diagnosis to be uncovered by any existing
//! test. `tests/serve_subprocess_managed_indexing.rs` (T20-06) tests two
//! *different* worktrees indexed in parallel by one daemon; this file is
//! the one place that races a real second OS process against the daemon's
//! own background task on the identical worktree.
//!
//! Mirrors `serve_subprocess_managed_indexing.rs`'s own harness almost
//! verbatim (`require_env`, `install_real_model`, `run_cli_with_ort`,
//! `spawn_serve`, `wait_until_ready`, `wait_for_exit`, `send_sigterm`,
//! `facts_for`, `SeqUuids`, `Client`/`tool_call`/`result_contains`),
//! duplicated here per this crate's established per-file-fixture
//! convention — same reason, same `ORT_DYLIB_PATH`/`LOCAL_RAG_TEST_MODEL_HOME`
//! opt-in contract (a real embedder is unavoidable: a worktree task skips
//! `project_generation` entirely until `LazyEmbedderProvider` reports ready).
//!
//! What this test actually probes: the standalone CLI's own `run_pipeline`
//! (`cli/index.rs`) calls `index_worktree` directly, with **no**
//! `write_locked`/`WorktreeLockRegistry` wrapper — that in-process Rust lock
//! cannot protect two separate OS processes from each other regardless. The
//! only real protection here is SQLite's own WAL mode + `busy_timeout=5000`
//! plus the generation/switch model being additive by construction
//! (`cli/mod.rs`'s own module doc: "concurrent indexers of the *same*
//! worktree are wasteful, never unsafe") — a claim that has existed since
//! T15-07 but had never been exercised by a genuine two-process race before
//! this test.

#![cfg(unix)]

mod support;

use std::path::{Path, PathBuf};
use std::process::{Child, Output, Stdio};
use std::time::{Duration, Instant};

use local_rag_core::identity::domain::path_fingerprint;
use local_rag_core::identity::{UuidSource, uuidv7_from};
use local_rag_core::paths::StoreLayout;
use local_rag_models::DEFAULT_MODEL_ID;
use local_rag_store::{StateDb, WorktreeKind, WorktreeRootFacts, register_managed_worktree};
use local_rag_test_support::TempHome;
use support::{Client, open_layout};

fn require_env() -> Option<(String, String)> {
    let dylib = std::env::var("ORT_DYLIB_PATH").ok();
    let model_home = std::env::var("LOCAL_RAG_TEST_MODEL_HOME").ok();
    match (dylib, model_home) {
        (Some(d), Some(m)) => Some((d, m)),
        _ => {
            eprintln!(
                "SKIP: ORT_DYLIB_PATH and/or LOCAL_RAG_TEST_MODEL_HOME are unset — \
                 set both to run G20's real-subprocess concurrent-reindex test."
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

fn run_cli_with_ort(home: &TempHome, dylib: &str, args: &[&str]) -> Output {
    let mut cmd = home.command(env!("CARGO_BIN_EXE_local-rag"));
    cmd.args(args);
    cmd.env("ORT_DYLIB_PATH", dylib);
    cmd.env("http_proxy", "http://127.0.0.1:1");
    cmd.env("https_proxy", "http://127.0.0.1:1");
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.output().expect("run local-rag")
}

/// Like [`run_cli_with_ort`], but spawned rather than awaited, with an
/// explicit `dir` — used only for the external `reindex` this test races
/// against the daemon's own watcher. `local-rag reindex` takes no path
/// argument at all (`cli::index::run_reindex`); it resolves the worktree
/// from its own current directory, so `dir` must be set on the child
/// process itself, not inherited from the test binary's own cwd.
fn spawn_cli_with_ort(home: &TempHome, dylib: &str, dir: &Path, args: &[&str]) -> Child {
    let mut cmd = home.command(env!("CARGO_BIN_EXE_local-rag"));
    cmd.args(args);
    cmd.current_dir(dir);
    cmd.env("ORT_DYLIB_PATH", dylib);
    cmd.env("http_proxy", "http://127.0.0.1:1");
    cmd.env("https_proxy", "http://127.0.0.1:1");
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.spawn().expect("spawn local-rag reindex")
}

fn spawn_serve(home: &TempHome, dylib: &str) -> Child {
    let mut cmd = home.command(env!("CARGO_BIN_EXE_local-rag"));
    cmd.arg("serve");
    cmd.env("RUST_LOG", "info");
    cmd.env("ORT_DYLIB_PATH", dylib);
    cmd.env("http_proxy", "http://127.0.0.1:1");
    cmd.env("https_proxy", "http://127.0.0.1:1");
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
            return;
        }
        if Instant::now() >= deadline {
            panic!("store.lock did not become ready within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_exit(child: &mut Child, timeout: Duration) -> std::process::ExitStatus {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("child did not exit within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn send_sigterm(pid: u32) {
    // SAFETY: `kill` with a valid pid and signal number is a plain,
    // side-effect-documented syscall; no memory is read or written.
    #[allow(unsafe_code)]
    let rc = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    assert_eq!(rc, 0, "kill(SIGTERM) failed");
}

fn facts_for(root: &Path) -> WorktreeRootFacts {
    let path = root.display().to_string();
    WorktreeRootFacts {
        observed_canonical_path: path.clone(),
        display_path: path.clone(),
        path_fingerprint: path_fingerprint(&path),
        kind: WorktreeKind::NonGit,
        common_dir_fingerprint: None,
        remote_fingerprint: None,
    }
}

struct SeqUuids {
    counter: std::sync::atomic::AtomicU64,
}

impl SeqUuids {
    fn new() -> Self {
        SeqUuids {
            counter: std::sync::atomic::AtomicU64::new(0),
        }
    }
}

impl UuidSource for SeqUuids {
    fn next_uuid(&self) -> local_rag_core::identity::Uuid {
        let n = self
            .counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        uuidv7_from(9_950_000 + n, [0x58; 10])
    }
}

fn tool_call(id: u32, query: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"search_code","arguments":{{"query":"{query}"}}}}}}"#
    )
}

fn result_contains(body: &serde_json::Value, needle: &str) -> bool {
    if body["result"]["isError"] != serde_json::Value::Bool(false) {
        return false;
    }
    let Some(text) = body["result"]["content"][0]["text"].as_str() else {
        return false;
    };
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(text) else {
        return false;
    };
    parsed["results"].as_array().is_some_and(|results| {
        results
            .iter()
            .any(|r| r["path"].as_str().is_some_and(|p| p.contains(needle)))
    })
}

/// Poll `search_code` for `needle` until it appears (bounded) — the same
/// event-driven-poll idiom `serve_subprocess_managed_indexing.rs` already
/// establishes, reused here for both the baseline generation and the
/// post-race settle.
fn wait_until_searchable(socket_path: &Path, repo_root: &str, needle: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let body = {
            let mut client = Client::connect(socket_path);
            client.call_and_read(&tool_call(1, needle), Some(repo_root))
        };
        if result_contains(&body, "main.rs") {
            return;
        }
        if Instant::now() >= deadline {
            panic!("worktree never became searchable for {needle:?} within {timeout:?}: {body}");
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Count of `generation` rows in state `active` for `worktree_id` — the
/// literal "итоговое активное поколение — одно" check, not just trusting
/// `worktree_projection_state.active_generation_id`'s own pointer (which
/// could in principle point at a stale/wrong row if something upstream were
/// broken; counting the source of truth directly is the stronger check).
fn active_generation_count(layout: &StoreLayout, worktree_id: &str) -> i64 {
    let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
    let conn = state.open_read().expect("open a read connection");
    conn.query_row(
        "SELECT COUNT(*) FROM generation WHERE worktree_id = ?1 AND state = 'active'",
        rusqlite::params![worktree_id],
        |r| r.get(0),
    )
    .expect("count active generations")
}

#[tokio::test]
async fn a_live_daemon_and_an_external_reindex_race_the_same_worktree_safely() {
    let Some((dylib, model_home)) = require_env() else {
        return;
    };
    let (home, layout) = open_layout();
    install_real_model(&layout, &model_home);

    let init = run_cli_with_ort(&home, &dylib, &["init"]);
    assert_eq!(init.status.code(), Some(0), "{init:?}");

    let uuids = SeqUuids::new();
    let now_ms = 1_000;
    let root = home.join("racer");
    std::fs::create_dir_all(&root).expect("create racer dir");
    std::fs::write(root.join("main.rs"), "fn racer_marker_one() {}\n").expect("seed racer");

    let worktree_id = {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        let repo_id = uuids.next_uuid();
        let worktree_id = uuids.next_uuid();
        let facts = facts_for(&root);
        local_rag::indexing::register_new_worktree(&state, repo_id, worktree_id, &facts, now_ms)
            .await
            .expect("register worktree");
        let worktree_id_str = worktree_id.to_string();
        state
            .writer()
            .transaction(move |tx| register_managed_worktree(tx, &worktree_id_str, now_ms))
            .await
            .expect("enroll managed worktree");
        worktree_id
    };
    let worktree_id_str = worktree_id.to_string();

    let mut daemon = spawn_serve(&home, &dylib);
    wait_until_ready(&layout, Duration::from_secs(30));

    let socket_path = layout.socket_path();
    let repo_root = root.to_string_lossy().into_owned();

    // Baseline: the supervisor's own startup reconcile must finish before
    // this test forces a race — otherwise a "not found" result later would
    // be ambiguous between "the race broke something" and "the daemon just
    // hadn't indexed yet."
    wait_until_searchable(
        &socket_path,
        &repo_root,
        "racer_marker_one",
        Duration::from_secs(60),
    );
    assert_eq!(
        active_generation_count(&layout, &worktree_id_str),
        1,
        "baseline generation must be singly active before the race begins"
    );

    // Force real two-process concurrency: change the file (which the
    // daemon's own filesystem watcher will pick up on its own debounce) and
    // spawn an external `local-rag reindex` of the identical worktree at
    // essentially the same instant — no attempt to hit an exact interleaving
    // (impossible to guarantee across two OS processes), only a realistic
    // window of overlap. The assertions below check final-state invariants
    // that must hold regardless of exactly how the two writers interleaved.
    std::fs::write(root.join("main.rs"), "fn racer_marker_two() {}\n").expect("modify racer");
    let mut external_reindex = spawn_cli_with_ort(&home, &dylib, &root, &["reindex"]);

    let reindex_status = wait_for_exit(&mut external_reindex, Duration::from_secs(60));
    assert!(
        reindex_status.success(),
        "external `local-rag reindex` must exit cleanly even racing the daemon: {reindex_status:?}"
    );

    // Let the daemon's own watcher-triggered cycle (if it hasn't already
    // finished) settle too.
    wait_until_searchable(
        &socket_path,
        &repo_root,
        "racer_marker_two",
        Duration::from_secs(60),
    );

    // `local-rag doctor` — clean (exit 0) is `DoctorReport::is_clean()`,
    // which already runs `check_dense` (shard validate-on-open) per
    // worktree as part of its `heads` finding — no separate shard check
    // needed.
    let doctor = run_cli_with_ort(&home, &dylib, &["doctor"]);
    assert_eq!(
        doctor.status.code(),
        Some(0),
        "doctor must report clean: {doctor:?}"
    );

    // Search still answers correctly for the post-race content.
    wait_until_searchable(
        &socket_path,
        &repo_root,
        "racer_marker_two",
        Duration::from_secs(10),
    );

    // Exactly one active generation — no dangling double-active state from
    // the race.
    assert_eq!(
        active_generation_count(&layout, &worktree_id_str),
        1,
        "exactly one generation must be active after the race settles"
    );

    send_sigterm(daemon.id());
    let status = wait_for_exit(&mut daemon, Duration::from_secs(20));
    assert!(status.success(), "daemon must exit cleanly: {status:?}");
}
