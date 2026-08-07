//! The real per-connection HELLO/WELCOME/INCOMPATIBLE/SHUTDOWN_REQUEST
//! handler (spec 02 §4.2) — T15-02. Replaces T15-01's provisional
//! `daemon::handshake_stub`, which existed only so the store-lock liveness
//! probe (`daemon::probe`) had something live to talk to before this task.
//!
//! # Why a `ShutdownRequest` does not close the connection
//!
//! `handle_connection` keeps looping after receiving `ShutdownRequest` —
//! it only signals `HandshakeContext::shutdown_requested` and continues.
//! The requesting proxy needs to observe EOF only once this daemon has
//! *actually finished draining* (spec 13 §4: "old daemon finishes in-flight
//! jobs, releases, exits" — the proxy waits for that before treating the
//! path as clear to spawn a new daemon). No explicit teardown code makes
//! that true: `main.rs::run_serve`'s manually built `tokio::runtime::
//! Runtime` is a local variable that drops once `serve()`'s top-level
//! future — which already `.await`s the full `DaemonHandle::shutdown()`
//! drain sequence (checkpoint, cache close, lock release) before returning —
//! completes. Dropping a `Runtime` forcibly drops every still-running task,
//! including this connection's, which closes its `UnixStream` and is
//! observed by the proxy as EOF — but only *after* the drain the `Runtime`
//! was still driving to completion. Closing the connection here instead
//! would let a proxy race ahead of the real drain.
//!
//! # `RequestHandler` — type before backend
//!
//! Real MCP tool dispatch is T15-03's job. This task ships only the
//! transport, the context envelope, and [`EchoRequestHandler`] — a stub
//! that proves the wiring (T15-02's own tests assert context survives a
//! round trip) without knowing anything about MCP tool schemas. The same
//! "type before backend" precedent this project already used for
//! `ProjectionStore` (T07-01) and `Generator` (T11-03).
//!
//! # Why `RequestHandler::handle` returns `Option`, not `Box<RawValue>`
//!
//! T15-03, `daemon::mcp`'s consumer: MCP's own handshake requires the client
//! to send `notifications/initialized` — a JSON-RPC **notification** (no
//! `id`), which JSON-RPC 2.0 §4.1 says MUST NOT receive a response at all.
//! `handle_connection`'s `Message::Request` arm only writes a `Message::
//! Response` when `handle` returns `Some`; on `None` it writes nothing and
//! loops. This is load-bearing all the way to the proxy: `local-rag-proxy`'s
//! `relay.rs` forwards every `Message::Response` straight to the client's
//! stdout with no request/response pairing of its own (spec 11 §1's "thin
//! pass-through"), so answering a notification here would put an
//! unsolicited, unmatched line on the client's stdin — a real protocol
//! violation a strict MCP client need not tolerate. `RequestHandler` is an
//! internal Rust trait (not a documented wire contract — spec 02 §4.2 fixes
//! `RequestEnvelope`/`ResponseEnvelope`, not this trait's shape), so
//! widening its return type here is ordinary interface evolution for the
//! implementer T15-02's own doc already named as "later," not a deviation
//! from already-shipped behavior.

use std::ops::RangeInclusive;
use std::sync::Arc;

use local_rag_protocol::{
    Hello, Incompatible, MAX_MESSAGE_BYTES, MCP_PASSTHROUGH_VERSION, Message, RequestContext,
    ResponseEnvelope, Welcome, decode_message, encode_message, negotiate_proto,
};
use serde_json::value::RawValue;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Notify, oneshot, watch};

use super::mode::DaemonMode;
use super::session::SessionRegistry;
use super::tool_calls::ToolCallCounters;

/// Everything a per-connection task needs — cheap to clone per accept
/// (`Arc<str>`/`watch::Receiver`/`SessionRegistry`/`ToolCallCounters`/
/// `Arc<Notify>` all already share their underlying state across clones).
#[derive(Clone)]
pub struct HandshakeContext {
    pub instance_uuid: Arc<str>,
    pub daemon_version: Arc<str>,
    pub supported_proto: RangeInclusive<u16>,
    pub mode: watch::Receiver<DaemonMode>,
    pub sessions: SessionRegistry,
    /// `tools/call` observability counters (spec 11 §2, T19-05) — a guard
    /// begins tracking this connection's session id alongside the existing
    /// `SessionRegistry` registration below; `mcp::McpHandler` records into
    /// the same shared counters on every dispatched call.
    pub tool_calls: ToolCallCounters,
    /// Signaled once when any connection sends `ShutdownRequest` (spec 02
    /// §4.2, 13 §4's upgrade flow) — `lifecycle::wait_for_shutdown_trigger`
    /// is the reader.
    pub shutdown_requested: Arc<Notify>,
}

