//! The desired-set write-ahead switch (spec 05 §5 `[FIXED]`) — T07-03.
//!
//! [`switch`] drives one generation-switch-or-model-space-switch (the same
//! protocol, spec 04 §8/05 §5): write-ahead (`status='updating'`, target tuple,
//! fresh `projection_op_id`, one SQLite tx) → desired-set reconcile against the
//! shard (no backend command log — just `expected \ existing` upserted,
//! `existing \ expected` deleted, head written last) → commit (`status='clean'`,
//! `active := target`, `projected := target`, plus the generation-state moves,
//! one SQLite tx). A crash/error between write-ahead and commit leaves
//! `status='updating'` — detectable, and left exactly as spec 05 §5 says,
//! because this function simply does not call commit; **retrying is calling
//! [`switch`] again** with the same target (see the function doc).
//!
//! ## Scope (T07-03)
//!
//! Four pieces of the full spec 05 §5 protocol are intentionally reduced because
//! their real machinery is owned by later, already-planned groups (not new
//! deviations — see `docs/specification/05-projection.md`'s T07-03 as-built
//! note for the full reasoning):
//!
//! - the required representation-kind set is [`crate::expected::REQUIRED_REPRESENTATION_KINDS`],
//!   a hardcoded pair, not a `model_space_representation` lookup (T11-01);
//! - vectors come from a caller-supplied [`VectorSource`], not a real
//!   `embedding_cache` (T11-02);
//! - the `∪ changed` term of spec 05 §5 step 3 is realized as empty — `ShardHandle`
//!   has no vector-read-back, and `upsert` is idempotent by id, so only
//!   `expected \ existing` is upserted;
//! - preconditions (spec 05 §5 step 0: target generation `projection_ready` /
//!   target model space `active` with full coverage) and the per-worktree WRITE
//!   lock (step 3) are the caller's responsibility — the lock hierarchy is
//!   T09-01.

use std::collections::BTreeSet;
use std::fmt;
use std::path::Path;

use local_rag_core::identity::{Uuid, UuidSource};
use local_rag_store::rusqlite::Transaction;
use local_rag_store::{
    GenerationState, GenerationTransitionError, OpenError, ProjectionStateChange,
    ProjectionStateError, ProjectionStatus, StateDb, WriteError, generation_state,
    projection_state, set_current_generation, transition_generation, write_projection_state,
};

use crate::contract::{
    PointId, ProjectionError, ProjectionHead, ProjectionPoint, ProjectionStore, RepresentationKind,
    ShardParams,
};
use crate::expected::expected_points;
use crate::identity::head;

/// Supplies the vector backing a projection point, keyed by the fields that
/// derive its [`crate::contract::PointId`] beyond the tuple itself
/// (spec 05 §5 step 1: "vectors come from `embedding_cache`").
///
/// This is the seam T11-02's real `embedding_cache`-backed implementation slots
/// into; it has deliberately no "compute/embed" method, so the switch itself can
/// never trigger re-embedding — it only ever reads what it is given, and only
/// for points not already present in the shard (see [`switch`]).
///
/// Every call site below takes `&(dyn VectorSource + Send + Sync)`, not a bare
/// `&dyn VectorSource` — the trait itself carries no supertrait bound, matching
/// [`local_rag_core::identity::UuidSource`]'s own usage-site-only tightening
/// (T05-03): `open_and_validate`'s fill (T09-02, `crates/projection::manager`)
/// holds this reference across an `.await` inside a `tokio::spawn`ed task, which
/// requires `Send`, which for a `&T` reference requires `T: Sync`.
pub trait VectorSource {
    /// The vector for `occurrence_id`'s `kind`, or `None` if it is not (yet)
    /// available. Spec 05 §5 step 0's precondition means this should not happen
    /// in normal operation; [`switch`] surfaces it as
    /// [`SwitchError::MissingVector`] rather than guessing or degrading.
    fn vector(&self, occurrence_id: &str, kind: RepresentationKind) -> Option<Vec<f32>>;
}

