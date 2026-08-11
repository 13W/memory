//! The daemon startup/shutdown orchestrator (spec 02 §4.1's five ordered
//! startup steps, §4.3's shutdown sequence) — T15-01.

use std::ops::RangeInclusive;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use local_rag_core::DataPolicy;
use local_rag_core::identity::{Uuid, UuidSource};
use local_rag_core::paths::StoreLayout;
use local_rag_embed::GeneratorPool;
use local_rag_index::classify::ClassifierConfig;
use local_rag_search::QueryEmbedder;
use local_rag_store::{
    CacheDb, CacheOpenError, OpenError, RetentionParams, StateDb, WorktreeLockRegistry, WriteError,
};
#[cfg(unix)]
use tokio::net::UnixListener;
use tokio::sync::{Notify, oneshot, watch};
use tokio::task::JoinHandle;

use super::embedder_provider::LazyEmbedderProvider;
use super::error::migration_only_reason;
#[cfg(unix)]
use super::handshake::{HandshakeContext, serve_connections};
use super::idle::{IdleGateInputs, idle_eligible};
use super::indexing::{SupervisorHandle, SupervisorParams, spawn_supervisor};
use super::jobs::JobRegistry;
use super::lock::{self, StoreLockError, StoreLockGuard, StoreLockInfo};
use super::mcp::McpHandler;
use super::memory::build_memory_context;
use super::mode::DaemonMode;
#[cfg(unix)]
use super::probe::SocketLivenessProbe;
use super::query_embedder::{code_query_embedder, memory_query_embedder};
use super::resume::{
    build_best_effort_pool, log_resume_sweep, resume_spool_import, resume_stale_consolidation_runs,
};
use super::search::build_search_engine;
use super::session::SessionRegistry;
#[cfg(unix)]
use super::shutdown::ShutdownSignal;
use super::shutdown::drain_and_shutdown;
use super::telemetry::TelemetryState;
use super::tool_calls::ToolCallCounters;

/// Why [`DaemonHandle::start`] could not bring the daemon up at all (distinct
/// from [`DaemonMode::MigrationOnly`], which is a *successful* start in a
/// degraded serving mode — these are the conditions that leave nothing
/// reachable at all).
#[derive(Debug)]
#[non_exhaustive]
pub enum DaemonStartupError {
    /// Step 0: creating/verifying the store directory tree failed — a
    /// permission or ownership mismatch (spec 12 §6, T16-04), before any of
    /// the five ordered startup steps below even begin.
    Path(local_rag_core::paths::PathError),
    /// Step 1: the store is held by another live instance, or a lock I/O
    /// error.
    Lock(StoreLockError),
    /// Step 2: `state.sqlite` could not even be opened for a non-migration
    /// reason (migration failures instead produce
    /// [`DaemonMode::MigrationOnly`] — see [`DaemonHandle::start`]'s doc).
    State(OpenError),
    /// Step 2: seeding/reading `store_instance_uuid` failed.
    StoreInstanceUuid(WriteError),
    /// Step 3: `cache.sqlite` could not be opened.
    Cache(CacheOpenError),
    /// Step 4: binding the UDS endpoint failed.
    Bind(std::io::Error),
    /// Step 4: writing the readiness marker into `store.lock` failed.
    MarkReady(std::io::Error),
}

impl std::fmt::Display for DaemonStartupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DaemonStartupError::Path(e) => {
                write!(f, "could not prepare the store directory tree: {e}")
            }
            DaemonStartupError::Lock(e) => write!(f, "{e}"),
            DaemonStartupError::State(e) => write!(f, "could not open state.sqlite: {e}"),
            DaemonStartupError::StoreInstanceUuid(e) => {
                write!(f, "could not seed store_instance_uuid: {e}")
            }
            DaemonStartupError::Cache(e) => write!(f, "could not open cache.sqlite: {e}"),
            DaemonStartupError::Bind(e) => write!(f, "could not bind the daemon socket: {e}"),
            DaemonStartupError::MarkReady(e) => {
                write!(f, "could not write the store.lock readiness marker: {e}")
            }
        }
    }
}

impl std::error::Error for DaemonStartupError {}

