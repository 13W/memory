//! Validate-on-open orchestration and the single rebuild recovery path
//! (spec 05 §6/§7 `[FIXED]`, plus the quarantine-rotation half of §8 deferred
//! here by D-004) — T07-04.
//!
//! [`open_and_validate`] is the entry point: read the current
//! `worktree_projection_state` row, try to open the shard, run
//! [`crate::validate::validate`], and — on any divergence — repair via
//! [`rebuild`]. `state.sqlite`'s FSM only allows `Dirty → Rebuilding` (not a
//! direct `Clean/Updating/Rebuilding → Rebuilding`, spec 04 §2), so recovery is
//! **three** separate committed transactions, not two: [`mark_dirty`] (records
//! the divergence, so a crash right here still re-enters the same path on the
//! next open, spec 05 §6), [`begin_rebuild`] (mints a fresh op id, abandons any
//! in-flight switch target — rebuild always targets the **active** tuple, never
//! a stale target), and [`finish_rebuild`] (`projected := active`,
//! `status='clean'`). Unlike [`crate::switch::switch`]'s commit, no
//! generation-state transition happens here: rebuild never changes *which*
//! generation is active, only re-syncs the shard to match it.
//!
//! Full rebuild is always a destroy/quarantine-then-recreate, never a
//! desired-set diff against the existing shard (that is `switch`'s fast path,
//! spec 05 §7: "Full rebuild is the recovery default; delta is only the normal
//! fast path"). This also sidesteps the question of whether a shard's existing
//! partial content (e.g. from a crashed rebuild) is trustworthy: it never is,
//! by construction, once we have decided to rebuild.
//!
//! ## Scope (T07-04)
//!
//! Vectors are sourced through T07-03's [`crate::switch::VectorSource`] seam
//! (still not the real `embedding_cache`, T11-02): a missing vector surfaces as
//! [`RebuildError::MissingVector`] **before** any shard write in this rebuild
//! attempt, so the shard never goes `clean` with a partial expected set — the
//! row is left at `status='rebuilding'` for the next open to retry, exactly
//! like a crash would.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use local_rag_core::identity::{Uuid, UuidSource};
use local_rag_store::rusqlite::Transaction;
use local_rag_store::{
    OpenError, ProjectionStateChange, ProjectionStateError, ProjectionStatus, StateDb, WriteError,
    projection_state, write_projection_state,
};

use crate::contract::{
    Hash32, PointId, ProjectionError, ProjectionPoint, ProjectionStore, RepresentationKind,
    ShardParams,
};
use crate::expected::expected_points;
use crate::identity::{head as build_head, manifest_hash};
use crate::switch::VectorSource;
use crate::validate::{Divergence, validate};

/// Quarantined shards are kept for at most this many rebuild cycles for
/// diagnostics, then deleted (spec 05 §8 `[SPEC]`).
pub const QUARANTINE_RETENTION: usize = 2;

/// Why [`rebuild`] was invoked — decides quarantine (suspected corruption,
/// spec 05 §7's "on suspicion of backend corruption") vs. plain destroy
/// (the shard opened fine; its content just diverged).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebuildCause {
    /// The shard directory could not even be opened (spec 05 §10 F12).
    Unopenable,
    /// The shard opened, but [`crate::validate::validate`] found a divergence.
    Divergent(Divergence),
}

impl fmt::Display for RebuildCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RebuildCause::Unopenable => write!(f, "shard unopenable (suspected corruption)"),
            RebuildCause::Divergent(d) => write!(f, "divergence: {d}"),
        }
    }
}

/// The outcome of a completed [`rebuild`].
#[derive(Debug, Clone, PartialEq)]
pub struct RebuildOutcome {
    /// The op id minted for this rebuild (matches the written head).
    pub projection_op_id: Uuid,
    /// The number of points in the rebuilt shard.
    pub point_count: u64,
    /// Where the old shard was moved, if it was quarantined rather than
    /// destroyed outright.
    pub quarantined: Option<PathBuf>,
}

