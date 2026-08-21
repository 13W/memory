//! The daemon lifecycle (spec 02 §4) — T15-01.
//!
//! [`lock`] is the store lock (L0, spec 02 §2/§4.1/§5) with stale-owner
//! recovery; [`probe`] is the liveness handshake `lock` uses to tell a live
//! prior owner apart from a dead one (PID reuse-safe), now speaking
//! [`handshake`]'s real HELLO/WELCOME wire protocol. [`session`]/[`jobs`]
//! are the two live-count registries the idle-shutdown gate ([`idle`])
//! reads. [`resume`] is the two startup catch-up passes spec 02 §4.1 step 5
//! names. [`mode`] is the daemon's serving mode (spec 02 §6); [`handshake`]
//! is the real per-connection HELLO/WELCOME/INCOMPATIBLE/SHUTDOWN_REQUEST +
//! MCP passthrough handler (spec 02 §4.2, T15-02); [`error`] maps a
//! migration failure into both. [`gitroot`] git-probes an MCP request's
//! `worktree_root` path into the registry's `RequestRoot` (spec 02 §3.3,
//! T15-03). [`search`] builds the `local_rag_search::SearchEngine` the MCP
//! code-query tools call; [`memory`] builds the analogous [`MemoryContext`]
//! the MCP status/memory-read tools call (spec 11 §2, T15-04). [`mcp`] is
//! the real MCP JSON-RPC dispatcher (spec 11 §2, T15-03/T15-04) — the
//! `RequestHandler` `lifecycle` wires in place of T15-02's
//! `EchoRequestHandler`. [`consolidation_trigger`] is the continuous
//! consolidation-trigger background worker (spec 07 §6, D-024) —
//! [`resume`]'s missing continuous quarter. [`embedder_provider`] is the
//! daemon's single owner of its ONNX sessions — at most two per process
//! (T20-03) — opened lazily so a model installed after startup needs no
//! restart (D-037); [`query_embedder`] adapts those sessions'
//! `local_rag_embed::Embedder`s into `search`'s `QueryEmbedder` seam (T15-07).
//! [`indexing`] is the daemon's own per-worktree background indexer (spec 06
//! §1, T20-05) — the second caller of `local_rag_index::reconcile::
//! {spawn_reconciler, spawn_watcher}` after `local-rag watch`, projecting
//! under `L2.write` (`local_rag::indexing::write_locked`, T20-04).
//! [`lifecycle`] composes all of the above into the
//! five startup steps and the shutdown sequence ([`shutdown`]) —
//! [`lifecycle::run`] is what `main.rs`'s `serve` command drives.

pub mod consolidation_trigger;
pub mod embedder_provider;
pub mod error;
pub mod gc;
pub mod gitroot;
pub mod handshake;
pub mod idle;
pub mod indexing;
pub mod jobs;
pub mod lifecycle;
pub mod lock;
pub mod mcp;
pub mod memory;
pub mod mode;
pub mod normalization;
pub mod probe;
pub mod query_embedder;
pub mod resume;
pub mod search;
pub mod session;
pub mod shutdown;
pub mod telemetry;
pub mod tool_calls;

pub use consolidation_trigger::{
    ConsolidationTriggerParams, SessionTickOutcome, consolidation_trigger_tick,
    run_consolidation_trigger,
};
pub use embedder_provider::{LazyEmbedderProvider, LazyProvider, ProviderProbe};
pub use error::{error_envelope, migration_only_reason};
pub use gitroot::{case_sensitivity, probe as probe_worktree_root, request_root};
#[cfg(unix)]
pub use handshake::serve_connections;
pub use handshake::{EchoRequestHandler, HandshakeContext, RequestHandler};
pub use idle::{IdleGateInputs, idle_eligible};
pub use indexing::{
    WorktreeTaskHandle, WorktreeTaskParams, WorktreeTaskStartError, WorktreeTaskStatus,
    spawn_worktree_task,
};
pub use jobs::{JobGuard, JobKind, JobRegistry};
#[cfg(unix)]
pub use lifecycle::wait_for_shutdown_trigger;
pub use lifecycle::{DaemonHandle, DaemonStartupError, ShutdownReason, StartOptions, run};
pub use lock::{
    StoreLockError, StoreLockFileState, StoreLockGuard, StoreLockInfo, acquire,
    read_store_lock_file,
};
pub use mcp::McpHandler;
pub use memory::{MemoryContext, build_memory_context};
pub use mode::{DaemonMode, MigrationOnlyReason};
pub use probe::LIVENESS_PROBE_TIMEOUT_MS;
#[cfg(unix)]
pub use probe::{CallAdminError, call_admin, fetch_welcome};
pub use query_embedder::{
    EmbedderQueryAdapter, LazyQueryEmbedder, MemoryEmbedderQueryAdapter, code_query_embedder,
    memory_query_embedder,
};
pub use resume::{
    ConsolidationResumeError, ResumeOutcome, build_best_effort_pool, resume_spool_import,
    resume_stale_consolidation_runs,
};
pub use search::{NoRebuildVectorSource, build_search_engine};
pub use session::{SessionGuard, SessionRegistry};
pub use shutdown::{ShutdownSignal, drain_and_shutdown};
