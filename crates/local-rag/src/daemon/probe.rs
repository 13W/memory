//! The store-lock liveness probe (spec 02 §4.1: "verify the owning process
//! exists **and** its instance UUID matches a live handshake on the socket").
//!
//! Speaks the real HELLO/WELCOME handshake (spec 02 §4.2, [`super::handshake`])
//! against `store_instance_uuid` in [`local_rag_protocol::Welcome`] — a
//! recovering daemon that finds `store.lock` already held needs to tell "a
//! live daemon, genuinely still this store's owner" apart from "a
//! dead/replaced process whose lock file or PID happens to still be lying
//! around": `kill(pid, 0)` alone cannot make that distinction, because an
//! unrelated process may have been assigned the same PID since (PID reuse)
//! — this is exactly the gap the card's "PID reuse mismatch" test exercises.
//! This probe is itself never a real proxy: it sends its own synthetic HELLO
//! (fixed `session_id`, no `worktree_root`) purely to elicit a WELCOME to
//! compare `store_instance_uuid` against, and never registers a session or
//! sends any `Request`.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use local_rag_core::process::pid_exists;
use local_rag_protocol::{Hello, Message, PROTO_VERSION, Welcome, decode_message, encode_message};

/// The result of probing a candidate store-lock owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LivenessOutcome {
    /// The PID exists **and** the socket answered WELCOME with the expected
    /// `store_instance_uuid`: the same daemon that wrote the lock is still
    /// running.
    Alive,
    /// The PID is gone, or the socket did not answer WELCOME with a matching
    /// `store_instance_uuid` — unreachable, timed out, INCOMPATIBLE, or (PID
    /// reuse) a different process entirely.
    Stale,
}

/// How to determine whether a candidate `store.lock` owner is still alive.
///
/// A trait, not a bare function, so tests can inject a synthetic probe target
/// deterministically (a hand-rolled listener in the test process) instead of
/// needing a second real daemon.
pub trait LivenessProbe {
    /// Check whether `pid`/`expected_instance_uuid` together identify a
    /// still-live owner.
    fn check(&self, pid: u32, expected_instance_uuid: &str) -> LivenessOutcome;
}

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

/// The production [`LivenessProbe`]: connects to the store's UDS, sends
/// HELLO, and reads one newline-terminated WELCOME (or INCOMPATIBLE) line.
#[cfg(unix)]
pub struct SocketLivenessProbe {
    socket_path: PathBuf,
    timeout: Duration,
}

#[cfg(unix)]
impl SocketLivenessProbe {
    /// A probe targeting `socket_path` with the default
    /// [`LIVENESS_PROBE_TIMEOUT_MS`] budget.
    pub fn new(socket_path: PathBuf) -> Self {
        Self {
            socket_path,
            timeout: Duration::from_millis(LIVENESS_PROBE_TIMEOUT_MS),
        }
    }
}

#[cfg(unix)]
impl LivenessProbe for SocketLivenessProbe {
    fn check(&self, pid: u32, expected_instance_uuid: &str) -> LivenessOutcome {
        if !pid_exists(pid) {
            return LivenessOutcome::Stale;
        }
        match read_welcome(&self.socket_path, self.timeout) {
            Some(welcome) if welcome.store_instance_uuid == expected_instance_uuid => {
                LivenessOutcome::Alive
            }
            // INCOMPATIBLE, garbage, a timeout, or a mismatched uuid are all
            // exactly "stale" to this recovery probe — it has no version
            // negotiation of its own to fall back to.
            _ => LivenessOutcome::Stale,
        }
    }
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

    /// A hand-rolled listener that reads (and discards) one HELLO line, then
    /// replies with one WELCOME line carrying `store_instance_uuid` — enough
    /// to drive [`SocketLivenessProbe`] without a real daemon.
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

    #[test]
    fn matching_instance_uuid_and_live_pid_is_alive() {
        let dir = tempdir();
        let socket_path = bind_greeter(dir.path(), "uuid-a");
        let probe = SocketLivenessProbe::new(socket_path);
        assert_eq!(
            probe.check(std::process::id(), "uuid-a"),
            LivenessOutcome::Alive
        );
    }

    #[test]
    fn mismatched_instance_uuid_is_stale_even_though_the_pid_is_alive() {
        // The PID-reuse scenario: the process at `pid` is genuinely alive
        // (our own test process), but it is not the daemon `store.lock`
        // claims — a different `instance_uuid` answers.
        let dir = tempdir();
        let socket_path = bind_greeter(dir.path(), "uuid-different");
        let probe = SocketLivenessProbe::new(socket_path);
        assert_eq!(
            probe.check(std::process::id(), "uuid-a"),
            LivenessOutcome::Stale
        );
    }

    #[test]
    fn dead_pid_is_stale_regardless_of_the_socket() {
        let dir = tempdir();
        let socket_path = bind_greeter(dir.path(), "uuid-a");
        let dead_pid = spawn_and_reap();
        let probe = SocketLivenessProbe::new(socket_path);
        assert_eq!(probe.check(dead_pid, "uuid-a"), LivenessOutcome::Stale);
    }

    #[test]
    fn no_listener_at_all_is_stale() {
        let dir = tempdir();
        let socket_path = dir.path().join("no-such-daemon.sock");
        let probe = SocketLivenessProbe::new(socket_path);
        assert_eq!(
            probe.check(std::process::id(), "uuid-a"),
            LivenessOutcome::Stale
        );
    }

    #[test]
    fn incompatible_proto_is_stale() {
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
        let probe = SocketLivenessProbe::new(socket_path);
        assert_eq!(
            probe.check(std::process::id(), "uuid-a"),
            LivenessOutcome::Stale
        );
    }

    fn spawn_and_reap() -> u32 {
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn trivial child");
        let pid = child.id();
        child.wait().expect("wait");
        pid
    }

    fn tempdir() -> local_rag_test_support::TempHome {
        local_rag_test_support::TempHome::new().expect("temp home")
    }
}
