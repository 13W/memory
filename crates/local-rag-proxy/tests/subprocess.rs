//! T15-02 acceptance tests driving the **real** `local-rag-proxy` binary
//! (and the real `local-rag serve` daemon it spawns) as genuine OS
//! processes — the scenarios that only exist across a real process
//! boundary: cold-start spawn, signal detachment, and end-to-end context
//! passthrough. Mirrors `crates/local-rag/tests/serve_subprocess.rs`'s own
//! established idiom (`TempHome::command`, `libc::kill` for a real SIGTERM
//! — `Child::kill` is `SIGKILL`-only on unix).

#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Stdio};
use std::time::{Duration, Instant};

use local_rag_core::paths::StoreLayout;
use local_rag_core::process::pid_exists;
use local_rag_test_support::TempHome;

fn open_layout(home: &TempHome) -> StoreLayout {
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    layout
}

fn spawn_proxy(home: &TempHome, extra_env: &[(&str, &str)]) -> Child {
    let mut cmd = home.command(env!("CARGO_BIN_EXE_local-rag-proxy"));
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.spawn().expect("spawn local-rag-proxy")
}

/// Poll `store.lock` until it parses with `ready: true`, or panic after
/// `timeout` — real, bounded wall-clock waiting is inherent to driving a
/// real child process (here, the daemon `local-rag-proxy` itself spawns)
/// across a real OS scheduling boundary. Nothing asserts on the duration,
/// only on eventual readiness.
fn wait_until_daemon_ready(layout: &StoreLayout, timeout: Duration) -> serde_json::Value {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(bytes) = std::fs::read(layout.store_lock())
            && let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes)
            && json.get("ready").and_then(|v| v.as_bool()) == Some(true)
        {
            return json;
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

/// Send a real `SIGTERM` (not `SIGKILL` — `std::process::Child::kill` is
/// unix-`SIGKILL`-only and cannot express this).
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

fn daemon_pid(layout: &StoreLayout) -> u32 {
    let bytes = std::fs::read(layout.store_lock()).expect("read store.lock");
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("parse store.lock");
    json["pid"].as_u64().expect("pid field") as u32
}

fn write_line(stdin: &mut ChildStdin, line: &str) {
    writeln!(stdin, "{line}").expect("write stdin line");
    stdin.flush().expect("flush stdin");
}

/// Read one line, bounded by `timeout`. Takes and returns ownership of the
/// reader (rather than `&mut`) so the blocking read can run on a dedicated
/// thread: a hung proxy must fail the test with a clear timeout, not hang
/// the whole suite waiting on a synchronous read with no bound.
fn read_line(
    mut stdout: BufReader<ChildStdout>,
    timeout: Duration,
) -> (BufReader<ChildStdout>, String) {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let result = stdout.read_line(&mut line).map(|_| line);
        let _ = tx.send((stdout, result));
    });
    let (stdout, result) = rx
        .recv_timeout(timeout)
        .expect("proxy did not answer within the timeout");
    (stdout, result.expect("read stdout line"))
}