/// Startup parameters (spec 02 §4.1) — every value the section leaves to the
/// caller (config, clock, entropy) rather than fixing.
pub struct StartOptions {
    pub layout: StoreLayout,
    pub daemon_version: String,
    pub now_ms: i64,
    pub uuids: Arc<dyn UuidSource + Send + Sync>,
    pub write_queue_capacity: usize,
    pub payload_ttl_hours: u64,
    pub consolidation_lease_ms: i64,
    pub consolidation_renew_interval_ms: i64,
    pub data_policy: DataPolicy,
    /// The `proto` range this daemon accepts in HELLO (spec 02 §4.2). A
    /// field, not a hardcoded constant, so lifecycle-level tests can inject
    /// a narrow/incompatible range and exercise the INCOMPATIBLE path over a
    /// real `UnixStream` without a second binary. Production callers pass
    /// [`local_rag_protocol::SUPPORTED_PROTO_RANGE`].
    pub supported_proto: RangeInclusive<u16>,
    /// The MCP code-query tools' shard cache bound (`config.daemon.
    /// max_open_shards`, T15-03).
    pub max_open_shards: u32,
    /// The daemon's single owner of its ONNX sessions (T20-03) — at most two
    /// per process (`code_raw` + `memory`), shared between `query_embedder`/
    /// `memory_query_embedder` below and (T20-05/T20-06) indexing's backfill
    /// pool. Production callers pass `Arc::new(LazyEmbedderProvider::new(
    /// &layout))`.
    pub embedder_provider: Arc<LazyEmbedderProvider>,
    /// The daemon's single `L2` write/read lock registry (spec 02 §5,
    /// T20-04) — one `Arc<WorktreeLockRegistry>` per daemon process, shared
    /// between `build_search_engine`'s `SearchEngine` (read side, T09-03)
    /// and `local_rag::indexing::write_locked` (write side, T20-04/T20-05).
    /// Production callers pass `Arc::new(WorktreeLockRegistry::new())`.
    pub locks: Arc<WorktreeLockRegistry>,
    /// The MCP code-query tools' dense-leg query embedder. `None` — the
    /// production case — derives it from `embedder_provider` via
    /// `code_query_embedder`, so "one session per kind" holds by
    /// construction rather than by caller discipline. `Some(..)` exists only
    /// so tests can inject a fixed provider without ONNX (the same seam
    /// `supported_proto` already offers).
    pub query_embedder: Option<Arc<dyn QueryEmbedder>>,
    /// The MCP `recall` tool's dense-leg query embedder (T15-04) — a
    /// **different** trait from `query_embedder` above (`local_rag_memory::
    /// recall::QueryEmbedder`, not `local_rag_search::QueryEmbedder`: the
    /// two seams embed under different `RepresentationKind`s, `memory` vs
    /// `code_raw`). Same `None`-derives-from-`embedder_provider` contract as
    /// `query_embedder` above.
    pub memory_query_embedder: Option<Arc<dyn local_rag_memory::recall::QueryEmbedder>>,
    /// `recall`'s token budget (`config.memory.recall_token_budget`, spec 08
    /// §6 `[SPEC default 1500 tokens, config]`).
    pub recall_token_budget: u32,
    /// The continuous consolidation-trigger worker's window size
    /// (`config.memory.consolidation_batch_size`, D-024, spec 08 §4).
    pub consolidation_batch_size: i64,
    /// The continuous consolidation-trigger worker's backlog threshold
    /// (`config.memory.consolidation_queue_threshold`, D-024, spec 07 §6).
    pub consolidation_queue_threshold: i64,
    /// How often the continuous consolidation-trigger worker ticks (D-024).
    /// No `[SPEC]` number exists for it (the same bucket
    /// `wait_for_shutdown_trigger`'s own `poll_interval` occupies) — a plain
    /// parameter, not a config field, so lifecycle-level tests can drive it
    /// directly.
    pub consolidation_poll_interval: Duration,
    /// T20-06's indexing supervisor's per-worktree retention defaults
    /// (`config.storage`, `RetentionParams::from_storage_config`) — the same
    /// derivation `local_rag::indexing::finish_index_ctx` already uses for
    /// the CLI's own `IndexCtx`, applied here to every managed worktree's
    /// `WorktreeTaskParams`.
    pub retention: RetentionParams,
    /// T20-06's indexing supervisor's per-worktree file classifier
    /// (`config.index`, `ClassifierConfig::from_index_config`) — same
    /// derivation as `retention` above.
    pub classifier: ClassifierConfig,
    /// How often the indexing supervisor's backstop poll re-reads
    /// `managed_worktree` and reconciles its live task set against it, in
    /// case an `admin/projects_reload` (T20-07) notification was missed —
    /// "notify is a hint, the table is truth" (spec 06 §1's own discipline,
    /// applied to the registry). A plain parameter, not a config field, so
    /// lifecycle-level tests can drive it directly — same rationale as
    /// `consolidation_poll_interval` above.
    pub indexing_backstop_poll_interval: Duration,
}

