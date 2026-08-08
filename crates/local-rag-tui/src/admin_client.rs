//! Long-lived UDS client for the two `admin/*` JSON-RPC methods (spec 11 §7, T18-09) — polls
//! `admin/tail_calls`/`admin/tool_stats` on a background OS thread's own single-threaded tokio
//! runtime, publishing snapshots to the synchronous main loop over a [`std::sync::mpsc`] channel.
//! Shaped after `local-rag-proxy`'s own `connect`/`handshake` (HELLO/WELCOME over
//! `tokio::net::UnixStream`), without the stdin/stdout relay half — this client only ever
//! originates `admin/*` requests, never passes through arbitrary MCP. The bounded-line transport
//! (`read_bounded_line`/`write_message` below) is a near-duplicate of `local-rag-proxy`'s own
//! `transport.rs` — that crate is bin-only (no `lib.rs`), so it cannot be imported; this is the
//! third copy of the same ~40-line fragment in this workspace (D-002/D-010, the same trade-off
//! `local_rag_hook::recall`'s own doc comment already accepts over sharing a crate).
//!
//! # Holding the daemon awake
//!
//! Every connection this client makes registers a live session in `SessionRegistry` for as long
//! as it stays open (`daemon/handshake.rs::handle_connection`) — spec 02 §4.3's idle-shutdown
//! gate requires *zero* live sessions (`daemon/idle.rs`'s own `SessionRegistry::len()` check), so
//! a Logs screen left open keeps the daemon from idle-shutting-down for as long as this poller's
//! connection is alive. This is a deliberate, accepted consequence of a long-lived MCP session,
//! not a defect — [`AdminPoller`] is dropped, and the connection closes, the moment the user
//! leaves the Logs screen or quits (`main.rs`'s own `Screen::Logs` branch), so idle-shutdown
//! resumes normally the instant nobody is actually watching this screen.
//!
//! # Two independent liveness mechanisms — do not conflate them
//!
//! [`AdminPoller::drop`]'s responsiveness comes from [`poll_loop`]'s outer `tokio::select!` racing
//! the stop [`tokio::sync::Notify`] against the *entire* per-connection cycle
//! ([`run_one_connection`]) — dropping the losing future (whatever I/O it was mid-await on) is
//! cheap and immediate, so no per-operation timeout is needed to make `stop()` fast. [`CYCLE_TIMEOUT`]
//! exists for an unrelated concern: a daemon that accepts the connection but stops answering
//! (wedged, not dead) must still be detected and reconnected — without it the Logs screen would
//! freeze on stale data indefinitely while the app keeps running.

#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, TryRecvError};
use std::time::Duration;

use serde::Deserialize;

/// Poll cadence (card's own "~1с"), and doubles as the reconnect delay after a broken/failed
/// connection.
pub const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Bound on connect+HELLO/WELCOME, and separately on each `admin/tail_calls`+`admin/tool_stats`
/// round trip — not `AdminPoller::drop`'s responsiveness mechanism (see module doc), but
/// self-healing against a daemon that accepts the connection and then stops answering. Picked and
/// documented as chosen, not derived — same precedent `local_rag::daemon::probe::
/// LIVENESS_PROBE_TIMEOUT_MS`/`local_rag_hook::recall::RECALL_BUDGET` already set.
pub const CYCLE_TIMEOUT: Duration = Duration::from_secs(2);

#[cfg(unix)]
const HARNESS: &str = "local-rag-tui";

/// One row of `admin/tail_calls`'s `calls` array — a `Deserialize`-only mirror of
/// `local_rag::daemon::telemetry::CallRecord` (that type derives only `Serialize`; this crate
/// does not couple to `local-rag`'s daemon-internal telemetry type at the wire level — same field
/// names, no rename attributes needed, both sides already `snake_case`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CallRow {
    pub at_ms: i64,
    pub source: String,
    pub tool: String,
    pub duration_ms: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub is_error: bool,
}

/// One row of `admin/tool_stats`'s `tools` array — mirrors `ToolStatsEntry`
/// (`daemon/mcp/dispatch.rs`), which flattens `ToolStats` alongside the tool name.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ToolStatRow {
    pub tool: String,
    pub calls: u64,
    pub errors: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub total_ms: u64,
}