/// Shut a daemon down and wait for it to actually exit — test cleanup, not
/// part of the scenario under test. Called only once the daemon is already
/// known ready, so `daemon_pid` is expected to succeed.
fn cleanup_daemon(layout: &StoreLayout, timeout: Duration) {
    let pid = daemon_pid(layout);
    send_sigterm(pid);
    let deadline = Instant::now() + timeout;
    while pid_exists(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Real cold-start spawn (no daemon pre-running) through a real MCP
/// handshake, end to end across both binaries.
///
/// Before T15-03's real `RequestHandler` replaced `EchoRequestHandler`,
/// this test drove one echo call and asserted the echoed `context` matched
/// what the proxy sent — that assertion has no MCP-visible equivalent now
/// (a real handler answers with tool content, not its context back). That
/// coverage moved to `crates/local-rag/tests/mcp_tools.rs`'s
/// `explicit_context_routing_across_two_requests_on_one_connection`, and
/// transport-level context isolation stays covered by T15-02's own
/// `daemon::handshake::tests::two_requests_on_one_connection_keep_their_
/// own_context` (still run against `EchoRequestHandler`, unchanged).
#[test]
fn cold_start_spawns_a_daemon_and_completes_a_real_mcp_handshake() {
    let home = TempHome::new().expect("temp home");
    let layout = open_layout(&home);

    let mut proxy = spawn_proxy(
        &home,
        &[("LOCAL_RAG_SESSION_ID", "test-session-cold-start")],
    );
    let mut stdin = proxy.stdin.take().expect("proxy stdin");
    let stdout = BufReader::new(proxy.stdout.take().expect("proxy stdout"));

    // Real MCP handshake, cold-start spawn included:
    // initialize -> notifications/initialized -> tools/list, asserting
    // that the *next line read back* is tools/list's own response — the
    // only formulation that actually proves the notification produced no
    // stray line all the way through both binaries (T15-03's own
    // `daemon::handshake` unit test proves the daemon-local half; this is
    // the two-binary version).
    write_line(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
    );
    let (stdout, line) = read_line(stdout, Duration::from_secs(20));
    let response: serde_json::Value =
        serde_json::from_str(line.trim_end()).expect("parse response");
    assert_eq!(response["result"]["serverInfo"]["name"], "local-rag");
    assert!(
        response["result"]["instructions"]
            .as_str()
            .expect("instructions is a string")
            .contains("search_code"),
        "{response}"
    );

    write_line(
        &mut stdin,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
    );
    write_line(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
    );
    let (stdout, line) = read_line(stdout, Duration::from_secs(20));
    let response: serde_json::Value =
        serde_json::from_str(line.trim_end()).expect("parse response");
    assert_eq!(response["id"], serde_json::json!(2));
    let names: Vec<&str> = response["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "search_code",
            "get_file_context",
            "project_overview",
            "recall",
            "list_memory",
            "list_memory_candidates",
            "inspect_memory_evidence",
            "stats",
            "health",
        ]
    );
    let _ = stdout;

    wait_until_daemon_ready(&layout, Duration::from_secs(5)); // must already be true by now

    drop(stdin); // close stdin: the proxy's relay loop must exit cleanly
    let status = wait_for_exit(&mut proxy, Duration::from_secs(20));
    assert!(
        status.success(),
        "proxy must exit 0 once stdin closes: {status:?}"
    );

    cleanup_daemon(&layout, Duration::from_secs(20));
}

#[test]
fn sigterm_to_the_proxy_does_not_reach_the_daemon_it_spawned() {
    let home = TempHome::new().expect("temp home");
    let layout = open_layout(&home);

    let mut proxy = spawn_proxy(&home, &[("LOCAL_RAG_SESSION_ID", "test-session-detach")]);
    let mut stdin = proxy.stdin.take().expect("proxy stdin");
    let stdout = BufReader::new(proxy.stdout.take().expect("proxy stdout"));

    // Drive one real MCP call through so the handshake (and therefore the
    // spawn) is provably complete, not merely "the child process exists".
    write_line(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
    );
    let _ = read_line(stdout, Duration::from_secs(20));

    wait_until_daemon_ready(&layout, Duration::from_secs(5));
    let daemon = daemon_pid(&layout);
    assert!(pid_exists(daemon), "the spawned daemon must be alive");

    send_sigterm(proxy.id());
    let status = wait_for_exit(&mut proxy, Duration::from_secs(20));
    assert!(
        status.success(),
        "the proxy must exit 0 on SIGTERM: {status:?}"
    );

    // The critical assertion: detachment via `process_group(0)` means a
    // signal sent only to the proxy's own pid never reaches the daemon it
    // spawned, which is running in its own process group.
    assert!(
        pid_exists(daemon),
        "the daemon must remain alive after the proxy that spawned it is killed"
    );

    cleanup_daemon(&layout, Duration::from_secs(20));
}