/// T15-03's seam: the real MCP tool dispatcher implements this later.
/// Native `async fn` in a trait (stable since Rust 1.75; this workspace's
/// MSRV 1.96 already satisfies it) — no `dyn`/`async-trait` boilerplate,
/// since the daemon only ever runs one concrete handler at a time.
pub trait RequestHandler: Clone + Send + Sync + 'static {
    /// Produce the MCP JSON-RPC response for one already-contextualized
    /// request, or `None` if `mcp` was a JSON-RPC **notification** (no `id`
    /// — `notifications/initialized` is the one every MCP session sends).
    /// JSON-RPC 2.0 §4.1 forbids a response to a notification; `None` is
    /// what lets [`handle_connection`] honor that (see its own doc for why
    /// this matters all the way out to the proxy's stdout).
    ///
    /// Explicit `-> impl Future<..> + Send` rather than a bare `async fn`:
    /// native async-fn-in-trait's return type is `Send` only when every
    /// captured value is, and the compiler cannot infer that bound across an
    /// unconstrained implementer — `handle_connection` needs it to
    /// `tokio::spawn` the connection task, so it must be part of the trait's
    /// contract, not left implicit.
    fn handle(
        &self,
        ctx: RequestContext,
        mcp: Box<RawValue>,
    ) -> impl std::future::Future<Output = Option<Box<RawValue>>> + Send;
}

/// T15-02's own stub: echoes the received context and payload back inside
/// the reply, so "context on every request" is directly assertable from a
/// real end-to-end proxy round trip with zero real MCP dispatch logic.
/// Always answers (`Some`) — it has no notion of JSON-RPC notifications,
/// that arrives with T15-03's real `RequestHandler`.
#[derive(Debug, Clone, Copy, Default)]
pub struct EchoRequestHandler;

#[derive(serde::Serialize)]
struct EchoResponse<'a> {
    echo: bool,
    context: RequestContext,
    received: &'a RawValue,
}

impl RequestHandler for EchoRequestHandler {
    async fn handle(&self, ctx: RequestContext, mcp: Box<RawValue>) -> Option<Box<RawValue>> {
        let body = EchoResponse {
            echo: true,
            context: ctx,
            received: &mcp,
        };
        let text = serde_json::to_string(&body).expect("EchoResponse always serializes");
        Some(RawValue::from_string(text).expect("serde_json::to_string always yields valid JSON"))
    }
}

/// Accept connections on `listener`, spawning one long-lived task per
/// connection to speak the full handshake + passthrough protocol. Runs
/// until `stop` fires, then returns. Existing per-connection tasks are
/// **not** individually tracked or aborted here — see this module's own doc
/// for why that is deliberate, not an oversight.
///
/// A malformed accept (a transient OS error) is retried — the listener
/// itself stays bound; only one accept attempt failed (same policy T15-01's
/// `handshake_stub` already established).
#[cfg(unix)]
pub async fn serve_connections<H: RequestHandler>(
    listener: UnixListener,
    ctx: HandshakeContext,
    handler: H,
    mut stop: oneshot::Receiver<()>,
) {
    loop {
        tokio::select! {
            _ = &mut stop => return,
            accepted = listener.accept() => {
                let Ok((stream, _addr)) = accepted else { continue };
                let ctx = ctx.clone();
                let handler = handler.clone();
                tokio::spawn(async move {
                    handle_connection(stream, ctx, handler).await;
                });
            }
        }
    }
}