/// A running daemon instance, in-process (spec 02 §4.1 steps 1–5 complete;
/// step 5's two resume passes run in the background — see
/// [`DaemonHandle::resume_handles`]).
///
/// This is what tests drive directly (no subprocess needed for pure
/// lock/lifecycle/idle-gating scenarios) and what [`run`] wraps with the
/// OS-signal/idle wait for the real `serve` binary.
pub struct DaemonHandle {
    pub layout: StoreLayout,
    pub mode: watch::Receiver<DaemonMode>,
    pub sessions: SessionRegistry,
    pub jobs: JobRegistry,
    pub lock_info: StoreLockInfo,
    pub socket_path: PathBuf,
    /// Signaled once a connected proxy sends `SHUTDOWN_REQUEST` (spec 13
    /// §4's upgrade flow) — [`wait_for_shutdown_trigger`] races this
    /// against the OS signal and the idle gate.
    pub shutdown_requested: Arc<Notify>,
    /// This process's ONNX sessions (T20-03) — outlives `start()` because the
    /// sessions live as long as the daemon does. T20-05's per-worktree
    /// indexing task takes its own `Arc::clone` of this instead of
    /// `indexing::finish_index_ctx`'s CLI-only pattern of opening a third and
    /// fourth session.
    pub embedder_provider: Arc<LazyEmbedderProvider>,
    /// This process's single `L2` lock registry (T20-04) — outlives `start()`
    /// for the same reason `embedder_provider` does: T20-05's per-worktree
    /// indexing task takes its own `Arc::clone` of this rather than
    /// constructing a second, unshared registry.
    pub locks: Arc<WorktreeLockRegistry>,
    state_db: Option<Arc<StateDb>>,
    cache_db: Option<Arc<CacheDb>>,
    lock_guard: Option<StoreLockGuard>,
    handshake_stop: Option<oneshot::Sender<()>>,
    handshake_join: Option<JoinHandle<()>>,
    /// The two startup catch-up passes (spec 02 §4.1 step 5), spawned
    /// non-blocking relative to readiness. Both terminate on their own, so
    /// `shutdown` blind-awaits these to completion before draining — spec 02
    /// §4.3's "cancel reconciles at the next safe point" applied to the only
    /// background work T15-01's own scope had.
    resume_handles: Vec<JoinHandle<()>>,
    /// The continuous consolidation-trigger worker (D-024, spec 07 §6) —
    /// unlike `resume_handles` above, this is a `loop { tick }` that never
    /// completes on its own, so `shutdown` signals it via this sender
    /// (rather than blind-awaiting its `JoinHandle`), mirroring
    /// `handshake_stop`/`handshake_join`.
    consolidation_trigger_stop: Option<oneshot::Sender<()>>,
    consolidation_trigger_join: Option<JoinHandle<()>>,
    /// The daemon-managed indexing supervisor (T20-06) — one background task
    /// per `enabled` `managed_worktree` row. `None` exactly in
    /// `DaemonMode::MigrationOnly` (same `state_db.as_ref()` gate as
    /// `consolidation_trigger_*` above): there is no usable `state.sqlite` to
    /// read the registry from. `shutdown` consumes it (its own `shutdown()`
    /// stops every worktree task it owns) before `drain_and_shutdown` closes
    /// the store.
    indexing_supervisor: Option<SupervisorHandle>,
}

