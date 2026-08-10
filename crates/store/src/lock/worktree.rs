//! The per-worktree `RwLock` registry realizing L2 (spec 02 §5).
//!
//! [`WorktreeLockRegistry::read_bounded`] (T09-03) is the entry point
//! `local_rag_search`'s pipeline uses for the read side (spec 06 §3). The
//! write side's first production adopter is T20-04: `daemon::lifecycle`
//! holds the one `Arc<WorktreeLockRegistry>` a daemon process ever
//! constructs (`StartOptions`/`DaemonHandle::locks`), shared between
//! `SearchEngine` and `local_rag::indexing::write_locked` — the typed entry
//! point the per-worktree indexing task (T20-05) wraps its whole
//! `reconcile_once → project_generation` cycle in. Adopting this registry
//! *inside* `local_rag_index::reconcile::driver` or the `projection` crate's
//! own `switch` is still explicitly not done — both stay the caller's job,
//! unchanged since T09-01/T11-05.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::RwLock;

use super::level::LockLevel;
use super::order::checked_scope_async;

/// A registry of per-worktree read/write locks (spec 02 §5 L2).
///
/// Entries are created on first use and kept for the registry's lifetime —
/// `worktree_id`s are UUIDv7s, never reused, so a stale entry after a worktree
/// is detached/removed is at worst a few dozen bytes, never a correctness
/// issue. Eviction was `[OPEN]`, left for whichever task first owns a
/// long-lived registry instance — T20-04 is that task, and its decision is
/// **not to introduce eviction**: entry count is bounded by the number of
/// distinct worktrees one daemon process ever touches, and the process itself
/// exits on idle (spec 02 §4.3), so the bound resets on its own. Eviction
/// would additionally need a refcount against guards a caller might still be
/// holding when its entry would otherwise be dropped — real complexity this
/// bound never pays for in practice.
#[derive(Debug, Default)]
pub struct WorktreeLockRegistry {
    entries: Mutex<HashMap<String, Arc<RwLock<()>>>>,
}

impl WorktreeLockRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Run `body` holding `worktree_id`'s lock for **read** (L2.read).
    ///
    /// Per spec 06 §3, the read side is meant to span an entire hybrid-search
    /// pipeline — `body` may `.await` arbitrarily.
    pub async fn read<Fut>(&self, worktree_id: &str, body: Fut) -> Fut::Output
    where
        Fut: Future,
    {
        let lock = self.entry(worktree_id);
        checked_scope_async(LockLevel::L2Read, async move {
            let _guard = lock.read().await;
            body.await
        })
        .await
    }

    /// Run `body` holding `worktree_id`'s lock for **write** (L2.write).
    ///
    /// Per spec 02 §5, this is the per-worktree write path's outermost lock —
    /// reconcile/switch/rebuild all serialize through it; the two switch axes
    /// (generation, model space) are serialized by this same lock.
    pub async fn write<Fut>(&self, worktree_id: &str, body: Fut) -> Fut::Output
    where
        Fut: Future,
    {
        let lock = self.entry(worktree_id);
        checked_scope_async(LockLevel::L2Write, async move {
            let _guard = lock.write().await;
            body.await
        })
        .await
    }

    /// Run `body` holding `worktree_id`'s lock for **read** (L2.read), bounding
    /// only the *wait* for the guard (spec 02 §6: "Generation switch in flight |
    /// search waits on L2.read (bounded); timeout → BUSY_RETRY").
    ///
    /// Once the guard is acquired, `body` runs with no further time bound —
    /// spec 02 §6 bounds the wait for the lock, not the pipeline's own
    /// execution time. A plain `tokio::time::timeout` wrapped around the whole
    /// [`read`](Self::read) call would also cancel an already-in-flight `body`
    /// on timeout, which is a different (and wrong) thing to bound.
    pub async fn read_bounded<Fut>(
        &self,
        worktree_id: &str,
        wait: Duration,
        body: Fut,
    ) -> Result<Fut::Output, ReadTimedOut>
    where
        Fut: Future,
    {
        let lock = self.entry(worktree_id);
        let guard = tokio::time::timeout(wait, lock.read())
            .await
            .map_err(|_elapsed| ReadTimedOut)?;
        Ok(checked_scope_async(LockLevel::L2Read, async move {
            let _guard = guard;
            body.await
        })
        .await)
    }

    /// The (possibly freshly created) `RwLock` for `worktree_id`.
    fn entry(&self, worktree_id: &str) -> Arc<RwLock<()>> {
        let mut entries = self
            .entries
            .lock()
            .expect("worktree lock registry mutex poisoned");
        entries
            .entry(worktree_id.to_owned())
            .or_insert_with(|| Arc::new(RwLock::new(())))
            .clone()
    }
}

/// [`WorktreeLockRegistry::read_bounded`] timed out waiting for a concurrent
/// writer to release L2.write (spec 02 §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadTimedOut;

impl std::fmt::Display for ReadTimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("timed out waiting for L2.read (a writer holds L2.write)")
    }
}

impl std::error::Error for ReadTimedOut {}
