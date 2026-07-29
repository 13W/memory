//! The store-lock liveness probe (spec 02 §4.1: "verify the owning process
//! exists **and** its instance UUID matches a live handshake on the socket").
//!
//! This is a minimal, explicitly provisional greeting — **not** the real
//! HELLO/WELCOME handshake (spec 02 §4.2, T15-02). It exists only so a
//! daemon that finds `store.lock` already held can tell "a live daemon,
//! genuinely still this store's owner" apart from "a dead/replaced process
//! whose lock file or PID happens to still be lying around": `kill(pid, 0)`
//! alone cannot make that distinction, because an unrelated process may have
//! been assigned the same PID since (PID reuse) — this is exactly the gap
//! the card's "PID reuse mismatch" test exercises. [`Greeting`] is also what
//! [`super::handshake_stub`] writes on every accepted connection; T15-02
//! replaces that per-connection handler wholesale (a real framed HELLO
//! parse), never the listener, the lock, or this probe's shape.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::Duration;

use local_rag_core::process::pid_exists;

/// The one-line JSON greeting a connecting probe (or, until T15-02, any raw
/// connection) reads from the store's socket.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Greeting {
    /// The daemon's process identity for this run (spec 02 §2's `store.lock`
    /// `instance_uuid` — the same value, so a probe can compare them).
    pub instance_uuid: String,
    /// The daemon binary's version string.
    pub daemon_version: String,
    /// `"normal"` or `"migration_only"` (spec 02 §6; [`super::mode::DaemonMode`]).
    pub mode: String,
}

/// The result of probing a candidate store-lock owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LivenessOutcome {
    /// The PID exists **and** the socket answered with the expected
    /// `instance_uuid`: the same daemon that wrote the lock is still running.
    Alive,
    /// The PID is gone, or the socket did not answer with a matching
    /// `instance_uuid` — unreachable, timed out, or (PID reuse) a different
    /// process entirely.
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

/// Bounded wait for the liveness probe's connect + greeting read.
///
/// Not a `[SPEC]` number — the section names the *mechanism* ("a live
/// handshake on the socket") but not a budget for this internal recovery
/// probe. Picked and documented as chosen, not derived, the same precedent
/// `local_rag_search::DEFAULT_L2_READ_WAIT_BUDGET` sets for an analogous
/// internal bounded wait.
pub const LIVENESS_PROBE_TIMEOUT_MS: u64 = 1000;

/// The production [`LivenessProbe`]: connects to the store's UDS and reads
/// one newline-terminated [`Greeting`] line.
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
        match read_greeting(&self.socket_path, self.timeout) {
            Some(greeting) if greeting.instance_uuid == expected_instance_uuid => {
                LivenessOutcome::Alive
            }
            _ => LivenessOutcome::Stale,
        }
    }
}

/// Connect to `socket_path` and read one [`Greeting`] line, bounded by
/// `timeout`. Any failure along the way (no listener, refused, timed out,
/// malformed line) is folded into `None` — every one of those is exactly
/// "stale" to the caller, not a distinct error to report.
#[cfg(unix)]
fn read_greeting(socket_path: &Path, timeout: Duration) -> Option<Greeting> {
    let stream = std::os::unix::net::UnixStream::connect(socket_path).ok()?;
    stream.set_read_timeout(Some(timeout)).ok()?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).ok()?;
    serde_json::from_str(line.trim_end()).ok()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::net::UnixListener;

    fn bind_greeter(dir: &std::path::Path, greeting: &Greeting) -> PathBuf {
        let socket_path = dir.join("daemon.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind");
        let greeting = greeting.clone();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let line = serde_json::to_string(&greeting).unwrap();
                let _ = writeln!(stream, "{line}");
            }
        });
        socket_path
    }

    #[test]
    fn matching_instance_uuid_and_live_pid_is_alive() {
        let dir = tempdir();
        let socket_path = bind_greeter(
            dir.path(),
            &Greeting {
                instance_uuid: "uuid-a".to_string(),
                daemon_version: "0.0.0".to_string(),
                mode: "normal".to_string(),
            },
        );
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
        let socket_path = bind_greeter(
            dir.path(),
            &Greeting {
                instance_uuid: "uuid-different".to_string(),
                daemon_version: "0.0.0".to_string(),
                mode: "normal".to_string(),
            },
        );
        let probe = SocketLivenessProbe::new(socket_path);
        assert_eq!(
            probe.check(std::process::id(), "uuid-a"),
            LivenessOutcome::Stale
        );
    }

    #[test]
    fn dead_pid_is_stale_regardless_of_the_socket() {
        let dir = tempdir();
        let socket_path = bind_greeter(
            dir.path(),
            &Greeting {
                instance_uuid: "uuid-a".to_string(),
                daemon_version: "0.0.0".to_string(),
                mode: "normal".to_string(),
            },
        );
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