impl DaemonHandle {
    /// Run spec 02 §4.1's five startup steps, in order.
    ///
    /// A migration failure (`state.sqlite` newer than this binary, a
    /// checksum drift, or any other migration-framework refusal) does
    /// **not** abort startup: steps 3 and 5 are skipped (there is no usable
    /// `state.sqlite` to bind them to), but step 4 still runs — the socket
    /// still binds and the lock still gets marked ready — so the daemon
    /// enters [`DaemonMode::MigrationOnly`] instead of leaving nothing
    /// reachable at all (spec 02 §6 `[FIXED]`: "nothing degrades silently").
    /// Every other startup failure (lock contention, a non-migration state
    /// error, a cache-open failure, a bind failure) is a genuine
    /// [`DaemonStartupError`] — nothing is reachable, and the caller
    /// (`main.rs`) reports it and exits.
    /// Windows has no local IPC transport implemented yet (named-pipe
    /// support across this crate/`local-rag-proxy`/`local-rag-hook` is not
    /// implemented — D-033, a separate follow-up, not part of this
    /// platform-portability fix). Startup fails immediately with a typed
    /// error, before any side effect (no directory/lock/store touched),
    /// rather than failing to compile or binding a transport that does not
    /// exist on this platform.
    #[cfg(not(unix))]
    pub async fn start(_opts: StartOptions) -> Result<DaemonHandle, DaemonStartupError> {
        Err(DaemonStartupError::Bind(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "local-rag daemon IPC is not yet implemented on Windows (named pipes; tracked separately)",
        )))
    }

    #[cfg(unix)]
    pub async fn start(opts: StartOptions) -> Result<DaemonHandle, DaemonStartupError> {
        let StartOptions {
            layout,
            daemon_version,
            now_ms,
            uuids,
            write_queue_capacity,
            payload_ttl_hours,
            consolidation_lease_ms,
            consolidation_renew_interval_ms,
            data_policy,
            supported_proto,
            max_open_shards,
            embedder_provider,
            locks,
            query_embedder: query_embedder_override,
            memory_query_embedder: memory_query_embedder_override,
            recall_token_budget,
            consolidation_batch_size,
            consolidation_queue_threshold,
            consolidation_poll_interval,
            retention,
            classifier,
            indexing_backstop_poll_interval,
        } = opts;

        // T20-03: derive the two query-facing embedders from the shared
        // `embedder_provider` unless a test injected a fixed one — keeps
        // "at most one ONNX session per kind" a construction guarantee
        // rather than a caller convention.
        let query_embedder =
            query_embedder_override.unwrap_or_else(|| code_query_embedder(&embedder_provider));
        let memory_query_embedder = memory_query_embedder_override
            .unwrap_or_else(|| memory_query_embedder(&embedder_provider));

        layout.ensure().map_err(DaemonStartupError::Path)?;

        // Step 1: store lock (L0), with stale-owner recovery.
        let instance_uuid = uuids.next_uuid().to_string();
        let pid = std::process::id();
        // `acquire` does blocking file-lock and (on contention) blocking
        // socket I/O (`SocketLivenessProbe`, up to `LIVENESS_PROBE_TIMEOUT_MS`).
        // Called directly, that blocking work would run *on this async task's
        // own executor thread* — on a single-worker runtime, that starves the
        // very connection-accept task (spec 02 §4.1 step 4) another
        // instance's probe would need answered, making every liveness check
        // spuriously time out. `spawn_blocking` moves it to the blocking
        // thread pool, the same discipline `StateWriter`/`CacheWriter`
        // already use for their own blocking SQLite I/O (dedicated OS
        // threads, never inline on an async task).
        let mut lock_guard = {
            let layout = layout.clone();
            let instance_uuid = instance_uuid.clone();
            let daemon_version = daemon_version.clone();
            tokio::task::spawn_blocking(move || {
                let probe = SocketLivenessProbe::new(layout.socket_path());
                lock::acquire(
                    &layout,
                    &instance_uuid,
                    pid,
                    &daemon_version,
                    now_ms,
                    &probe,
                )
            })
            .await
            .expect("the lock-acquire task must not panic")
            .map_err(DaemonStartupError::Lock)?
        };
        tracing::info!(instance_uuid = %instance_uuid, pid, "store lock acquired");

        // Step 2: open state.sqlite (runs migrations under L1 internally).
        let (mode_tx, mode_rx) = watch::channel(DaemonMode::Normal);
        let (state_db, cache_db) =
            match StateDb::open_with_capacity(layout.state_db(), write_queue_capacity) {
                Ok(state_db) => {
                    let candidate = uuids.next_uuid().to_string();
                    let store_instance_uuid = state_db
                        .writer()
                        .transaction(move |tx| {
                            local_rag_store::ensure_store_instance_uuid(tx, &candidate)
                        })
                        .await
                        .map_err(DaemonStartupError::StoreInstanceUuid)?;

                    // Step 3: open/validate cache.sqlite.
                    let cache_db = CacheDb::open(layout.cache_db(), &store_instance_uuid)
                        .map_err(DaemonStartupError::Cache)?;
                    tracing::info!("state.sqlite and cache.sqlite opened");
                    (Some(Arc::new(state_db)), Some(Arc::new(cache_db)))
                }
                Err(OpenError::Migration(boxed)) => {
                    let reason = migration_only_reason(&boxed);
                    tracing::warn!(?reason, "degraded to migration-only mode");
                    let _ = mode_tx.send(DaemonMode::MigrationOnly { reason });
                    (None, None)
                }
                Err(other) => return Err(DaemonStartupError::State(other)),
            };

        // The MCP code-query tools' `SearchEngine` and the MCP status/
        // memory-read tools' `MemoryContext` — both `None` exactly in
        // `MigrationOnly` (no usable `state.sqlite`/`cache.sqlite` to build
        // either from, built from the same `(state_db, cache_db)` pair so
        // the two `Option`s can never disagree); `McpHandler` already knows
        // how to answer `tools/call` in that case without either (spec 02
        // §6 `[FIXED]`: "nothing degrades silently").
        let (engine, memory_ctx) = match (&state_db, &cache_db) {
            (Some(state), Some(cache)) => (
                Some(build_search_engine(
                    Arc::clone(state),
                    Arc::clone(cache),
                    layout.clone(),
                    Arc::clone(&uuids),
                    query_embedder,
                    max_open_shards,
                    Arc::clone(&locks),
                )),
                Some(build_memory_context(
                    Arc::clone(state),
                    Arc::clone(cache),
                    memory_query_embedder,
                    recall_token_budget,
                    Arc::clone(&uuids),
                )),
            ),
            _ => (None, None),
        };

        // Step 4: bind endpoint, write readiness marker, start the real
        // HELLO/WELCOME connection handler.
        let listener =
            UnixListener::bind(layout.socket_path()).map_err(DaemonStartupError::Bind)?;
        tracing::info!(socket = %layout.socket_path().display(), "listening");
        lock_guard
            .mark_ready(now_ms, &layout.socket_path())
            .map_err(DaemonStartupError::MarkReady)?;
        let lock_info = lock_guard.info().clone();
        tracing::info!(
            socket = %layout.socket_path().display(),
            pid,
            daemon_version = %daemon_version,
            instance_uuid = %instance_uuid,
            "daemon ready"
        );

        // `jobs` and the indexing supervisor are constructed here — still
        // within step 4 ("start workers", spec 02 §4.1 step 4, the same
        // clause `daemon::indexing::supervisor`'s own module doc already
        // cites) — rather than down among the step-5 resume passes below,
        // specifically so `McpHandler::new` (built a few lines down) can be
        // handed a `SupervisorClient` (T20-07): `McpHandler` starts serving
        // connections independently of this function's own remaining
        // lifetime, so it can only hold an owned, `Clone`-able client, never
        // a borrowed `&SupervisorHandle` built later.
        let jobs = JobRegistry::new();
        // T20-06: the daemon-managed indexing supervisor — one `T20-05` task
        // per `enabled` `managed_worktree` row. Same "both present or
        // neither" store-availability guard `engine`/`memory_ctx` above
        // already use (no usable `state.sqlite`/`cache.sqlite` in
        // `MigrationOnly`).
        let indexing_supervisor = match (&state_db, &cache_db) {
            (Some(state), Some(cache)) => {
                tracing::info!(job = "indexing_supervisor", "background job spawned");
                let model_space_id: Uuid = local_rag_store::DEFAULT_MODEL_SPACE_ID
                    .parse()
                    .expect("DEFAULT_MODEL_SPACE_ID is a valid UUID");
                Some(spawn_supervisor(SupervisorParams {
                    state: Arc::clone(state),
                    cache: Arc::clone(cache),
                    layout: layout.clone(),
                    uuids: Arc::clone(&uuids),
                    locks: Arc::clone(&locks),
                    embedder_provider: Arc::clone(&embedder_provider),
                    jobs: jobs.clone(),
                    model_space_id,
                    retention,
                    data_policy,
                    classifier,
                    backstop_poll_interval: indexing_backstop_poll_interval,
                }))
            }
            _ => None,
        };

        let sessions = SessionRegistry::new();
        let tool_calls = ToolCallCounters::new();
        let telemetry = TelemetryState::new();
        let shutdown_requested = Arc::new(Notify::new());
        let handshake_ctx = HandshakeContext {
            instance_uuid: Arc::from(instance_uuid.as_str()),
            daemon_version: Arc::from(daemon_version.as_str()),
            supported_proto,
            mode: mode_rx.clone(),
            sessions: sessions.clone(),
            tool_calls: tool_calls.clone(),
            telemetry: telemetry.clone(),
            now_ms: system_now_ms,
            shutdown_requested: Arc::clone(&shutdown_requested),
        };
        let mcp_handler = McpHandler::new(
            engine,
            memory_ctx,
            mode_rx.clone(),
            system_now_ms,
            tool_calls,
            telemetry,
            indexing_supervisor.as_ref().map(SupervisorHandle::client),
        );
        let (handshake_stop_tx, handshake_stop_rx) = oneshot::channel();
        let handshake_join = tokio::spawn(serve_connections(
            listener,
            handshake_ctx,
            mcp_handler,
            handshake_stop_rx,
        ));

        // Step 5: resume passes — startup catch-up only (see `daemon::resume`'s
        // own scope note), spawned non-blocking relative to readiness.
        //
        // D-054: built exactly once, here, and shared (via `Arc`) between
        // `spawn_consolidation_resume` and `spawn_consolidation_trigger`
        // below — each used to call `build_best_effort_pool` independently,
        // and since both are `tokio::spawn`ed concurrently, whichever one's
        // `LlamaBackend::init()` lost that race failed with
        // `BackendAlreadyInitialized` (llama.cpp's backend handle is a
        // process-wide singleton, not reentrant) and silently fell back to
        // an empty pool for the rest of the daemon's uptime — no amount of
        // waiting or retrying recovered it, only a restart, and even then
        // only a coin flip on which task won. `.map` keeps this gated behind
        // `state_db.is_some()`, matching both spawns' own existing guard —
        // no model load in `MigrationOnly` mode, where neither spawn runs.
        let pool = state_db
            .as_ref()
            .map(|_| Arc::new(build_best_effort_pool(&layout)));
        let mut resume_handles = Vec::new();
        if let Some(ref db) = state_db {
            tracing::info!(job = "spool_resume", "background job spawned");
            resume_handles.push(tokio::spawn(spawn_spool_resume(
                Arc::clone(db),
                layout.clone(),
                Arc::clone(&uuids),
                jobs.clone(),
                now_ms,
                payload_ttl_hours,
            )));
            tracing::info!(job = "consolidation_resume", "background job spawned");
            resume_handles.push(tokio::spawn(spawn_consolidation_resume(
                Arc::clone(db),
                Arc::clone(pool.as_ref().expect("state_db present implies pool built")),
                jobs.clone(),
                consolidation_lease_ms,
                consolidation_renew_interval_ms,
                now_ms,
                data_policy,
                Arc::clone(&uuids),
            )));
        }

        // D-024: the continuous consolidation-trigger worker (spec 07 §6) —
        // same `state_db`-present guard as the two resume passes above (no
        // usable `state.sqlite` in `MigrationOnly`), but its own
        // signal-then-await cancellation pair (see `DaemonHandle::shutdown`),
        // not `resume_handles` — this loop never completes on its own.
        let (consolidation_trigger_stop, consolidation_trigger_join) = match state_db.as_ref() {
            Some(db) => {
                tracing::info!(job = "consolidation_trigger", "background job spawned");
                let (stop_tx, stop_rx) = oneshot::channel();
                let join = tokio::spawn(spawn_consolidation_trigger(
                    Arc::clone(db),
                    layout.clone(),
                    Arc::clone(pool.as_ref().expect("state_db present implies pool built")),
                    Arc::clone(&uuids),
                    jobs.clone(),
                    consolidation_lease_ms,
                    consolidation_renew_interval_ms,
                    consolidation_batch_size,
                    consolidation_queue_threshold,
                    payload_ttl_hours,
                    consolidation_poll_interval,
                    data_policy,
                    stop_rx,
                ));
                (Some(stop_tx), Some(join))
            }
            None => (None, None),
        };

        Ok(DaemonHandle {
            socket_path: layout.socket_path(),
            layout,
            mode: mode_rx,
            sessions,
            jobs,
            lock_info,
            shutdown_requested,
            embedder_provider,
            locks,
            state_db,
            cache_db,
            lock_guard: Some(lock_guard),
            handshake_stop: Some(handshake_stop_tx),
            handshake_join: Some(handshake_join),
            resume_handles,
            consolidation_trigger_stop,
            consolidation_trigger_join,
            indexing_supervisor,
        })
    }

    /// The idle-shutdown gate's current inputs (spec 02 §4.3).
    pub fn idle_inputs(&self) -> IdleGateInputs {
        let pending_spool_bytes = match &self.state_db {
            Some(db) => {
                local_rag_store::store_has_pending_spool_bytes(db, &self.layout).unwrap_or(true)
            }
            // `MigrationOnly`: no usable state.sqlite, so no spool import is
            // happening either — never the reason to refuse an idle exit.
            None => false,
        };
        IdleGateInputs {
            live_sessions: self.sessions.len(),
            pending_spool_bytes,
            running_jobs: self.jobs.len(),
        }
    }

    /// Whether the daemon is currently eligible for idle shutdown (spec 02
    /// §4.3).
    pub fn is_idle_eligible(&self) -> bool {
        idle_eligible(&self.idle_inputs())
    }

    /// The daemon-managed indexing supervisor (T20-06), if this daemon has
    /// one — `None` exactly in `DaemonMode::MigrationOnly`. A future
    /// `admin/projects_reload` (T20-07) calls `reload()` through this.
    pub fn indexing_supervisor(&self) -> Option<&SupervisorHandle> {
        self.indexing_supervisor.as_ref()
    }

    /// Drain and release the store (spec 02 §4.3): await the startup resume
    /// passes to their natural completion, signal-then-await the continuous
    /// consolidation-trigger worker (D-024 — unlike the resume passes, it
    /// never completes on its own), then checkpoint, close the cache, and
    /// release the lock.
    pub async fn shutdown(mut self) {
        tracing::info!("daemon stopping");
        for handle in self.resume_handles.drain(..) {
            log_if_task_panicked("a startup resume task", handle.await);
        }
        if let Some(stop) = self.consolidation_trigger_stop.take() {
            let _ = stop.send(());
        }
        if let Some(join) = self.consolidation_trigger_join.take() {
            log_if_task_panicked("the consolidation-trigger worker", join.await);
        }
        if let Some(supervisor) = self.indexing_supervisor.take() {
            // Stops every worktree task it owns (each flushing its own last
            // successful generation first, T20-05) before returning — must
            // finish here, strictly before `drain_and_shutdown` below closes
            // `state`/`cache`.
            supervisor.shutdown().await;
        }
        tracing::debug!("background jobs stopped");
        if let Some(handshake_join) = self.handshake_join.take() {
            // `handshake_join` is only the *accept loop* — aborting it stops
            // new connections, matching spec 02 §4.3 step 1 ("stop
            // accepting"). Already-accepted connections run as independent
            // `tokio::spawn`ed tasks this handle never tracked; they are
            // deliberately left running rather than awaited or aborted here
            // — `daemon::handshake`'s own module doc explains why a
            // connection that sent `SHUTDOWN_REQUEST` must stay open through
            // this very drain, and `main.rs::run_serve`'s `Runtime` drop is
            // what actually reclaims them, safe by construction the same way
            // a hard kill is (spec 02 §4.3).
            handshake_join.abort();
        }
        tracing::debug!("no longer accepting connections");
        let lock_guard = self.lock_guard.take().expect("shutdown runs once");
        drain_and_shutdown(
            &self.layout,
            self.state_db.take(),
            self.cache_db.take(),
            lock_guard,
            self.handshake_stop.take(),
        )
        .await;
        tracing::info!("daemon stopped");
    }
}

