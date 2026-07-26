//! The production model axis: per-worktree model-space migration — T11-05.
//!
//! Spec 10 §4's `[FIXED]` migration recipe has six steps; the registry
//! (T11-01), the cache (T11-02), the provider pool (T11-03) and the backfill
//! worker (T11-04) cover steps 1–3. This module is **step 4** —
//!
//! > 4. Per-worktree switch: standard write-ahead switch (05 §5) on the MODEL
//! >    axis, serialized with generation switches by the same per-worktree
//! >    writer `[FIXED]`.
//!
//! — plus spec 05 §8's `[FIXED]` companion:
//!
//! > **Dormant worktree model migration**: opening a worktree whose
//! > `active_model_space_id` is retiring/absent switches it to the default space
//! > via the standard switch protocol before serving dense search.
//!
//! # "Standard switch protocol" means literally the same function
//!
//! [`switch_model_space`] does not reimplement anything: it establishes the
//! preconditions the protocol assumes and then calls [`crate::switch::switch`]
//! with the worktree's **current** generation, so exactly one axis moves. The
//! two-axis guard (`BothAxesMovedAtOnce`, T07-02) would reject anything else, and
//! spec 04 §8 `[FIXED]` is explicit that a combined request "is executed as two
//! sequential switches".
//!
//! # Preconditions this module owns (spec 05 §5 step 0)
//!
//! `switch()` documents that step 0 is the caller's responsibility. On the model
//! axis it is two checks, both against the registry:
//!
//! * the target space is `active` — `eligible_as_target`, which is `true` for no
//!   other state (a `retiring` space is explicitly "no longer selectable as
//!   target", spec 04 §3);
//! * its stored coverage is complete for its own required kinds —
//!   `Coverage::fully_covered`, the same value `transition_model_space` gated
//!   `building → projection_ready` on. Re-checking here is not redundant: a space
//!   reaches `active` once, but the content it must cover keeps growing as new
//!   generations are indexed, and a switch on stale coverage would fail mid-flight
//!   with `MissingVector` after the write-ahead already committed.
//!
//! # No global barrier
//!
//! Nothing here is store-wide. Each call touches one worktree's row, and spec 02
//! §5's lock hierarchy has no store-wide write lock at all — which is what makes
//! spec 04 §3's `[FIXED]` "there is **no global write barrier**" structural rather
//! than a promise. Two worktrees migrate independently, in any order, possibly
//! never (a worktree that is never opened simply keeps running its old space
//! until it is).
//!
//! Serializing a worktree's own switches is `L2.write`
//! (`local_rag_store::lock::WorktreeLockRegistry`) and stays the caller's job, as
//! `switch()` documents; adopting it inside the projection crate is group 15's
//! wiring (spec 02 §5's T09-01 note).

use std::fmt;
use std::path::PathBuf;

use local_rag_core::identity::{Uuid, UuidSource};
use local_rag_core::paths::StoreLayout;
use local_rag_store::{
    Coverage, ModelSpaceState, OpenError, RepresentationKey, StateDb, default_model_space_id,
    eligible_as_target, model_space_required_kinds, model_space_required_representation_ids,
    model_space_state, projection_state, representation_key, rusqlite,
};

use crate::contract::{ProjectionStore, RepresentationKind, ShardParams};
use crate::switch::{SwitchError, SwitchOutcome, VectorSource, switch};

/// Why a model-axis migration was refused or failed.
#[derive(Debug)]
#[non_exhaustive]
pub enum ModelSwitchError {
    /// Reading `state.sqlite` failed.
    Sqlite(rusqlite::Error),
    /// Opening a `state.sqlite` read connection failed.
    Open(OpenError),
    /// No `worktree_projection_state` row exists for this worktree.
    UnknownWorktree,
    /// The worktree has no active generation yet, so there is nothing to
    /// re-project under a different model space (bootstrap is the generation
    /// axis's job, not this one).
    NoActiveGeneration,
    /// The target model space does not exist in the registry.
    UnknownModelSpace {
        /// The id that was requested.
        model_space_id: String,
    },
    /// The target model space is not `active` (spec 04 §3 / 05 §5 step 0).
    NotEligible {
        /// The id that was requested.
        model_space_id: String,
        /// The state it is actually in.
        state: ModelSpaceState,
    },
    /// The target model space's stored coverage is not complete for its own
    /// required kinds — switching would fail mid-flight with a missing vector
    /// (spec 05 §5 step 0).
    IncompleteCoverage {
        /// The id that was requested.
        model_space_id: String,
    },
    /// The target model space requires no representation this projection can
    /// size a shard from (`code_raw`'s `dimensions`).
    NoShardParams {
        /// The id that was requested.
        model_space_id: String,
    },
    /// The store has no `default_model_space_id` pointer — a bootstrap the
    /// migration seeds, so its absence means a corrupt store rather than a
    /// state this code should paper over.
    NoDefaultModelSpace,
    /// The switch protocol itself failed (see [`SwitchError`]).
    Switch(SwitchError),
}

