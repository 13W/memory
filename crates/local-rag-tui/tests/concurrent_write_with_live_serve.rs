//! G18 gate: TUI writes and a live daemon's own MCP writes against the same `state.sqlite`,
//! interleaved, must never surface a lock conflict — ADR-0008's WAL + `busy_timeout=5000` claim
//! (spec 02 §5) covers *every* external writer, not only reads. `status_live.rs` already proves
//! the read-vs-write half (a read-only connection never conflicts with the daemon's writer); this
//! is the write-vs-write half, closing a coverage gap identified while running the G18 gate — no
//! prior test drove a TUI mutation (`execute_repo_settings_action`) concurrently with a real
//! daemon's own MCP writes (`remember`) against the same file. Mirrors `status_live.rs`'s own
//! `local_rag_binary_path`/`spawn_serve`/`wait_until_ready`/`stop_serve` idiom, and
//! `repo_settings_offline.rs`'s own seed/`block_on` idiom.

#![cfg(unix)]

use std::future::Future;
use std::path::Path;
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use local_rag_core::paths::StoreLayout;
use local_rag_store::{StateDb, create_repository};
use local_rag_test_support::TempHome;
use local_rag_tui::repo_settings::{
    RepoSettingsAction, RepoSettingsNav, execute_repo_settings_action,
};
use local_rag_tui::status::{DaemonStatus, probe_daemon};

/// Interleaved writes per side — enough for real contention on the same file without turning this
/// into a load test.
const N: usize = 20;

fn block_on<F: Future>(fut: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a current-thread tokio runtime")
        .block_on(fut)
}

fn open_layout() -> (TempHome, StoreLayout) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    (home, layout)
}

/// Locate the real `local-rag` binary next to this integration test binary — same lookup
/// `status_live.rs` already established (`CARGO_BIN_EXE_local-rag` is not set for another
/// package's binary regardless of dev-vs-normal dependency edge).
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

// ---- a minimal raw MCP client, mirrors `admin_client.rs`'s own `connect_and_handshake`/
// `write_message`/read-a-line shape, but for `tools/call` `remember` rather than `admin/*` — this
// crate has no shared non-admin client to reuse. ----

type Reader = tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>;
type Writer = tokio::net::unix::OwnedWriteHalf;

async fn connect_and_handshake(socket_path: &Path, session_id: &str) -> (Reader, Writer) {
    let stream = tokio::net::UnixStream::connect(socket_path)
        .await
        .expect("connect to daemon");
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = tokio::io::BufReader::new(read_half);

    let hello = local_rag_protocol::Message::Hello(local_rag_protocol::Hello {
        proto: local_rag_protocol::PROTO_VERSION,
        proxy_version: local_rag_core::VERSION.to_string(),
        session_id: session_id.to_string(),
        worktree_root: None,
        harness: "g18-concurrency-test".to_string(),
    });
    write_message(&mut write_half, &hello).await;

    let line = read_line(&mut reader).await;
    match local_rag_protocol::decode_message(&line).expect("decode WELCOME") {
        local_rag_protocol::Message::Welcome(_) => (reader, write_half),
        other => panic!("expected WELCOME, got {other:?}"),
    }
}

async fn read_line<R: tokio::io::AsyncBufRead + Unpin>(reader: &mut R) -> String {
    use tokio::io::AsyncBufReadExt;
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .await
        .expect("read a response line");
    line.trim_end().to_string()
}

async fn write_message<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    msg: &local_rag_protocol::Message,
) {
    use tokio::io::AsyncWriteExt;
    let bytes = local_rag_protocol::encode_message(msg).expect("encode");
    writer.write_all(&bytes).await.expect("write");
    writer.flush().await.expect("flush");
}

async fn remember_call(reader: &mut Reader, writer: &mut Writer, i: usize) -> serde_json::Value {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": i,
        "method": "tools/call",
        "params": {
            "name": "remember",
            "arguments": {"text": format!("g18 concurrency fact {i}"), "kind": "fact"},
        },
    });
    let mcp = serde_json::value::RawValue::from_string(body.to_string()).expect("raw value");
    let request = local_rag_protocol::Message::Request(local_rag_protocol::RequestEnvelope {
        context: local_rag_protocol::RequestContext {
            session_id: "g18-concurrency-test".to_string(),
            worktree_root: None,
            repo_hint: None,
        },
        mcp,
    });
    write_message(writer, &request).await;
    let line = read_line(reader).await;
    match local_rag_protocol::decode_message(&line).expect("decode response") {
        local_rag_protocol::Message::Response(env) => {
            serde_json::from_str(env.mcp.get()).expect("parse response json")
        }
        other => panic!("expected Response, got {other:?}"),
    }
}