/// The result of a completed [`switch`].
#[derive(Debug, Clone, PartialEq)]
pub struct SwitchOutcome {
    /// The op id minted for this switch (matches the written [`ProjectionHead`]).
    pub projection_op_id: Uuid,
    /// Points upserted (missing from the shard before this switch).
    pub upserted: usize,
    /// Points deleted (present in the shard but not in the expected set).
    pub deleted: usize,
    /// The head written to the shard.
    pub head: ProjectionHead,
}

/// Why [`commit_switch`] rejected a commit — always **before** any write in that
/// transaction (see its doc for the pre-flight ordering).
#[derive(Debug)]
#[non_exhaustive]
pub enum SwitchCommitError {
    /// The target (or a to-be-retired) generation id has no `generation` row.
    UnknownGeneration(String),
    /// A generation-state move (`ProjectionReady → Active` or `Active →
    /// Retiring`) was illegal (spec 04 §1).
    Generation(GenerationTransitionError),
    /// The projection-state commit itself was rejected (spec 04 §2).
    ProjectionState(ProjectionStateError),
}

impl fmt::Display for SwitchCommitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SwitchCommitError::UnknownGeneration(id) => write!(f, "unknown generation {id}"),
            SwitchCommitError::Generation(e) => write!(f, "{e}"),
            SwitchCommitError::ProjectionState(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SwitchCommitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SwitchCommitError::UnknownGeneration(_) => None,
            SwitchCommitError::Generation(e) => Some(e),
            SwitchCommitError::ProjectionState(e) => Some(e),
        }
    }
}

/// Why a [`switch`] failed.
#[derive(Debug)]
#[non_exhaustive]
pub enum SwitchError {
    /// A `state.sqlite` transaction (write-ahead or commit) failed at the
    /// infrastructure level (rolled back; the store is unchanged by that step).
    Write(WriteError),
    /// Reading `expected_points` from `state.sqlite` failed.
    Sqlite(local_rag_store::rusqlite::Error),
    /// Opening a `state.sqlite` read connection failed.
    Open(OpenError),
    /// The write-ahead was rejected at the domain level (spec 04 §2) — nothing
    /// was written; the shard was never touched.
    WriteAhead(ProjectionStateError),
    /// A `ProjectionStore`/`ShardHandle` call failed. `state.sqlite` is left
    /// exactly as the write-ahead left it (`status='updating'`) — detectable,
    /// per spec 05 §5's "crash between 2-4" note.
    Backend(ProjectionError),
    /// [`VectorSource::vector`] returned `None` for an expected point — spec 05
    /// §5 step 0's coverage precondition was not actually met.
    MissingVector {
        /// The occurrence missing a vector.
        occurrence_id: String,
        /// Which representation was missing.
        representation_kind: RepresentationKind,
    },
    /// The commit was rejected at the domain level (spec 04 §1/§2) — nothing was
    /// written; the shard already reflects the target tuple (a later switch or
    /// rebuild will reconcile the state-row divergence).
    Commit(SwitchCommitError),
    /// A named pre-commit failpoint fired (test-only, `failpoints` feature):
    /// spec 05 §10 F4's kill point, between the shard write landing and the
    /// final SQLite commit. Never present in a release build.
    #[cfg(feature = "failpoints")]
    Failpoint(&'static str),
}

impl fmt::Display for SwitchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SwitchError::Write(e) => write!(f, "projection switch: transaction failed: {e}"),
            SwitchError::Sqlite(e) => write!(f, "projection switch: state.sqlite read failed: {e}"),
            SwitchError::Open(e) => write!(
                f,
                "projection switch: could not open a read connection: {e}"
            ),
            SwitchError::WriteAhead(e) => write!(f, "projection switch: write-ahead rejected: {e}"),
            SwitchError::Backend(e) => write!(f, "projection switch: backend error: {e}"),
            SwitchError::MissingVector {
                occurrence_id,
                representation_kind,
            } => write!(
                f,
                "projection switch: no vector for occurrence {occurrence_id} representation {}",
                representation_kind.as_str()
            ),
            SwitchError::Commit(e) => write!(f, "projection switch: commit rejected: {e}"),
            #[cfg(feature = "failpoints")]
            SwitchError::Failpoint(name) => write!(f, "projection switch: failpoint {name} fired"),
        }
    }
}