impl fmt::Display for ModelSwitchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ModelSwitchError::Sqlite(e) => write!(f, "model switch: state.sqlite read failed: {e}"),
            ModelSwitchError::Open(e) => {
                write!(f, "model switch: could not open a read connection: {e}")
            }
            ModelSwitchError::UnknownWorktree => {
                write!(f, "model switch: no projection state for this worktree")
            }
            ModelSwitchError::NoActiveGeneration => {
                write!(f, "model switch: worktree has no active generation")
            }
            ModelSwitchError::UnknownModelSpace { model_space_id } => {
                write!(f, "model switch: unknown model space {model_space_id}")
            }
            ModelSwitchError::NotEligible {
                model_space_id,
                state,
            } => write!(
                f,
                "model switch: model space {model_space_id} is {}, only an active space is a legal target",
                state.as_str()
            ),
            ModelSwitchError::IncompleteCoverage { model_space_id } => write!(
                f,
                "model switch: model space {model_space_id} does not fully cover its required kinds"
            ),
            ModelSwitchError::NoShardParams { model_space_id } => write!(
                f,
                "model switch: model space {model_space_id} has no code_raw representation to size a shard from"
            ),
            ModelSwitchError::NoDefaultModelSpace => {
                write!(f, "model switch: store has no default_model_space_id")
            }
            ModelSwitchError::Switch(e) => write!(f, "model switch: {e}"),
        }
    }
}

impl std::error::Error for ModelSwitchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ModelSwitchError::Sqlite(e) => Some(e),
            ModelSwitchError::Open(e) => Some(e),
            ModelSwitchError::Switch(e) => Some(e),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for ModelSwitchError {
    fn from(e: rusqlite::Error) -> Self {
        ModelSwitchError::Sqlite(e)
    }
}

impl From<SwitchError> for ModelSwitchError {
    fn from(e: SwitchError) -> Self {
        ModelSwitchError::Switch(e)
    }
}

/// The shard directory a `(worktree, model_space)` pair projects into —
/// `projection/<worktree_id>/<model_space_id>/` (spec 05 §2's backend-defined
/// contents, split per space by T11-05).
pub fn shard_dir(layout: &StoreLayout, worktree_id: &Uuid, model_space_id: &Uuid) -> PathBuf {
    layout.projection_shard_space(&worktree_id.to_string(), &model_space_id.to_string())
}

/// The canonical `RepresentationKey` of `model_space_id`'s **`code_raw`**
/// representation (spec 03 §2.2) — the one representation v0's dense leg
/// searches over (spec 09 §3: "v0 ships `code_raw`"; `code_context` is `[OPEN]`,
/// decided by the benchmark).
///
/// Two callers, deliberately sharing one lookup (T12-02): [`params_for_model_space`]
/// takes `dimensions`/`distance_metric` from it to open a shard, and the search
/// pipeline's dense leg takes the whole key — `model_id` included — to embed the
/// query with the *same* model the points were embedded with. Reading them from
/// one place is what keeps "query embedding from the active model
/// representation" (spec 09 §3) true by construction rather than by convention.
pub fn code_raw_representation_key(
    conn: &rusqlite::Connection,
    model_space_id: &Uuid,
) -> Result<RepresentationKey, ModelSwitchError> {
    let id = model_space_id.to_string();
    let representations = model_space_required_representation_ids(conn, &id)?;
    let representation_id = representations
        .into_iter()
        .find(|(kind, _)| *kind == local_rag_store::RepresentationKind::CodeRaw)
        .map(|(_, id)| id)
        .ok_or_else(|| ModelSwitchError::NoShardParams {
            model_space_id: id.clone(),
        })?;
    representation_key(conn, &representation_id)?
        .ok_or(ModelSwitchError::NoShardParams { model_space_id: id })
}

