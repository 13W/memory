//! `local-rag` daemon + CLI entry point.
//!
//! `version` is a diagnostic no-op; `serve` runs the daemon lifecycle (spec
//! 02 §4, T15-01). The rest of the CLI surface — `status`/`stop`/`restart`/
//! `init`, `index`/`reindex`/`watch`, `repo`/`worktree`, `rebuild` (spec 11
//! §6) — lives in [`cli`] (T15-07); `memory`/`gc`/`stats` are T15-08 (D-025);
//! `inspect`/`export`/`purge` are T16-02 (D-025); `doctor` is T16-03 (D-025).

mod cli;

use std::process::ExitCode;
use std::sync::Arc;

use local_rag::daemon::{
    DaemonStartupError, ShutdownReason, StartOptions, StoreLockError, code_query_embedder,
    memory_query_embedder,
};
use local_rag_core::identity::SystemUuidV7;
use local_rag_core::paths::{StoreLayout, SystemEnv, config_dir, data_dir};
use local_rag_protocol::ErrorEnvelope;
use local_rag_store::{DEFAULT_WRITE_QUEUE_CAPACITY, LEASE_DURATION_MS, LEASE_RENEW_INTERVAL_MS};

const BIN: &str = "local-rag";

/// T17-04: a test-only override for the `daemon_version` this process
/// advertises in `WELCOME` — never compiled into a release/distribution
/// build (`failpoints` is off by default; see this crate's `Cargo.toml`).
///
/// `local_rag_core::VERSION` is `env!("CARGO_PKG_VERSION")`, fixed at compile
/// time — there is no way to make one compiled binary answer with a genuinely
/// different version at runtime otherwise. This exists so a real compiled
/// `local-rag serve` process can stand in for "an old daemon" in
/// `local-rag-proxy/tests/subprocess.rs`'s cross-binary-version upgrade test,
/// without a second historical binary or machine (none is available: no
/// network, no second checkout). Mirrors the env-var hand-off
/// `daemon::resume::test_resume_pause` already uses for the same class of
/// problem (a child process configuring itself before doing anything else).
#[cfg(feature = "failpoints")]
fn test_daemon_version_override() -> Option<String> {
    std::env::var("LOCAL_RAG_TEST_FAKE_DAEMON_VERSION")
        .ok()
        .filter(|s| !s.is_empty())
}

#[cfg(not(feature = "failpoints"))]
fn test_daemon_version_override() -> Option<String> {
    None
}

