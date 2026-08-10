//! Tracked background jobs (spec 02 §4.3 idle-shutdown gate: "no running
//! index/consolidation/GC jobs") — T15-01.
//!
//! T15-01's own scope was narrow (see [`super::resume`]): only the two
//! startup catch-up passes spec 02 §4.1 step 5 names ran at first, so only
//! [`JobKind::SpoolImport`]/[`JobKind::ConsolidationResume`] existed. D-024
//! then wired the continuous consolidation-trigger worker (spec 07 §6:
//! checkpoint on `Stop`, queue-size threshold, best-effort `SessionEnd`),
//! adding [`JobKind::ConsolidationTrigger`] rather than inventing a second
//! registry (`#[non_exhaustive]` keeps this open without a breaking change,
//! the same pattern `local_rag_protocol::ErrorCode` already uses) — a future
//! reconcile/backfill/GC trigger follows the identical path.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// The kind of a tracked background job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum JobKind {
    /// A startup spool-import catch-up pass (spec 02 §4.1 step 5; 07 §6) —
    /// also reused per-tick by the continuous consolidation-trigger worker
    /// (D-024): the same operation (`import_session_tail`), just repeated.
    SpoolImport,
    /// A startup consolidation crash-resume pass (spec 02 §4.1 step 5; 08 §4)
    /// — also reused per-tick by the continuous consolidation-trigger worker
    /// (D-024) for its own stale-run recovery sweep.
    ConsolidationResume,
    /// The continuous consolidation-trigger worker (D-024, spec 07 §6)
    /// opening a fresh consolidation window — distinct from
    /// [`JobKind::ConsolidationResume`], which only recovers crashed/expired
    /// runs.
    ConsolidationTrigger,
    /// A per-worktree indexing task (T20-05, spec 06 §1) projecting a newly
    /// reconciled generation (embed → activate → materialize) under
    /// `L2.write` — held only for that active span, not while the task
    /// merely waits for its next `successes`/`failures` trigger.
    Reconcile,
}

#[derive(Debug, Default)]
struct Inner {
    jobs: Mutex<HashMap<u64, JobKind>>,
    next_token: AtomicU64,
}

/// A registry of currently-running background jobs.
///
/// Cheaply cloneable (shares the same underlying map); every clone observes
/// the same set of in-flight jobs.
#[derive(Debug, Clone, Default)]
pub struct JobRegistry {
    inner: Arc<Inner>,
}

impl JobRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark one job of `kind` as started. It is marked finished when the
    /// returned [`JobGuard`] drops — hold it for the job's entire duration,
    /// including the span while it merely *waits* to start (e.g. queued
    /// behind a lock), so idle-shutdown correctly sees "a job is running"
    /// for the whole window, not just its active portion.
    pub fn begin(&self, kind: JobKind) -> JobGuard {
        let token = self.inner.next_token.fetch_add(1, Ordering::Relaxed);
        self.inner
            .jobs
            .lock()
            .expect("job registry mutex poisoned")
            .insert(token, kind);
        tracing::debug!(?kind, token, "job started");
        JobGuard {
            inner: Arc::clone(&self.inner),
            kind,
            token,
        }
    }

    /// The number of currently running jobs.
    pub fn len(&self) -> usize {
        self.inner
            .jobs
            .lock()
            .expect("job registry mutex poisoned")
            .len()
    }

    /// Whether no jobs are currently running.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// An RAII handle for one running job: marks it finished on drop.
#[derive(Debug)]
pub struct JobGuard {
    inner: Arc<Inner>,
    kind: JobKind,
    token: u64,
}

impl Drop for JobGuard {
    fn drop(&mut self) {
        self.inner
            .jobs
            .lock()
            .expect("job registry mutex poisoned")
            .remove(&self.token);
        tracing::debug!(kind = ?self.kind, token = self.token, "job finished");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_registry_is_empty() {
        let registry = JobRegistry::new();
        assert!(registry.is_empty());
    }

    #[test]
    fn beginning_grows_and_dropping_shrinks() {
        let registry = JobRegistry::new();
        let a = registry.begin(JobKind::SpoolImport);
        assert_eq!(registry.len(), 1);
        let b = registry.begin(JobKind::ConsolidationResume);
        assert_eq!(registry.len(), 2);

        drop(a);
        assert_eq!(registry.len(), 1);
        drop(b);
        assert!(registry.is_empty());
    }

    #[test]
    fn two_jobs_of_the_same_kind_do_not_collide() {
        let registry = JobRegistry::new();
        let a = registry.begin(JobKind::SpoolImport);
        let b = registry.begin(JobKind::SpoolImport);
        assert_eq!(registry.len(), 2);
        drop(a);
        assert_eq!(registry.len(), 1);
        drop(b);
        assert!(registry.is_empty());
    }
}
