//! The generation retention sweep, and the pin set it must respect — D-066.
//!
//! Spec 06 §5's `[FIXED]` mark-and-sweep is implemented in
//! [`local_rag_store::retention`], but until this module it had no caller
//! outside its own tests: the store never actually collected. The reporter's
//! live store had 3396 generations in `retiring` and had grown to 15 GB.
//!
//! Two callers share this module so they cannot disagree about what is
//! sweepable — the daemon's startup job (`daemon::gc`) and `local-rag gc`
//! (`cli::gc`). Same split as [`crate::indexing`], which already holds
//! `write_locked` for a daemon/CLI pair.
//!
//! # The pin set
//!
//! [`ExternalPins`] is the seam spec 06 §5 leaves for references the mark phase
//! cannot see from generation state alone. Its as-built note (T06-01, point 5)
//! expected groups 14/16 — "memory evidence / audit / export" — to populate it.
//! As built, they do not: `memory_evidence` references
//! `observation_envelope(observation_id)`, and no memory/audit/export table
//! foreign-keys `generation` or `file_revision` at all.
//!
//! The one table that does is `worktree_projection_state`, through three
//! columns (`active_`/`projected_`/`target_generation_id`). A generation it
//! still names must never be swept: with `foreign_keys=ON` the `DELETE` from
//! `generation` would fail and roll the batch back. That is the whole of
//! [`sweep_external_pins`] — and it is why the sweep is safe to turn on now.
//!
//! Job leases ([`local_rag_store::JobLease`]) stay empty: the lease subsystem
//! spec 06 §5 names does not exist, and the sweep's own candidate rule
//! (`state ∈ {retiring, failed}`) already excludes everything an in-flight
//! indexing cycle is writing.

use local_rag_store::{
    ExternalPins, OpenError, RetentionParams, StateDb, SweepError, SweepPlan, SweepReport,
    referenced_generation_ids, run_sweep,
};

/// Why a sweep could not run.
///
/// Both read failures are fatal to the sweep rather than degrading to an empty
/// pin set: empty is not a safe default here, it is the exact input that would
/// let the sweep delete a generation `worktree_projection_state` still points
/// at. Shaped after `local_rag_store::HousekeepingError`, which splits its own
/// open failure from its query failure for the same reason.
#[derive(Debug)]
#[non_exhaustive]
pub enum GcError {
    /// Opening the read-only state connection failed; nothing was swept.
    Open(OpenError),
    /// The pin set could not be read; nothing was swept.
    Pins(rusqlite::Error),
    /// The sweep itself failed. Earlier batches stand; re-running resumes.
    Sweep(SweepError),
}

impl std::fmt::Display for GcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GcError::Open(e) => write!(f, "could not open the state store: {e}"),
            GcError::Pins(e) => write!(f, "could not read the sweep pin set: {e}"),
            GcError::Sweep(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for GcError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            GcError::Open(e) => Some(e),
            GcError::Pins(e) => Some(e),
            GcError::Sweep(e) => Some(e),
        }
    }
}

/// The generations no sweep may collect, whatever their state (spec 06 §5's
/// `ExternalPins`).
///
/// Reads `worktree_projection_state`'s three generation columns store-wide —
/// see this module's docs for why that is the complete external pin set as
/// built, and `local_rag_store::referenced_generation_ids` for why all three
/// columns rather than just `active`.
pub fn sweep_external_pins(state: &StateDb) -> Result<ExternalPins, GcError> {
    let conn = state.open_read().map_err(GcError::Open)?;
    Ok(ExternalPins {
        referenced_generations: referenced_generation_ids(&conn).map_err(GcError::Pins)?,
        ..ExternalPins::default()
    })
}

/// Sweep unpinned `retiring`/`failed` generations and the content they orphan.
///
/// Idempotent and resumable by construction (`run_sweep`'s own contract): each
/// batch is its own committed transaction and the sweepable set is recomputed
/// from the live database on every call, so an interrupted sweep is healed by
/// calling this again — which the next daemon start does anyway.
pub async fn run_generation_sweep(
    state: &StateDb,
    retention: &RetentionParams,
    now_ms: i64,
) -> Result<SweepReport, GcError> {
    let pins = sweep_external_pins(state)?;
    run_sweep(state, retention, &pins, now_ms)
        .await
        .map_err(GcError::Sweep)
}

/// What [`run_generation_sweep`] would delete, deleting nothing (`gc --dry-run`).
pub async fn plan_generation_sweep(
    state: &StateDb,
    retention: &RetentionParams,
    now_ms: i64,
) -> Result<SweepPlan, GcError> {
    let pins = sweep_external_pins(state)?;
    local_rag_store::plan_sweep(state, retention, &pins, now_ms)
        .await
        .map_err(GcError::Sweep)
}
