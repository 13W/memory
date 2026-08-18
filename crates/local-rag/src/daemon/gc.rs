//! The startup retention sweep (spec 02 §4.1, spec 06 §5) — D-066.
//!
//! Not in [`super::resume`], deliberately: those two passes recover work a
//! crash interrupted (spec 02 §4.1 step 5, "Resume: pending spool import,
//! crashed consolidation runs"). This is planned maintenance, which spec 06 §5
//! describes without prescribing a schedule — "Metrics that drive (**not
//! schedule**) maintenance". Startup is the owner's chosen trigger; the policy
//! it enforces still comes from config
//! (`[storage].retired_generations_keep`/`retired_generations_ttl_h`).
//!
//! # Why a background job and not a startup step
//!
//! The sweep is batched (500 rows/tx, spec 06 §5) through the same global
//! writer queue as every mutation, and its first run on a store with a backlog
//! is long: the reporter's store had 3396 `retiring` generations when this was
//! written. Blocking `daemon ready` on it would time out the proxy's
//! connect-or-spawn budget (spec 13 §2) on exactly the stores that need
//! collecting most. So it is spawned like the resume passes and reports
//! afterwards.
//!
//! Because it takes a [`JobKind::Gc`] guard for its whole duration, spec 02
//! §4.3's idle-shutdown gate ("no running index/consolidation/GC jobs") sees
//! it and will not shut the daemon down mid-sweep.

use std::sync::Arc;

use local_rag_store::{RetentionParams, StateDb};

use super::jobs::{JobKind, JobRegistry};
use crate::gc::run_generation_sweep;

/// Run one retention sweep and report it, never propagating a failure.
///
/// A sweep that fails is a `warn!`, not a startup error: the store is exactly
/// as usable uncollected as collected, earlier batches stand, and `run_sweep`
/// is idempotent — the next daemon start (or `local-rag gc`) resumes it. The
/// daemon must never fail to come up over housekeeping, the same discipline
/// [`super::resume::resume_spool_import`] states for its own directory-read
/// failure.
pub async fn spawn_startup_gc(
    db: Arc<StateDb>,
    jobs: JobRegistry,
    retention: RetentionParams,
    now_ms: i64,
) {
    let _job = jobs.begin(JobKind::Gc);
    match run_generation_sweep(&db, &retention, now_ms).await {
        Ok(report) => tracing::info!(
            generations = report.generations,
            occurrences = report.occurrences,
            edges = report.edges,
            generation_files = report.generation_files,
            skipped_files = report.skipped_files,
            file_revisions = report.file_revisions,
            content_blobs = report.content_blobs,
            parsed_units = report.parsed_units,
            unresolved_references = report.unresolved_references,
            total = report.total(),
            "retention sweep finished"
        ),
        Err(e) => tracing::warn!(reason = %e, "retention sweep failed; will retry on next start"),
    }
}