/// The [`ShardParams`] a model space's shards are opened with: the `dimensions`
/// and `distance_metric` of its `code_raw` representation (spec 03 §2.2's
/// canonical `RepresentationKey`, via [`code_raw_representation_key`]).
///
/// This is where "different dimensions ⇒ separate shard layout" (spec 10 §4
/// `[FIXED]`) becomes mechanical rather than aspirational: params are a property
/// of the space, and each space owns a directory, so a 768-dimension space can
/// never be asked to write into a 256-dimension one's shard. The metric rides
/// along for the same reason — spec 09 §3's "distance per
/// `representation.distance_metric`" is a property of the space, not of the
/// caller (T12-02).
pub fn params_for_model_space(
    conn: &rusqlite::Connection,
    model_space_id: &Uuid,
) -> Result<ShardParams, ModelSwitchError> {
    let key = code_raw_representation_key(conn, model_space_id)?;
    Ok(ShardParams {
        dimensions: key.dimensions as usize,
        distance_metric: key.distance_metric,
    })
}

/// Check spec 05 §5 step 0's model-axis preconditions against the registry.
///
/// Runs before the write-ahead, so a refusal leaves `state.sqlite` and the shard
/// completely untouched.
fn check_target(
    conn: &rusqlite::Connection,
    model_space_id: &Uuid,
) -> Result<(), ModelSwitchError> {
    let id = model_space_id.to_string();
    let Some(state) = model_space_state(conn, &id)? else {
        return Err(ModelSwitchError::UnknownModelSpace { model_space_id: id });
    };
    if !eligible_as_target(state) {
        return Err(ModelSwitchError::NotEligible {
            model_space_id: id,
            state,
        });
    }

    let required = model_space_required_kinds(conn, &id)?;
    let coverage: Option<String> = conn
        .query_row(
            "SELECT coverage FROM model_space WHERE model_space_id = ?1",
            rusqlite::params![id],
            |r| r.get(0),
        )
        .ok()
        .flatten();
    let coverage = coverage
        .as_deref()
        .and_then(|json| Coverage::from_json(json).ok())
        .unwrap_or_default();
    if !coverage.fully_covered(&required) {
        return Err(ModelSwitchError::IncompleteCoverage { model_space_id: id });
    }
    Ok(())
}

/// Migrate `worktree_id` to `target_model_space_id` on the model axis
/// (spec 10 §4 step 4).
///
/// Keeps the worktree's current generation, so exactly one axis moves. The
/// caller holds `L2.write` for this worktree (spec 02 §5 `[FIXED]`; `switch()`
/// documents the same requirement) — this function does not take the lock.
///
/// Idempotent by construction: a worktree already on the target space is a no-op
/// (`Ok(None)`), and a call interrupted after the write-ahead is resumed by
/// simply calling again — the write-ahead's `Updating → Updating` self-transition
/// is legal and the desired-set reconcile recomputes what is missing.
#[allow(clippy::too_many_arguments)]
pub async fn switch_model_space(
    db: &StateDb,
    store: &dyn ProjectionStore,
    layout: &StoreLayout,
    worktree_id: Uuid,
    target_model_space_id: Uuid,
    vectors: &(dyn VectorSource + Send + Sync),
    uuids: &(dyn UuidSource + Send + Sync),
    now_ms: i64,
) -> Result<Option<SwitchOutcome>, ModelSwitchError> {
    let read = db.open_read().map_err(ModelSwitchError::Open)?;
    let row = projection_state(&read, &worktree_id.to_string())?
        .ok_or(ModelSwitchError::UnknownWorktree)?;

    let Some(active_generation_id) = row.active_generation_id.as_deref() else {
        return Err(ModelSwitchError::NoActiveGeneration);
    };
    if row.active_model_space_id.as_deref() == Some(&target_model_space_id.to_string()) {
        // Already there. Not an error: the dormant path and an explicit
        // migration converge on the same target, and either may run first.
        return Ok(None);
    }
    // Written exclusively by `switch`/`rebuild` as `Uuid::to_string()`, never
    // external input (the same reasoning `rebuild` records at its own parse).
    let generation_id: Uuid = active_generation_id
        .parse()
        .expect("stored active_generation_id is always a UUID minted by switch/rebuild");

    check_target(&read, &target_model_space_id)?;
    let params = params_for_model_space(&read, &target_model_space_id)?;
    drop(read);

    let dir = shard_dir(layout, &worktree_id, &target_model_space_id);
    let outcome = switch(
        db,
        store,
        &dir,
        params,
        worktree_id,
        generation_id,
        target_model_space_id,
        vectors,
        uuids,
        now_ms,
    )
    .await?;
    Ok(Some(outcome))
}

