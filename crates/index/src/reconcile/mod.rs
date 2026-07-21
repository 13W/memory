//! Worktree reconcile: turning an authoritative tree scan into a durable
//! generation (spec 06 §2) — group 05.
//!
//! [`build_generation`] is the core builder (T05-03): it consumes a
//! [`ScanManifest`](crate::scan::ScanManifest), applies structural sharing, and
//! persists `generation_file`/`skipped_file`/occurrences up to `projection_ready`.
//!
//! The scheduler (T05-04) drives that builder: [`schedule`] is the pure
//! debounce/coalescing engine, [`driver`] the async per-worktree reconcile loop
//! (scan → build) plus its registry composition, and [`watcher`] the `notify`
//! filesystem-watcher adapter. Typed retry/failure handling (T05-05) folds each
//! reconcile outcome into an observable [`driver::ReconcileFailure`] (counter +
//! exponential backoff + `last_error`) and marks a failed generation `failed`
//! without ever routing it (spec 04 §1).

pub mod build;
pub mod driver;
pub mod schedule;
pub mod watcher;

pub use build::{BuildError, BuildErrorKind, BuildOutcome, build_generation};
pub use driver::{
    MetaError, ReconcileError, ReconcileFailure, ReconcileHandle, ReconcileReport, WorktreeMeta,
    WorktreeReconciler, load_worktree_meta, nested_prune_roots, reconcile_once, spawn_reconciler,
};
pub use schedule::{
    DEBOUNCE_MS, Debouncer, PERIODIC_MS, PlannedReconcile, RETRY_BACKOFF_BASE_MS,
    RETRY_BACKOFF_MAX_MS, ScheduleConfig, TriggerKind,
};
pub use watcher::{WatchEvent, spawn_watcher, watch_event_to_trigger};
