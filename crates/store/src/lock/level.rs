//! The lock levels of spec 02 §5's strict-ordering table.

/// One level of the store's lock hierarchy (spec 02 §5 `[SPEC]`).
///
/// A task may only acquire a lock whose [`rank`](LockLevel::rank) is *strictly
/// greater* than that of any lock it already holds — no exceptions. `L2Read`/
/// `L2Write` and `L4a`/`L4b` are separate variants (the read/write sides of the
/// per-worktree lock, and the two independent write queues, are distinguishable
/// resources) but share a rank: the spec's table numbers them `L2`/`L4`, not as
/// four independently orderable levels, so nesting either sibling under the
/// other is exactly as forbidden as nesting a level under itself.
///
/// `L0` (`store.lock`) and `L3` (the shard-manager map) have no real
/// synchronization primitive yet — `L0` is a daemon-lifecycle concern (T15),
/// `L3` is the shard LRU manager (T09-02) — so today only their rank
/// participates in order-checking; whichever task later adds the real
/// primitive calls [`checked_scope_sync`](super::order::checked_scope_sync) /
/// [`checked_scope_async`](super::order::checked_scope_async) around it, same
/// as every other level here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LockLevel {
    /// `store.lock` — OS file lock, whole store, one daemon (T15).
    L0,
    /// `migration.lock` — schema migrations, exclusive with normal operation.
    L1,
    /// Per-worktree `RwLock`, read side — index/projection read consistency.
    L2Read,
    /// Per-worktree `RwLock`, write side — index/projection write consistency.
    L2Write,
    /// The shard-manager's open-shard LRU map (T09-02).
    L3,
    /// `state.sqlite`'s bounded write queue — the single physical SQLite writer.
    L4a,
    /// `cache.sqlite`'s bounded write queue — same, for the cache.
    L4b,
}

impl LockLevel {
    /// The numeric rank used for order comparison (spec 02 §5's `#` column).
    ///
    /// Comparison always goes through `rank()`, never a derived `Ord` on the
    /// enum itself: declaration order would wrongly make `L2Read < L2Write` and
    /// `L4a < L4b` distinguishable, when the spec's table says they are siblings
    /// at the same numbered level.
    pub const fn rank(self) -> u8 {
        match self {
            LockLevel::L0 => 0,
            LockLevel::L1 => 1,
            LockLevel::L2Read | LockLevel::L2Write => 2,
            LockLevel::L3 => 3,
            LockLevel::L4a | LockLevel::L4b => 4,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn l2_variants_share_a_rank() {
        assert_eq!(LockLevel::L2Read.rank(), LockLevel::L2Write.rank());
    }

    #[test]
    fn l4_variants_share_a_rank() {
        assert_eq!(LockLevel::L4a.rank(), LockLevel::L4b.rank());
    }

    #[test]
    fn ranks_are_strictly_increasing_across_distinct_levels() {
        assert!(LockLevel::L0.rank() < LockLevel::L1.rank());
        assert!(LockLevel::L1.rank() < LockLevel::L2Read.rank());
        assert!(LockLevel::L2Write.rank() < LockLevel::L3.rank());
        assert!(LockLevel::L3.rank() < LockLevel::L4a.rank());
    }
}