/// Whether `worktree_id` must migrate before its shard may serve dense search,
/// and to which space (spec 05 §8 `[FIXED]`).
///
/// Returns the default space when the worktree's active space is absent,
/// `retiring`, or `failed` — "retiring/absent" in the spec's words, with `failed`
/// folded in because a failed space is likewise never a legal target
/// (`eligible_as_target`) and leaving a worktree pointed at one would strand it.
/// A worktree already on a legal space is left alone even if a newer default
/// exists: spec 10 §4 step 4 makes moving a *healthy* worktree an explicit
/// migration, not something an open silently performs.
pub fn dormant_migration_target(
    conn: &rusqlite::Connection,
    worktree_id: &Uuid,
) -> Result<Option<Uuid>, ModelSwitchError> {
    let Some(row) = projection_state(conn, &worktree_id.to_string())? else {
        return Ok(None);
    };
    let needs_migration = match row.active_model_space_id.as_deref() {
        None => row.active_generation_id.is_some(),
        Some(id) => match model_space_state(conn, id)? {
            None => true,
            Some(state) => matches!(state, ModelSpaceState::Retiring | ModelSpaceState::Failed),
        },
    };
    if !needs_migration {
        return Ok(None);
    }

    let default = default_model_space_id(conn)?.ok_or(ModelSwitchError::NoDefaultModelSpace)?;
    if row.active_model_space_id.as_deref() == Some(default.as_str()) {
        // The default itself is the unusable one — migrating to it would be a
        // no-op, and pretending otherwise would loop on every open.
        return Ok(None);
    }
    Ok(Some(
        default
            .parse()
            .expect("stored default_model_space_id is always a UUID"),
    ))
}

/// Migrate a dormant worktree to the default model space if it needs it
/// (spec 05 §8 `[FIXED]`), returning `None` when nothing was due.
///
/// Called on the open path, before validate-on-open decides whether the shard is
/// serviceable: after this returns, the worktree's active tuple names a space
/// that is legal to serve from.
#[allow(clippy::too_many_arguments)]
pub async fn migrate_dormant_on_open(
    db: &StateDb,
    store: &dyn ProjectionStore,
    layout: &StoreLayout,
    worktree_id: Uuid,
    vectors: &(dyn VectorSource + Send + Sync),
    uuids: &(dyn UuidSource + Send + Sync),
    now_ms: i64,
) -> Result<Option<SwitchOutcome>, ModelSwitchError> {
    let read = db.open_read().map_err(ModelSwitchError::Open)?;
    let target = dormant_migration_target(&read, &worktree_id)?;
    drop(read);

    let Some(target) = target else {
        return Ok(None);
    };
    switch_model_space(
        db,
        store,
        layout,
        worktree_id,
        target,
        vectors,
        uuids,
        now_ms,
    )
    .await
}

/// The representation kinds a model space requires, in the projection's own
/// vocabulary — re-exported for callers assembling a [`VectorSource`].
pub fn required_kinds(
    conn: &rusqlite::Connection,
    model_space_id: &Uuid,
) -> Result<Vec<RepresentationKind>, ModelSwitchError> {
    Ok(
        model_space_required_kinds(conn, &model_space_id.to_string())?
            .into_iter()
            .filter_map(crate::vectors::store_kind_to_projection)
            .collect(),
    )
}