impl std::error::Error for SwitchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SwitchError::Write(e) => Some(e),
            SwitchError::Sqlite(e) => Some(e),
            SwitchError::Open(e) => Some(e),
            SwitchError::WriteAhead(e) => Some(e),
            SwitchError::Backend(e) => Some(e),
            SwitchError::MissingVector { .. } => None,
            SwitchError::Commit(e) => Some(e),
            #[cfg(feature = "failpoints")]
            SwitchError::Failpoint(_) => None,
        }
    }
}

/// Write-ahead (spec 05 §5 step 2): read the current tuple, then move to
/// `status='updating'` with the target tuple and a fresh op id, keeping
/// `active`/`projected` verbatim. Returns the prior `active_generation_id`
/// (`None` on first switch) so the commit step knows what to retire.
///
/// `Ok(Err(UnknownWorktree))` if no `worktree_projection_state` row exists (the
/// caller must have called `insert_projection_state` first); no mutation on
/// rejection (the underlying `write_projection_state` guarantees this).
fn write_ahead(
    tx: &Transaction<'_>,
    worktree_id: &str,
    target_generation_id: &str,
    target_model_space_id: &str,
    projection_op_id: &str,
    now_ms: i64,
) -> local_rag_store::rusqlite::Result<Result<Option<String>, ProjectionStateError>> {
    let Some(current) = projection_state(tx, worktree_id)? else {
        return Ok(Err(ProjectionStateError::UnknownWorktree));
    };
    let change = ProjectionStateChange {
        status_to: Some(ProjectionStatus::Updating),
        active_generation_id: current.active_generation_id.clone(),
        active_model_space_id: current.active_model_space_id.clone(),
        projected_generation_id: current.projected_generation_id.clone(),
        projected_model_space_id: current.projected_model_space_id.clone(),
        target_generation_id: Some(target_generation_id.to_string()),
        target_model_space_id: Some(target_model_space_id.to_string()),
        projection_op_id: Some(projection_op_id.to_string()),
        last_error: None,
    };
    match write_projection_state(tx, worktree_id, &change, now_ms)? {
        Ok(()) => Ok(Ok(current.active_generation_id)),
        Err(e) => Ok(Err(e)),
    }
}