#[cfg(unix)]
async fn handle_connection<H: RequestHandler>(
    stream: UnixStream,
    ctx: HandshakeContext,
    handler: H,
) {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    let Some(hello) = read_hello(&mut reader).await else {
        return; // malformed/oversized/EOF before a valid HELLO: nothing to say back
    };

    let (reply, accepted) = match negotiate_proto(&ctx.supported_proto, hello.proto) {
        Ok(proto) => (
            Message::Welcome(Welcome {
                proto,
                daemon_version: ctx.daemon_version.to_string(),
                store_instance_uuid: ctx.instance_uuid.to_string(),
                capabilities: Vec::new(),
                mcp_passthrough_version: MCP_PASSTHROUGH_VERSION,
                spool_max_format_version: local_rag_core::spool::FORMAT_VERSION,
                mode: ctx.mode.borrow().as_str().to_string(),
            }),
            true,
        ),
        Err((min_proto, max_proto)) => (
            Message::Incompatible(Incompatible {
                min_proto,
                max_proto,
                daemon_version: ctx.daemon_version.to_string(),
            }),
            false,
        ),
    };
    if write_message(&mut write_half, &reply).await.is_err() || !accepted {
        return;
    }

    let _session_guard = ctx.sessions.register(hello.session_id.clone());
    let _tool_call_guard = ctx.tool_calls.begin_session(hello.session_id.clone());

    loop {
        let Some(line) = read_bounded_line(&mut reader, MAX_MESSAGE_BYTES).await else {
            return; // EOF, I/O error, or an oversized line: session over
        };
        let Ok(msg) = decode_message(&line) else {
            return; // protocol garbage: drop the connection
        };
        match msg {
            Message::Request(env) => {
                let Some(response) = handler.handle(env.context, env.mcp).await else {
                    // A JSON-RPC notification: no response, by construction
                    // — see this module's own doc for why.
                    continue;
                };
                let out = Message::Response(ResponseEnvelope { mcp: response });
                if write_message(&mut write_half, &out).await.is_err() {
                    return;
                }
            }
            Message::ShutdownRequest(_) => {
                ctx.shutdown_requested.notify_one();
                // Deliberately keep looping — see this module's own doc.
            }
            // Hello/Welcome/Incompatible/Response are never valid from a
            // proxy past the handshake: protocol violation, drop it.
            Message::Hello(_)
            | Message::Welcome(_)
            | Message::Incompatible(_)
            | Message::Response(_) => {
                return;
            }
        }
    }
}

async fn read_hello<R: AsyncBufRead + Unpin>(reader: &mut R) -> Option<Hello> {
    let line = read_bounded_line(reader, MAX_MESSAGE_BYTES).await?;
    match decode_message(&line) {
        Ok(Message::Hello(hello)) => Some(hello),
        _ => None,
    }
}

/// Read one `\n`-terminated line, bounded to `max_bytes`. Unlike a bare
/// `AsyncBufReadExt::read_until` (which buffers without limit until it finds
/// the delimiter), this checks the accumulated length after **every**
/// underlying read, so a peer that never sends `\n` cannot force unbounded
/// growth — the check happens incrementally, not only once the whole
/// (potentially unbounded) line has already been buffered.
async fn read_bounded_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max_bytes: usize,
) -> Option<String> {
    let mut out = Vec::new();
    loop {
        let available = reader.fill_buf().await.ok()?;
        if available.is_empty() {
            return None; // clean EOF, with or without a partial line: either way, nothing usable
        }
        match available.iter().position(|&b| b == b'\n') {
            Some(pos) => {
                out.extend_from_slice(&available[..pos]);
                let consumed = pos + 1;
                reader.consume(consumed);
                if out.len() > max_bytes {
                    return None;
                }
                return String::from_utf8(out).ok();
            }
            None => {
                out.extend_from_slice(available);
                let consumed = available.len();
                reader.consume(consumed);
                if out.len() > max_bytes {
                    return None;
                }
            }
        }
    }
}

