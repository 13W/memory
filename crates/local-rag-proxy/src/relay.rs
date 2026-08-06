//! Bidirectional stdio<->UDS relay (spec 11 §1's "thin pass-through", adding
//! `RequestContext` to every relayed call), the reconnect loop that keeps a
//! live session across an independently initiated daemon restart (D-038),
//! and this proxy's own SIGTERM/CTRL-C listener.

use std::path::Path;
use std::time::Duration;

use serde_json::value::RawValue;
use tokio::io::{AsyncBufRead, AsyncWrite};

use local_rag_protocol::{Message, RequestContext, RequestEnvelope};

#[cfg(unix)]
use crate::connect::DEFAULT_BACKOFF;
use crate::error::ProxyError;
#[cfg(unix)]
use crate::handshake::{
    EstablishedSession, MAX_UPGRADE_ROUNDS, SessionParams, establish_session, session_warnings,
};
use crate::transport::{read_bounded_line, write_line, write_message};

/// A bound on consecutive reconnect attempts that never produce a working
/// session — the same defensive shape, and deliberately the same value, as
/// [`MAX_UPGRADE_ROUNDS`]: a daemon that cannot come up, or that accepts a
/// connection and immediately drops it, must not spin this proxy forever.
/// The budget resets as soon as a reconnected session answers a request, so
/// a long-lived session survives arbitrarily many *successful* daemon
/// restarts.
#[cfg(unix)]
pub const MAX_RECONNECT_ROUNDS: u32 = MAX_UPGRADE_ROUNDS;

/// JSON-RPC "server error" (the `-32000..=-32099` implementation-defined
/// range) for a request the daemon never got to answer.
const TRANSPORT_ERROR_CODE: i32 = -32000;

/// Contains no `"` or `\`, so [`transport_error_line`] can splice it into a
/// JSON string literal without an escaping pass.
const TRANSPORT_ERROR_MESSAGE: &str =
    "the local-rag daemon closed the connection before answering this request";

/// This proxy's own SIGTERM/CTRL-C listener — a standalone copy of
/// `local-rag`'s `daemon::shutdown::ShutdownSignal`, not a shared type: five
/// lines per call site does not earn extraction into a shared crate, the
/// same trade-off `daemon::shutdown`'s own module doc already accepts
/// (D-002/D-010-style duplication). This is deliberately **not** forwarded
/// to a spawned daemon — a daemon this proxy spawns runs in its own process
/// group (`connect::spawn_detached_daemon`) specifically so a signal here
/// never reaches it; only this proxy's own relay loop reacts.
#[cfg(unix)]
pub struct ShutdownSignal {
    term: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl ShutdownSignal {
    /// Install the SIGTERM handler now (mirrors `daemon::shutdown::
    /// ShutdownSignal::install`'s own doc on why installing before other
    /// startup work matters: a signal delivered before the first `wait()`
    /// call must still be observed, not lost to the OS default disposition).
    pub fn install() -> Self {
        use tokio::signal::unix::{SignalKind, signal};
        let term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        ShutdownSignal { term }
    }

    /// Wait for SIGTERM or CTRL-C (SIGINT), whichever arrives first.
    pub async fn wait(&mut self) {
        tokio::select! {
            _ = self.term.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
    }
}

/// Everything [`relay`] needs to re-establish a session after the daemon
/// goes away mid-relay: exactly the inputs `main` used for the first
/// connection, so a reconnect reproduces it identically — same
/// `session_id`/`worktree_root` (spec 02 §3.3), same connect-or-spawn path.
#[cfg(unix)]
pub struct DaemonEndpoint<'a> {
    pub socket_path: &'a Path,
    pub daemon_binary: &'a Path,
    pub params: &'a SessionParams,
}

/// Why [`relay_connection`] stopped relaying on one connection.
#[cfg(unix)]
#[derive(Debug, PartialEq, Eq)]
enum RelayStop {
    /// stdin reached EOF, or this proxy's own shutdown signal fired: this
    /// process is finished, there is nothing to reconnect for.
    Done,
    /// The daemon went away underneath a still-live client. `answered`
    /// records whether this connection ever relayed a response, which is
    /// what distinguishes "a working daemon was restarted" from "a daemon
    /// that accepts and immediately drops".
    DaemonClosed { answered: bool },
}

/// Ids of requests already handed to the daemon whose response has not come
/// back yet.
///
/// This proxy replays nothing across a reconnect — it holds no session state
/// to resume with (spec 11 §1) — but a request the daemon died holding must
/// still terminate: without this, the client would wait forever for a
/// response nobody is left to send. Each unfinished id gets a JSON-RPC error
/// instead, which an MCP client can retry on the reconnected session.
#[derive(Default)]
struct PendingRequests(Vec<String>);

impl PendingRequests {
    fn record(&mut self, mcp: &RawValue) {
        if let Some(id) = message_id(mcp) {
            self.0.push(id);
        }
    }