/// What one poll cycle produced, or why it produced nothing — [`AdminPoller::latest`]'s return
/// type, and `logs::render_logs`'s only input.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum LogsSnapshot {
    /// No daemon reachable — connect failed, or the handshake/a request timed out.
    #[default]
    Unreachable,
    /// The background poller thread is gone (it panicked, or this platform has no UDS transport
    /// at all — D-033) — distinct from `Unreachable` so a crash is visible on-screen, not
    /// silently indistinguishable from "daemon not running".
    PollerStopped,
    Connected {
        /// Oldest first, exactly the wire order `admin/tail_calls` answers with — display order
        /// (newest-first) is `logs.rs`'s own concern, not this client's.
        calls: Vec<CallRow>,
        tools: Vec<ToolStatRow>,
    },
}

/// Owns the background thread; dropping it stops the thread (see module doc on why no separate
/// bounded-join machinery is needed beyond the `select!` in [`poll_loop`]).
pub struct AdminPoller {
    stop: Arc<tokio::sync::Notify>,
    handle: Option<std::thread::JoinHandle<()>>,
    rx: mpsc::Receiver<LogsSnapshot>,
    last: LogsSnapshot,
}

impl AdminPoller {
    /// Start polling `socket_path` in the background. Never blocks, never fails — an unreachable
    /// daemon simply means every [`latest`](Self::latest) call returns `LogsSnapshot::Unreachable`
    /// until one becomes reachable.
    #[cfg(unix)]
    pub fn start(socket_path: PathBuf) -> Self {
        let stop = Arc::new(tokio::sync::Notify::new());
        let (tx, rx) = mpsc::channel();
        let stop_bg = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("local-rag-tui-admin-poller".to_string())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("a current-thread tokio runtime");
                runtime.block_on(poll_loop(socket_path, tx, stop_bg));
            })
            .expect("spawn admin poller thread");
        Self {
            stop,
            handle: Some(handle),
            rx,
            last: LogsSnapshot::Unreachable,
        }
    }

    /// No UDS transport exists on this platform yet (D-033) — mirrors
    /// `status::probe_daemon`'s own `#[cfg(not(unix))]` fallback: always `Unreachable`, no
    /// thread spawned.
    #[cfg(not(unix))]
    pub fn start(_socket_path: PathBuf) -> Self {
        Self {
            stop: Arc::new(tokio::sync::Notify::new()),
            handle: None,
            rx: mpsc::channel().1,
            last: LogsSnapshot::Unreachable,
        }
    }

    /// Drain every pending snapshot and remember the newest — called every `run_app` iteration
    /// on the Logs screen, mirroring every other screen's own "recompute every frame" discipline
    /// (here: "re-read the channel every frame", the closest analog for a push-from-background
    /// source instead of a pull-on-demand one).
    pub fn latest(&mut self) -> LogsSnapshot {
        loop {
            match self.rx.try_recv() {
                Ok(snapshot) => self.last = snapshot,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.last = LogsSnapshot::PollerStopped;
                    break;
                }
            }
        }
        self.last.clone()
    }
}

impl Drop for AdminPoller {
    fn drop(&mut self) {
        self.stop.notify_one();
        if let Some(handle) = self.handle.take() {
            // `Err` only on a panicked poller thread — nothing to propagate; `latest()`'s
            // `Disconnected` arm already surfaces that on-screen the moment it happens.
            let _ = handle.join();
        }
    }
}

#[cfg(unix)]
async fn poll_loop(
    socket_path: PathBuf,
    tx: mpsc::Sender<LogsSnapshot>,
    stop: Arc<tokio::sync::Notify>,
) {
    // Fixed prefix + this process's own pid, so two concurrently running `local-rag-tui`
    // instances never share one literal session_id — `SessionRegistry` tolerates duplicates fine
    // (see its own doc), this is a cheap, unambiguous nicety, not a correctness fix.
    let session_id = format!("local-rag-tui-logs-{}", std::process::id());
    loop {
        tokio::select! {
            () = notified(&stop) => return,
            () = run_one_connection(&socket_path, &session_id, &tx) => {}
        }
        tokio::select! {
            () = notified(&stop) => return,
            () = tokio::time::sleep(POLL_INTERVAL) => {}
        }
    }
}

#[cfg(unix)]
async fn notified(stop: &tokio::sync::Notify) {
    stop.notified().await;
}