/// Commit (spec 05 §5 step 4): `active := target`, `projected := target`,
/// `target := NULL`, `status='clean'`, plus the generation-state moves — all in
/// one transaction ("generation transition in same final tx").
///
/// Both generation-state moves (target `ProjectionReady|Active → Active`, and —
/// if there was a different prior active generation — `Active → Retiring`) are
/// **pre-flighted** with the pure [`GenerationState::check_transition`] before
/// any write in this transaction, so a rejection here leaves it untouched; the
/// projection-state commit is attempted next (its own internal check-then-write
/// leaves nothing written on rejection either), and only once every guard has
/// passed are the actual mutations applied.
fn commit_switch(
    tx: &Transaction<'_>,
    worktree_id: &str,
    target_generation_id: &str,
    target_model_space_id: &str,
    projection_op_id: &str,
    now_ms: i64,
) -> local_rag_store::rusqlite::Result<Result<Option<String>, SwitchCommitError>> {
    // Pre-flight: the target's move to Active.
    let Some(target_state) = generation_state(tx, target_generation_id)? else {
        return Ok(Err(SwitchCommitError::UnknownGeneration(
            target_generation_id.to_string(),
        )));
    };
    if let Err(illegal) = target_state.check_transition(GenerationState::Active) {
        return Ok(Err(SwitchCommitError::Generation(
            GenerationTransitionError::Illegal(illegal),
        )));
    }

    let Some(current) = projection_state(tx, worktree_id)? else {
        return Ok(Err(SwitchCommitError::ProjectionState(
            ProjectionStateError::UnknownWorktree,
        )));
    };
    let prev_generation_id = current.active_generation_id.clone();
    // The generation actually being retired by this switch — `None` on
    // bootstrap (no prior active generation) or a model-axis-only switch
    // (target == prev, so nothing retires).
    let outgoing: Option<&str> = prev_generation_id
        .as_deref()
        .filter(|prev_id| *prev_id != target_generation_id);

    // Pre-flight: the outgoing generation's move to Retiring, if applicable.
    if let Some(prev_id) = outgoing {
        let Some(prev_state) = generation_state(tx, prev_id)? else {
            return Ok(Err(SwitchCommitError::UnknownGeneration(
                prev_id.to_string(),
            )));
        };
        if let Err(illegal) = prev_state.check_transition(GenerationState::Retiring) {
            return Ok(Err(SwitchCommitError::Generation(
                GenerationTransitionError::Illegal(illegal),
            )));
        }
    }

    // The guarded projection-state commit itself.
    let commit_change = ProjectionStateChange {
        status_to: Some(ProjectionStatus::Clean),
        active_generation_id: Some(target_generation_id.to_string()),
        active_model_space_id: Some(target_model_space_id.to_string()),
        projected_generation_id: Some(target_generation_id.to_string()),
        projected_model_space_id: Some(target_model_space_id.to_string()),
        target_generation_id: None,
        target_model_space_id: None,
        projection_op_id: Some(projection_op_id.to_string()),
        last_error: None,
    };
    if let Err(e) = write_projection_state(tx, worktree_id, &commit_change, now_ms)? {
        return Ok(Err(SwitchCommitError::ProjectionState(e)));
    }

    // Both moves were pre-validated above.
    if let Err(e) = transition_generation(tx, target_generation_id, GenerationState::Active)? {
        return Ok(Err(SwitchCommitError::Generation(e)));
    }
    if let Some(prev_id) = outgoing
        && let Err(e) = transition_generation(tx, prev_id, GenerationState::Retiring)?
    {
        return Ok(Err(SwitchCommitError::Generation(e)));
    }
    set_current_generation(tx, worktree_id, target_generation_id)?;

    Ok(Ok(prev_generation_id))
}

