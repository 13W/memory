//! Reading a running daemon's own greeting, without being a proxy.
//!
//! Speaks the real HELLO/WELCOME handshake (spec 02 §4.2, [`super::handshake`])
//! to obtain [`local_rag_protocol::Welcome`] — `store_instance_uuid` tells "a
//! live daemon, genuinely still this store's owner" apart from "a
//! dead/replaced process whose lock file or PID happens to still be lying
//! around", which `kill(pid, 0)` alone cannot do (PID reuse), and `Welcome
//! .mode` is the only channel distinguishing `Normal` from `MigrationOnly`
//! (spec 11 §6). [`fetch_welcome`] never registers a session or sends any
//! `Request`: it sends one synthetic HELLO (fixed `session_id`, no
//! `worktree_root`), reads one line, and disconnects.
//!
//! Its callers are the CLI liveness checks — `local-rag status`, `stop`, and
//! `cli::mod`'s shared `read_store_lock_file → pid_exists → fetch_welcome`
//! path. It used to have one more: a `LivenessProbe` trait behind
//! `daemon::lock`'s reclaim decision, deleted with that decision in D-084.
//! Whether the owner answers is a fine thing to *report*; it was never
//! evidence that the owner was dead, because a daemon in shutdown stops
//! answering long before it lets go of the lock.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::time::Duration;

use local_rag_protocol::{
    Hello, Message, PROTO_VERSION, RequestContext, RequestEnvelope, Welcome, decode_message,
    encode_message,
};
use serde_json::Value;
use serde_json::value::RawValue;

/// Bounded wait for the liveness probe's connect + HELLO/WELCOME round trip.
///
/// Not a `[SPEC]` number — the section names the *mechanism* ("a live
/// handshake on the socket") but not a budget for this internal recovery
/// probe. Picked and documented as chosen, not derived, the same precedent
/// `local_rag_search::DEFAULT_L2_READ_WAIT_BUDGET` sets for an analogous
/// internal bounded wait.
pub const LIVENESS_PROBE_TIMEOUT_MS: u64 = 1000;

/// The `session_id` this probe's own synthetic HELLO carries. Fixed rather
/// than a fresh UUID per probe: nothing ever reads it back (the probe never
/// registers with `SessionRegistry` — it disconnects the instant it has read
/// WELCOME), so a stable, greppable sentinel is strictly more useful than
/// entropy would be.
const PROBE_SESSION_ID: &str = "store-lock-liveness-probe";

/// Connect to `socket_path` and read the live daemon's own [`Welcome`],
/// bounded by `timeout` — a thin public wrapper over [`read_welcome`]
/// (`local-rag status`, T15-07: `Welcome.mode` is the only channel that
/// distinguishes `Normal` from `MigrationOnly` — `store.lock`'s own `ready`
/// flag is set in both cases, spec 11 §6). `None` on any failure (no
/// listener, refused, timed out, INCOMPATIBLE, malformed line): every failure
/// is uniformly unreachable to this caller, which has no version negotiation
/// of its own to fall back on.
#[cfg(unix)]
pub fn fetch_welcome(socket_path: &Path, timeout: Duration) -> Option<Welcome> {
    read_welcome(socket_path, timeout)
}

/// The `session_id` [`call_admin`]'s own synthetic HELLO carries — distinct
/// from [`PROBE_SESSION_ID`] so the two one-shot client roles (liveness
/// probe vs. admin verb call) stay greppable apart in any future log/debug
/// output, even though neither is ever read back by anything today.
const ADMIN_CLIENT_SESSION_ID: &str = "local-rag-admin-client";

/// Why [`call_admin`] could not return a result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallAdminError {
    /// Could not connect, or the daemon did not complete the HELLO/WELCOME
    /// handshake at all — no live daemon at `socket_path`, or something is
    /// seriously wrong with the one that is there.
    Unreachable,
    /// The handshake completed, but the verb's own response did not arrive
    /// within `timeout`.
    Timeout,
    /// The daemon answered with a JSON-RPC error — e.g. `-32602` for
    /// `admin/reconcile_now`'s own unknown/unmanaged-worktree case (T20-07).
    JsonRpcError { code: i64, message: String },
}

