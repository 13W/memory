//! `local-rag stop` / `local-rag restart` (spec 11 §6).
//!
//! Both use `libc::kill(pid, SIGTERM)` — already a real (non-dev)
//! `[target.'cfg(unix)'.dependencies]` of this crate — rather than the
//! protocol-level `ShutdownRequest` message. The daemon's own SIGTERM
//! handler (`daemon::shutdown::ShutdownSignal`) and `ShutdownRequest`
//! (`daemon::handshake`'s `Message::ShutdownRequest` arm) both drive the
//! *identical* `drain_and_shutdown` sequence (`lifecycle::
//! wait_for_shutdown_trigger` races `signal.wait()` against
//! `shutdown_requested.notified()` — same destination, either trigger), so
//! there is no capability gap from choosing SIGTERM. `ShutdownRequest`'s own
//! `requested_by_proxy_version` field is specifically a version-upgrade
//! handoff announcement (`local-rag-proxy::handshake::establish_session`'s
//! own retry loop) — reusing it here would mean opening a full HELLO/WELCOME
//! session, and either lying in that field or growing the protocol, just to
//! accomplish what a bare `kill` already does via the daemon's pre-existing
//! signal handler (the same primitive `tests/serve_subprocess.rs`'s own
//! `send_sigterm` already exercises against a real daemon).

use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use local_rag_core::paths::StoreLayout;
use local_rag_core::process::pid_exists;

use local_rag::daemon::{LIVENESS_PROBE_TIMEOUT_MS, StoreLockInfo};

use super::{fail, resolve_layout_and_config};

const BIN: &str = "local-rag";

/// `stop`'s bounded wait for the daemon to actually finish draining — chosen,
/// not derived (same class of number as `LIVENESS_PROBE_TIMEOUT_MS`/
/// `IDLE_POLL_INTERVAL`): generous enough for a real WAL checkpoint under
/// load, short enough that a CLI command does not hang indefinitely on a
/// genuinely stuck process.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(50);
/// `restart`'s bounded wait for the freshly spawned daemon to become ready —
/// more generous than `STOP_TIMEOUT`: a fresh daemon may run real startup
/// resume passes before reaching readiness (`tests/serve_subprocess.rs`'s
/// own tests budget 20s for this under test conditions).
const RESTART_READY_TIMEOUT: Duration = Duration::from_secs(30);
const RESTART_POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StopOutcome {
    /// No lock file, or a lock file whose owner is already dead/unreachable
    /// — nothing to stop. Idempotent no-op, matching this codebase's
    /// dominant convergence convention (`register_representation`'s `ON
    /// CONFLICT`, `install_model`'s already-installed short-circuit).
    NotRunning,
    Stopped,
    TimedOut {
        pid: u32,
    },
}

/// Read `store.lock` and determine whether its recorded owner is genuinely
/// still alive — the same ready/pid/`Welcome` three-way `cli::status` uses.
fn alive_owner(layout: &StoreLayout) -> Option<StoreLockInfo> {
    let bytes = std::fs::read(layout.store_lock()).ok()?;
    let info: StoreLockInfo = serde_json::from_slice(&bytes).ok()?;

    let alive = if info.ready {
        pid_exists(info.pid) && {
            #[cfg(unix)]
            {
                local_rag::daemon::fetch_welcome(
                    &layout.socket_path(),
                    Duration::from_millis(LIVENESS_PROBE_TIMEOUT_MS),
                )
                .is_some_and(|w| w.store_instance_uuid == info.instance_uuid)
            }
            #[cfg(not(unix))]
            {
                false
            }
        }
    } else {
        pid_exists(info.pid)
    };

    alive.then_some(info)
}

