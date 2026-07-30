//! Bidirectional stdio<->UDS relay (spec 11 §1's "thin pass-through", adding
//! `RequestContext` to every relayed call) and this proxy's own SIGTERM/
//! CTRL-C listener.

use serde_json::value::RawValue;
use tokio::io::{AsyncBufRead, AsyncWrite};

use local_rag_protocol::{Message, RequestContext, RequestEnvelope};

use crate::error::ProxyError;
use crate::transport::{read_bounded_line, write_line, write_message};

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

/// Relay stdin -> UDS (wrapping each line in a `RequestEnvelope` carrying
/// `context`) and UDS -> stdout (unwrapping `ResponseEnvelope`) until either
/// side closes or the shutdown signal fires. `context` is fixed for the
/// whole call — every relayed request carries byte-identical
/// session_id/worktree_root/repo_hint (spec 02 §3.3, 11 §1): this proxy
/// holds no per-request state of its own to vary it by.
pub async fn relay<I, O, R, W>(
    mut stdin: I,
    mut stdout: O,
    mut daemon_reader: R,
    mut daemon_writer: W,
    context: RequestContext,
    mut signal: ShutdownSignal,
) -> Result<(), ProxyError>
where
    I: AsyncBufRead + Unpin,
    O: AsyncWrite + Unpin,
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    loop {
        tokio::select! {
            _ = signal.wait() => return Ok(()),
            line = read_bounded_line(&mut stdin) => {
                match line.map_err(ProxyError::Transport)? {
                    None => return Ok(()), // stdin closed: the client disconnected
                    Some(text) => {
                        let mcp = RawValue::from_string(text).map_err(ProxyError::Protocol)?;
                        let request = Message::Request(RequestEnvelope { context: context.clone(), mcp });
                        write_message(&mut daemon_writer, &request).await.map_err(ProxyError::Transport)?;
                    }
                }
            }
            line = read_bounded_line(&mut daemon_reader) => {
                match line.map_err(ProxyError::Transport)? {
                    None => return Ok(()), // the daemon closed: nothing left to relay
                    Some(text) => {
                        match local_rag_protocol::decode_message(&text).map_err(ProxyError::Protocol)? {
                            Message::Response(resp) => {
                                write_line(&mut stdout, resp.mcp.get()).await.map_err(ProxyError::Transport)?;
                            }
                            _ => return Err(ProxyError::UnexpectedMessage),
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use local_rag_protocol::ResponseEnvelope;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};

    /// `relay` needs a `ShutdownSignal` that never fires within a test's
    /// lifetime; `#[cfg(unix)]`'s real signal listener is exactly that when
    /// nothing ever sends it a signal, so tests use `ShutdownSignal::install`
    /// directly rather than a separate test double.
    fn never_firing_signal() -> ShutdownSignal {
        ShutdownSignal::install()
    }

    #[tokio::test]
    async fn one_stdin_line_becomes_one_contextualized_request_and_the_response_comes_back() {
        let (stdin_client, stdin_server) = tokio::io::duplex(4096);
        let (stdout_client, mut stdout_server) = tokio::io::duplex(4096);
        let (daemon_client, mut daemon_server) = tokio::io::duplex(4096);
        let (daemon_read, daemon_write) = tokio::io::split(daemon_client);

        let context = RequestContext {
            session_id: "sess-1".to_string(),
            worktree_root: Some("/repo".to_string()),
            repo_hint: None,
        };

        let relay_handle = tokio::spawn(relay(
            BufReader::new(stdin_server),
            stdout_client,
            BufReader::new(daemon_read),
            daemon_write,
            context.clone(),
            never_firing_signal(),
        ));

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
                    mcp: RawValue::from_string("{\"id\":1,\"result\":true}".to_string()).unwrap(),
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
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), relay_handle)
            .await
            .expect("relay must exit once stdin closes")
            .expect("relay task must not panic");
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn two_relayed_requests_carry_byte_identical_context_independent_of_content() {
        let (mut stdin_client, stdin_server) = tokio::io::duplex(4096);
        let (stdout_client, _stdout_server) = tokio::io::duplex(4096);
        let (daemon_client, mut daemon_server) = tokio::io::duplex(4096);
        let (daemon_read, daemon_write) = tokio::io::split(daemon_client);

        let context = RequestContext {
            session_id: "sess-2".to_string(),
            worktree_root: None,
            repo_hint: None,
        };

        let relay_handle = tokio::spawn(relay(
            BufReader::new(stdin_server),
            stdout_client,
            BufReader::new(daemon_read),
            daemon_write,
            context.clone(),
            never_firing_signal(),
        ));

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
}