    fn resolve(&mut self, mcp: &RawValue) {
        if let Some(id) = message_id(mcp)
            && let Some(pos) = self.0.iter().position(|p| *p == id)
        {
            self.0.remove(pos);
        }
    }

    fn drain_transport_errors(&mut self) -> Vec<String> {
        self.0
            .drain(..)
            .map(|id| transport_error_line(&id))
            .collect()
    }
}

/// The raw JSON text of a JSON-RPC message's `id`, or `None` for a
/// notification. Kept as raw text rather than a parsed value because JSON-RPC
/// 2.0 §5 requires the response id to be the request's id, and the raw token
/// round-trips byte-for-byte with no number-formatting question to answer.
fn message_id(mcp: &RawValue) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct IdOnly<'a> {
        #[serde(borrow, default)]
        id: Option<&'a RawValue>,
    }
    let parsed: IdOnly = serde_json::from_str(mcp.get()).ok()?;
    parsed
        .id
        .map(|id| id.get().to_string())
        .filter(|id| id != "null")
}

/// `id` is spliced in as the raw JSON token it arrived as — already valid
/// JSON by construction, so the result is too.
fn transport_error_line(id: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":{id},"error":{{"code":{TRANSPORT_ERROR_CODE},"message":"{TRANSPORT_ERROR_MESSAGE}"}}}}"#
    )
}

/// Relay stdin <-> the daemon for as long as this process lives, reconnecting
/// whenever the daemon closes the connection from under a still-live client
/// (D-038: `local-rag restart`, `local-rag stop`, a crash, an OOM kill —
/// every drop this proxy did not itself request). `context` is fixed for the
/// whole call, reconnects included — every relayed request carries
/// byte-identical session_id/worktree_root/repo_hint (spec 02 §3.3, 11 §1):
/// this proxy holds no per-request state of its own to vary it by.
#[cfg(unix)]
pub async fn relay<I, O>(
    mut stdin: I,
    mut stdout: O,
    mut session: EstablishedSession,
    endpoint: DaemonEndpoint<'_>,
    context: RequestContext,
    mut signal: ShutdownSignal,
) -> Result<(), ProxyError>
where
    I: AsyncBufRead + Unpin,
    O: AsyncWrite + Unpin,
{
    let mut attempts_without_progress = 0u32;
    loop {
        let stop = relay_connection(
            &mut stdin,
            &mut stdout,
            &mut session.reader,
            &mut session.writer,
            &context,
            &mut signal,
        )
        .await?;
        match stop {
            RelayStop::Done => return Ok(()),
            RelayStop::DaemonClosed { answered: true } => attempts_without_progress = 0,
            RelayStop::DaemonClosed { answered: false } => {}
        }

        eprintln!(
            "{}: the daemon closed the connection; reconnecting",
            crate::BIN
        );
        session = loop {
            attempts_without_progress += 1;
            if attempts_without_progress > MAX_RECONNECT_ROUNDS {
                return Err(ProxyError::ReconnectLoopExceeded);
            }
            // A restarting daemon needs a moment to let go: an orderly drain
            // closes this connection only after releasing the store (spec 02
            // §4.3), but a crashed one leaves its lock behind for the next
            // daemon to reclaim. One base backoff delay — the same 250ms
            // `connect_or_spawn`'s own first retry waits — covers that, and
            // keeps a daemon that accepts and instantly drops from spinning
            // this loop.
            tokio::time::sleep(Duration::from_millis(DEFAULT_BACKOFF.base_ms)).await;
            match establish_session(
                endpoint.socket_path,
                endpoint.daemon_binary,
                endpoint.params,
            )
            .await
            {
                Ok(session) => break session,
                Err(e) => eprintln!("{}: reconnect attempt failed: {e}", crate::BIN),
            }
        };
        for warning in session_warnings(&session.welcome) {
            eprintln!("{}: {warning}", crate::BIN);
        }
    }
}

