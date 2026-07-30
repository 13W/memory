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
//! migration failure into both. [`lifecycle`] composes all of the above into
//! the five startup steps and the shutdown sequence ([`shutdown`]) —
//! [`lifecycle::run`] is what `main.rs`'s `serve` command drives.

pub mod error;
pub mod handshake;
pub mod idle;
pub mod jobs;
pub mod lifecycle;
pub mod lock;
pub mod mode;
pub mod probe;
pub mod resume;
pub mod session;
pub mod shutdown;

pub use error::{error_envelope, migration_only_reason};
pub use handshake::{EchoRequestHandler, HandshakeContext, RequestHandler, serve_connections};
pub use idle::{IdleGateInputs, idle_eligible};
pub use jobs::{JobGuard, JobKind, JobRegistry};
pub use lifecycle::{
    DaemonHandle, DaemonStartupError, ShutdownReason, StartOptions, run, wait_for_shutdown_trigger,
};
pub use lock::{StoreLockError, StoreLockGuard, StoreLockInfo, acquire};
pub use mode::{DaemonMode, MigrationOnlyReason};
#[cfg(unix)]
pub use probe::SocketLivenessProbe;
pub use probe::{LIVENESS_PROBE_TIMEOUT_MS, LivenessOutcome, LivenessProbe};
pub use resume::{
    ConsolidationResumeError, ResumeOutcome, build_best_effort_pool, resume_spool_import,
    resume_stale_consolidation_runs,
};
pub use session::{SessionGuard, SessionRegistry};
#[cfg(unix)]
pub use shutdown::ShutdownSignal;
pub use shutdown::drain_and_shutdown;
