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