/// One connection's worth of relaying: stdin -> UDS (wrapping each line in a
/// `RequestEnvelope` carrying `context`) and UDS -> stdout (unwrapping
/// `ResponseEnvelope`) until either side closes or the shutdown signal fires.
#[cfg(unix)]
async fn relay_connection<I, O, R, W>(
    stdin: &mut I,
    stdout: &mut O,
    daemon_reader: &mut R,
    daemon_writer: &mut W,
    context: &RequestContext,
    signal: &mut ShutdownSignal,
) -> Result<RelayStop, ProxyError>
where
    I: AsyncBufRead + Unpin,
    O: AsyncWrite + Unpin,
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut pending = PendingRequests::default();
    let mut answered = false;
    loop {
        tokio::select! {
            _ = signal.wait() => return Ok(RelayStop::Done),
            line = read_bounded_line(stdin) => {
                match line.map_err(ProxyError::Transport)? {
                    None => return Ok(RelayStop::Done), // stdin closed: the client disconnected
                    Some(text) => {
                        let mcp = RawValue::from_string(text).map_err(ProxyError::Protocol)?;
                        pending.record(&mcp);
                        let request = Message::Request(RequestEnvelope { context: context.clone(), mcp });
                        // A write failure here is the same event the read half
                        // reports as EOF, observed from the other side: the
                        // daemon died between its last line and this write.
                        if write_message(daemon_writer, &request).await.is_err() {
                            return daemon_closed(stdout, &mut pending, answered).await;
                        }
                    }
                }
            }
            line = read_bounded_line(daemon_reader) => {
                match line {
                    Ok(None) => return daemon_closed(stdout, &mut pending, answered).await,
                    // A framing violation is the daemon misbehaving, not the
                    // daemon leaving — reconnecting would only repeat it.
                    Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                        return Err(ProxyError::Transport(e));
                    }
                    Err(_) => return daemon_closed(stdout, &mut pending, answered).await,
                    Ok(Some(text)) => {
                        match local_rag_protocol::decode_message(&text).map_err(ProxyError::Protocol)? {
                            Message::Response(resp) => {
                                pending.resolve(&resp.mcp);
                                write_line(stdout, resp.mcp.get()).await.map_err(ProxyError::Transport)?;
                                answered = true;
                            }
                            _ => return Err(ProxyError::UnexpectedMessage),
                        }
                    }
                }
            }
        }
    }
}