/// The result of [`open_and_validate`].
#[derive(Debug, Clone, PartialEq)]
pub enum OpenOutcome {
    /// No switch has ever completed for this worktree (`active_generation_id`/
    /// `active_model_space_id` are both `NULL`) — nothing to validate or serve
    /// yet; the shard is not touched.
    NoActiveTuple,
    /// Every predicate passed; the shard is trustworthy.
    Valid,
    /// A divergence was detected and repaired.
    Rebuilt(RebuildOutcome),
}

/// Why [`rebuild`] or [`open_and_validate`] failed.
#[derive(Debug)]
#[non_exhaustive]
pub enum RebuildError {
    /// A `state.sqlite` transaction failed at the infrastructure level (rolled
    /// back; the store is unchanged by that step).
    Write(WriteError),
    /// Reading `expected_points` or the current row from `state.sqlite` failed.
    Sqlite(local_rag_store::rusqlite::Error),
    /// Opening a `state.sqlite` read connection failed.
    Open(OpenError),
    /// No `worktree_projection_state` row exists for this worktree.
    UnknownWorktree,
    /// [`mark_dirty`] was rejected at the domain level.
    MarkDirty(ProjectionStateError),
    /// [`begin_rebuild`] was rejected at the domain level.
    BeginRebuild(ProjectionStateError),
    /// [`finish_rebuild`] was rejected at the domain level.
    FinishRebuild(ProjectionStateError),
    /// A `ProjectionStore`/`ShardHandle` call failed.
    Backend(ProjectionError),
    /// [`VectorSource::vector`] returned `None` for an expected point — the
    /// shard is left at `status='rebuilding'` (never `clean` with a partial
    /// expected set); the next open retries.
    MissingVector {
        /// The occurrence missing a vector.
        occurrence_id: String,
        /// Which representation was missing.
        representation_kind: RepresentationKind,
    },
    /// Moving the shard directory into quarantine (or rotating old
    /// quarantined copies) failed.
    Io(io::Error),
}

impl fmt::Display for RebuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RebuildError::Write(e) => write!(f, "rebuild: transaction failed: {e}"),
            RebuildError::Sqlite(e) => write!(f, "rebuild: state.sqlite read failed: {e}"),
            RebuildError::Open(e) => write!(f, "rebuild: could not open a read connection: {e}"),
            RebuildError::UnknownWorktree => {
                write!(f, "rebuild: no projection state for this worktree")
            }
            RebuildError::MarkDirty(e) => write!(f, "rebuild: mark-dirty rejected: {e}"),
            RebuildError::BeginRebuild(e) => write!(f, "rebuild: begin-rebuild rejected: {e}"),
            RebuildError::FinishRebuild(e) => write!(f, "rebuild: finish-rebuild rejected: {e}"),
            RebuildError::Backend(e) => write!(f, "rebuild: backend error: {e}"),
            RebuildError::MissingVector {
                occurrence_id,
                representation_kind,
            } => write!(
                f,
                "rebuild: no vector for occurrence {occurrence_id} representation {}",
                representation_kind.as_str()
            ),
            RebuildError::Io(e) => write!(f, "rebuild: quarantine I/O failed: {e}"),
        }
    }
}

impl std::error::Error for RebuildError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RebuildError::Write(e) => Some(e),
            RebuildError::Sqlite(e) => Some(e),
            RebuildError::Open(e) => Some(e),
            RebuildError::UnknownWorktree => None,
            RebuildError::MarkDirty(e) => Some(e),
            RebuildError::BeginRebuild(e) => Some(e),
            RebuildError::FinishRebuild(e) => Some(e),
            RebuildError::Backend(e) => Some(e),
            RebuildError::MissingVector { .. } => None,
            RebuildError::Io(e) => Some(e),
        }
    }
}

