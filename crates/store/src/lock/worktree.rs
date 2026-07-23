//! The per-worktree `RwLock` registry realizing L2 (spec 02 §5).
//!
//! The write side of L2 already exists *structurally* today — one reconcile
//! task per worktree (`local_rag_index::reconcile::driver`) serializes writes
//! without an explicit lock object. This registry is the actual lock object:
//! adopting it into that driver and the projection switch is later work
//! (T09-04, group 15), not this module. [`WorktreeLockRegistry::read_bounded`]
//! (T09-03) is the entry point `local_rag_search`'s pipeline uses for the read
//! side (spec 06 §3).

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
/// issue. Whether to evict entries is `[OPEN]`: left for whichever task first
/// owns a long-lived registry instance to revisit if it is ever observed to
/// matter in practice.
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