fn main() -> ExitCode {
    use clap::Parser;
    let cli = cli::Cli::parse();
    match cli.command {
        cli::Command::Version => {
            println!("{}", local_rag_core::version_line(BIN));
            ExitCode::SUCCESS
        }
        cli::Command::Serve => run_serve(),
        cli::Command::Status(args) => cli::status::run(args),
        cli::Command::Stop => cli::service::run_stop(),
        cli::Command::Restart => cli::service::run_restart(),
        cli::Command::Init(args) => cli::init::run(args),
        cli::Command::Index(args) => cli::index::run_index(args),
        cli::Command::Reindex => cli::index::run_reindex(),
        cli::Command::Watch => cli::watch::run_watch(),
        cli::Command::Repo { command } => cli::repo::run(command),
        cli::Command::Worktree { command } => cli::worktree::run(command),
        cli::Command::Rebuild(args) => cli::rebuild::run(args),
        cli::Command::Memory { command } => cli::memory::run(command),
        cli::Command::Gc(args) => cli::gc::run(args),
        cli::Command::Stats(args) => cli::stats::run(args),
        cli::Command::Inspect(args) => cli::inspect::run(args),
        cli::Command::Export(args) => cli::export::run(args),
        cli::Command::Purge(args) => cli::purge::run(args),
        cli::Command::Doctor(args) => cli::doctor::run(args),
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
    let code_embedder = code_query_embedder(&layout);
    let memory_embedder = memory_query_embedder(&layout);

    let opts = StartOptions {
        layout,
        daemon_version: test_daemon_version_override()
            .unwrap_or_else(|| local_rag_core::VERSION.to_string()),
        now_ms,
        uuids: Arc::new(SystemUuidV7),
        write_queue_capacity: DEFAULT_WRITE_QUEUE_CAPACITY,
        payload_ttl_hours: config.storage.payload_ttl_hours,
        consolidation_lease_ms: LEASE_DURATION_MS,
        consolidation_renew_interval_ms: LEASE_RENEW_INTERVAL_MS,
        data_policy: config.models.data_policy,
        supported_proto: local_rag_protocol::SUPPORTED_PROTO_RANGE,
        max_open_shards: config.daemon.max_open_shards,
        query_embedder: code_embedder,
        memory_query_embedder: memory_embedder,
        recall_token_budget: config.memory.recall_token_budget,
        consolidation_batch_size: config.memory.consolidation_batch_size,
        consolidation_queue_threshold: config.memory.consolidation_queue_threshold,
        consolidation_poll_interval: CONSOLIDATION_POLL_INTERVAL,
    };
    let idle_shutdown_secs = config.daemon.idle_shutdown_secs;

    match local_rag::daemon::run(opts, idle_shutdown_secs, IDLE_POLL_INTERVAL).await {
        Ok(ShutdownReason::Signal) => ExitCode::SUCCESS,
        Ok(ShutdownReason::Idle) => ExitCode::SUCCESS,
        Ok(ShutdownReason::UpgradeRequested) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{BIN}: {}", startup_error_message(&e));
            ExitCode::FAILURE
        }
    }
}

const IDLE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
/// How often the continuous consolidation-trigger worker ticks (D-024). No
/// `[SPEC]` number exists for it — the same bucket `IDLE_POLL_INTERVAL`
/// above occupies.
const CONSOLIDATION_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);

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

#[cfg(test)]
mod tests {
    use super::*;
    use local_rag::daemon::StoreLockInfo;

    fn owner(ready: bool) -> StoreLockInfo {
        StoreLockInfo {
            instance_uuid: "instance-a".to_string(),
            pid: 4242,
            daemon_version: "9.9.9".to_string(),
            started_at: 1_000,
            ready,
            ready_at: ready.then_some(1_500),
            socket_path: ready.then(|| "/tmp/does-not-matter/daemon.sock".to_string()),
        }
    }

    /// spec 02 §6's `MIGRATION_IN_PROGRESS` row: the CLI-level message for a
    /// live-but-not-yet-`ready` owner (still starting, most commonly still
    /// migrating) — this branch has no test at any level (its sibling,
    /// `STORE_LOCKED` below, is proven end to end in
    /// `tests/serve_subprocess.rs`; this one only exists as this pure
    /// function, so a unit test is what closes the gap, G15/D-026).
    #[test]
    fn a_not_ready_owner_reports_migration_in_progress() {
        let err = DaemonStartupError::Lock(StoreLockError::Locked {
            owner: owner(false),
        });
        let message = startup_error_message(&err);
        assert!(message.contains("MIGRATION_IN_PROGRESS"), "{message}");
        assert!(message.contains("still be starting up"), "{message}");
        assert!(message.contains("4242"), "{message}");
    }

    /// The sibling branch: a fully-`ready` owner reports `STORE_LOCKED`
    /// instead — asserted here side by side with the branch above so the
    /// `ready` flag is what is proven to select between them, not just each
    /// message's own text in isolation.
    #[test]
    fn a_ready_owner_reports_store_locked() {
        let err = DaemonStartupError::Lock(StoreLockError::Locked { owner: owner(true) });
        let message = startup_error_message(&err);
        assert!(message.contains("STORE_LOCKED"), "{message}");
        assert!(message.contains("already served by pid 4242"), "{message}");
        assert!(!message.contains("MIGRATION_IN_PROGRESS"), "{message}");
    }
}
