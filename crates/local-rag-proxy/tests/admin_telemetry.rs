//! T18-08 real-subprocess acceptance test for the daemon's telemetry
//! (spec 11 §7): a real `local-rag serve` daemon, a real `local-rag-proxy`
//! relaying calls over stdin/stdout, and a real `local-rag-hook`'s recall
//! RPC, all talking to the same store — proving `admin/tail_calls`/
//! `admin/tool_stats` see both connection kinds with a correctly distinct
//! `source`, and that polling those two methods never pollutes their own
//! log. Mirrors `subprocess.rs`'s own real-daemon-subprocess idiom
//! (`TempHome::command`, sibling-binary-path lookup, bounded waits) and
//! `local-rag-hook/tests/recall_rpc.rs`'s own `run_hook` idiom — a third
//! copy of a handful of small helpers, the same deliberate duplication
//! precedent `local_rag_hook::recall`'s own doc comment already accepts
//! over sharing a crate for ~40-line transport/test fragments.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Output, Stdio};
use std::time::{Duration, Instant};

use local_rag_core::paths::StoreLayout;
use local_rag_core::process::pid_exists;
use local_rag_test_support::TempHome;

/// Locate a real sibling product binary next to this integration test
/// binary (`target/<profile>/<name>`, one directory up from
/// `target/<profile>/deps/`) — `env!("CARGO_BIN_EXE_<name>")` only resolves
/// the *current* package's own binary targets, never a (dev-)dependency's,
/// verified empirically and already documented identically by
/// `subprocess.rs`'s own `local_rag_binary_path` and
/// `local-rag-hook/tests/recall_rpc.rs`'s own copy of the same finding.
fn sibling_binary_path(name: &str) -> std::path::PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    let deps_dir = exe.parent().expect("deps dir");
    let profile_dir = deps_dir.parent().expect("profile dir");
    let candidate = profile_dir.join(name);
    assert!(
        candidate.is_file(),
        "expected a sibling {name} binary at {candidate:?} (built via this package's own \
         {name} dev-dependency)"
    );
    candidate
}

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

fn wait_until_daemon_ready(layout: &StoreLayout, timeout: Duration) {
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

fn daemon_pid(layout: &StoreLayout) -> u32 {
    let bytes = std::fs::read(layout.store_lock()).expect("read store.lock");
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("parse store.lock");
    json["pid"].as_u64().expect("pid field") as u32
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

fn cleanup_daemon(layout: &StoreLayout, timeout: Duration) {
    let pid = daemon_pid(layout);
    send_sigterm(pid);
    let deadline = Instant::now() + timeout;
    while pid_exists(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn write_line(stdin: &mut ChildStdin, line: &str) {
    writeln!(stdin, "{line}").expect("write stdin line");
    stdin.flush().expect("flush stdin");
}

/// Read one line, bounded by `timeout`. Takes and returns ownership of the
/// reader (rather than `&mut`) so the blocking read can run on a dedicated
/// thread — a hung proxy must fail the test with a clear timeout, not hang
/// the whole suite. Mirrors `subprocess.rs`'s own `read_line`.
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

fn run_hook(home: &TempHome, event: &serde_json::Value) -> Output {
    let mut child = home
        .command(sibling_binary_path("local-rag-hook"))
        .arg("spool-write")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn local-rag-hook");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(event.to_string().as_bytes())
        .expect("write stdin");
    child.wait_with_output().expect("wait for local-rag-hook")
}

fn session_start_event(session_id: &str) -> serde_json::Value {
    serde_json::json!({"session_id": session_id, "hook_event_name": "SessionStart"})
}

/// End to end: a real `ping` through the real proxy (the daemon's own
/// `initialize`/`tools/call` machinery is exercised plenty elsewhere —
/// `ping` is the smallest call that still produces a genuine, distinct
/// telemetry record), a real hook recall RPC against the same running
/// daemon, then `admin/tail_calls`/`admin/tool_stats` relayed through the
/// SAME proxy connection (the proxy is a thin, method-agnostic pass-through
/// — `subprocess.rs`'s own tests already rely on this for `initialize`/
/// `tools/list`).
#[test]
fn admin_endpoints_see_both_sources_and_do_not_self_pollute() {
    let home = TempHome::new().expect("temp home");
    let layout = open_layout(&home);

    let mut proxy = spawn_proxy(&home, &[("LOCAL_RAG_SESSION_ID", "test-session-telemetry")]);
    let mut stdin = proxy.stdin.take().expect("proxy stdin");
    let stdout = BufReader::new(proxy.stdout.take().expect("proxy stdout"));

    write_line(&mut stdin, r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#);
    let (stdout, line) = read_line(stdout, Duration::from_secs(20));
    let ping_response: serde_json::Value =
        serde_json::from_str(line.trim_end()).expect("parse ping response");
    assert!(
        ping_response.get("error").is_none(),
        "ping must succeed: {ping_response}"
    );

    wait_until_daemon_ready(&layout, Duration::from_secs(5));

    let hook_output = run_hook(&home, &session_start_event("hook-session"));
    assert!(
        hook_output.status.success(),
        "local-rag-hook must always exit 0 (fail-open): {hook_output:?}"
    );

    write_line(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":2,"method":"admin/tail_calls"}"#,
    );
    let (stdout, line) = read_line(stdout, Duration::from_secs(20));
    let first: serde_json::Value =
        serde_json::from_str(line.trim_end()).expect("parse admin/tail_calls response");
    let first_calls = first["result"]["calls"].as_array().expect("calls array");
    assert_eq!(
        first_calls.len(),
        2,
        "exactly the ping and the hook's recall call, no more: {first}"
    );
    let sources: Vec<&str> = first_calls
        .iter()
        .map(|c| c["source"].as_str().expect("source"))
        .collect();
    assert!(
        sources.contains(&"claude-code"),
        "the proxy's own harness must show up verbatim: {first}"
    );
    assert!(
        sources.contains(&"claude-code-hook"),
        "the hook's harness must be distinct from the proxy's: {first}"
    );
    let tools: Vec<&str> = first_calls
        .iter()
        .map(|c| c["tool"].as_str().expect("tool"))
        .collect();
    assert!(tools.contains(&"ping"), "{first}");
    assert!(tools.contains(&"recall"), "{first}");

    // Polling admin/tail_calls a second time must not have added its own
    // predecessor call to the tail — self-exclusion.
    write_line(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":3,"method":"admin/tail_calls"}"#,
    );
    let (stdout, line) = read_line(stdout, Duration::from_secs(20));
    let second: serde_json::Value =
        serde_json::from_str(line.trim_end()).expect("parse second admin/tail_calls response");
    assert_eq!(
        second["result"]["calls"], first["result"]["calls"],
        "polling admin/tail_calls must not pollute its own tail"
    );

    write_line(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":4,"method":"admin/tool_stats"}"#,
    );
    let (stdout, line) = read_line(stdout, Duration::from_secs(20));
    let stats: serde_json::Value =
        serde_json::from_str(line.trim_end()).expect("parse admin/tool_stats response");
    let stats_tools: Vec<&str> = stats["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t["tool"].as_str().expect("tool"))
        .collect();
    assert_eq!(
        stats_tools,
        vec!["ping", "recall"],
        "sorted by tool name, and admin/* never counts itself: {stats}"
    );
    let _ = stdout;

    drop(stdin);
    let status = proxy.wait().expect("wait for proxy exit");
    assert!(status.success(), "proxy must exit 0: {status:?}");

    cleanup_daemon(&layout, Duration::from_secs(20));
}