/// Move `worktree_projection_state` to `status='dirty'`, recording `reason`
/// into `last_error`. Everything else (active/projected/target/op id) is left
/// verbatim — legal from every status (`Clean/Updating/Dirty/Rebuilding →
/// Dirty` are all in T07-02's legal-transition set), so a crash right after
/// this commits still re-enters the same recovery path on the next open.
fn mark_dirty(
    tx: &Transaction<'_>,
    worktree_id: &str,
    reason: &str,
    now_ms: i64,
) -> local_rag_store::rusqlite::Result<Result<(), ProjectionStateError>> {
    let Some(current) = projection_state(tx, worktree_id)? else {
        return Ok(Err(ProjectionStateError::UnknownWorktree));
    };
    let change = ProjectionStateChange {
        status_to: Some(ProjectionStatus::Dirty),
        active_generation_id: current.active_generation_id,
        active_model_space_id: current.active_model_space_id,
        projected_generation_id: current.projected_generation_id,
        projected_model_space_id: current.projected_model_space_id,
        target_generation_id: current.target_generation_id,
        target_model_space_id: current.target_model_space_id,
        projection_op_id: current.projection_op_id,
        last_error: Some(reason.to_string()),
    };
    write_projection_state(tx, worktree_id, &change, now_ms)
}

/// Move `worktree_projection_state` to `status='rebuilding'` with a fresh
/// `projection_op_id`, clearing any in-flight switch target — rebuild always
/// targets the **active** tuple (spec 05 §7), never resumes an interrupted
/// switch. Active/projected are left verbatim.
fn begin_rebuild(
    tx: &Transaction<'_>,
    worktree_id: &str,
    new_op_id: &str,
    now_ms: i64,
) -> local_rag_store::rusqlite::Result<Result<(), ProjectionStateError>> {
    let Some(current) = projection_state(tx, worktree_id)? else {
        return Ok(Err(ProjectionStateError::UnknownWorktree));
    };
    let change = ProjectionStateChange {
        status_to: Some(ProjectionStatus::Rebuilding),
        active_generation_id: current.active_generation_id,
        active_model_space_id: current.active_model_space_id,
        projected_generation_id: current.projected_generation_id,
        projected_model_space_id: current.projected_model_space_id,
        target_generation_id: None,
        target_model_space_id: None,
        projection_op_id: Some(new_op_id.to_string()),
        last_error: current.last_error,
    };
    write_projection_state(tx, worktree_id, &change, now_ms)
}

/// Move `worktree_projection_state` to `status='clean'`, realigning
/// `projected := active` (spec 05 §7's final tx) and clearing `last_error` (the
/// rebuild succeeded). `op_id` is re-threaded through unchanged — a `None` in
/// [`ProjectionStateChange`] clears the column, so it must be passed
/// explicitly, exactly like `switch.rs`'s `commit_switch`.
fn finish_rebuild(
    tx: &Transaction<'_>,
    worktree_id: &str,
    op_id: &str,
    now_ms: i64,
) -> local_rag_store::rusqlite::Result<Result<(), ProjectionStateError>> {
    let Some(current) = projection_state(tx, worktree_id)? else {
        return Ok(Err(ProjectionStateError::UnknownWorktree));
    };
    let change = ProjectionStateChange {
        status_to: Some(ProjectionStatus::Clean),
        active_generation_id: current.active_generation_id.clone(),
        active_model_space_id: current.active_model_space_id.clone(),
        projected_generation_id: current.active_generation_id,
        projected_model_space_id: current.active_model_space_id,
        target_generation_id: None,
        target_model_space_id: None,
        projection_op_id: Some(op_id.to_string()),
        last_error: None,
    };
    write_projection_state(tx, worktree_id, &change, now_ms)
}

/// Move `shard_dir` into `quarantine_dir/<worktree_id>-<quarantine_id>` (a
/// UUIDv7 suffix, so lexicographic sort is chronological order), then delete
/// the oldest same-worktree quarantined copies beyond [`QUARANTINE_RETENTION`]
/// (spec 05 §8).
fn quarantine_shard(
    quarantine_dir: &Path,
    worktree_id: &str,
    shard_dir: &Path,
    quarantine_id: Uuid,
) -> io::Result<PathBuf> {
    fs::create_dir_all(quarantine_dir)?;
    let dest = quarantine_dir.join(format!("{worktree_id}-{quarantine_id}"));
    fs::rename(shard_dir, &dest)?;
    rotate_quarantine(quarantine_dir, worktree_id)?;
    Ok(dest)
}

