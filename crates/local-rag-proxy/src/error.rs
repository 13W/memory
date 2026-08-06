//! Proxy-local diagnostics (spec 02 §4.2, 13 §2/§4).
//!
//! Distinct from [`local_rag_protocol::ErrorCode`], which is the *daemon*-
//! side MCP tool error vocabulary (spec 02 §6). Nothing here is ever an MCP
//! JSON-RPC response: a handshake or relay failure goes to stderr and the
//! process exits non-zero instead.
//!
//! The one place this proxy does synthesize a JSON-RPC error is
//! `relay::PendingRequests` (D-038): a request already relayed to a daemon
//! that then died has an `id` this proxy has already seen, and answering it
//! is what stops the client waiting forever on a response nobody is left to
//! send. That is bookkeeping about requests in flight, not a diagnostic
//! vocabulary, so it lives with the relay rather than here.

use std::fmt;

/// Why `run_proxy` could not establish or maintain a session.
#[derive(Debug)]
pub enum ProxyError {
    /// The daemon binary could not be located next to this proxy binary.
    DaemonBinaryNotFound,
    /// Spawning a detached daemon process failed.
    Spawn(std::io::Error),
    /// The connect-or-spawn backoff budget was exhausted without a live
    /// daemon.
    ConnectTimedOut,
    /// An I/O error on the UDS connection to the daemon, or on stdin/stdout.
    Transport(std::io::Error),
    /// The connection closed before completing the handshake.
    HandshakeClosed,
    /// A line that didn't decode as this protocol's `Message`.
    Protocol(serde_json::Error),
    /// The daemon replied with something other than WELCOME/INCOMPATIBLE
    /// immediately after HELLO, or something other than RESPONSE during the
    /// relay phase — a protocol violation from an otherwise well-formed
    /// message.
    UnexpectedMessage,
    /// `WELCOME` never arrived — the daemon replied INCOMPATIBLE, naming its
    /// own supported `proto` range.
    Incompatible {
        min_proto: u16,
        max_proto: u16,
        daemon_version: String,
    },
    /// After `SHUTDOWN_REQUEST`, the old daemon did not close the connection
    /// within the upgrade timeout.
    UpgradeTimedOut,
    /// The upgrade retry loop ran out of rounds without reaching a version-
    /// matched daemon — a persistently flapping/misbehaving daemon, not a
    /// normal one-shot upgrade.
    UpgradeLoopExceeded,
    /// The relay's reconnect loop ran out of rounds without reaching a daemon
    /// that stays up — one that never comes back, or that accepts a
    /// connection and immediately drops it, rather than the ordinary restart
    /// this proxy recovers from (D-038).
    ReconnectLoopExceeded,
}

impl fmt::Display for ProxyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProxyError::DaemonBinaryNotFound => {
                write!(
                    f,
                    "could not locate the local-rag daemon binary next to this proxy"
                )
            }
            ProxyError::Spawn(e) => write!(f, "could not spawn the daemon: {e}"),
            ProxyError::ConnectTimedOut => {
                write!(f, "timed out waiting for the daemon to become reachable")
            }
            ProxyError::Transport(e) => write!(f, "connection error: {e}"),
            ProxyError::HandshakeClosed => write!(
                f,
                "the daemon closed the connection before completing the handshake"
            ),
            ProxyError::Protocol(e) => write!(f, "malformed protocol message: {e}"),
            ProxyError::UnexpectedMessage => {
                write!(
                    f,
                    "received an unexpected message for this phase of the protocol"
                )
            }
            ProxyError::Incompatible {
                min_proto,
                max_proto,
                daemon_version,
            } => write!(
                f,
                "protocol version mismatch: daemon {daemon_version} supports {min_proto}..={max_proto}"
            ),
            ProxyError::UpgradeTimedOut => write!(
                f,
                "timed out waiting for the old daemon to release the store after an upgrade request"
            ),
            ProxyError::UpgradeLoopExceeded => {
                write!(
                    f,
                    "gave up after repeated version-mismatch upgrade attempts"
                )
            }
            ProxyError::ReconnectLoopExceeded => {
                write!(
                    f,
                    "gave up reconnecting: the daemon connection kept dropping without a usable session"
                )
            }
        }
    }
}

impl std::error::Error for ProxyError {}