/// D-046: `JoinHandle::await`'s `Err` is a lost background-task panic —
/// discarding it outright (`let _ = …await`, as `shutdown` used to) means a
/// fatal bug in a startup-resume pass or the consolidation-trigger worker
/// leaves zero trace anywhere, ever. `label` names the task for the log
/// line. None of the handles `shutdown` joins are ever `.abort()`'d (unlike
/// `handshake_join`), so `is_panic()` is expected to always hold on `Err`
/// here — checked explicitly anyway, rather than assumed, since silently
/// logging a non-panic `Err` under a "panicked" label would misdiagnose it.
fn log_if_task_panicked(label: &str, result: Result<(), tokio::task::JoinError>) {
    if let Err(e) = result
        && e.is_panic()
    {
        tracing::error!("local-rag: {label} panicked: {e}");
    }
}

/// D-030: the outcome of every session resumed here is reported, not
/// discarded — a stalled or failed session (spec 11 §4 `[FIXED concern]`: "a
/// newer hook binary writing a newer format than the running daemon supports
/// is a reportable incompatibility, not silent loss") is written to stderr
/// immediately, and is independently re-derivable later via
/// `local_rag_store::diagnose_spool_tail` (wired into `local-rag doctor`).
async fn spawn_spool_resume(
    db: Arc<StateDb>,
    layout: StoreLayout,
    uuids: Arc<dyn UuidSource + Send + Sync>,
    jobs: JobRegistry,
    now_ms: i64,
    payload_ttl_hours: u64,
) {
    let results =
        resume_spool_import(&db, &layout, &*uuids, &jobs, now_ms, payload_ttl_hours).await;
    for (session_id, outcome) in results {
        match outcome {
            Ok(outcome) => {
                if let Some(reason) = outcome.stalled_on {
                    tracing::warn!(
                        "local-rag: spool session {session_id} stalled on import: {reason}"
                    );
                }
            }
            Err(e) => {
                tracing::error!("local-rag: spool session {session_id} failed to import: {e}");
            }
        }
    }
}