pub(crate) fn stop_running_daemon(layout: &StoreLayout) -> StopOutcome {
    let Some(info) = alive_owner(layout) else {
        return StopOutcome::NotRunning;
    };

    #[cfg(unix)]
    {
        // SAFETY: `kill` with a valid pid and signal number is a plain,
        // side-effect-documented syscall; no memory is read or written.
        #[allow(unsafe_code)]
        let rc = unsafe { libc::kill(info.pid as libc::pid_t, libc::SIGTERM) };
        if rc != 0 {
            // The process died between the liveness check above and this
            // call — already gone is already stopped, not a failure.
            return StopOutcome::Stopped;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = &info;
    }

    let deadline = Instant::now() + STOP_TIMEOUT;
    loop {
        if !layout.store_lock().exists() || !pid_exists(info.pid) {
            return StopOutcome::Stopped;
        }
        if Instant::now() >= deadline {
            return StopOutcome::TimedOut { pid: info.pid };
        }
        std::thread::sleep(STOP_POLL_INTERVAL);
    }
}

pub fn run_stop() -> ExitCode {
    let (layout, _config) = match resolve_layout_and_config() {
        Ok(v) => v,
        Err(e) => return fail(BIN, &e),
    };
    match stop_running_daemon(&layout) {
        StopOutcome::NotRunning => {
            println!("{BIN}: not running");
            ExitCode::SUCCESS
        }
        StopOutcome::Stopped => {
            println!("{BIN}: stopped");
            ExitCode::SUCCESS
        }
        StopOutcome::TimedOut { pid } => fail(
            BIN,
            &format!("pid {pid} did not stop within {STOP_TIMEOUT:?}"),
        ),
    }
}

/// Spawn a fresh, fully detached `local-rag serve` — its own process group
/// (unix — a signal to this CLI's own group, e.g. a terminal Ctrl-C, must
/// not reach it) and `Stdio::null()` on all three standard streams, mirroring
/// `local-rag-proxy::connect::spawn_detached_daemon`'s exact technique
/// (duplicated here, not imported: a ~10-line helper does not earn a new
/// `local-rag` → `local-rag-proxy` dependency edge that does not otherwise
/// exist, the same "each binary carries its own trivial copy" precedent
/// `main.rs::system_now_ms`'s own doc comment already states).
fn spawn_detached_serve(exe: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Command::new(exe)
            .arg("serve")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        Command::new(exe)
            .arg("serve")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        Ok(())
    }
}

/// Bounded wait for `store.lock` to report `ready: true` — mirrors
/// `tests/serve_subprocess.rs::wait_until_ready`'s exact polling shape (real,
/// bounded wall-clock waiting inherent to driving a real child process across
/// a real OS scheduling boundary; nothing here asserts on the *duration*).
fn wait_until_ready(layout: &StoreLayout, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(bytes) = std::fs::read(layout.store_lock())
            && let Ok(info) = serde_json::from_slice::<StoreLockInfo>(&bytes)
            && info.ready
        {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(RESTART_POLL_INTERVAL);
    }
}

pub fn run_restart() -> ExitCode {
    let (layout, _config) = match resolve_layout_and_config() {
        Ok(v) => v,
        Err(e) => return fail(BIN, &e),
    };

    match stop_running_daemon(&layout) {
        StopOutcome::NotRunning | StopOutcome::Stopped => {}
        StopOutcome::TimedOut { pid } => {
            return fail(
                BIN,
                &format!("pid {pid} did not stop in time; not restarting"),
            );
        }
    }

    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            return fail(
                BIN,
                &format!("could not resolve this binary's own path: {e}"),
            );
        }
    };
    if let Err(e) = spawn_detached_serve(&exe) {
        return fail(BIN, &format!("could not spawn a new daemon: {e}"));
    }

    if wait_until_ready(&layout, RESTART_READY_TIMEOUT) {
        println!("{BIN}: restarted");
        ExitCode::SUCCESS
    } else {
        fail(
            BIN,
            &format!("new daemon did not become ready within {RESTART_READY_TIMEOUT:?}"),
        )
    }
}
