//! Worktree reconcile: turning an authoritative tree scan into a durable
//! generation (spec 06 §2) — group 05.
//!
//! [`build_generation`] is the core builder (T05-03): it consumes a
//! [`ScanManifest`](crate::scan::ScanManifest), applies structural sharing, and
//! persists `generation_file`/`skipped_file`/occurrences up to `projection_ready`.
//! The scheduler/triggers (T05-04) and typed retry/failure handling (T05-05) are
//! later siblings in this module.

pub mod build;

pub use build::{BuildError, BuildErrorKind, BuildOutcome, build_generation};
