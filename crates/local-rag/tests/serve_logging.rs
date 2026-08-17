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

/// X-007: `serve` with stderr sent to `/dev/null` — byte-for-byte the shape
/// `local-rag-proxy` spawns it in (`connect.rs:76-82`), which is the normal MCP
/// setup and the exact case where X-004's stderr stream is unreadable.
fn spawn_serve_with_stderr_discarded(home: &TempHome, rust_log: &str) -> Child {
    let mut cmd = home.command(env!("CARGO_BIN_EXE_local-rag"));
    cmd.arg("serve");
    cmd.env("RUST_LOG", rust_log);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::null());
    cmd.spawn().expect("spawn local-rag serve")
}

/// The single rotated log file's contents, plus its file name for assertions
/// about the naming scheme. Panics if the directory holds anything other than
/// exactly one log file — a rotation that fired mid-test would be a real
/// finding, not something to paper over by concatenating.
fn read_only_log_file(layout: &StoreLayout) -> (String, String) {
    let dir = layout.logs_dir();
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.is_file())
        .collect();
    files.sort();
    assert_eq!(
        files.len(),
        1,
        "expected exactly one log file in {}, found {files:?}",
        dir.display(),
    );
    let name = files[0]
        .file_name()
        .expect("file name")
        .to_string_lossy()
        .into_owned();
    let body = std::fs::read_to_string(&files[0]).expect("read the log file");
    (body, name)
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

/// X-007: the same stream reaches a file under `logs_dir`, and it does so when
/// stderr is discarded — the case that made X-004's output unreachable in the
/// normal MCP setup.
#[test]
fn the_log_file_captures_the_stream_even_with_stderr_discarded() {
    let (home, layout) = open_layout();
    let mut child = spawn_serve_with_stderr_discarded(&home, "info");
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

    let (log, name) = read_only_log_file(&layout);
    assert!(
        name.starts_with("daemon.") && name.ends_with(".log"),
        "rotated files are named daemon.<date>.log, got {name:?}",
    );

    // Everything the stderr test asserts, now read back from disk instead —
    // including the shutdown lines, which prove the synchronous writer does not
    // lose the tail at process exit the way a dropped `WorkerGuard` would.
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
            log.contains(needle),
            "missing {needle:?} in the log file:\n{log}"
        );
    }
    assert!(
        log.contains("search_code") && log.contains("duration_ms="),
        "the per-request line must reach the file too:\n{log}"
    );

    // Privacy carries over to the new sink: X-004's boundary is about the
    // events themselves, but a second destination is exactly where such a rule
    // silently rots, so assert it here as well.
    assert!(
        !log.contains(SENTINEL),
        "a request argument leaked into the log file:\n{log}"
    );
}

/// The filter applies to the file exactly as it does to stderr — one
/// `resolve_filter` result feeds both layers, so `RUST_LOG` cannot end up
/// quieting one sink while the other stays chatty.
#[test]
fn the_log_file_honours_the_same_filter_as_stderr() {
    let (home, layout) = open_layout();
    let mut child = spawn_serve_with_stderr_discarded(&home, "error");
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

    // The file is still created (the appender opens it eagerly), but carries no
    // info-level line.
    let (log, _) = read_only_log_file(&layout);
    assert!(
        !log.contains("daemon ready") && !log.contains("search_code"),
        "RUST_LOG=error must quiet the file sink too:\n{log}"
    );
    assert!(
        !log.contains(SENTINEL),
        "a request argument leaked into the log file at error level:\n{log}"
    );
}