/// One connection's whole lifetime: connect + HELLO/WELCOME, then poll forever until an error or
/// timeout, at which point it emits `Unreachable` and returns — [`poll_loop`]'s caller reconnects.
#[cfg(unix)]
async fn run_one_connection(socket_path: &Path, session_id: &str, tx: &mpsc::Sender<LogsSnapshot>) {
    let context = local_rag_protocol::RequestContext {
        session_id: session_id.to_string(),
        worktree_root: None,
        repo_hint: None,
    };
    let Ok(Ok((mut reader, mut writer))) = tokio::time::timeout(
        CYCLE_TIMEOUT,
        connect_and_handshake(socket_path, session_id),
    )
    .await
    else {
        let _ = tx.send(LogsSnapshot::Unreachable);
        return;
    };
    loop {
        let cycle =
            tokio::time::timeout(CYCLE_TIMEOUT, poll_once(&mut reader, &mut writer, &context));
        match cycle.await {
            Ok(Some(snapshot)) => {
                if tx.send(snapshot).is_err() {
                    return; // the receiver (AdminPoller) is gone — nothing left to serve
                }
            }
            _ => {
                let _ = tx.send(LogsSnapshot::Unreachable);
                return;
            }
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

#[cfg(unix)]
type Reader = tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>;
#[cfg(unix)]
type Writer = tokio::net::unix::OwnedWriteHalf;

#[cfg(unix)]
async fn connect_and_handshake(
    socket_path: &Path,
    session_id: &str,
) -> std::io::Result<(Reader, Writer)> {
    let stream = tokio::net::UnixStream::connect(socket_path).await?;
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = tokio::io::BufReader::new(read_half);

    let hello = local_rag_protocol::Message::Hello(local_rag_protocol::Hello {
        proto: local_rag_protocol::PROTO_VERSION,
        proxy_version: local_rag_core::VERSION.to_string(),
        session_id: session_id.to_string(),
        worktree_root: None,
        harness: HARNESS.to_string(),
    });
    write_message(&mut write_half, &hello).await?;

    let line = read_bounded_line(&mut reader).await?.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "connection closed before WELCOME",
        )
    })?;
    match local_rag_protocol::decode_message(&line)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?
    {
        local_rag_protocol::Message::Welcome(_) => Ok((reader, write_half)),
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "expected WELCOME",
        )),
    }
}

/// One `admin/tail_calls` + `admin/tool_stats` round trip on an already-handshaken connection.
/// `None` on any malformed/unexpected response — the caller treats that identically to a
/// transport error (reconnect).
#[cfg(unix)]
async fn poll_once(
    reader: &mut Reader,
    writer: &mut Writer,
    context: &local_rag_protocol::RequestContext,
) -> Option<LogsSnapshot> {
    let calls_text = admin_request(reader, writer, context, "admin/tail_calls")
        .await
        .ok()?;
    let calls = parse_result_field::<Vec<CallRow>>(&calls_text, "calls")?;
    let tools_text = admin_request(reader, writer, context, "admin/tool_stats")
        .await
        .ok()?;
    let tools = parse_result_field::<Vec<ToolStatRow>>(&tools_text, "tools")?;
    Some(LogsSnapshot::Connected { calls, tools })
}

#[cfg(unix)]
async fn admin_request(
    reader: &mut Reader,
    writer: &mut Writer,
    context: &local_rag_protocol::RequestContext,
    method: &str,
) -> std::io::Result<String> {
    let body = format!(r#"{{"jsonrpc":"2.0","id":1,"method":"{method}"}}"#);
    let mcp = serde_json::value::RawValue::from_string(body)
        .expect("admin request body is well-formed JSON by construction");
    let request = local_rag_protocol::Message::Request(local_rag_protocol::RequestEnvelope {
        context: context.clone(),
        mcp,
    });
    write_message(writer, &request).await?;

    let line = read_bounded_line(reader).await?.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "connection closed mid-poll",
        )
    })?;
    match local_rag_protocol::decode_message(&line)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?
    {
        local_rag_protocol::Message::Response(env) => Ok(env.mcp.get().to_string()),
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "expected Response",
        )),
    }
}

/// Pull `result.<field>` out of a JSON-RPC response body and deserialize it as `T` — `None` on a
/// JSON-RPC-level error response (no `result` key), a missing field, or a shape mismatch.
fn parse_result_field<T: serde::de::DeserializeOwned>(text: &str, field: &str) -> Option<T> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let field_value = value.get("result")?.get(field)?.clone();
    serde_json::from_value(field_value).ok()
}

/// Read one `\n`-terminated line, bounded to `MAX_MESSAGE_BYTES`. Unlike a bare
/// `AsyncBufReadExt::read_until` (which buffers without limit until it finds the delimiter), this
/// checks the accumulated length after **every** underlying read.
#[cfg(unix)]
async fn read_bounded_line<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<String>> {
    use tokio::io::AsyncBufReadExt;

    let mut out = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(None); // clean EOF
        }
        match available.iter().position(|&b| b == b'\n') {
            Some(pos) => {
                out.extend_from_slice(&available[..pos]);
                reader.consume(pos + 1);
                if out.len() > local_rag_protocol::MAX_MESSAGE_BYTES {
                    return Err(too_long());
                }
                return String::from_utf8(out)
                    .map(Some)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e));
            }
            None => {
                out.extend_from_slice(available);
                let consumed = available.len();
                reader.consume(consumed);
                if out.len() > local_rag_protocol::MAX_MESSAGE_BYTES {
                    return Err(too_long());
                }
            }
        }
    }
}