impl std::fmt::Display for CallAdminError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallAdminError::Unreachable => write!(f, "the daemon is unreachable"),
            CallAdminError::Timeout => write!(f, "the daemon did not answer in time"),
            CallAdminError::JsonRpcError { code, message } => {
                write!(f, "JSON-RPC error {code}: {message}")
            }
        }
    }
}

impl std::error::Error for CallAdminError {}

/// Call one `admin/*` JSON-RPC verb (spec 11 §8, T20-07) synchronously over a
/// fresh UDS connection: the same HELLO/WELCOME preamble [`read_welcome`]
/// sends — but, unlike that function, the connection stays open afterward,
/// so the `RequestEnvelope`/`ResponseEnvelope` round trip that carries the
/// actual verb can follow on the same stream. A fresh, one-shot connection
/// per call (this client has no proxy session of its own to relay through,
/// nor any reason to keep one alive) — the minimal synchronous client
/// `local-rag project` (T20-08, not implemented here) is expected to build
/// typed wrappers on top of.
#[cfg(unix)]
pub fn call_admin(
    socket_path: &Path,
    timeout: Duration,
    method: &str,
    params: Option<Value>,
) -> Result<Value, CallAdminError> {
    let mut stream = std::os::unix::net::UnixStream::connect(socket_path)
        .map_err(|_| CallAdminError::Unreachable)?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|_| CallAdminError::Unreachable)?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|_| CallAdminError::Unreachable)?;

    let hello = Message::Hello(Hello {
        proto: PROTO_VERSION,
        proxy_version: local_rag_core::VERSION.to_string(),
        session_id: ADMIN_CLIENT_SESSION_ID.to_string(),
        worktree_root: None,
        harness: "local-rag-admin-client".to_string(),
    });
    let bytes = encode_message(&hello).map_err(|_| CallAdminError::Unreachable)?;
    stream
        .write_all(&bytes)
        .map_err(|_| CallAdminError::Unreachable)?;

    let mut reader = BufReader::new(
        stream
            .try_clone()
            .map_err(|_| CallAdminError::Unreachable)?,
    );
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|_| CallAdminError::Unreachable)?;
    match decode_message(line.trim_end()).map_err(|_| CallAdminError::Unreachable)? {
        Message::Welcome(_) => {}
        _ => return Err(CallAdminError::Unreachable),
    }

    let mcp_request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });
    let mcp_raw = RawValue::from_string(mcp_request.to_string())
        .expect("a serde_json::Value always serializes to valid JSON");
    let envelope = Message::Request(RequestEnvelope {
        context: RequestContext {
            session_id: ADMIN_CLIENT_SESSION_ID.to_string(),
            worktree_root: None,
            repo_hint: None,
        },
        mcp: mcp_raw,
    });
    let bytes = encode_message(&envelope).map_err(|_| CallAdminError::Unreachable)?;
    stream
        .write_all(&bytes)
        .map_err(|_| CallAdminError::Unreachable)?;

    line.clear();
    reader
        .read_line(&mut line)
        .map_err(|_| CallAdminError::Timeout)?;
    let response: Value = match decode_message(line.trim_end()) {
        Ok(Message::Response(env)) => {
            serde_json::from_str(env.mcp.get()).map_err(|_| CallAdminError::Timeout)?
        }
        _ => return Err(CallAdminError::Timeout),
    };

    if let Some(error) = response.get("error") {
        let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        return Err(CallAdminError::JsonRpcError { code, message });
    }
    Ok(response.get("result").cloned().unwrap_or(Value::Null))
}

