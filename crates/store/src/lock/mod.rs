//! The store's strict lock-acquisition hierarchy (spec 02 §5 `[SPEC]`) — T09-01.
//!
//! Six levels, `L0` (whole store) through `L4b` (the cache write queue): a task
//! may only acquire a lock with a strictly higher [`LockLevel::rank`] than any
//! it already holds. This module ships the **primitive** — typed levels, the
//! per-worktree [`WorktreeLockRegistry`] (L2), and debug/test order
//! enforcement — plus in-place instrumentation of the two pieces that already
//! physically exist: `MigrationLock` (L1, `crate::migrate`) and
//! `StateWriter`/`CacheWriter` (L4a/L4b, `crate::state`/`crate::cache`).
//!
//! `L0` (`store.lock`) and `L3` (the shard-manager map) have no real
//! synchronization primitive yet — see [`LockLevel`]'s docs. Adopting this
//! hierarchy into the projection switch is later work (T11-05, group 11); the
//! reconcile driver's own adoption has no dedicated task yet in the current
//! plan. The read side is adopted by `local_rag_search` (T09-03) via
//! [`WorktreeLockRegistry::read_bounded`].

mod level;
mod order;
mod worktree;

pub use level::LockLevel;
pub use order::{OrderViolation, check_order, checked_scope_async, checked_scope_sync, held_level};
pub use worktree::{ReadTimedOut, WorktreeLockRegistry};
