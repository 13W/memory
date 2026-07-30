//! T15-07 acceptance tests for `local-rag status`/`stop`/`restart` (spec 11
//! §6), driving the real compiled `local-rag` binary as a genuine second OS
//! process — mirrors `tests/serve_subprocess.rs`'s own `spawn_serve`/
//! `wait_until_ready`/`wait_for_exit` shapes (duplicated here per this
//! crate's own per-file-fixture convention, not promoted to a shared
//! `support` module).

#![cfg(unix)]

use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use local_rag_core::paths::StoreLayout;
use local_rag_test_support::TempHome;

fn open_layout() -> (TempHome, StoreLayout) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    (home, layout)
}

fn spawn_serve(home: &TempHome) -> Child {
    let mut cmd = home.command(env!("CARGO_BIN_EXE_local-rag"));
    cmd.arg("serve");
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.spawn().expect("spawn local-rag serve")
}

fn run_cli(home: &TempHome, args: &[&str]) -> Output {
    let mut cmd = home.command(env!("CARGO_BIN_EXE_local-rag"));
    cmd.args(args);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.output().expect("run local-rag")
}

/// Poll `store.lock` until it parses with `ready: true`, or panic after
/// `timeout` — real, bounded wall-clock waiting inherent to driving a real
/// child process, same accepted precedent as `serve_subprocess.rs`'s own
/// `wait_until_ready`.
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

fn wait_until_not_running(layout: &StoreLayout, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if !layout.store_lock().exists() {
            return;
        }
        if Instant::now() >= deadline {
            panic!("store.lock still present after {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn status_reports_not_running_before_serve_starts() {
    let (home, _layout) = open_layout();
    let output = run_cli(&home, &["status"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("not running"),
        "{:?}",
        output.stdout
    );
}

#[test]
fn status_reports_running_once_serve_is_ready() {
    let (home, layout) = open_layout();
    let mut daemon = spawn_serve(&home);
    wait_until_ready(&layout, Duration::from_secs(20));

    let output = run_cli(&home, &["status", "--json"]);
    assert_eq!(output.status.code(), Some(0));
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("valid json on stdout");
    assert_eq!(json["state"], "running");
    assert_eq!(json["daemon_mode"], "normal");
    assert!(json["pid"].is_u64());
    assert!(json["instance_uuid"].is_string());
    assert!(json["socket_path"].is_string());

    let _ = daemon.kill();
    let _ = daemon.wait();
}

#[test]
fn stop_actually_stops_a_running_daemon() {
    let (home, layout) = open_layout();
    let mut daemon = spawn_serve(&home);
    wait_until_ready(&layout, Duration::from_secs(20));

    let output = run_cli(&home, &["stop"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");

    wait_for_exit(&mut daemon, Duration::from_secs(10));
    wait_until_not_running(&layout, Duration::from_secs(5));

    let status = run_cli(&home, &["status"]);
    assert_eq!(
        status.status.code(),
        Some(1),
        "must report not running after stop"
    );
}

#[test]
fn stop_on_a_never_started_store_is_a_no_op() {
    let (home, _layout) = open_layout();
    let output = run_cli(&home, &["stop"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("not running"));
}

#[test]
fn restart_cycles_the_process_to_a_new_pid() {
    let (home, layout) = open_layout();
    let mut daemon = spawn_serve(&home);
    wait_until_ready(&layout, Duration::from_secs(20));
    let before: serde_json::Value =
        serde_json::from_slice(&std::fs::read(layout.store_lock()).unwrap()).unwrap();
    let pid_before = before["pid"].as_u64().expect("pid");

    let output = run_cli(&home, &["restart"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");

    // The old process must actually be gone (restart's own stop step killed
    // it) — not just superseded by a new lock owner.
    wait_for_exit(&mut daemon, Duration::from_secs(10));

    wait_until_ready(&layout, Duration::from_secs(30));
    let after: serde_json::Value =
        serde_json::from_slice(&std::fs::read(layout.store_lock()).unwrap()).unwrap();
    let pid_after = after["pid"].as_u64().expect("pid");
    assert_ne!(
        pid_before, pid_after,
        "restart must spawn a genuinely new process"
    );

    // Clean up the detached new daemon.
    let _ = run_cli(&home, &["stop"]);
}

#[test]
fn restart_with_nothing_running_starts_fresh() {
    let (home, layout) = open_layout();
    let output = run_cli(&home, &["restart"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    wait_until_ready(&layout, Duration::from_secs(30));

    let status = run_cli(&home, &["status"]);
    assert_eq!(status.status.code(), Some(0));

    let _ = run_cli(&home, &["stop"]);
}

#[test]
fn status_stop_restart_reject_unknown_arguments() {
    let (home, _layout) = open_layout();
    for args in [
        vec!["status", "--bogus"],
        vec!["stop", "extra"],
        vec!["restart", "extra"],
    ] {
        let output = run_cli(&home, &args);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
    }
}

/// Silence an unused-import warning on non-unix targets where this whole
/// file is `#![cfg(unix)]`-gated out anyway; kept for symmetry with
/// `serve_subprocess.rs`'s own use of `Command` indirectly via `TempHome`.
#[allow(dead_code)]
fn _keep_command_import(_: Command) {}
