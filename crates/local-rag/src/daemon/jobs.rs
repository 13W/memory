//! Tracked background jobs (spec 02 §4.3 idle-shutdown gate: "no running
//! index/consolidation/GC jobs") — T15-01.
//!
//! T15-01's own scope is narrow (see [`super::resume`]): only the two
//! startup catch-up passes spec 02 §4.1 step 5 names run today, so only
//! [`JobKind::SpoolImport`]/[`JobKind::ConsolidationResume`] exist. The
//! registry itself is generic over `JobKind` — a later group-15 card wiring
//! a continuous reconcile/backfill/GC trigger adds its own variant and
//! tracks it through this same registry rather than inventing a second one
//! (`#[non_exhaustive]` keeps that open without a breaking change, the same
//! pattern `local_rag_protocol::ErrorCode` already uses).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// The kind of a tracked background job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum JobKind {
    /// A startup spool-import catch-up pass (spec 02 §4.1 step 5; 07 §6).
    SpoolImport,
    /// A startup consolidation crash-resume pass (spec 02 §4.1 step 5; 08 §4).
    ConsolidationResume,
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
        JobGuard {
            inner: Arc::clone(&self.inner),
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
    token: u64,
}

impl Drop for JobGuard {
    fn drop(&mut self) {
        self.inner
            .jobs
            .lock()
            .expect("job registry mutex poisoned")
            .remove(&self.token);
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
