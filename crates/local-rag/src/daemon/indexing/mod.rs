//! Per-worktree background indexing (spec 06 §1, T20-05) — the daemon's own
//! "watch loop", the second caller of `local_rag_index::reconcile::
//! {spawn_reconciler, spawn_watcher}` after `local-rag watch` (T15-07).
//!
//! [`worktree_task`] is one worktree's task, directly constructible and
//! testable in isolation; [`supervisor`] (T20-06) starts one per `enabled`
//! row in the `managed_worktree` registry (T20-01).

pub mod supervisor;
pub mod worktree_task;

pub use supervisor::{ReloadOutcome, SupervisorHandle, SupervisorParams, spawn_supervisor};
pub use worktree_task::{
    WorktreeTaskHandle, WorktreeTaskParams, WorktreeTaskStartError, WorktreeTaskStatus,
    spawn_worktree_task,
};
