//! The per-worktree `RwLock` registry realizing L2 (spec 02 §5).
//!
//! The write side of L2 already exists *structurally* today — one reconcile
//! task per worktree (`local_rag_index::reconcile::driver`) serializes writes
//! without an explicit lock object. This registry is the actual lock object:
//! adopting it into that driver, the projection switch, and a future search
//! executor is later work (T09-03/T09-04, group 12/15), not this module.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};

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