async fn write_message<W: AsyncWrite + Unpin>(w: &mut W, msg: &Message) -> std::io::Result<()> {
    let bytes =
        encode_message(msg).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    w.write_all(&bytes).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use local_rag_test_support::TempHome;
    use std::io::{BufRead, BufReader as StdBufReader, Write};
    use std::os::unix::net::UnixStream as StdUnixStream;

    fn handshake_ctx(supported_proto: RangeInclusive<u16>) -> HandshakeContext {
        let (_mode_tx, mode_rx) = watch::channel(DaemonMode::Normal);
        HandshakeContext {
            instance_uuid: Arc::from("instance-a"),
            daemon_version: Arc::from("0.0.0"),
            supported_proto,
            mode: mode_rx,
            sessions: SessionRegistry::new(),
            tool_calls: ToolCallCounters::new(),
            shutdown_requested: Arc::new(Notify::new()),
        }
    }

    fn bind(home: &TempHome) -> (std::path::PathBuf, UnixListener) {
        let socket_path = home.join("daemon.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind");
        (socket_path, listener)
    }

    /// Blocking std client helper: write one line, read one line back —
    /// mirrors T15-01's own `handshake_stub` test idiom (a `spawn_blocking`
    /// std client against the real async listener keeps tests simple).
    fn write_line(stream: &mut StdUnixStream, msg: &Message) {
        let mut bytes = encode_message(msg).unwrap();
        stream.write_all(&bytes).unwrap();
        bytes.clear();
    }

    fn read_line(reader: &mut StdBufReader<StdUnixStream>) -> Option<Message> {
        let mut line = String::new();
        let n = reader.read_line(&mut line).ok()?;
        if n == 0 {
            return None;
        }
        decode_message(line.trim_end()).ok()
    }

    #[tokio::test]
    async fn compatible_hello_gets_welcome_with_the_negotiated_fields() {
        let home = TempHome::new().expect("temp home");
        let (socket_path, listener) = bind(&home);
        let ctx = handshake_ctx(1..=1);
        let (_stop_tx, stop_rx) = oneshot::channel();
        let server = tokio::spawn(serve_connections(
            listener,
            ctx,
            EchoRequestHandler,
            stop_rx,
        ));

        let welcome = tokio::task::spawn_blocking(move || {
            let mut stream = StdUnixStream::connect(&socket_path).expect("connect");
            write_line(
                &mut stream,
                &Message::Hello(Hello {
                    proto: 1,
                    proxy_version: "0.0.0".to_string(),
                    session_id: "sess-1".to_string(),
                    worktree_root: Some("/repo".to_string()),
                    harness: "claude-code".to_string(),
                }),
            );
            let mut reader = StdBufReader::new(stream);
            read_line(&mut reader)
        })
        .await
        .expect("blocking task");

        match welcome {
            Some(Message::Welcome(w)) => {
                assert_eq!(w.proto, 1);
                assert_eq!(w.daemon_version, "0.0.0");
                assert_eq!(w.store_instance_uuid, "instance-a");
                assert_eq!(w.mode, "normal");
                assert_eq!(w.mcp_passthrough_version, MCP_PASSTHROUGH_VERSION);
                assert_eq!(
                    w.spool_max_format_version,
                    local_rag_core::spool::FORMAT_VERSION
                );
            }
            other => panic!("expected Welcome, got {other:?}"),
        }
        server.abort();
    }

    #[tokio::test]
    async fn incompatible_proto_gets_incompatible_and_the_connection_closes() {
        let home = TempHome::new().expect("temp home");
        let (socket_path, listener) = bind(&home);
        let ctx = handshake_ctx(2..=3);
        let (_stop_tx, stop_rx) = oneshot::channel();
        let server = tokio::spawn(serve_connections(
            listener,
            ctx,
            EchoRequestHandler,
            stop_rx,
        ));

        let (incompatible, closed) = tokio::task::spawn_blocking(move || {
            let mut stream = StdUnixStream::connect(&socket_path).expect("connect");
            write_line(
                &mut stream,
                &Message::Hello(Hello {
                    proto: 1,
                    proxy_version: "0.0.0".to_string(),
                    session_id: "sess-1".to_string(),
                    worktree_root: None,
                    harness: "claude-code".to_string(),
                }),
            );
            let mut reader = StdBufReader::new(stream);
            let msg = read_line(&mut reader);
            // The connection must close right after: a further read hits EOF.
            let mut probe = String::new();
            let closed = reader.read_line(&mut probe).map(|n| n == 0).unwrap_or(true);
            (msg, closed)
        })
        .await
        .expect("blocking task");

        match incompatible {
            Some(Message::Incompatible(i)) => {
                assert_eq!(i.min_proto, 2);
                assert_eq!(i.max_proto, 3);
                assert_eq!(i.daemon_version, "0.0.0");
            }
            other => panic!("expected Incompatible, got {other:?}"),
        }
        assert!(closed, "the connection must close after Incompatible");
        server.abort();
    }

    #[tokio::test]
    async fn two_requests_on_one_connection_keep_their_own_context() {
        let home = TempHome::new().expect("temp home");
        let (socket_path, listener) = bind(&home);
        let ctx = handshake_ctx(1..=1);
        let (_stop_tx, stop_rx) = oneshot::channel();
        let server = tokio::spawn(serve_connections(
            listener,
            ctx,
            EchoRequestHandler,
            stop_rx,
        ));

        let (first, second) = tokio::task::spawn_blocking(move || {
            let mut stream = StdUnixStream::connect(&socket_path).expect("connect");
            write_line(
                &mut stream,
                &Message::Hello(Hello {
                    proto: 1,
                    proxy_version: "0.0.0".to_string(),
                    session_id: "sess-1".to_string(),
                    worktree_root: Some("/repo-a".to_string()),
                    harness: "claude-code".to_string(),
                }),
            );
            let mut reader = StdBufReader::new(stream.try_clone().unwrap());
            let _welcome = read_line(&mut reader);

            let context_a = RequestContext {
                session_id: "sess-1".to_string(),
                worktree_root: Some("/repo-a".to_string()),
                repo_hint: None,
            };
            write_line(
                &mut stream,
                &Message::Request(local_rag_protocol::RequestEnvelope {
                    context: context_a.clone(),
                    mcp: RawValue::from_string(r#"{"call":1}"#.to_string()).unwrap(),
                }),
            );
            let first = read_line(&mut reader);

            let context_b = RequestContext {
                session_id: "sess-1".to_string(),
                worktree_root: Some("/repo-b".to_string()),
                repo_hint: None,
            };
            write_line(
                &mut stream,
                &Message::Request(local_rag_protocol::RequestEnvelope {
                    context: context_b,
                    mcp: RawValue::from_string(r#"{"call":2}"#.to_string()).unwrap(),
                }),
            );
            let second = read_line(&mut reader);
            (first, second)
        })
        .await
        .expect("blocking task");

        let extract_context = |msg: Option<Message>| -> RequestContext {
            match msg {
                Some(Message::Response(r)) => {
                    let echoed: serde_json::Value = serde_json::from_str(r.mcp.get()).unwrap();
                    serde_json::from_value(echoed["context"].clone()).unwrap()
                }
                other => panic!("expected Response, got {other:?}"),
            }
        };
        let ctx_a = extract_context(first);
        let ctx_b = extract_context(second);
        assert_eq!(ctx_a.worktree_root.as_deref(), Some("/repo-a"));
        assert_eq!(ctx_b.worktree_root.as_deref(), Some("/repo-b"));
        assert_ne!(
            ctx_a.worktree_root, ctx_b.worktree_root,
            "each request's own context must survive independently"
        );
        server.abort();
    }

    #[tokio::test]
    async fn shutdown_request_notifies_without_closing_the_connection() {
        let home = TempHome::new().expect("temp home");
        let (socket_path, listener) = bind(&home);
        let ctx = handshake_ctx(1..=1);
        let shutdown_flag = ctx.shutdown_requested.clone();
        let (_stop_tx, stop_rx) = oneshot::channel();
        let server = tokio::spawn(serve_connections(
            listener,
            ctx,
            EchoRequestHandler,
            stop_rx,
        ));

        let notified = shutdown_flag.notified();
        let still_responds = tokio::task::spawn_blocking(move || {
            let mut stream = StdUnixStream::connect(&socket_path).expect("connect");
            write_line(
                &mut stream,
                &Message::Hello(Hello {
                    proto: 1,
                    proxy_version: "0.0.0".to_string(),
                    session_id: "sess-1".to_string(),
                    worktree_root: None,
                    harness: "claude-code".to_string(),
                }),
            );
            let mut reader = StdBufReader::new(stream.try_clone().unwrap());
            let _welcome = read_line(&mut reader);

            write_line(
                &mut stream,
                &Message::ShutdownRequest(local_rag_protocol::ShutdownRequest {
                    requested_by_proxy_version: "0.0.1".to_string(),
                    reason: "version_mismatch".to_string(),
                }),
            );

            // The connection must still be usable afterward.
            write_line(
                &mut stream,
                &Message::Request(local_rag_protocol::RequestEnvelope {
                    context: RequestContext {
                        session_id: "sess-1".to_string(),
                        worktree_root: None,
                        repo_hint: None,
                    },
                    mcp: RawValue::from_string("{}".to_string()).unwrap(),
                }),
            );
            matches!(read_line(&mut reader), Some(Message::Response(_)))
        });

        tokio::time::timeout(std::time::Duration::from_secs(5), notified)
            .await
            .expect("shutdown_requested must fire");
        assert!(
            still_responds.await.expect("blocking task"),
            "the connection must stay open and keep answering after ShutdownRequest"
        );
        server.abort();
    }

    #[tokio::test]
    async fn stop_signal_ends_the_accept_loop() {
        let home = TempHome::new().expect("temp home");
        let (_socket_path, listener) = bind(&home);
        let ctx = handshake_ctx(1..=1);
        let (stop_tx, stop_rx) = oneshot::channel();
        let server = tokio::spawn(serve_connections(
            listener,
            ctx,
            EchoRequestHandler,
            stop_rx,
        ));

        stop_tx.send(()).expect("send stop");
        tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await
            .expect("the accept loop must actually stop on signal")
            .expect("task did not panic");
    }

    #[tokio::test]
    async fn an_oversized_line_drops_the_connection_instead_of_growing_unbounded() {
        let home = TempHome::new().expect("temp home");
        let (socket_path, listener) = bind(&home);
        let ctx = handshake_ctx(1..=1);
        let (_stop_tx, stop_rx) = oneshot::channel();
        let server = tokio::spawn(serve_connections(
            listener,
            ctx,
            EchoRequestHandler,
            stop_rx,
        ));

        let closed = tokio::task::spawn_blocking(move || {
            let mut stream = StdUnixStream::connect(&socket_path).expect("connect");
            // One byte past the cap, no trailing newline at all.
            let oversized = vec![b'a'; MAX_MESSAGE_BYTES + 1];
            let _ = stream.write_all(&oversized);
            let _ = stream.flush();
            let mut reader = StdBufReader::new(stream);
            let mut probe = String::new();
            reader.read_line(&mut probe).map(|n| n == 0).unwrap_or(true)
        })
        .await
        .expect("blocking task");

        assert!(
            closed,
            "an oversized line must drop the connection, not buffer forever"
        );
        server.abort();
    }

    /// A handler standing in for a real MCP dispatcher's notification
    /// handling: answers every payload except the literal `"notify"`
    /// string, which it treats as a JSON-RPC notification and answers with
    /// `None`.
    #[derive(Debug, Clone, Copy, Default)]
    struct SometimesSilentHandler;

    impl RequestHandler for SometimesSilentHandler {
        async fn handle(&self, _ctx: RequestContext, mcp: Box<RawValue>) -> Option<Box<RawValue>> {
            if mcp.get() == "\"notify\"" {
                return None;
            }
            Some(mcp)
        }
    }

    #[tokio::test]
    async fn a_handler_returning_none_writes_no_response_line() {
        let home = TempHome::new().expect("temp home");
        let (socket_path, listener) = bind(&home);
        let ctx = handshake_ctx(1..=1);
        let (_stop_tx, stop_rx) = oneshot::channel();
        let server = tokio::spawn(serve_connections(
            listener,
            ctx,
            SometimesSilentHandler,
            stop_rx,
        ));

        let next_reply = tokio::task::spawn_blocking(move || {
            let mut stream = StdUnixStream::connect(&socket_path).expect("connect");
            write_line(
                &mut stream,
                &Message::Hello(Hello {
                    proto: 1,
                    proxy_version: "0.0.0".to_string(),
                    session_id: "sess-1".to_string(),
                    worktree_root: None,
                    harness: "claude-code".to_string(),
                }),
            );
            let mut reader = StdBufReader::new(stream.try_clone().unwrap());
            let _welcome = read_line(&mut reader);

            let context = RequestContext {
                session_id: "sess-1".to_string(),
                worktree_root: None,
                repo_hint: None,
            };
            write_line(
                &mut stream,
                &Message::Request(local_rag_protocol::RequestEnvelope {
                    context: context.clone(),
                    mcp: RawValue::from_string("\"notify\"".to_string()).unwrap(),
                }),
            );
            write_line(
                &mut stream,
                &Message::Request(local_rag_protocol::RequestEnvelope {
                    context,
                    mcp: RawValue::from_string("\"real-call\"".to_string()).unwrap(),
                }),
            );
            // Racing a stray line for the notification against the real
            // call's response: if the handler wrote anything for "notify",
            // THIS read observes it instead of "real-call"'s answer.
            read_line(&mut reader)
        })
        .await
        .expect("blocking task");

        match next_reply {
            Some(Message::Response(env)) => {
                assert_eq!(
                    env.mcp.get(),
                    "\"real-call\"",
                    "the notification must not have produced a stray response line"
                );
            }
            other => panic!("expected the real call's Response, got {other:?}"),
        }
        server.abort();
    }
}
