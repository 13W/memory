//! `local-rag` daemon + CLI entry point.
//!
//! `version` is a diagnostic no-op; `serve` runs the daemon lifecycle (spec
//! 02 §4, T15-01). The rest of the CLI surface (`status`/`stop`/`restart`/
//! `init`/`index`/...) is later group-15 cards.

use std::process::ExitCode;
use std::sync::Arc;

use local_rag::daemon::{DaemonStartupError, ShutdownReason, StartOptions, StoreLockError};
use local_rag_core::identity::SystemUuidV7;
use local_rag_core::paths::{StoreLayout, SystemEnv, config_dir, data_dir};
use local_rag_protocol::ErrorEnvelope;
use local_rag_store::{DEFAULT_WRITE_QUEUE_CAPACITY, LEASE_DURATION_MS, LEASE_RENEW_INTERVAL_MS};

const BIN: &str = "local-rag";

fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        Some("version" | "--version" | "-V") => {
            println!("{}", local_rag_core::version_line(BIN));
            ExitCode::SUCCESS
        }
        Some("serve") => run_serve(),
        _ => {
            eprintln!("usage: {BIN} version|serve");
            ExitCode::from(2)
        }
    }
}

/// `local-rag serve` — daemon lifecycle (spec 02 §4). A manually built
/// multi-thread runtime, not `#[tokio::main]`: this is the CLI's own single
/// entry point among several planned subcommands (group 15), so the runtime
/// is constructed explicitly here rather than attached to `main` itself.
fn run_serve() -> ExitCode {
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("{BIN}: could not start the async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    rt.block_on(serve())
}

async fn serve() -> ExitCode {
    let env = SystemEnv;
    let layout = match StoreLayout::resolve(&env) {
        Ok(layout) => layout,
        Err(e) => {
            eprintln!("{BIN}: could not resolve the store directory: {e}");
            return ExitCode::FAILURE;
        }
    };
    let config = match config_dir(&env).map(|dir| local_rag_core::config::Config::load(&dir)) {
        Ok(Ok(config)) => config,
        Ok(Err(e)) => {
            eprintln!("{BIN}: could not load config.toml: {e}");
            return ExitCode::FAILURE;
        }
        Err(e) => {
            eprintln!("{BIN}: could not resolve the config directory: {e}");
            return ExitCode::FAILURE;
        }
    };
    let _ = data_dir(&env); // resolved via `layout`; kept for a clearer error above if it fails

    let now_ms = system_now_ms();

    let opts = StartOptions {
        layout,
        daemon_version: local_rag_core::VERSION.to_string(),
        now_ms,
        uuids: Arc::new(SystemUuidV7),
        write_queue_capacity: DEFAULT_WRITE_QUEUE_CAPACITY,
        payload_ttl_hours: config.storage.payload_ttl_hours,
        consolidation_lease_ms: LEASE_DURATION_MS,
        consolidation_renew_interval_ms: LEASE_RENEW_INTERVAL_MS,
        data_policy: config.models.data_policy,
    };
    let idle_shutdown_secs = config.daemon.idle_shutdown_secs;

    match local_rag::daemon::run(opts, idle_shutdown_secs, IDLE_POLL_INTERVAL).await {
        Ok(ShutdownReason::Signal) => ExitCode::SUCCESS,
        Ok(ShutdownReason::Idle) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{BIN}: {}", startup_error_message(&e));
            ExitCode::FAILURE
        }
    }
}

const IDLE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// The current wall-clock time as Unix milliseconds (production seam).
///
/// Mirrors `local_rag_hook::clock::system_now_ms` exactly — each production
/// binary carries its own trivial copy rather than a shared `local-rag-core`
/// helper, the same convention that file's own doc comment establishes.
fn system_now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A human-readable startup failure message, naming the canonical error code
/// (spec 02 §6) a future MCP/status surface would also report.
fn startup_error_message(e: &DaemonStartupError) -> String {
    match e {
        DaemonStartupError::Lock(StoreLockError::Locked { owner }) if !owner.ready => {
            let env = ErrorEnvelope::migration_in_progress();
            format!(
                "another instance (pid {}) appears to still be starting up, possibly \
                 migrating — retry shortly [{}]",
                owner.pid, env.code
            )
        }
        DaemonStartupError::Lock(StoreLockError::Locked { owner }) => {
            let env = ErrorEnvelope::store_locked(owner.pid, &owner.instance_uuid);
            format!(
                "the store is already served by pid {} (instance {}) [{}]",
                owner.pid, owner.instance_uuid, env.code
            )
        }
        other => other.to_string(),
    }
}