/// TUI mutation writes and a live daemon's own MCP-driven writes, interleaved against the same
/// `state.sqlite`, must both complete successfully — no `SQLITE_BUSY`/"database is locked" ever
/// surfaces to either caller. `busy_timeout=5000` (spec 02 §5) is the documented backstop this
/// asserts against, the same one every CLI command already relies on for the identical pattern
/// (ADR-0008's own justification for why the TUI needs no daemon for these screens at all).
#[test]
fn tui_writes_and_a_live_daemons_mcp_writes_never_conflict() {
    let (home, layout) = open_layout();
    let daemon = spawn_serve(&home);
    wait_until_ready(&layout, Duration::from_secs(20));

    // One repository for the TUI side to write settings against — a single, sequential seed
    // write before the concurrent phase starts, not itself part of the contention being tested.
    block_on(async {
        let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
        db.writer()
            .transaction(|tx| {
                create_repository(tx, "repo-a", None, 1_000)?;
                Ok(())
            })
            .await
            .expect("seed repo tx");
    });

    let socket_path = layout.socket_path();
    let tui_layout = layout.clone();

    // Side A: the TUI's own write path, off the async runtime entirely (mirrors `main.rs`'s own
    // synchronous event loop calling `execute_repo_settings_action`, which drives its own
    // throwaway `rt::block_on` runtime internally per call).
    let tui_writer = std::thread::spawn(move || {
        for i in 0..N {
            match execute_repo_settings_action(
                &tui_layout,
                RepoSettingsAction::SetSetting {
                    repo_id: "repo-a".to_string(),
                    key: "concurrency_probe".to_string(),
                    value: i.to_string(),
                    list_selected: 0,
                },
            ) {
                RepoSettingsNav::RepoDetail { error: None, .. } => {}
                other => panic!("TUI write #{i} failed: {other:?}"),
            }
        }
    });

    // Side B: a real daemon-side writer — genuine MCP `remember` calls over a real UDS
    // connection, landing through the daemon's own `StateWriter` queue.
    let mcp_results = block_on(async {
        let (mut reader, mut writer) =
            connect_and_handshake(&socket_path, "g18-concurrency-test").await;
        let mut results = Vec::with_capacity(N);
        for i in 0..N {
            results.push(remember_call(&mut reader, &mut writer, i).await);
        }
        results
    });

    tui_writer.join().expect("TUI writer thread panicked");

    for (i, resp) in mcp_results.iter().enumerate() {
        assert!(
            resp.get("error").is_none(),
            "remember #{i} returned a JSON-RPC-level error: {resp}"
        );
        assert_eq!(
            resp["result"]["isError"],
            serde_json::Value::Bool(false),
            "remember #{i} returned an in-band tool error: {resp}"
        );
    }

    // Both sides' writes actually landed.
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    let conn = db.open_read().expect("read connection");
    let settings = local_rag_store::repo_settings(&conn, "repo-a").expect("read settings");
    assert_eq!(
        settings,
        vec![("concurrency_probe".to_string(), (N - 1).to_string())],
        "the TUI's own N upserts on one key must leave exactly the last value, one row"
    );
    let fact_count: i64 = local_rag_store::memory_entry_counts(&conn)
        .expect("memory entry counts")
        .into_iter()
        .filter(|row| row.kind == local_rag_store::MemoryKind::Fact)
        .map(|row| row.count)
        .sum();
    assert_eq!(
        fact_count, N as i64,
        "every remember call must have created its own entry"
    );

    // The daemon itself is still alive and answering after the contention phase.
    let status = probe_daemon(&layout, Duration::from_secs(1));
    assert!(
        matches!(status, DaemonStatus::Running { .. }),
        "daemon must still be alive and answering: {status:?}"
    );

    stop_serve(daemon);
}
