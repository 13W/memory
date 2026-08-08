//! X-004 acceptance tests: `local-rag serve` emits a live `tracing` log to
//! stderr — lifecycle steps, one line per request, the shutdown reason — and
//! never leaks a request's payload into it.
//!
//! Drives the **real** binary as a genuine second OS process (mirrors
//! `tests/serve_subprocess.rs`'s own idiom), because the whole point under
//! test is what a human running `local-rag serve` in a terminal actually
//! sees on stderr. `RUST_LOG` is always set explicitly in the spawned
//! child's environment — `TempHome::command` only clears `HOME`, so an
//! ambient `RUST_LOG` in the developer's own shell would otherwise leak in
//! and make these assertions non-deterministic.

#![cfg(unix)]

use std::io::Read;
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use local_rag_core::paths::StoreLayout;
use local_rag_test_support::TempHome;

mod support;
use support::Client;

fn open_layout() -> (TempHome, StoreLayout) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    (home, layout)
}

fn spawn_serve(home: &TempHome, rust_log: &str) -> Child {
    let mut cmd = home.command(env!("CARGO_BIN_EXE_local-rag"));
    cmd.arg("serve");
    cmd.env("RUST_LOG", rust_log);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.spawn().expect("spawn local-rag serve")
}

/// Same idiom as `tests/serve_subprocess.rs::wait_until_ready`.
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
    assert_eq!(
        rc,
        0,
        "kill(SIGTERM) failed: {}",
        std::io::Error::last_os_error()
    );
}

fn read_stderr(child: &mut Child) -> String {
    let mut buf = Vec::new();
    child
        .stderr
        .take()
        .expect("stderr was piped")
        .read_to_end(&mut buf)
        .expect("read stderr");
    String::from_utf8_lossy(&buf).into_owned()
}

const SENTINEL: &str = "X004-PRIVACY-SENTINEL-9f3c9e7e";

#[test]
fn info_level_shows_startup_daemon_ready_one_line_per_request_and_the_stop_reason() {
    let (home, layout) = open_layout();
    let mut child = spawn_serve(&home, "info");
    wait_until_ready(&layout, Duration::from_secs(20));

    {
        let mut client = Client::connect(&layout.socket_path());
        client.call_and_read(
            &format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"search_code","arguments":{{"query":"{SENTINEL}"}}}}}}"#
            ),
            None,
        );
    }

    send_sigterm(child.id());
    let status = wait_for_exit(&mut child, Duration::from_secs(20));
    assert!(status.success(), "must exit cleanly: {status:?}");

    let stderr = read_stderr(&mut child);

    for needle in [
        "store lock acquired",
        "state.sqlite and cache.sqlite opened",
        "listening",
        "daemon ready",
        "session opened",
        "shutdown triggered",
        "daemon stopped",
    ] {
        assert!(
            stderr.contains(needle),
            "missing {needle:?} in stderr:\n{stderr}"
        );
    }

    // The one line per request: method label, metadata, no payload.
    assert!(
        stderr.contains("search_code"),
        "the request's tool name must appear: {stderr}"
    );
    assert!(
        stderr.contains("duration_ms="),
        "request line must carry duration_ms: {stderr}"
    );
    assert!(
        stderr.contains("bytes_in="),
        "request line must carry bytes_in: {stderr}"
    );
    assert!(
        stderr.contains("bytes_out="),
        "request line must carry bytes_out: {stderr}"
    );

    // Privacy: the request's own argument value must never reach the log.
    assert!(
        !stderr.contains(SENTINEL),
        "a request argument leaked into the log: {stderr}"
    );
}

#[test]
fn error_level_is_silent_about_per_request_lines() {
    let (home, layout) = open_layout();
    let mut child = spawn_serve(&home, "error");
    wait_until_ready(&layout, Duration::from_secs(20));

    {
        let mut client = Client::connect(&layout.socket_path());
        client.call_and_read(
            &format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"search_code","arguments":{{"query":"{SENTINEL}"}}}}}}"#
            ),
            None,
        );
    }

    send_sigterm(child.id());
    let status = wait_for_exit(&mut child, Duration::from_secs(20));
    assert!(status.success(), "must exit cleanly: {status:?}");

    let stderr = read_stderr(&mut child);
    assert!(
        !stderr.contains("daemon ready") && !stderr.contains("search_code"),
        "RUST_LOG=error must suppress info-level startup/request lines: {stderr}"
    );
    assert!(
        !stderr.contains(SENTINEL),
        "a request argument leaked into the log even at error level: {stderr}"
    );
}