#[cfg(unix)]
async fn daemon_closed<O: AsyncWrite + Unpin>(
    stdout: &mut O,
    pending: &mut PendingRequests,
    answered: bool,
) -> Result<RelayStop, ProxyError> {
    for line in pending.drain_transport_errors() {
        write_line(stdout, &line)
            .await
            .map_err(ProxyError::Transport)?;
    }
    Ok(RelayStop::DaemonClosed { answered })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use local_rag_protocol::ResponseEnvelope;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};

    /// `relay_connection` needs a `ShutdownSignal` that never fires within a
    /// test's lifetime; the real signal listener is exactly that when nothing
    /// ever sends it a signal, so tests use `ShutdownSignal::install`
    /// directly rather than a separate test double.
    fn never_firing_signal() -> ShutdownSignal {
        ShutdownSignal::install()
    }

    fn context(session_id: &str) -> RequestContext {
        RequestContext {
            session_id: session_id.to_string(),
            worktree_root: Some("/repo".to_string()),
            repo_hint: None,
        }
    }

    fn raw(text: &str) -> Box<RawValue> {
        RawValue::from_string(text.to_string()).unwrap()
    }

    #[tokio::test]
    async fn one_stdin_line_becomes_one_contextualized_request_and_the_response_comes_back() {
        let (stdin_client, stdin_server) = tokio::io::duplex(4096);
        let (stdout_client, mut stdout_server) = tokio::io::duplex(4096);
        let (daemon_client, mut daemon_server) = tokio::io::duplex(4096);
        let (daemon_read, daemon_write) = tokio::io::split(daemon_client);

        let ctx = context("sess-1");
        let relay_handle = tokio::spawn(async move {
            let mut stdin = BufReader::new(stdin_server);
            let mut stdout = stdout_client;
            let mut reader = BufReader::new(daemon_read);
            let mut writer = daemon_write;
            let mut signal = never_firing_signal();
            relay_connection(
                &mut stdin,
                &mut stdout,
                &mut reader,
                &mut writer,
                &ctx,
                &mut signal,
            )
            .await
        });

        // Client -> proxy: one MCP line on stdin.
        let mut stdin_client = stdin_client;
        stdin_client.write_all(b"{\"id\":1}\n").await.unwrap();

        // Proxy -> daemon: read back the Request, assert its context, reply.
        let mut daemon_reader = BufReader::new(&mut daemon_server);
        let mut line = String::new();
        tokio::io::AsyncBufReadExt::read_line(&mut daemon_reader, &mut line)
            .await
            .unwrap();
        match local_rag_protocol::decode_message(line.trim_end()).unwrap() {
            Message::Request(env) => {
                assert_eq!(env.context.session_id, "sess-1");
                assert_eq!(env.mcp.get(), "{\"id\":1}");
                let response = Message::Response(ResponseEnvelope {
                    mcp: raw("{\"id\":1,\"result\":true}"),
                });
                let bytes = local_rag_protocol::encode_message(&response).unwrap();
                daemon_server.write_all(&bytes).await.unwrap();
            }
            other => panic!("expected Request, got {other:?}"),
        }

        // Proxy -> client: the response line on stdout.
        let mut buf = [0u8; 256];
        let n = stdout_server.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"{\"id\":1,\"result\":true}\n");

        drop(stdin_client); // close stdin: the relay must return cleanly
        let stop = tokio::time::timeout(std::time::Duration::from_secs(5), relay_handle)
            .await
            .expect("relay must exit once stdin closes")
            .expect("relay task must not panic")
            .expect("relay must not error");
        assert_eq!(stop, RelayStop::Done);
    }

    #[tokio::test]
    async fn two_relayed_requests_carry_byte_identical_context_independent_of_content() {
        let (mut stdin_client, stdin_server) = tokio::io::duplex(4096);
        let (stdout_client, _stdout_server) = tokio::io::duplex(4096);
        let (daemon_client, mut daemon_server) = tokio::io::duplex(4096);
        let (daemon_read, daemon_write) = tokio::io::split(daemon_client);

        let ctx = context("sess-2");
        let relay_handle = tokio::spawn(async move {
            let mut stdin = BufReader::new(stdin_server);
            let mut stdout = stdout_client;
            let mut reader = BufReader::new(daemon_read);
            let mut writer = daemon_write;
            let mut signal = never_firing_signal();
            relay_connection(
                &mut stdin,
                &mut stdout,
                &mut reader,
                &mut writer,
                &ctx,
                &mut signal,
            )
            .await
        });

        stdin_client.write_all(b"{\"call\":1}\n").await.unwrap();
        stdin_client.write_all(b"{\"call\":2}\n").await.unwrap();

        let mut daemon_reader = BufReader::new(&mut daemon_server);
        let mut contexts = Vec::new();
        for _ in 0..2 {
            let mut line = String::new();
            tokio::io::AsyncBufReadExt::read_line(&mut daemon_reader, &mut line)
                .await
                .unwrap();
            match local_rag_protocol::decode_message(line.trim_end()).unwrap() {
                Message::Request(env) => contexts.push(env.context),
                other => panic!("expected Request, got {other:?}"),
            }
        }
        assert_eq!(contexts[0], contexts[1]);
        assert_eq!(contexts[0].session_id, "sess-2");

        drop(stdin_client);
        relay_handle.abort();
    }

    /// D-038: the daemon vanishing mid-request must terminate that request,
    /// not leave the client waiting on a response nobody is left to send.
    #[tokio::test]
    async fn a_request_in_flight_when_the_daemon_vanishes_gets_a_transport_error_on_stdout() {
        let (mut stdin_client, stdin_server) = tokio::io::duplex(4096);
        let (stdout_client, stdout_server) = tokio::io::duplex(4096);
        let (daemon_client, mut daemon_server) = tokio::io::duplex(4096);
        let (daemon_read, daemon_write) = tokio::io::split(daemon_client);

        let ctx = context("sess-3");
        let relay_handle = tokio::spawn(async move {
            let mut stdin = BufReader::new(stdin_server);
            let mut stdout = stdout_client;
            let mut reader = BufReader::new(daemon_read);
            let mut writer = daemon_write;
            let mut signal = never_firing_signal();
            relay_connection(
                &mut stdin,
                &mut stdout,
                &mut reader,
                &mut writer,
                &ctx,
                &mut signal,
            )
            .await
        });

        stdin_client
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"tools/list\"}\n")
            .await
            .unwrap();

        // The daemon receives the request, then dies without answering it.
        let mut line = String::new();
        tokio::io::AsyncBufReadExt::read_line(&mut BufReader::new(&mut daemon_server), &mut line)
            .await
            .unwrap();
        drop(daemon_server);

        let mut stdout_reader = BufReader::new(stdout_server);
        let mut error_line = String::new();
        tokio::io::AsyncBufReadExt::read_line(&mut stdout_reader, &mut error_line)
            .await
            .unwrap();
        let error: serde_json::Value = serde_json::from_str(error_line.trim_end()).unwrap();
        assert_eq!(error["id"], serde_json::json!(7));
        assert_eq!(
            error["error"]["code"],
            serde_json::json!(TRANSPORT_ERROR_CODE)
        );

        let stop = tokio::time::timeout(std::time::Duration::from_secs(5), relay_handle)
            .await
            .expect("relay must observe the daemon closing")
            .expect("relay task must not panic")
            .expect("a daemon that closes is not an error");
        assert_eq!(stop, RelayStop::DaemonClosed { answered: false });
    }

    #[test]
    fn an_answered_request_is_no_longer_pending() {
        let mut pending = PendingRequests::default();
        pending.record(&raw(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#));
        pending.record(&raw(r#"{"jsonrpc":"2.0","id":"two","method":"ping"}"#));
        pending.resolve(&raw(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#));

        let errors = pending.drain_transport_errors();
        assert_eq!(errors.len(), 1);
        let error: serde_json::Value = serde_json::from_str(&errors[0]).unwrap();
        assert_eq!(error["id"], serde_json::json!("two"));
    }

    #[test]
    fn a_notification_is_never_pending_because_it_is_never_answered() {
        let mut pending = PendingRequests::default();
        pending.record(&raw(
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        ));
        pending.record(&raw(r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#));
        assert!(pending.drain_transport_errors().is_empty());
    }
}