/// Delete the oldest quarantined copies of `worktree_id` beyond
/// [`QUARANTINE_RETENTION`] (spec 05 §8). Pure FS core — no `StateDb` needed,
/// mirrors `local_rag_store::housekeeping`'s style.
fn rotate_quarantine(quarantine_dir: &Path, worktree_id: &str) -> io::Result<()> {
    let prefix = format!("{worktree_id}-");
    let mut entries: Vec<PathBuf> = fs::read_dir(quarantine_dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(&prefix))
        })
        .collect();
    entries.sort();
    while entries.len() > QUARANTINE_RETENTION {
        let oldest = entries.remove(0);
        fs::remove_dir_all(&oldest)?;
    }
    Ok(())
}

/// Full rebuild of `worktree_id`'s shard to match its **active** tuple (spec 05
/// §7). Always destroys/quarantines first and recreates from scratch — never a
/// desired-set diff against the existing shard (that is `switch`'s fast path).
#[allow(clippy::too_many_arguments)]
async fn rebuild(
    db: &StateDb,
    store: &dyn ProjectionStore,
    shard_dir: &Path,
    quarantine_dir: &Path,
    shard_params: ShardParams,
    worktree_id: Uuid,
    active_generation_id: Uuid,
    active_model_space_id: Uuid,
    cause: RebuildCause,
    vectors: &(dyn VectorSource + Send + Sync),
    uuids: &(dyn UuidSource + Send + Sync),
    now_ms: i64,
) -> Result<RebuildOutcome, RebuildError> {
    let wt = worktree_id.to_string();
    let op = uuids.next_uuid();
    let op_str = op.to_string();

    // Record the divergence and enter `dirty` before any repair (spec 05 §6: a
    // crash right here still re-enters the same path on the next open).
    let (w, reason) = (wt.clone(), cause.to_string());
    db.writer()
        .transaction(move |tx| mark_dirty(tx, &w, &reason, now_ms))
        .await
        .map_err(RebuildError::Write)?
        .map_err(RebuildError::MarkDirty)?;

    // dirty -> rebuilding: fresh op id, abandon any in-flight switch target.
    let (w, o) = (wt.clone(), op_str.clone());
    db.writer()
        .transaction(move |tx| begin_rebuild(tx, &w, &o, now_ms))
        .await
        .map_err(RebuildError::Write)?
        .map_err(RebuildError::BeginRebuild)?;

    // Destroy or quarantine the old shard (spec 05 §7).
    let quarantined = match &cause {
        RebuildCause::Unopenable => {
            let dest = quarantine_shard(quarantine_dir, &wt, shard_dir, uuids.next_uuid())
                .map_err(RebuildError::Io)?;
            Some(dest)
        }
        RebuildCause::Divergent(_) => {
            let shard = store
                .open(shard_dir, shard_params)
                .map_err(RebuildError::Backend)?;
            shard.destroy().map_err(RebuildError::Backend)?;
            None
        }
    };

    // Fresh shard + full desired-set upsert for the ACTIVE tuple — no diff
    // needed, the shard is freshly empty.
    let shard = store
        .open(shard_dir, shard_params)
        .map_err(RebuildError::Backend)?;
    let read = db.open_read().map_err(RebuildError::Open)?;
    let expected = expected_points(
        &read,
        &worktree_id,
        &active_generation_id,
        &active_model_space_id,
    )
    .map_err(RebuildError::Sqlite)?;
    drop(read);

    let mut points = Vec::with_capacity(expected.len());
    for p in &expected {
        let vector = vectors
            .vector(&p.occurrence_id, p.representation_kind)
            .ok_or_else(|| RebuildError::MissingVector {
                occurrence_id: p.occurrence_id.clone(),
                representation_kind: p.representation_kind,
            })?;
        points.push(ProjectionPoint {
            point_id: p.point_id.clone(),
            vector,
        });
    }
    if !points.is_empty() {
        shard.upsert(&points).map_err(RebuildError::Backend)?;
    }
    let all_ids: Vec<PointId> = expected.iter().map(|p| p.point_id.clone()).collect();
    let point_count = all_ids.len() as u64;
    let written_head = build_head(
        worktree_id,
        active_generation_id,
        active_model_space_id,
        op,
        &all_ids,
    );
    shard
        .write_head(&written_head)
        .map_err(RebuildError::Backend)?;

    // rebuilding -> clean; projected realigned to active.
    let (w, o) = (wt, op_str);
    db.writer()
        .transaction(move |tx| finish_rebuild(tx, &w, &o, now_ms))
        .await
        .map_err(RebuildError::Write)?
        .map_err(RebuildError::FinishRebuild)?;

    Ok(RebuildOutcome {
        projection_op_id: op,
        point_count,
        quarantined,
    })
}