/// D-046/D-047: report this stale-run resume sweep's outcome via `tracing` —
/// [`log_resume_sweep`] (shared with `consolidation_trigger.rs`'s own
/// per-tick stale-run-recovery call, D-047's own reason for existing: before
/// it, that second call site kept discarding this exact sweep's result on
/// every tick, not just here at startup).
#[allow(clippy::too_many_arguments)]
async fn spawn_consolidation_resume(
    db: Arc<StateDb>,
    pool: Arc<GeneratorPool>,
    jobs: JobRegistry,
    lease_ms: i64,
    renew_interval_ms: i64,
    now_ms: i64,
    data_policy: DataPolicy,
    uuids: Arc<dyn UuidSource + Send + Sync>,
) {
    let generate =
        |window| local_rag_memory::router::route(&db, &pool, data_policy, &*uuids, window);
    log_resume_sweep(
        resume_stale_consolidation_runs(
            &db,
            &jobs,
            lease_ms,
            renew_interval_ms,
            now_ms,
            local_rag_core::BUILD_ID,
            generate,
        )
        .await,
    );
}

/// D-024: the continuous consolidation-trigger worker (spec 07 §6).
///
/// Unlike [`spawn_consolidation_resume`]'s `generate` above — which borrows
/// `db`/`pool`/`uuids` from this function's own stack frame and is used
/// entirely within it — this worker is `tokio::spawn`ed independently and so
/// needs a `'static` `generate`. A `move` closure that captured `db`/`pool`/
/// `uuids` directly would make each call's returned future borrow from the
/// closure's own fields, which `Fn`'s signature cannot allow to escape
/// (`Fut` is one fixed associated type, not tied to a per-call borrow).
/// Instead, the outer closure holds `Arc` clones and hands each call a fresh
/// clone to own inside its own `async move` block — the returned future then
/// owns its data instead of borrowing the closure's.
#[allow(clippy::too_many_arguments)]
async fn spawn_consolidation_trigger(
    db: Arc<StateDb>,
    layout: StoreLayout,
    pool: Arc<GeneratorPool>,
    uuids: Arc<dyn UuidSource + Send + Sync>,
    jobs: JobRegistry,
    lease_ms: i64,
    renew_interval_ms: i64,
    batch_size: i64,
    queue_threshold: i64,
    payload_ttl_hours: u64,
    poll_interval: Duration,
    data_policy: DataPolicy,
    stop: oneshot::Receiver<()>,
) {
    let generate = {
        let db = Arc::clone(&db);
        let uuids = Arc::clone(&uuids);
        move |window| {
            let db = Arc::clone(&db);
            let pool = Arc::clone(&pool);
            let uuids = Arc::clone(&uuids);
            async move {
                local_rag_memory::router::route(&db, &pool, data_policy, &*uuids, window).await
            }
        }
    };
    let params = super::consolidation_trigger::ConsolidationTriggerParams {
        lease_ms,
        renew_interval_ms,
        batch_size,
        queue_threshold,
        payload_ttl_hours,
    };
    super::consolidation_trigger::run_consolidation_trigger(
        db,
        layout,
        uuids,
        jobs,
        params,
        poll_interval,
        local_rag_core::BUILD_ID,
        generate,
        stop,
    )
    .await;
}