#[cfg(unix)]
fn too_long() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "line exceeds MAX_MESSAGE_BYTES",
    )
}

/// Encode and write one [`local_rag_protocol::Message`], flushing.
#[cfg(unix)]
async fn write_message<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    msg: &local_rag_protocol::Message,
) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;

    let bytes = local_rag_protocol::encode_message(msg)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    writer.write_all(&bytes).await?;
    writer.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_result_field_reads_calls_from_a_successful_response() {
        let text = r#"{"jsonrpc":"2.0","id":1,"result":{"calls":[
            {"at_ms":1,"source":"claude-code","tool":"recall","duration_ms":5,"bytes_in":10,"bytes_out":20,"is_error":false}
        ]}}"#;
        let calls: Vec<CallRow> = parse_result_field(text, "calls").expect("parses");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].tool, "recall");
        assert_eq!(calls[0].source, "claude-code");
        assert!(!calls[0].is_error);
    }

    #[test]
    fn parse_result_field_reads_tools_from_a_successful_response() {
        let text = r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[
            {"tool":"recall","calls":2,"errors":1,"bytes_in":20,"bytes_out":40,"total_ms":10}
        ]}}"#;
        let tools: Vec<ToolStatRow> = parse_result_field(text, "tools").expect("parses");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].tool, "recall");
        assert_eq!(tools[0].calls, 2);
        assert_eq!(tools[0].errors, 1);
    }

    #[test]
    fn parse_result_field_is_none_on_a_json_rpc_level_error() {
        let text = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"nope"}}"#;
        assert_eq!(parse_result_field::<Vec<CallRow>>(text, "calls"), None);
    }

    #[test]
    fn parse_result_field_is_none_on_garbage() {
        assert_eq!(
            parse_result_field::<Vec<CallRow>>("not json", "calls"),
            None
        );
        assert_eq!(
            parse_result_field::<Vec<CallRow>>(r#"{"result":{}}"#, "calls"),
            None
        );
    }

    #[test]
    fn latest_defaults_to_unreachable_before_anything_arrives() {
        let (_tx, rx) = mpsc::channel();
        let mut poller = AdminPoller {
            stop: Arc::new(tokio::sync::Notify::new()),
            handle: None,
            rx,
            last: LogsSnapshot::Unreachable,
        };
        assert_eq!(poller.latest(), LogsSnapshot::Unreachable);
    }

    #[test]
    fn latest_reports_poller_stopped_once_the_sender_is_dropped() {
        let (tx, rx) = mpsc::channel::<LogsSnapshot>();
        drop(tx);
        let mut poller = AdminPoller {
            stop: Arc::new(tokio::sync::Notify::new()),
            handle: None,
            rx,
            last: LogsSnapshot::Unreachable,
        };
        assert_eq!(poller.latest(), LogsSnapshot::PollerStopped);
    }

    #[test]
    fn latest_drains_to_the_newest_pending_snapshot() {
        let (tx, rx) = mpsc::channel();
        tx.send(LogsSnapshot::Unreachable).unwrap();
        tx.send(LogsSnapshot::Connected {
            calls: vec![],
            tools: vec![],
        })
        .unwrap();
        let mut poller = AdminPoller {
            stop: Arc::new(tokio::sync::Notify::new()),
            handle: None,
            rx,
            last: LogsSnapshot::Unreachable,
        };
        assert_eq!(
            poller.latest(),
            LogsSnapshot::Connected {
                calls: vec![],
                tools: vec![]
            }
        );
    }

    #[cfg(unix)]
    mod live {
        use super::*;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixListener;

        fn socket_path(dir: &std::path::Path) -> PathBuf {
            dir.join("daemon.sock")
        }

        fn tempdir() -> local_rag_test_support::TempHome {
            local_rag_test_support::TempHome::new().expect("temp home")
        }

        /// A hand-rolled fake daemon: accepts one connection, answers WELCOME, then answers
        /// exactly one `admin/tail_calls` and one `admin/tool_stats` request with fixed data —
        /// enough to drive `run_one_connection`'s real async code without a real daemon.
        async fn fake_daemon_one_cycle(listener: UnixListener) {
            let (stream, _) = listener.accept().await.expect("accept");
            let (read_half, mut write_half) = stream.into_split();
            let mut reader = BufReader::new(read_half);

            let mut line = String::new();
            reader.read_line(&mut line).await.expect("read HELLO");

            let welcome = local_rag_protocol::Message::Welcome(local_rag_protocol::Welcome {
                proto: local_rag_protocol::PROTO_VERSION,
                daemon_version: "0.0.0".to_string(),
                store_instance_uuid: "fake-instance".to_string(),
                capabilities: Vec::new(),
                mcp_passthrough_version: local_rag_protocol::MCP_PASSTHROUGH_VERSION,
                spool_max_format_version: 1,
                mode: "normal".to_string(),
            });
            write_half
                .write_all(&local_rag_protocol::encode_message(&welcome).unwrap())
                .await
                .expect("write WELCOME");

            for body in [
                r#"{"jsonrpc":"2.0","id":1,"result":{"calls":[{"at_ms":1,"source":"claude-code","tool":"recall","duration_ms":5,"bytes_in":10,"bytes_out":20,"is_error":false}]}}"#,
                r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[{"tool":"recall","calls":1,"errors":0,"bytes_in":10,"bytes_out":20,"total_ms":5}]}}"#,
            ] {
                let mut request_line = String::new();
                reader
                    .read_line(&mut request_line)
                    .await
                    .expect("read admin request");
                let response =
                    local_rag_protocol::Message::Response(local_rag_protocol::ResponseEnvelope {
                        mcp: serde_json::value::RawValue::from_string(body.to_string()).unwrap(),
                    });
                write_half
                    .write_all(&local_rag_protocol::encode_message(&response).unwrap())
                    .await
                    .expect("write Response");
            }
        }

        #[tokio::test]
        async fn a_successful_cycle_yields_a_connected_snapshot() {
            let home = tempdir();
            let path = socket_path(home.path());
            let listener = UnixListener::bind(&path).expect("bind");
            let fake = tokio::spawn(fake_daemon_one_cycle(listener));

            let context = local_rag_protocol::RequestContext {
                session_id: "test".to_string(),
                worktree_root: None,
                repo_hint: None,
            };
            let (mut reader, mut writer) = connect_and_handshake(&path, "test")
                .await
                .expect("connect + handshake");
            let snapshot = poll_once(&mut reader, &mut writer, &context)
                .await
                .expect("a snapshot");
            match snapshot {
                LogsSnapshot::Connected { calls, tools } => {
                    assert_eq!(calls.len(), 1);
                    assert_eq!(calls[0].tool, "recall");
                    assert_eq!(tools.len(), 1);
                    assert_eq!(tools[0].calls, 1);
                }
                other => panic!("expected Connected, got {other:?}"),
            }
            fake.await.expect("fake daemon task");
        }

        #[tokio::test]
        async fn no_listener_at_all_is_unreachable_quickly() {
            let home = tempdir();
            let path = socket_path(home.path()); // never bound
            let (tx, rx) = mpsc::channel();
            let stop = Arc::new(tokio::sync::Notify::new());
            tokio::time::timeout(
                Duration::from_secs(5),
                run_one_connection(&path, "test", &tx),
            )
            .await
            .expect("run_one_connection must not hang against a missing listener");
            drop(stop);
            assert_eq!(rx.recv().unwrap(), LogsSnapshot::Unreachable);
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn stop_cancels_a_connection_hung_mid_handshake() {
            // A listener that accepts and then never writes anything back — simulates a daemon
            // wedged mid-handshake. `stop()` must still return promptly.
            let home = tempdir();
            let path = socket_path(home.path());
            let listener = UnixListener::bind(&path).expect("bind");
            let _hung = tokio::spawn(async move {
                let (_stream, _) = listener.accept().await.expect("accept");
                std::future::pending::<()>().await; // never answers
            });

            let stop = Arc::new(tokio::sync::Notify::new());
            let (tx, _rx) = mpsc::channel();
            let stop_task = Arc::clone(&stop);
            let task = tokio::spawn(async move {
                tokio::select! {
                    () = notified(&stop_task) => {}
                    () = run_one_connection(&path, "test", &tx) => {}
                }
            });

            // Give the task a moment to actually be stuck inside the handshake, then signal stop.
            tokio::time::sleep(Duration::from_millis(50)).await;
            stop.notify_one();
            tokio::time::timeout(Duration::from_secs(1), task)
                .await
                .expect("stop must cancel a hung handshake promptly")
                .expect("task must not panic");
        }
    }
}
