//! `local-rag status [--json]` (spec 11 §6) — report whether a daemon is
//! running against this store, and if so, in which mode.
//!
//! `store.lock`'s own `ready` flag is set identically whether the daemon
//! ended up in `Normal` or `MigrationOnly` mode (`lifecycle.rs`'s own step-2
//! branch calls `mark_ready` unconditionally either way) — `Welcome.mode` is
//! the *only* channel that tells the two apart (spec 11 §6.176-178: "a store
//! genuinely mid-migration is diagnosable earlier, over a different channel:
//! the handshake `WELCOME`'s own `mode` field … or the CLI's `local-rag
//! status`"), which is why this command probes the live socket rather than
//! trusting the lock file alone.

use std::process::ExitCode;
use std::time::Duration;

use local_rag_core::paths::StoreLayout;
use local_rag_core::process::pid_exists;

use local_rag::daemon::{LIVENESS_PROBE_TIMEOUT_MS, StoreLockFileState, read_store_lock_file};

use super::{EXIT_USAGE, fail, resolve_layout_and_config};

const BIN: &str = "local-rag";

/// Exit codes: 0 running, 1 not running, 3 starting/migrating, 2 usage error
/// (reserved uniformly across every subcommand, see [`super::EXIT_USAGE`]).
const EXIT_RUNNING: u8 = 0;
const EXIT_NOT_RUNNING: u8 = 1;
const EXIT_STARTING: u8 = 3;

#[derive(Debug, Clone, PartialEq)]
enum StatusReport {
    NotRunning,
    Starting {
        pid: u32,
    },
    Running {
        pid: u32,
        instance_uuid: String,
        daemon_version: String,
        daemon_mode: String,
        started_at: i64,
        ready_at: Option<i64>,
        socket_path: String,
    },
}

impl StatusReport {
    fn exit_code(&self) -> ExitCode {
        match self {
            StatusReport::NotRunning => ExitCode::from(EXIT_NOT_RUNNING),
            StatusReport::Starting { .. } => ExitCode::from(EXIT_STARTING),
            StatusReport::Running { .. } => ExitCode::from(EXIT_RUNNING),
        }
    }

    fn to_json(&self) -> serde_json::Value {
        match self {
            StatusReport::NotRunning => serde_json::json!({"state": "not_running"}),
            StatusReport::Starting { pid } => {
                serde_json::json!({"state": "starting", "pid": pid})
            }
            StatusReport::Running {
                pid,
                instance_uuid,
                daemon_version,
                daemon_mode,
                started_at,
                ready_at,
                socket_path,
            } => serde_json::json!({
                "state": "running",
                "pid": pid,
                "instance_uuid": instance_uuid,
                "daemon_version": daemon_version,
                "daemon_mode": daemon_mode,
                "started_at": started_at,
                "ready_at": ready_at,
                "socket_path": socket_path,
            }),
        }
    }

    fn print_human(&self) {
        match self {
            StatusReport::NotRunning => println!("{BIN}: not running"),
            StatusReport::Starting { pid } => {
                println!("{BIN}: starting (pid {pid}, possibly migrating)")
            }
            StatusReport::Running {
                pid,
                instance_uuid,
                daemon_mode,
                socket_path,
                ..
            } => {
                println!(
                    "{BIN}: running (pid {pid}, mode {daemon_mode}, instance {instance_uuid}, \
                     socket {socket_path})"
                );
            }
        }
    }
}

/// Read `store.lock` and, if `ready`, probe the live socket — see this
/// module's own doc for why the probe (not just the lock file) is required
/// to distinguish `starting`/`migrating` from a fully `running` `Normal`
/// daemon.
fn compute_status(layout: &StoreLayout) -> StatusReport {
    let info = match read_store_lock_file(layout) {
        StoreLockFileState::Absent | StoreLockFileState::Corrupt => {
            return StatusReport::NotRunning;
        }
        StoreLockFileState::Parsed(info) => info,
    };

    if !info.ready {
        return if pid_exists(info.pid) {
            StatusReport::Starting { pid: info.pid }
        } else {
            StatusReport::NotRunning
        };
    }

    if !pid_exists(info.pid) {
        return StatusReport::NotRunning;
    }

    #[cfg(unix)]
    {
        let welcome = local_rag::daemon::fetch_welcome(
            &layout.socket_path(),
            Duration::from_millis(LIVENESS_PROBE_TIMEOUT_MS),
        );
        match welcome {
            Some(w) if w.store_instance_uuid == info.instance_uuid => StatusReport::Running {
                pid: info.pid,
                instance_uuid: info.instance_uuid,
                daemon_version: w.daemon_version,
                daemon_mode: w.mode,
                started_at: info.started_at,
                ready_at: info.ready_at,
                socket_path: info
                    .socket_path
                    .unwrap_or_else(|| layout.socket_path().display().to_string()),
            },
            _ => StatusReport::NotRunning,
        }
    }
    #[cfg(not(unix))]
    {
        StatusReport::NotRunning
    }
}

pub fn run(args: impl Iterator<Item = String>) -> ExitCode {
    let mut json = false;
    for arg in args {
        match arg.as_str() {
            "--json" => json = true,
            other => {
                eprintln!("{BIN} status: unknown argument {other:?}");
                return ExitCode::from(EXIT_USAGE);
            }
        }
    }

    let (layout, _config) = match resolve_layout_and_config() {
        Ok(v) => v,
        Err(e) => return fail(BIN, &e),
    };

    let report = compute_status(&layout);
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report.to_json())
                .expect("status report always serializes")
        );
    } else {
        report.print_human();
    }
    report.exit_code()
}