/// The current wall-clock time as Unix milliseconds — the live clock
/// [`McpHandler`] reads on every request (spec 09's FTS staleness decision
/// is clock-dependent, so a value frozen at daemon startup would misjudge
/// it on every request after the first). Mirrors `main.rs::system_now_ms`/
/// `local_rag_hook::clock::system_now_ms` exactly — this project's
/// established convention is each call site carries its own trivial copy
/// rather than a shared helper.
fn system_now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Why [`run`] stopped waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownReason {
    /// SIGTERM or CTRL-C.
    Signal,
    /// Continuously idle-eligible for the configured grace period (spec 02
    /// §3.1 `idle_shutdown_secs`).
    Idle,
    /// A connected proxy sent `SHUTDOWN_REQUEST` (spec 13 §4's upgrade
    /// flow: a newer proxy detected a daemon version mismatch) — `run`
    /// drains and exits so a replacement daemon can take the lock.
    UpgradeRequested,
}

/// Wait for whichever comes first: an OS shutdown signal (already installed
/// — see [`ShutdownSignal`]'s own doc for why installation must have
/// happened before `start()`, not here), a proxy's `SHUTDOWN_REQUEST` (spec
/// 13 §4), or continuous idle eligibility for `idle_shutdown_secs`.
/// `poll_interval` is how often the idle gate is re-checked while waiting —
/// a plain parameter (no `[SPEC]` number exists for it), so tests can drive
/// it with `tokio::time::pause`/`advance` instead of real sleeps.
#[cfg(unix)]
pub async fn wait_for_shutdown_trigger(
    handle: &DaemonHandle,
    signal: &mut ShutdownSignal,
    idle_shutdown_secs: u64,
    poll_interval: Duration,
) -> ShutdownReason {
    tracing::debug!(idle_shutdown_secs, "waiting for a shutdown trigger");
    let idle_budget = Duration::from_secs(idle_shutdown_secs);
    let mut idle_since: Option<tokio::time::Instant> = None;
    let mut ticker = tokio::time::interval(poll_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = signal.wait() => return ShutdownReason::Signal,
            _ = handle.shutdown_requested.notified() => return ShutdownReason::UpgradeRequested,
            _ = ticker.tick() => {
                if handle.is_idle_eligible() {
                    let since = *idle_since.get_or_insert_with(tokio::time::Instant::now);
                    if since.elapsed() >= idle_budget {
                        return ShutdownReason::Idle;
                    }
                } else {
                    idle_since = None;
                }
            }
        }
    }
}