/// Connect to `socket_path`, send this probe's own synthetic HELLO, and read
/// one [`Welcome`] line, bounded by `timeout`. Any failure along the way (no
/// listener, refused, timed out, INCOMPATIBLE, malformed line) folds into
/// `None` — every one of those is exactly "stale" to the caller, not a
/// distinct error to report.
#[cfg(unix)]
fn read_welcome(socket_path: &Path, timeout: Duration) -> Option<Welcome> {
    let mut stream = std::os::unix::net::UnixStream::connect(socket_path).ok()?;
    stream.set_read_timeout(Some(timeout)).ok()?;
    stream.set_write_timeout(Some(timeout)).ok()?;

    let hello = Message::Hello(Hello {
        proto: PROTO_VERSION,
        proxy_version: local_rag_core::VERSION.to_string(),
        session_id: PROBE_SESSION_ID.to_string(),
        worktree_root: None,
        harness: "local-rag-liveness-probe".to_string(),
    });
    let bytes = encode_message(&hello).ok()?;
    stream.write_all(&bytes).ok()?;

    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).ok()?;
    match decode_message(line.trim_end()).ok()? {
        Message::Welcome(welcome) => Some(welcome),
        _ => None,
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;

    /// A hand-rolled listener that reads (and discards) one HELLO line, then
    /// replies with one WELCOME line carrying `store_instance_uuid` — enough
    /// to drive [`fetch_welcome`] without a real daemon.
    fn bind_greeter(dir: &std::path::Path, store_instance_uuid: &str) -> PathBuf {
        let socket_path = dir.join("daemon.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind");
        let store_instance_uuid = store_instance_uuid.to_string();
        std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
                let mut line = String::new();
                let _ = reader.read_line(&mut line); // consume HELLO, content unused
                let welcome = Message::Welcome(Welcome {
                    proto: PROTO_VERSION,
                    daemon_version: "0.0.0".to_string(),
                    store_instance_uuid,
                    capabilities: Vec::new(),
                    mcp_passthrough_version: local_rag_protocol::MCP_PASSTHROUGH_VERSION,
                    spool_max_format_version: local_rag_core::spool::FORMAT_VERSION,
                    mode: "normal".to_string(),
                });
                let bytes = encode_message(&welcome).expect("encode welcome");
                let mut stream = stream;
                let _ = stream.write_all(&bytes);
            }
        });
        socket_path
    }

    /// The budget every caller passes; short enough that a test with no
    /// listener at all is not a wall-clock wait.
    fn budget() -> Duration {
        Duration::from_millis(LIVENESS_PROBE_TIMEOUT_MS)
    }

    #[test]
    fn a_live_daemon_answers_with_its_own_store_instance_uuid() {
        let dir = tempdir();
        let socket_path = bind_greeter(dir.path(), "uuid-a");
        let welcome = fetch_welcome(&socket_path, budget()).expect("a greeter answers");
        assert_eq!(welcome.store_instance_uuid, "uuid-a");
    }

    #[test]
    fn a_different_daemon_answers_with_a_different_uuid() {
        // The PID-reuse scenario, which is why callers compare the uuid and
        // not just "something answered": the listener is genuinely alive, but
        // it is not the daemon `store.lock` claims.
        let dir = tempdir();
        let socket_path = bind_greeter(dir.path(), "uuid-different");
        let welcome = fetch_welcome(&socket_path, budget()).expect("a greeter answers");
        assert_ne!(welcome.store_instance_uuid, "uuid-a");
    }

    #[test]
    fn no_listener_at_all_is_none() {
        let dir = tempdir();
        let socket_path = dir.path().join("no-such-daemon.sock");
        assert!(fetch_welcome(&socket_path, budget()).is_none());
    }

    #[test]
    fn incompatible_proto_is_none() {
        let dir = tempdir();
        let socket_path = dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind");
        std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
                let mut line = String::new();
                let _ = reader.read_line(&mut line);
                let incompatible = Message::Incompatible(local_rag_protocol::Incompatible {
                    min_proto: 2,
                    max_proto: 3,
                    daemon_version: "9.9.9".to_string(),
                });
                let bytes = encode_message(&incompatible).expect("encode incompatible");
                let mut stream = stream;
                let _ = stream.write_all(&bytes);
            }
        });
        assert!(fetch_welcome(&socket_path, budget()).is_none());
    }

    fn tempdir() -> local_rag_test_support::TempHome {
        local_rag_test_support::TempHome::new().expect("temp home")
    }
}
