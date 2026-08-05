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
//! [`resume`]'s missing continuous quarter. [`query_embedder`] adapts a real
//! `local_rag_embed::Embedder` into `search`'s `QueryEmbedder` seam (T15-07).
//! [`lifecycle`] composes all of the above into the five startup steps and
//! the shutdown sequence ([`shutdown`]) — [`lifecycle::run`] is what
//! `main.rs`'s `serve` command drives.

pub mod consolidation_trigger;
pub mod error;
pub mod gitroot;
pub mod handshake;
pub mod idle;
pub mod jobs;
pub mod lifecycle;
pub mod lock;
pub mod mcp;
pub mod memory;
pub mod mode;
pub mod probe;
pub mod query_embedder;
pub mod resume;
pub mod search;
pub mod session;
pub mod shutdown;

pub use consolidation_trigger::{
    ConsolidationTriggerParams, SessionTickOutcome, consolidation_trigger_tick,
    run_consolidation_trigger,
};
pub use error::{error_envelope, migration_only_reason};
pub use gitroot::{case_sensitivity, probe as probe_worktree_root, request_root};
#[cfg(unix)]
pub use handshake::serve_connections;
pub use handshake::{EchoRequestHandler, HandshakeContext, RequestHandler};
pub use idle::{IdleGateInputs, idle_eligible};
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
pub use probe::{LIVENESS_PROBE_TIMEOUT_MS, LivenessOutcome, LivenessProbe};
#[cfg(unix)]
pub use probe::{SocketLivenessProbe, fetch_welcome};
pub use query_embedder::EmbedderQueryAdapter;
pub use resume::{
    ConsolidationResumeError, ResumeOutcome, build_best_effort_pool, resume_spool_import,
    resume_stale_consolidation_runs,
};
pub use search::{NoRebuildVectorSource, build_search_engine};
pub use session::{SessionGuard, SessionRegistry};
#[cfg(unix)]
pub use shutdown::ShutdownSignal;
pub use shutdown::drain_and_shutdown;
