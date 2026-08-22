//! Startup catch-up resume passes (spec 02 §4.1 step 5) — T15-01.
//!
//! Both [`spool`] and [`consolidation`] are one-shot passes run once at
//! startup, never a continuous scheduler — see each module's own doc for the
//! exact scope boundary against later group-15 cards.

pub mod consolidation;
pub mod spool;

pub use consolidation::{
    ConsolidationResumeError, ResumeOutcome, build_best_effort_pool, log_resume_sweep,
    resume_stale_consolidation_runs,
};
pub use spool::resume_spool_import;

/// A test-only pause point, consulted once per resumed item (one per spool
/// session, one per stale consolidation run) — never compiled into a
/// release/distribution build (`failpoints` is off by default; see this
/// crate's `Cargo.toml`).
///
/// Not the shared `local_rag_test_support::Failpoints` registry: that
/// registry lives *in-process* (a `OnceLock`), which cannot be armed by a
/// separate test process across a real `local-rag serve` subprocess
/// boundary. `LOCAL_RAG_TEST_RESUME_DELAY_MS` is this crate's own version of
/// exactly the env-var hand-off `local-rag-hook`'s
/// `LOCAL_RAG_HOOK_FAILPOINT` already uses for the identical problem (a
/// child process arming its own state before doing anything else) — see
/// `crates/local-rag-hook/src/main.rs::arm_failpoint_from_env`'s doc comment.
/// Used by `tests/serve_subprocess.rs`'s "SIGTERM at safe points" scenario to
/// create a real, observable window where a resume job is provably still in
/// flight when the signal arrives.
#[cfg(feature = "failpoints")]
pub(crate) async fn test_resume_pause() {
    if let Ok(raw) = std::env::var("LOCAL_RAG_TEST_RESUME_DELAY_MS")
        && let Ok(ms) = raw.parse::<u64>()
    {
        tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
    }
}

#[cfg(not(feature = "failpoints"))]
pub(crate) async fn test_resume_pause() {}

/// The same test-only pause point, but **blocking** — a `std::thread::sleep`
/// straight from an `async fn`, which no `abort()` can preempt.
///
/// [`test_resume_pause`] models a worker that shutdown can cancel; this one
/// models the case both shutdown budgets actually exist for, and the only one
/// that matters for exclusivity (D-090): a worker inside a *synchronous*
/// stretch, which keeps running after it has been cancelled and after the
/// daemon has logged `daemon stopped`. In production that stretch is
/// `run_backfill`'s `blob_index` on a dedicated indexing thread; reproducing
/// it through the real one would need a real ONNX model and a real repository
/// (`tests/serve_subprocess_managed_indexing.rs`'s own opt-in problem), while
/// the property under test — the store lock is not released while such a
/// worker lives — is about the shutdown sequence, not about indexing.
///
/// Armed the same way and for the same reason as `LOCAL_RAG_TEST_RESUME_
/// DELAY_MS`: an env-var hand-off, because the test lives in another process.
#[cfg(feature = "failpoints")]
pub(crate) fn test_resume_blocking_stall() {
    if let Ok(raw) = std::env::var("LOCAL_RAG_TEST_RESUME_BLOCKING_STALL_MS")
        && let Ok(ms) = raw.parse::<u64>()
    {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
}

#[cfg(not(feature = "failpoints"))]
pub(crate) fn test_resume_blocking_stall() {}