/// Validate-on-open (spec 05 §6): run on every shard open (daemon start, LRU
/// re-open, post-crash) before the shard may serve any search. Repairs via
/// [`rebuild`] on any divergence.
///
/// Returns [`OpenOutcome::NoActiveTuple`] without touching the shard when no
/// switch has ever completed for this worktree (bootstrap).
#[allow(clippy::too_many_arguments)]
pub async fn open_and_validate(
    db: &StateDb,
    store: &dyn ProjectionStore,
    shard_dir: &Path,
    quarantine_dir: &Path,
    shard_params: ShardParams,
    worktree_id: Uuid,
    vectors: &(dyn VectorSource + Send + Sync),
    uuids: &(dyn UuidSource + Send + Sync),
    now_ms: i64,
) -> Result<OpenOutcome, RebuildError> {
    let read = db.open_read().map_err(RebuildError::Open)?;
    let row = projection_state(&read, &worktree_id.to_string())
        .map_err(RebuildError::Sqlite)?
        .ok_or(RebuildError::UnknownWorktree)?;
    drop(read);

    let (Some(active_generation_id), Some(active_model_space_id)) = (
        row.active_generation_id.as_deref(),
        row.active_model_space_id.as_deref(),
    ) else {
        return Ok(OpenOutcome::NoActiveTuple);
    };
    // Written exclusively by `switch`/`rebuild` themselves as `Uuid::to_string()`
    // — never external input, so a parse failure here is our own corruption,
    // not a scenario to recover from gracefully.
    let active_generation_id: Uuid = active_generation_id
        .parse()
        .expect("stored active_generation_id is always a UUID minted by switch/rebuild");
    let active_model_space_id: Uuid = active_model_space_id
        .parse()
        .expect("stored active_model_space_id is always a UUID minted by switch/rebuild");

    let cause = match store.open(shard_dir, shard_params) {
        Err(_) => Some(RebuildCause::Unopenable),
        Ok(shard) => {
            let current_head = shard.read_head().map_err(RebuildError::Backend)?;
            let shard_point_count = shard.point_count().map_err(RebuildError::Backend)?;
            let computed_manifest = match &current_head {
                Some(h) => {
                    let ids: Vec<PointId> =
                        shard.point_ids().map_err(RebuildError::Backend)?.collect();
                    manifest_hash(&h.worktree_id, &h.generation_id, &h.model_space_id, &ids)
                }
                // Never inspected: `validate` returns at `HeadMissing` before
                // reaching the manifest check when `head` is `None`.
                None => Hash32::from_hex(String::new()),
            };
            drop(shard);
            validate(
                &row,
                current_head.as_ref(),
                shard_point_count,
                &computed_manifest,
            )
            .map(RebuildCause::Divergent)
        }
    };

    match cause {
        None => Ok(OpenOutcome::Valid),
        Some(cause) => {
            let outcome = rebuild(
                db,
                store,
                shard_dir,
                quarantine_dir,
                shard_params,
                worktree_id,
                active_generation_id,
                active_model_space_id,
                cause,
                vectors,
                uuids,
                now_ms,
            )
            .await?;
            Ok(OpenOutcome::Rebuilt(outcome))
        }
    }
}
