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
//!
//! # The payload TTL sweep (T23-09, `D-123`)
//!
//! `local_rag_store::run_payload_ttl_sweep` (T13-05) is spec 12 §3's `[FIXED]`
//! "enforced by a sweeper" for `observation_payload`'s TTL — implemented,
//! exported, tested, and until this card reachable only from
//! `local-rag gc`, a human typing a command. [`run_payload_ttl_worker`] gives
//! it the daemon-side caller D-066 already gave the generation sweep, but on
//! its own schedule rather than the generation sweep's: `payload_ttl_hours`
//! defaults to 72h and a real daemon runs for days, so a start-only trigger
//! would enforce the TTL only across restarts — which is exactly how the
//! owner's live store reached 45651 overdue rows out of 46737 (`D-123`'s
//! measurement) with the sweeper compiled in the whole time. So this is a
//! [`tokio::time::interval`] whose *first* tick is immediate — that immediate
//! tick is the startup leg, and [`spawn_startup_gc`] above does not also run
//! this sweep, because a second startup call would be a guaranteed no-op
//! milliseconds later and only add a log line.
//!
//! Two properties are load-bearing, not incidental, and each has a mutation
//! test naming it in `tests/payload_ttl_schedule.rs`:
//!
//! - The [`JobKind::Gc`] guard is taken **inside the tick branch**, never
//!   across the wait between ticks — the same discipline
//!   [`JobKind::Normalization`]'s own doc states. A guard held across an
//!   hour-long wait would mean the daemon never sees `running_jobs == 0`
//!   again, and spec 02 §4.3's idle-shutdown gate would never fire.
//! - The clock is the **live** clock, read fresh every tick, never the
//!   `StartOptions.now_ms` this module's other sweep uses. That value is
//!   frozen at process start; anchoring a sweep that runs for the daemon's
//!   whole uptime to a frozen instant would stop finding anything overdue the
//!   moment the process outlived its own TTL window.
//!
//! **Order matters on the owner's live store, and it is ADR-0014's, not this
//! module's:** rescuing a parked session's backlog (`T23-03`) must run before
//! this sweep ever executes against that store, or the sweep deletes exactly
//! the payloads the repair exists to consolidate. This module has no way to
//! know whether a backlog was rescued — that is an operational sequencing
//! fact, recorded in `PROGRESS.md`'s `T23-09` evidence, not a condition the
//! code checks.

use std::sync::Arc;
use std::time::Duration;

use local_rag_store::{RetentionParams, StateDb};
use tokio::sync::oneshot;

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

/// Drive [`local_rag_store::run_payload_ttl_sweep`] on a fixed cadence until
/// `stop` fires — the daemon-side caller `T23-09`/`D-123` give the payload
/// TTL sweep, alongside `run_generation_sweep`'s startup-only one above.
///
/// Same loop shape as `daemon::normalization::run_normalization_worker`
/// (D-024's precedent): an [`tokio::time::interval`] whose first tick is
/// immediate, a `select!` against a `oneshot` so shutdown never waits out a
/// full period. That immediate first tick doubles as the startup sweep this
/// module's own doc explains is deliberately not duplicated in
/// [`spawn_startup_gc`].
///
/// A failed sweep is a `warn!`, never fatal — the same discipline
/// [`spawn_startup_gc`] states: the sweep is idempotent, and the next tick
/// resumes it.
pub async fn run_payload_ttl_worker(
    db: Arc<StateDb>,
    jobs: JobRegistry,
    poll_interval: Duration,
    now_ms: impl Fn() -> i64 + Send,
    mut stop: oneshot::Receiver<()>,
) {
    let mut ticker = tokio::time::interval(poll_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = &mut stop => return,
            _ = ticker.tick() => {
                // Taken here, inside the tick — never across the wait above,
                // or an idle daemon would never see `running_jobs == 0` again
                // and spec 02 §4.3's idle-shutdown gate would never fire.
                let _job = jobs.begin(JobKind::Gc);
                match local_rag_store::run_payload_ttl_sweep(&db, now_ms(), false).await {
                    // Silent on a tick that found nothing due: this runs for
                    // the daemon's whole uptime, and a line every hour saying
                    // "removed 0" is exactly the noise D-024's own worker
                    // avoids.
                    Ok(report) if report.payload_removed > 0 => tracing::info!(
                        payload_removed = report.payload_removed,
                        payload_retained = report.payload_retained,
                        total_envelopes = report.total_envelopes,
                        "payload TTL sweep removed overdue rows"
                    ),
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!(reason = %e, "payload TTL sweep failed; will retry on next tick")
                    }
                }
            }
        }
    }
}