/// The full daemon lifecycle: install the shutdown-signal handler, start,
/// serve until a shutdown trigger fires, drain. This is what `main.rs`'s
/// `serve` command runs.
///
/// The signal handler is installed **before** [`DaemonHandle::start`] runs —
/// see [`ShutdownSignal`]'s own doc comment for why that ordering is load-
/// bearing, not stylistic: a SIGTERM arriving during startup (lock/migrate/
/// cache/bind, spec 02 §4.1) must be caught too, not just one arriving after
/// the wait loop begins.
#[cfg(unix)]
pub async fn run(
    opts: StartOptions,
    idle_shutdown_secs: u64,
    idle_poll_interval: Duration,
) -> Result<ShutdownReason, DaemonStartupError> {
    let mut signal = ShutdownSignal::install();
    let handle = DaemonHandle::start(opts).await?;
    let reason =
        wait_for_shutdown_trigger(&handle, &mut signal, idle_shutdown_secs, idle_poll_interval)
            .await;
    tracing::info!(?reason, "shutdown triggered");
    handle.shutdown().await;
    Ok(reason)
}

/// Windows has no local IPC transport implemented yet — see
/// [`DaemonHandle::start`]'s Windows arm (D-033). Returns the same typed
/// error `start()` would, without touching `ShutdownSignal` (Unix-only) at
/// all.
#[cfg(not(unix))]
pub async fn run(
    _opts: StartOptions,
    _idle_shutdown_secs: u64,
    _idle_poll_interval: Duration,
) -> Result<ShutdownReason, DaemonStartupError> {
    Err(DaemonStartupError::Bind(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "local-rag daemon IPC is not yet implemented on Windows (named pipes; tracked separately)",
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `Write` sink that appends into a shared buffer — same minimal
    /// capture technique `daemon::consolidation_trigger`'s own D-046 test
    /// uses, duplicated per this crate's established per-file-fixture
    /// convention rather than factored into a shared test-support helper.
    #[derive(Clone)]
    struct SharedBuf(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("lock").extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// D-046 regression: before this, `shutdown()` discarded every
    /// `JoinHandle::await` outright (`let _ = …await`) — a panic inside a
    /// startup-resume pass or the consolidation-trigger worker left zero
    /// trace anywhere. A clean `Ok(())` (the overwhelmingly common case,
    /// since these handles are never `.abort()`'d) must stay silent.
    #[tokio::test]
    async fn log_if_task_panicked_reports_a_panic_but_stays_silent_on_success() {
        let panicked: Result<(), tokio::task::JoinError> =
            tokio::spawn(async { panic!("boom") }).await;
        assert!(panicked.is_err(), "the spawned task was expected to panic");

        let buf = SharedBuf(Arc::new(std::sync::Mutex::new(Vec::new())));
        let subscriber = tracing_subscriber::fmt()
            .with_writer({
                let buf = buf.clone();
                move || buf.clone()
            })
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            log_if_task_panicked("the doomed task", panicked);
            log_if_task_panicked("a healthy task", Ok(()));
        });

        let logged = String::from_utf8(buf.0.lock().expect("lock").clone()).expect("utf8");
        assert!(logged.contains("the doomed task"), "{logged}");
        assert!(logged.contains("panicked"), "{logged}");
        assert!(
            !logged.contains("a healthy task"),
            "a clean join must not be logged: {logged}"
        );
    }
}
