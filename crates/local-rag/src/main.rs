//! `local-rag` daemon + CLI entry point.
//!
//! `version` is a diagnostic no-op; `serve` runs the daemon lifecycle (spec
//! 02 §4, T15-01). The rest of the CLI surface — `status`/`stop`/`restart`/
//! `init`, `index`/`reindex`/`watch`, `repo`/`worktree`, `rebuild` (spec 11
//! §6) — lives in [`cli`] (T15-07); `memory`/`gc`/`stats` are T15-08 (D-025);
//! `inspect`/`export`/`purge` are T16-02 (D-025).

mod cli;

use std::process::ExitCode;
use std::sync::Arc;

use local_rag::daemon::{
    DaemonStartupError, EmbedderQueryAdapter, ShutdownReason, StartOptions, StoreLockError,
};
use local_rag_core::identity::SystemUuidV7;
use local_rag_core::paths::{StoreLayout, SystemEnv, config_dir, data_dir};
use local_rag_memory::recall::UnavailableEmbedder as UnavailableMemoryEmbedder;
use local_rag_models::{DEFAULT_MODEL_ID, OnnxEmbedder, find, is_installed};
use local_rag_protocol::ErrorEnvelope;
use local_rag_search::{QueryEmbedder, UnavailableEmbedder};
use local_rag_store::{DEFAULT_WRITE_QUEUE_CAPACITY, LEASE_DURATION_MS, LEASE_RENEW_INTERVAL_MS};

const BIN: &str = "local-rag";

fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        Some("version" | "--version" | "-V") => {
            println!("{}", local_rag_core::version_line(BIN));
            ExitCode::SUCCESS
        }
        Some("serve") => run_serve(),
        Some("status") => cli::status::run(std::env::args().skip(2)),
        Some("stop") => cli::service::run_stop(std::env::args().skip(2)),
        Some("restart") => cli::service::run_restart(std::env::args().skip(2)),
        Some("init") => cli::init::run(std::env::args().skip(2)),
        Some("index") => cli::index::run_index(std::env::args().skip(2)),
        Some("reindex") => cli::index::run_reindex(std::env::args().skip(2)),
        Some("watch") => cli::watch::run_watch(std::env::args().skip(2)),
        Some("repo") => cli::repo::run(std::env::args().skip(2)),
        Some("worktree") => cli::worktree::run(std::env::args().skip(2)),
        Some("rebuild") => cli::rebuild::run(std::env::args().skip(2)),
        Some("memory") => cli::memory::run(std::env::args().skip(2)),
        Some("gc") => cli::gc::run(std::env::args().skip(2)),
        Some("stats") => cli::stats::run(std::env::args().skip(2)),
        Some("inspect") => cli::inspect::run(std::env::args().skip(2)),
        Some("export") => cli::export::run(std::env::args().skip(2)),
        Some("purge") => cli::purge::run(std::env::args().skip(2)),
        _ => {
            eprintln!(
                "usage: {BIN} version|serve|status|stop|restart|init|index|reindex|watch|repo|worktree|rebuild|memory|gc|stats|inspect|export|purge"
            );
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
    let query_embedder = build_query_embedder(&layout);

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
        supported_proto: local_rag_protocol::SUPPORTED_PROTO_RANGE,
        max_open_shards: config.daemon.max_open_shards,
        query_embedder,
        // `recall`'s dense leg (T15-04) stays on the explicit-degradation
        // path (`dense_degraded: Some(EmbedFailed(..))`), deliberately, not
        // as an oversight: no `memory`-kind `RepresentationKey` has ever been
        // registered in production (D-013 assigned that to group 14, which
        // closed without doing it), so there is no real key to build a
        // provider against here — see `daemon::query_embedder`'s own module
        // doc for the full as-built rationale (T15-07).
        memory_query_embedder: Arc::new(UnavailableMemoryEmbedder),
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

/// Build `search_code`'s dense-leg provider from whatever the store already
/// has on disk (T15-07), gated on the same signal `cli::init` uses — the
/// default model's `.ok` marker — not on any flag or config toggle.
///
/// A missing model, or one that fails to open (corrupt install, no ONNX
/// Runtime on `PATH`/`ORT_DYLIB_PATH`), degrades to [`UnavailableEmbedder`]
/// rather than failing daemon startup: `search_code` already has a tested,
/// spec-correct `lexical_only` fallback for exactly this case
/// (`daemon::search::build_search_engine`'s own doc), and a store the
/// operator has not run `local-rag init --download-models` against yet must
/// still serve lexical search.
fn build_query_embedder(layout: &StoreLayout) -> Arc<dyn QueryEmbedder> {
    let Some(entry) = find(DEFAULT_MODEL_ID) else {
        return Arc::new(UnavailableEmbedder);
    };
    if !is_installed(layout, entry.model_id) {
        return Arc::new(UnavailableEmbedder);
    }
    match OnnxEmbedder::open(layout, entry) {
        Ok(embedder) => Arc::new(EmbedderQueryAdapter::new(embedder)),
        Err(e) => {
            eprintln!(
                "{BIN}: {} is installed but could not be opened ({e}); \
                 search_code will stay lexical_only until this is fixed",
                entry.model_id
            );
            Arc::new(UnavailableEmbedder)
        }
    }
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
