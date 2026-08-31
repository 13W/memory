//! T20-06's own mandatory acceptance scenario (group card, "Тесты"): two
//! `managed_worktree` rows indexed **in parallel by a real, second OS
//! process** — the direct test of the group's original complaint (two
//! concurrently-managed projects, one daemon). Everything else in this
//! group is exercised in-process (`tests/indexing_supervisor.rs`); this file
//! is the one place that spawns a genuine `local-rag serve` child, mirroring
//! `tests/serve_subprocess.rs`'s own established harness
//! (`TempHome::command(env!("CARGO_BIN_EXE_local-rag"))`, poll `store.lock`
//! for `ready: true`).
//!
//! A real ONNX model, not a test fixture, is unavoidable here: a worktree
//! task skips its own `project_generation` call entirely for as long as
//! `LazyEmbedderProvider` (T20-03) — the real `local-rag serve` binary's own
//! production constructor, with no CLI/env seam to substitute a fake
//! provider — reports the model not-yet-ready (`daemon::indexing::
//! worktree_task::project_one`'s own doc). So, like
//! `tests/offline_search_recall.rs`'s own `with_real_model` module, this
//! test is opt-in: it skips (does not fail) unless `ORT_DYLIB_PATH` and
//! `LOCAL_RAG_TEST_MODEL_HOME` are set to a machine that already has the
//! default model's ~295 MB of weights on disk.

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
                 set both to run T20-06's real-subprocess managed-indexing test."
            );
            None
        }
    }
}

/// Install the real default model into `layout` by symlinking it out of
/// `LOCAL_RAG_TEST_MODEL_HOME` — mirrors
/// `tests/offline_search_recall.rs::with_real_model::install_real_model`
/// exactly (never copied: the fixture weights are ~295 MB and this only runs
/// when explicitly opted into locally).
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

/// Run a `local-rag` subcommand to completion with `ORT_DYLIB_PATH` set and
/// outbound HTTP tripwired shut — mirrors `with_real_model::run_cli_with_ort`.
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

/// Mirrors `tests/serve_subprocess.rs::wait_until_ready` exactly.
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
        uuidv7_from(9_900_000 + n, [0x57; 10])
    }
}

fn tool_call(id: u32, query: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"search_code","arguments":{{"query":"{query}"}}}}}}"#
    )
}

#[tokio::test]
async fn two_managed_worktrees_are_indexed_in_parallel_by_one_real_daemon_process() {
    let Some((dylib, model_home)) = require_env() else {
        return;
    };
    let (home, layout) = open_layout();
    install_real_model(&layout, &model_home);

    let init = run_cli_with_ort(&home, &dylib, &["init"]);
    assert_eq!(init.status.code(), Some(0), "{init:?}");

    // Two real, distinct (non-git) worktree roots, each with one file whose
    // identifier is unique to it — pre-registered and enrolled directly
    // against `state.sqlite`, exactly the way a live `local-rag project add`
    // (T20-08, not yet built) would, before the daemon ever opens the store.
    let uuids = SeqUuids::new();
    let now_ms = 1_000;
    let root_a = home.join("project-a");
    let root_b = home.join("project-b");
    std::fs::create_dir_all(&root_a).expect("create project-a");
    std::fs::create_dir_all(&root_b).expect("create project-b");
    std::fs::write(root_a.join("main.rs"), "fn alpha_marker_function() {}\n")
        .expect("seed project-a");
    std::fs::write(root_b.join("main.rs"), "fn beta_marker_function() {}\n")
        .expect("seed project-b");

    {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        for root in [&root_a, &root_b] {
            let repo_id = uuids.next_uuid();
            let worktree_id = uuids.next_uuid();
            let facts = facts_for(root);
            local_rag::indexing::register_new_worktree(
                &state,
                repo_id,
                worktree_id,
                &facts,
                now_ms,
            )
            .await
            .expect("register worktree");
            let worktree_id_str = worktree_id.to_string();
            state
                .writer()
                .transaction(move |tx| register_managed_worktree(tx, &worktree_id_str, now_ms))
                .await
                .expect("enroll managed worktree");
        }
    }

    let mut child = spawn_serve(&home, &dylib);
    wait_until_ready(&layout, Duration::from_secs(30));

    let socket_path = layout.socket_path();
    let repo_a = root_a.to_string_lossy().into_owned();
    let repo_b = root_b.to_string_lossy().into_owned();

    // Bounded, event-driven wait for both worktrees to become independently
    // searchable — the supervisor's own two background tasks, not this test,
    // do the actual indexing; this only polls the outcome through the real
    // MCP wire protocol, exactly as a client would.
    let deadline = Instant::now() + Duration::from_secs(60);
    let (mut found_a, mut found_b) = (false, false);
    while !(found_a && found_b) {
        if Instant::now() >= deadline {
            send_sigterm(child.id());
            let _ = wait_for_exit(&mut child, Duration::from_secs(20));
            panic!(
                "both managed worktrees did not become searchable within the bound \
                 (found_a={found_a}, found_b={found_b})"
            );
        }
        let (socket_path, repo_a, repo_b) = (socket_path.clone(), repo_a.clone(), repo_b.clone());
        let (body_a, body_b) = tokio::task::spawn_blocking(move || {
            let mut client = Client::connect(&socket_path);
            let body_a =
                client.call_and_read(&tool_call(1, "alpha_marker_function"), Some(&repo_a));
            let body_b = client.call_and_read(&tool_call(2, "beta_marker_function"), Some(&repo_b));
            (body_a, body_b)
        })
        .await
        .expect("blocking task");

        found_a = result_contains(&body_a, "main.rs");
        found_b = result_contains(&body_b, "main.rs");
        if !(found_a && found_b) {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    send_sigterm(child.id());
    let status = wait_for_exit(&mut child, Duration::from_secs(20));
    assert!(status.success(), "must exit cleanly: {status:?}");
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
