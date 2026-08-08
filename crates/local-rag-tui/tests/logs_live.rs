//! T18-09's live-probe test: spawn a real `local-rag serve` subprocess and drive this crate's own
//! `local_rag_tui::admin_client::AdminPoller` directly against it — the actual code path
//! `main.rs`'s `Screen::Logs` branch runs, not a mocked transport. Mirrors `status_live.rs`'s own
//! `local_rag_binary_path`/`spawn_serve`/`wait_until_ready`/`stop_serve` pattern
//! (`CARGO_BIN_EXE_local-rag` is not set for another package's binary regardless of dev-vs-normal
//! dependency edge — verified empirically there).
//!
//! The three scenarios below are deliberately independent, not one growing test: **A** proves the
//! "daemon not running" stub path with nothing else running; **B** proves a real call — made
//! through a second, independent connection, mirroring how `admin_telemetry.rs` (T18-08) proves
//! `source` visibility across two distinct connections — becomes visible to the poller; **C**
//! proves the reconnect path (`error → Unreachable → retry`) that A and B individually never
//! exercise, since A never connects successfully and B's daemon is already up before the poller
//! starts.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use local_rag_core::paths::StoreLayout;
use local_rag_protocol::{
    Hello, Message, PROTO_VERSION, RequestContext, RequestEnvelope, decode_message, encode_message,
};
use local_rag_test_support::TempHome;
use local_rag_tui::admin_client::{AdminPoller, LogsSnapshot};

fn open_layout() -> (TempHome, StoreLayout) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    (home, layout)
}

/// Locate the real `local-rag` binary next to this integration test binary — see
/// `status_live.rs`'s own copy of this function for the full "why" (`CARGO_BIN_EXE_local-rag` is
/// not set for another package's binary, verified empirically).
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

/// Poll `poller.latest()` until `predicate` holds, or panic after `timeout` — the shared retry
/// shape every scenario below needs instead of a `sleep`-then-check-once race.
fn wait_for(
    poller: &mut AdminPoller,
    predicate: impl Fn(&LogsSnapshot) -> bool,
    timeout: Duration,
) -> LogsSnapshot {
    let deadline = Instant::now() + timeout;
    loop {
        let snapshot = poller.latest();
        if predicate(&snapshot) {
            return snapshot;
        }
        if Instant::now() >= deadline {
            panic!("condition not met within {timeout:?}: last snapshot = {snapshot:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn write_line(stream: &UnixStream, msg: &Message) {
    let bytes = encode_message(msg).expect("encode");
    (&mut &*stream).write_all(&bytes).expect("write");
}

fn read_line(reader: &mut BufReader<&UnixStream>) -> Message {
    let mut line = String::new();
    reader.read_line(&mut line).expect("read line");
    decode_message(line.trim_end()).expect("decode")
}

/// A minimal, independent connection (fresh implementation, seeding only — the same "not a reuse
/// of production internals" idiom `local-rag-hook/tests/recall_rpc.rs::call_tool` already
/// established) that does one real HELLO/WELCOME + `ping`, deliberately over a **different**
/// connection than the one `AdminPoller` itself holds — proving the daemon's telemetry is visible
/// across connections, not just self-referentially within the poller's own.
fn send_one_ping(socket_path: &Path, session_id: &str) {
    let stream = UnixStream::connect(socket_path).expect("connect to daemon");
    write_line(
        &stream,
        &Message::Hello(Hello {
            proto: PROTO_VERSION,
            proxy_version: "test-client".to_string(),
            session_id: session_id.to_string(),
            worktree_root: None,
            harness: "claude-code".to_string(),
        }),
    );
    let mut reader = BufReader::new(&stream);
    let Message::Welcome(_) = read_line(&mut reader) else {
        panic!("expected Welcome");
    };

    let mcp = serde_json::value::RawValue::from_string(
        r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#.to_string(),
    )
    .expect("valid raw json");
    write_line(
        &stream,
        &Message::Request(RequestEnvelope {
            context: RequestContext {
                session_id: session_id.to_string(),
                worktree_root: None,
                repo_hint: None,
            },
            mcp,
        }),
    );
    let Message::Response(_) = read_line(&mut reader) else {
        panic!("expected Response");
    };
}

/// Scenario A: no daemon at all.
#[test]
fn unreachable_daemon_shows_up_as_unreachable() {
    let (_home, layout) = open_layout();
    let mut poller = AdminPoller::start(layout.socket_path());
    let snapshot = wait_for(
        &mut poller,
        |s| matches!(s, LogsSnapshot::Unreachable),
        Duration::from_secs(5),
    );
    assert_eq!(snapshot, LogsSnapshot::Unreachable);
}

/// Scenario B: a real daemon, a real call made through an independent connection — the poller
/// must see it in both `admin/tail_calls` and `admin/tool_stats`.
#[test]
fn a_real_call_through_an_independent_connection_is_visible_to_the_poller() {
    let (home, layout) = open_layout();
    let daemon = spawn_serve(&home);
    wait_until_ready(&layout, Duration::from_secs(20));

    let mut poller = AdminPoller::start(layout.socket_path());
    send_one_ping(&layout.socket_path(), "logs-live-b");

    let snapshot = wait_for(
        &mut poller,
        |s| matches!(s, LogsSnapshot::Connected { calls, .. } if calls.iter().any(|c| c.tool == "ping")),
        Duration::from_secs(10),
    );
    match snapshot {
        LogsSnapshot::Connected { calls, tools } => {
            assert!(
                calls.iter().any(|c| c.tool == "ping" && !c.is_error),
                "{calls:?}"
            );
            let ping_stats = tools
                .iter()
                .find(|t| t.tool == "ping")
                .unwrap_or_else(|| panic!("no ping entry in tool stats: {tools:?}"));
            assert_eq!(ping_stats.calls, 1);
            assert_eq!(ping_stats.errors, 0);
        }
        other => panic!("expected Connected, got {other:?}"),
    }

    drop(poller);
    stop_serve(daemon);
}

/// Scenario C: the poller starts before any daemon exists (observes `Unreachable`), then a real
/// daemon starts — the poller must recover on its own, with no restart.
#[test]
fn the_poller_recovers_once_a_daemon_appears_after_it_started() {
    let (home, layout) = open_layout();
    let mut poller = AdminPoller::start(layout.socket_path());
    let _ = wait_for(
        &mut poller,
        |s| matches!(s, LogsSnapshot::Unreachable),
        Duration::from_secs(5),
    );

    let daemon = spawn_serve(&home);
    wait_until_ready(&layout, Duration::from_secs(20));

    let snapshot = wait_for(
        &mut poller,
        |s| matches!(s, LogsSnapshot::Connected { .. }),
        Duration::from_secs(10),
    );
    assert!(matches!(snapshot, LogsSnapshot::Connected { .. }));

    drop(poller);
    stop_serve(daemon);
}