/// Drive one write-ahead switch to `(target_generation_id, target_model_space_id)`
/// for `worktree_id` (spec 05 §5). Only one axis may actually differ from the
/// current active tuple — the two-axis guard (T07-02) enforces this at the
/// write-ahead step.
///
/// Preconditions (spec 05 §5 step 0 — target generation `projection_ready`,
/// target model space `active` with full coverage) and per-worktree
/// serialization (step 3's WRITE lock, T09-01) are the caller's responsibility;
/// this function assumes both hold and does not check them itself.
///
/// A `worktree_projection_state` row must already exist for `worktree_id`
/// (`insert_projection_state`); the shard directory is opened fresh via `store`
/// (the caller supplies it, e.g.
/// `StoreLayout::projection_shard(&worktree_id.to_string())`).
///
/// **Retry**: if a previous call failed after the write-ahead committed (backend
/// error, or the process crashed), simply call `switch` again with the same
/// target. The write-ahead's `Updating → Updating` is a legal self-transition
/// (T07-02), and the desired-set reconcile recomputes `existing :=
/// shard.point_ids()` fresh from whatever the shard actually holds, so
/// `expected \ existing` only redoes the missing part — no command-log replay.
#[allow(clippy::too_many_arguments)]
pub async fn switch(
    db: &StateDb,
    store: &dyn ProjectionStore,
    shard_dir: &Path,
    shard_params: ShardParams,
    worktree_id: Uuid,
    target_generation_id: Uuid,
    target_model_space_id: Uuid,
    vectors: &(dyn VectorSource + Send + Sync),
    uuids: &(dyn UuidSource + Send + Sync),
    now_ms: i64,
) -> Result<SwitchOutcome, SwitchError> {
    let wt = worktree_id.to_string();
    let gen_str = target_generation_id.to_string();
    let ms_str = target_model_space_id.to_string();
    let op = uuids.next_uuid();
    let op_str = op.to_string();

    // Step 2: WRITE-AHEAD, one SQLite tx, before any backend mutation.
    let (w, g, m, o) = (wt.clone(), gen_str.clone(), ms_str.clone(), op_str.clone());
    db.writer()
        .transaction(move |tx| write_ahead(tx, &w, &g, &m, &o, now_ms))
        .await
        .map_err(SwitchError::Write)?
        .map_err(SwitchError::WriteAhead)?;

    // expected_point_ids(target tuple) (spec 05 §4) — pure derivation, one read.
    let read = db.open_read().map_err(SwitchError::Open)?;
    let expected = expected_points(
        &read,
        &worktree_id,
        &target_generation_id,
        &target_model_space_id,
    )
    .map_err(SwitchError::Sqlite)?;
    drop(read);

    // Step 3: desired-set reconciliation against the shard.
    let shard = store
        .open(shard_dir, shard_params)
        .map_err(SwitchError::Backend)?;
    let existing: BTreeSet<PointId> = shard.point_ids().map_err(SwitchError::Backend)?.collect();
    let expected_ids: BTreeSet<PointId> = expected.iter().map(|p| p.point_id.clone()).collect();

    let mut to_upsert = Vec::new();
    for p in &expected {
        // Already present under the deterministic id: trusted as-is (see the
        // module doc's `∪ changed` note) — its vector is never looked up.
        if existing.contains(&p.point_id) {
            continue;
        }
        let vector = vectors
            .vector(&p.occurrence_id, p.representation_kind)
            .ok_or_else(|| SwitchError::MissingVector {
                occurrence_id: p.occurrence_id.clone(),
                representation_kind: p.representation_kind,
            })?;
        to_upsert.push(ProjectionPoint {
            point_id: p.point_id.clone(),
            vector,
        });
    }
    if !to_upsert.is_empty() {
        shard.upsert(&to_upsert).map_err(SwitchError::Backend)?;
    }

    let to_delete: Vec<PointId> = existing.difference(&expected_ids).cloned().collect();
    if !to_delete.is_empty() {
        shard.delete(&to_delete).map_err(SwitchError::Backend)?;
    }

    let all_ids: Vec<PointId> = expected.iter().map(|p| p.point_id.clone()).collect();
    let written_head = head(
        worktree_id,
        target_generation_id,
        target_model_space_id,
        op,
        &all_ids,
    );
    shard
        .write_head(&written_head)
        .map_err(SwitchError::Backend)?;

    // Step 4: COMMIT, one SQLite tx, after the backend; generation transition in
    // the same tx.
    //
    // Named seam for spec 05 §10 F4 ("kill after write_head, before SQLite
    // commit"): the shard already reflects the target tuple in full; only the
    // final tx is prevented from running, so `state.sqlite` is left exactly as
    // the write-ahead set it (`status='updating'`) while the head is already
    // ahead of it (ADR: T07-05).
    #[cfg(feature = "failpoints")]
    local_rag_test_support::fail_point!(
        "projection.switch.before_commit",
        Err(SwitchError::Failpoint("projection.switch.before_commit"))
    );

    db.writer()
        .transaction(move |tx| commit_switch(tx, &wt, &gen_str, &ms_str, &op_str, now_ms))
        .await
        .map_err(SwitchError::Write)?
        .map_err(SwitchError::Commit)?;

    Ok(SwitchOutcome {
        projection_op_id: op,
        upserted: to_upsert.len(),
        deleted: to_delete.len(),
        head: written_head,
    })
}
