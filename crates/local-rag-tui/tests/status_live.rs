//! T18-02's live-probe test: spawn a real `local-rag serve` subprocess and call this crate's own
//! `local_rag_tui::status` functions directly against it — not a CLI subprocess whose stdout gets
//! parsed, the actual code path this crate's `main.rs` runs. Mirrors the `local_rag_binary_path`/
//! `spawn_serve`/`wait_until_ready`/`stop_serve` pattern `crates/local-rag-hook/tests/
//! recall_rpc.rs` already established (`CARGO_BIN_EXE_local-rag` is not set for another package's
//! binary regardless of dev-vs-normal dependency edge — verified empirically there).

#![cfg(unix)]

use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use local_rag_core::paths::StoreLayout;
use local_rag_test_support::TempHome;
use local_rag_tui::status::{DaemonStatus, DurableCounts, probe_daemon, read_durable_counts};

fn open_layout() -> (TempHome, StoreLayout) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    (home, layout)
}

/// Locate the real `local-rag` binary next to this integration test binary.
///
/// `env!("CARGO_BIN_EXE_local-rag")` is not set for another package's binary just because it is a
/// (dev- or normal-) dependency (verified empirically — see `local-rag-hook/tests/recall_rpc.rs`'s
/// own doc comment on this). A cargo test binary lives at `target/<profile>/deps/<name>-<hash>`,
/// and a regular binary target lives one directory up, at `target/<profile>/<name>` — the same
/// "ships side by side" layout spec 13 §1 describes for the real npm-packaged distribution.
fn local_rag_binary_path() -> std::path::PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    let deps_dir = exe.parent().expect("deps dir");
    let profile_dir = deps_dir.parent().expect("profile dir");
    let candidate = profile_dir.join("local-rag");
    assert!(
        candidate.is_file(),
        "expected a sibling local-rag binary at {candidate:?} (built via this package's own \
         local-rag dependency)"
    );
    candidate
}

fn spawn_serve(home: &TempHome) -> Child {
    let mut cmd = home.command(local_rag_binary_path());
    cmd.arg("serve");
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.spawn().expect("spawn local-rag serve")
}

/// Poll `store.lock` until it parses with `ready: true`, or panic after `timeout` — real, bounded
/// wall-clock waiting inherent to driving a real child process, not a `sleep` stand-in for it.
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

fn stop_serve(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn status_is_running_against_a_real_serve_subprocess() {
    let (home, layout) = open_layout();
    let daemon = spawn_serve(&home);
    wait_until_ready(&layout, Duration::from_secs(20));

    let daemon_status = probe_daemon(&layout, Duration::from_secs(1));
    match daemon_status {
        DaemonStatus::Running {
            daemon_mode,
            socket_path,
            pid,
            ..
        } => {
            assert_eq!(daemon_mode, "normal");
            assert_eq!(socket_path, layout.socket_path().display().to_string());
            assert!(pid > 0);
        }
        other => {
            stop_serve(daemon);
            panic!("expected DaemonStatus::Running, got {other:?}");
        }
    }

    // Durable counts read the same live store concurrently — a regression check on ADR-0008's own
    // WAL + busy_timeout claim: a read-only connection never conflicts with the daemon's writer.
    let durable = read_durable_counts(&layout, home.path());
    match durable {
        DurableCounts::Available { .. } => {}
        other => {
            stop_serve(daemon);
            panic!("expected DurableCounts::Available, got {other:?}");
        }
    }

    stop_serve(daemon);
}
