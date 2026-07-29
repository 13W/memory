//! Filesystem housekeeping for store-managed directory trees (group 06, T06-03).
//!
//! # Scope (T06-03, split per deviation D-004)
//!
//! This module implements the **orphan shard-directory sweep** (spec 05 §8:
//! "Orphan shard directories (no worktree row): GC'd at startup sweep"): it deletes
//! every `projection/<name>` subdirectory whose `<name>` is not the `worktree_id` of
//! any row in the `worktree` table. That is the only part of the card with a
//! foundation that exists today — the per-worktree shard layout
//! ([`StoreLayout::projection_shard`](local_rag_core::paths::StoreLayout::projection_shard))
//! and the `worktree` registry.
//!
//! A worktree in state `detached` or `removing` still **has a row**, so its shard is
//! retained here; "orphan" means *no row at all*. This cleanly separates the
//! orphan sweep (implemented now) from the timed grace-destroy of a `removing`
//! worktree's shard (deferred — see below).
//!
//! ## Deferred to their owning cards (deviation D-004)
//!
//! The card's other targets are produced by subsystems that do not exist yet, so
//! they are implemented where those subsystems land, not here:
//!
//! - **quarantine rotation (≤ 2 rebuild cycles, spec 05 §8)** → group 07 / T07-04
//!   (validate-on-open + rebuild introduces quarantine) — **done**;
//! - **grace-destroy of a `removing`/`detached` worktree's shard (spec 05 §8)** →
//!   originally deferred to "group 07/09 shard lifecycle" pending a removal
//!   timestamp the `worktree` table did not have; **implemented here** by
//!   deviation D-007 (found by gate G09, where that deferral chain would
//!   otherwise have run out of owning cards) — see
//!   [`run_expired_shard_sweep`] below and migration 5
//!   (`worktree.state_changed_at`);
//! - **spool GC (sessions absent > 14 days with fully committed cursors, spec 07
//!   §6)** → group 13 / T13-05 (needed the `spool_import_cursor` table, which
//!   T13-04 added) — **implemented here** by [`run_spool_session_sweep`], the
//!   fourth sweep in this file.
//!
//! ## Added later: spool session GC (T13-05)
//!
//! [`run_spool_session_sweep`] is the odd one out among the four: it is the
//! only **async** sweep here, because deleting a session's leftover
//! `spool_import_cursor` row needs the bounded writer
//! ([`StateWriter::transaction`](crate::StateWriter::transaction)), not just a
//! read-only connection. "Absent" is read from
//! `spool_import_cursor.updated_at` rather than a filesystem mtime — T13-04's
//! importer only touches that column when it actually imports *new* bytes, so
//! a truly quiet session's cursor timestamp simply stops advancing, giving this
//! sweep a DB-only absence signal for free. "Fully committed" additionally
//! needs one filesystem fact per candidate ([`read_commit_state`]): does the
//! cursor's current segment file, if it still exists, have any bytes beyond
//! `committed_offset`, and does a next segment already exist? Deletion order
//! is deliberate: the directory goes first (best-effort, "never lose data" is
//! the higher priority), the cursor row second — a crash between the two
//! leaves a harmless orphaned row that the next pass's `read_commit_state`
//! (segment file absent ⇒ `(0, false)`) trivially re-qualifies as fully
//! committed and cleans up, whereas the reverse order would risk a resumed
//! session re-importing from scratch if its `session_id` were ever reused.
//!
//! ## Added later: unreferenced model-space directories (deviation D-011)
//!
//! T11-05 split a worktree's shard root one level deeper, per model space
//! (`projection/<worktree_id>/<model_space_id>/`, spec 05 §2). That created a
//! class of garbage neither sweep above can see — after a worktree migrates
//! A → B (spec 10 §4 steps 4–6) the worktree is alive and `active`, so its root
//! is neither orphaned nor expired, yet A's directory is dead. Gate G11 found the
//! requirement ("shard lifecycle follows registry lifecycle", spec 05 §8) had no
//! owning card, so [`run_unreferenced_space_sweep`] joins the two above here.
//!
//! # Idempotence & resume
//!
//! The sweep is a pure function of the current filesystem and the live worktree
//! set: re-running recomputes the set from the live database and finds no orphans
//! (they are gone), so an interruption between deletions is healed by simply running
//! again. Triggering it at startup and periodically is the daemon's job (group 15);
//! this module ships the idempotent, dry-run-capable sweep it will call.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::Path;

use local_rag_core::paths::StoreLayout;

use crate::memory::{CandidateState, pending_candidate_ages, transition_candidate};
use crate::observation::{all_cursors, delete_cursor, known_spool_sessions, read_cursor};
use crate::registry::{
    WorktreeState, WorktreeStateClock, all_worktree_ids, referenced_model_space_ids,
    worktree_state_clocks,
};
use crate::state::{OpenError, StateDb, WriteError};

/// How long a `detached`/`removing` worktree's shard is retained before it is
/// destroyed (spec 05 §8 `[SPEC: 7 days]`), in milliseconds.
///
/// A `[SPEC]` value, not `[FIXED]`: the section marks the duration itself as
/// tunable. It is a plain constant rather than a `config.toml` field because no
/// configuration surface for it exists (spec 02 §3.1); whichever task adds one
/// threads it through [`run_expired_shard_sweep`]'s explicit `grace_ms`
/// parameter, which exists precisely so callers — and tests with a fake clock —
/// are never forced through this default.
pub const SHARD_DESTROY_GRACE_MS: i64 = 7 * 24 * 60 * 60 * 1_000;

/// The outcome of an orphan shard-directory sweep — either the directories a real
/// sweep **removed** or those a dry run **would** remove.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShardSweepReport {
    /// Orphan shard directory names removed (or, for a dry run, that would be
    /// removed), sorted for determinism.
    pub removed: Vec<String>,
    /// Entries left in place: live worktree shards plus any non-directory or
    /// non-UTF-8 entries the sweep conservatively skips.
    pub retained: u64,
    /// Whether this was a dry run (nothing was actually deleted).
    pub dry_run: bool,
}

impl ShardSweepReport {
    /// Whether the sweep removed (or would remove) nothing.
    pub fn is_empty(&self) -> bool {
        self.removed.is_empty()
    }
}

/// A failure from [`run_orphan_shard_sweep`].
#[derive(Debug)]
#[non_exhaustive]
pub enum HousekeepingError {
    /// Opening the read-only state connection failed.
    Open(OpenError),
    /// Reading the live worktree set (or every session cursor) failed.
    Sqlite(rusqlite::Error),
    /// A filesystem operation (enumerate or remove) failed.
    Io(io::Error),
    /// A swept row's state-transition write failed (rolled back; the store is
    /// unchanged for that row — [`run_spool_session_sweep`] and
    /// [`run_candidate_expiry_sweep`] only).
    Write(WriteError),
}

impl std::fmt::Display for HousekeepingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HousekeepingError::Open(e) => {
                write!(f, "housekeeping could not open the state store: {e}")
            }
            HousekeepingError::Sqlite(e) => {
                write!(f, "housekeeping could not read the live set: {e}")
            }
            HousekeepingError::Io(e) => write!(f, "housekeeping filesystem sweep failed: {e}"),
            HousekeepingError::Write(e) => {
                write!(f, "housekeeping could not delete a cursor row: {e}")
            }
        }
    }
}

impl std::error::Error for HousekeepingError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            HousekeepingError::Open(e) => Some(e),
            HousekeepingError::Sqlite(e) => Some(e),
            HousekeepingError::Io(e) => Some(e),
            HousekeepingError::Write(e) => Some(e),
        }
    }
}

/// Remove every immediate subdirectory of `projection_dir` whose name is not in
/// `live` (spec 05 §8). Pure over its inputs (the directory and the live set), so it
/// is table-testable without a store — the codebase's "pure core + thin DB reader"
/// idiom (mirroring [`mark_pins`](crate::mark_pins)).
///
/// Conservative by construction: only **directories** are candidates (stray files /
/// symlinks are left in place), and a non-UTF-8 directory name — which can never
/// equal a `worktree_id` (always a UTF-8 UUIDv7) — is retained rather than deleted,
/// so the sweep never removes something it cannot positively identify as an orphan.
/// A missing `projection_dir` yields an empty report. `dry_run` reports what would
/// be removed without deleting anything.
pub fn sweep_orphan_shard_dirs(
    projection_dir: &Path,
    live: &BTreeSet<String>,
    dry_run: bool,
) -> io::Result<ShardSweepReport> {
    sweep_shard_dirs(projection_dir, dry_run, |name| !live.contains(name))
}

/// Remove every immediate subdirectory of `projection_dir` whose name is in
/// `doomed` — the worktrees whose grace period has elapsed (spec 05 §8,
/// D-007). The exact mirror of [`sweep_orphan_shard_dirs`] with the opposite
/// membership test: there, a name *absent* from the live set is deleted; here, a
/// name *present* in the doomed set is.
///
/// Same conservative rules apply (directories only, non-UTF-8 names retained,
/// missing directory ⇒ empty report, `dry_run` deletes nothing). Idempotent: a
/// second run finds the directories already gone and reports nothing, so a
/// worktree row that lingers in `removing` after its shard was destroyed never
/// produces repeated work.
pub fn sweep_expired_shard_dirs(
    projection_dir: &Path,
    doomed: &BTreeSet<String>,
    dry_run: bool,
) -> io::Result<ShardSweepReport> {
    sweep_shard_dirs(projection_dir, dry_run, |name| doomed.contains(name))
}

/// The shared traversal behind both sweeps: enumerate `projection_dir`'s
/// immediate subdirectories and remove those `should_remove` selects.
///
/// Factored out so the two sweeps cannot drift in their conservative handling of
/// non-directory entries, non-UTF-8 names, and a missing directory — the part
/// that is easy to get subtly wrong and identical for both.
fn sweep_shard_dirs(
    projection_dir: &Path,
    dry_run: bool,
    should_remove: impl Fn(&str) -> bool,
) -> io::Result<ShardSweepReport> {
    let mut removed = Vec::new();
    let mut retained: u64 = 0;

    let entries = match fs::read_dir(projection_dir) {
        Ok(entries) => entries,
        // No projection directory yet ⇒ nothing to sweep.
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Ok(ShardSweepReport {
                removed,
                retained,
                dry_run,
            });
        }
        Err(e) => return Err(e),
    };

    for entry in entries {
        let entry = entry?;
        // Only directories are shard dirs; leave stray files/symlinks untouched.
        if !entry.file_type()?.is_dir() {
            retained += 1;
            continue;
        }
        let name = entry.file_name();
        // A worktree_id is always valid UTF-8, so a non-UTF-8 name is never live —
        // retain it rather than delete an unrecognized directory.
        let Some(name) = name.to_str() else {
            retained += 1;
            continue;
        };
        if !should_remove(name) {
            retained += 1;
            continue;
        }
        // Selected for removal: an orphan, or a worktree past its grace
        // period (spec 05 §8).
        removed.push(name.to_string());
        if !dry_run {
            fs::remove_dir_all(entry.path())?;
        }
    }

    removed.sort();
    Ok(ShardSweepReport {
        removed,
        retained,
        dry_run,
    })
}

/// Sweep orphan shard directories under `layout`'s `projection/` against the live
/// worktree set read from `db` (spec 05 §8, T06-03).
///
/// Reads the complete `worktree` set through a read-only connection, then delegates
/// to [`sweep_orphan_shard_dirs`]. Idempotent and safe to run at startup and
/// periodically; `dry_run` reports without deleting.
pub fn run_orphan_shard_sweep(
    db: &StateDb,
    layout: &StoreLayout,
    dry_run: bool,
) -> Result<ShardSweepReport, HousekeepingError> {
    let conn = db.open_read().map_err(HousekeepingError::Open)?;
    let live: BTreeSet<String> = all_worktree_ids(&conn)
        .map_err(HousekeepingError::Sqlite)?
        .into_iter()
        .collect();
    sweep_orphan_shard_dirs(&layout.projection_dir(), &live, dry_run).map_err(HousekeepingError::Io)
}

/// Remove every `projection/<worktree_id>/<model_space_id>` directory whose
/// space is no longer referenced by that worktree's projection state (spec 05
/// §8, D-011).
///
/// `referenced` maps a `worktree_id` to the model space ids its
/// `worktree_projection_state` row names in **any** column
/// ([`referenced_model_space_ids`](crate::referenced_model_space_ids)) — spec 04
/// §3's own liveness phrase. A space directory outside that set is the shard of a
/// model space the worktree has migrated off (spec 10 §4 step 6): dead weight
/// that no other sweep reclaims, because the worktree itself is alive and the
/// root directory is therefore neither orphaned ([`sweep_orphan_shard_dirs`]) nor
/// expired ([`sweep_expired_shard_dirs`]).
///
/// Conservative in the two ways that matter, both mirroring the sweeps above:
///
/// - a worktree root **absent from `referenced`** (no projection state row at
///   all) is skipped wholesale rather than emptied — a worktree mid-bootstrap has
///   nothing to project, and a root with no worktree row is the orphan sweep's
///   business, not this one's;
/// - only directories are candidates, and non-UTF-8 names — which can never equal
///   a `model_space_id` — are retained.
///
/// Race-free against a switch in flight without any locking: spec 05 §5 commits
/// the write-ahead (which sets `target_model_space_id`) *before* the backend is
/// touched, so a target space is referenced from before its directory exists.
///
/// `removed` entries are `"<worktree_id>/<model_space_id>"` — the sweep works one
/// level deeper than its two siblings, so a bare name would be ambiguous.
pub fn sweep_unreferenced_space_dirs(
    projection_dir: &Path,
    referenced: &BTreeMap<String, BTreeSet<String>>,
    dry_run: bool,
) -> io::Result<ShardSweepReport> {
    let mut removed = Vec::new();
    let mut retained: u64 = 0;

    let roots = match fs::read_dir(projection_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Ok(ShardSweepReport {
                removed,
                retained,
                dry_run,
            });
        }
        Err(e) => return Err(e),
    };

    for root in roots {
        let root = root?;
        if !root.file_type()?.is_dir() {
            retained += 1;
            continue;
        }
        let root_name = root.file_name();
        let Some(root_name) = root_name.to_str() else {
            retained += 1;
            continue;
        };
        // No projection state row ⇒ this sweep has no opinion about the root's
        // contents (see the conservative rules above).
        let Some(live) = referenced.get(root_name) else {
            retained += 1;
            continue;
        };

        for space in fs::read_dir(root.path())? {
            let space = space?;
            if !space.file_type()?.is_dir() {
                retained += 1;
                continue;
            }
            let space_name = space.file_name();
            let Some(space_name) = space_name.to_str() else {
                retained += 1;
                continue;
            };
            if live.contains(space_name) {
                retained += 1;
                continue;
            }
            removed.push(format!("{root_name}/{space_name}"));
            if !dry_run {
                fs::remove_dir_all(space.path())?;
            }
        }
    }

    removed.sort();
    Ok(ShardSweepReport {
        removed,
        retained,
        dry_run,
    })
}

/// Whether `clock`'s worktree has been out of service long enough for its shard
/// to be destroyed (spec 05 §8: "remove/detach: grace period `[SPEC: 7 days]`,
/// then destroy"), D-007.
///
/// Pure — the caller supplies both the clock reading and the budget, so this is
/// table-testable and no fake-clock plumbing reaches the store.
///
/// Two decisions worth stating, because the spec sentence is terse:
///
/// - **`detached` counts, not just `removing`.** The section says
///   "remove/**detach**", and spec 04 §7 makes `detached` a state a worktree can
///   sit in indefinitely (its path stopped resolving). Reattaching via
///   `repo attach` transitions it back to `active`, which restamps
///   `state_changed_at` and so *resets* the budget — the shard of a worktree
///   that comes back before the deadline is never destroyed.
/// - **The comparison is `>=`, not `>`.** A grace period of exactly zero must
///   mean "destroy now" rather than "never", which is what a caller passing
///   `grace_ms = 0` (a forced sweep) would reasonably expect.
///
/// A clock in the future (clock skew, or a caller passing a `now_ms` older than
/// the stored stamp) yields a negative age and is therefore never due — the
/// safe direction for a destructive sweep.
pub fn shard_destroy_due(clock: &WorktreeStateClock, now_ms: i64, grace_ms: i64) -> bool {
    match clock.state {
        WorktreeState::Active => false,
        WorktreeState::Detached | WorktreeState::Removing => {
            now_ms.saturating_sub(clock.state_changed_at) >= grace_ms
        }
    }
}

/// The ids of every worktree whose shard is past its grace period (spec 05 §8),
/// D-007. A thin [`shard_destroy_due`] filter over the store-wide clock
/// reading, kept separate so the selection can be asserted independently of any
/// filesystem effect.
pub fn expired_shard_ids(
    clocks: &[WorktreeStateClock],
    now_ms: i64,
    grace_ms: i64,
) -> BTreeSet<String> {
    clocks
        .iter()
        .filter(|clock| shard_destroy_due(clock, now_ms, grace_ms))
        .map(|clock| clock.worktree_id.clone())
        .collect()
}

/// Destroy the shard directory of every worktree whose `detached`/`removing`
/// grace period has elapsed (spec 05 §8, D-007).
///
/// Reads `(state, state_changed_at)` for every worktree through a read-only
/// connection, selects the expired ones with [`expired_shard_ids`], and deletes
/// their `projection/<worktree_id>` directories. Idempotent and safe to run at
/// startup and periodically alongside [`run_orphan_shard_sweep`]; `dry_run`
/// reports without deleting. Pass [`SHARD_DESTROY_GRACE_MS`] for the spec
/// default.
///
/// **Scope boundary:** this destroys the *shard*, which is all spec 05 §8
/// ("Shard lifecycle follows registry lifecycle") governs. Deleting the
/// `worktree` row itself — spec 04 §7's "deleted after shard/spool/GC cleanup"
/// — additionally requires spool cleanup (group 13) and the registry-side
/// cascade, and stays with those tasks; a row that lingers in `removing` after
/// its shard is gone simply makes subsequent sweeps no-ops. Likewise, evicting a
/// still-open [`ShardHandle`] for a destroyed shard is the shard manager's
/// `remove()` (T09-02), which the daemon wires to this sweep in group 15 — the
/// same wiring deferral [`run_orphan_shard_sweep`] already carries.
pub fn run_expired_shard_sweep(
    db: &StateDb,
    layout: &StoreLayout,
    now_ms: i64,
    grace_ms: i64,
    dry_run: bool,
) -> Result<ShardSweepReport, HousekeepingError> {
    let conn = db.open_read().map_err(HousekeepingError::Open)?;
    let clocks = worktree_state_clocks(&conn).map_err(HousekeepingError::Sqlite)?;
    let doomed = expired_shard_ids(&clocks, now_ms, grace_ms);
    sweep_expired_shard_dirs(&layout.projection_dir(), &doomed, dry_run)
        .map_err(HousekeepingError::Io)
}

/// Destroy every per-model-space shard directory a live worktree no longer
/// references (spec 05 §8 "shard lifecycle follows registry lifecycle", spec 10
/// §4 step 6), D-011.
///
/// Reads each worktree's referenced model space set through a read-only
/// connection and delegates to [`sweep_unreferenced_space_dirs`]. Idempotent and
/// safe to run at startup and periodically alongside the two sibling sweeps;
/// `dry_run` reports without deleting.
///
/// **Why a third sweep.** T11-05 split a worktree's shard root one level deeper,
/// per model space (`projection/<worktree_id>/<model_space_id>/`, spec 05 §2),
/// which is what makes 10 §4's `[FIXED]` "different dimensions ⇒ never in place"
/// and "until step 4 commits … that worktree still runs A entirely" mechanical.
/// The cost is a new class of garbage the two existing sweeps cannot see: after a
/// worktree migrates A → B, both are on disk, the worktree is alive, and its root
/// is neither orphaned nor expired. On the *generation* axis the equivalent stale
/// data is reclaimed inside the switch itself (step 3's
/// `delete(existing \ expected)` runs against the same directory); the model axis
/// has no such step by construction, so the reclamation is here.
///
/// **Scope boundary:** this removes shard *directories*. The `model_space` row
/// itself may be deleted only under spec 04 §3's stricter store-wide rule (no row
/// references it in any column **and** no `embedding_cache` pins remain); no code
/// deletes model space rows today, and whichever task adds one owns that
/// precondition. Evicting a still-open handle for a destroyed directory is
/// `ShardManager::remove` (T09-02), wired by the daemon in group 15 — the same
/// wiring deferral the two sibling sweeps already carry.
pub fn run_unreferenced_space_sweep(
    db: &StateDb,
    layout: &StoreLayout,
    dry_run: bool,
) -> Result<ShardSweepReport, HousekeepingError> {
    let conn = db.open_read().map_err(HousekeepingError::Open)?;
    let referenced = referenced_model_space_ids(&conn).map_err(HousekeepingError::Sqlite)?;
    sweep_unreferenced_space_dirs(&layout.projection_dir(), &referenced, dry_run)
        .map_err(HousekeepingError::Io)
}

/// How long a session may go without a **new** import before it is eligible
/// for spool GC (spec 07 §6 `[SPEC: 14 days]`), in milliseconds. See
/// [`session_gc_due`].
pub const SPOOL_SESSION_ABSENCE_MS: i64 = 14 * 24 * 60 * 60 * 1_000;

/// The outcome of a spool session GC sweep (spec 07 §6, T13-05) — the session
/// directories (and their `spool_import_cursor` rows) a real sweep **removed**,
/// or those a dry run **would** remove.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SpoolSessionSweepReport {
    /// Session ids GC'd (or, for a dry run, that would be), sorted for
    /// determinism.
    pub removed: Vec<String>,
    /// Sessions considered but not eligible (not yet absent long enough, or
    /// not fully committed).
    pub retained: u64,
    /// Whether this was a dry run (nothing was actually deleted).
    pub dry_run: bool,
}

impl SpoolSessionSweepReport {
    /// Whether the sweep removed (or would remove) nothing.
    pub fn is_empty(&self) -> bool {
        self.removed.is_empty()
    }
}

/// Whether a session has gone without a new import for at least `absence_ms`
/// (spec 07 §6 `[SPEC: 14 days]`), given its `spool_import_cursor.updated_at`.
///
/// Pure, mirroring [`shard_destroy_due`]'s shape: `>=`, not `>` (an absence
/// exactly at the budget is due now, the same "zero grace means destroy now"
/// convention D-007 established), and a future timestamp (clock skew) yields a
/// negative age and is therefore never due — the safe direction for a
/// destructive sweep.
pub fn session_gc_due(now_ms: i64, absence_ms: i64, cursor_updated_at: i64) -> bool {
    now_ms.saturating_sub(cursor_updated_at) >= absence_ms
}

/// Whether a session's spool data is fully committed — the importer has
/// consumed every byte it can currently see — given the cursor's
/// `committed_offset`, the current on-disk length of the segment the cursor
/// points at, and whether a next segment file already exists.
///
/// Pure and table-testable, mirroring [`shard_destroy_due`]; the filesystem
/// facts it needs are gathered separately by [`read_commit_state`].
pub fn is_fully_committed(
    current_segment_len: u64,
    next_segment_exists: bool,
    committed_offset: u64,
) -> bool {
    committed_offset >= current_segment_len && !next_segment_exists
}

/// The two filesystem facts [`is_fully_committed`] needs for `session_id`'s
/// current cursor segment: its on-disk length, and whether the next segment
/// file already exists.
///
/// A missing current-segment file (its session directory was already removed
/// by a prior, interrupted sweep pass — see this module's doc) reads as
/// `(0, false)`, which [`is_fully_committed`] trivially accepts: there is
/// nothing left to commit.
fn read_commit_state(
    layout: &StoreLayout,
    session_id: &str,
    segment_seq: u32,
) -> io::Result<(u64, bool)> {
    let session_dir = layout.spool_session(session_id);
    let current_len = match fs::metadata(session_dir.join(format!("{segment_seq:06}.seg"))) {
        Ok(meta) => meta.len(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => 0,
        Err(e) => return Err(e),
    };
    let next_exists = session_dir
        .join(format!("{:06}.seg", segment_seq + 1))
        .exists();
    Ok((current_len, next_exists))
}

/// Garbage-collect every session whose spool data is both absent (spec 07 §6
/// `[SPEC: 14 days]`, [`session_gc_due`]) and fully committed
/// ([`is_fully_committed`]): delete its `spool/<session_id>/` directory, then
/// its now-orphaned `spool_import_cursor` row.
///
/// The only async sweep in this module — see this module's doc for why
/// (deleting the cursor row needs the bounded writer) and for the crash-safety
/// reasoning behind removing the directory before the row. A session with no
/// cursor row at all is never a candidate: it has never been imported, so it
/// cannot be "fully committed" by definition. Idempotent and safe to run
/// periodically alongside the three sibling sweeps; `dry_run` reports without
/// deleting anything.
pub async fn run_spool_session_sweep(
    db: &StateDb,
    layout: &StoreLayout,
    now_ms: i64,
    absence_ms: i64,
    dry_run: bool,
) -> Result<SpoolSessionSweepReport, HousekeepingError> {
    let cursors = {
        let conn = db.open_read().map_err(HousekeepingError::Open)?;
        all_cursors(&conn).map_err(HousekeepingError::Sqlite)?
    };

    let mut removed = Vec::new();
    let mut retained: u64 = 0;

    for cursor in cursors {
        if !session_gc_due(now_ms, absence_ms, cursor.updated_at) {
            retained += 1;
            continue;
        }
        let (current_len, next_exists) =
            read_commit_state(layout, &cursor.session_id, cursor.segment_seq)
                .map_err(HousekeepingError::Io)?;
        if !is_fully_committed(current_len, next_exists, cursor.committed_offset) {
            retained += 1;
            continue;
        }

        removed.push(cursor.session_id.clone());
        if !dry_run {
            let dir = layout.spool_session(&cursor.session_id);
            if dir.exists() {
                fs::remove_dir_all(&dir).map_err(HousekeepingError::Io)?;
            }
            db.writer()
                .transaction({
                    let session_id = cursor.session_id.clone();
                    move |tx| delete_cursor(tx, &session_id)
                })
                .await
                .map_err(HousekeepingError::Write)?;
        }
    }

    removed.sort();
    Ok(SpoolSessionSweepReport {
        removed,
        retained,
        dry_run,
    })
}

/// Whether the store has any spool bytes not yet imported into `state.sqlite`
/// (spec 02 §4.3's idle-shutdown gate: "no unimported spool bytes") — T15-01.
///
/// Composes the same two primitives [`run_spool_session_sweep`] uses to decide
/// "fully committed" ([`is_fully_committed`], [`read_commit_state`]), but over
/// **every** session directory on disk ([`known_spool_sessions`]), not just
/// those with a cursor row: a session the importer has never touched yet has
/// no `spool_import_cursor` row at all, and defaults to `(segment_seq: 1,
/// committed_offset: 0)` — the same default
/// [`import_session_tail`](crate::observation::import_session_tail) itself
/// uses — so a brand-new, never-imported session correctly counts as pending.
/// Short-circuits on the first pending session found.
pub fn store_has_pending_spool_bytes(
    db: &StateDb,
    layout: &StoreLayout,
) -> Result<bool, HousekeepingError> {
    let sessions = known_spool_sessions(layout).map_err(HousekeepingError::Io)?;
    if sessions.is_empty() {
        return Ok(false);
    }
    let conn = db.open_read().map_err(HousekeepingError::Open)?;
    for session_id in &sessions {
        let (segment_seq, committed_offset) = read_cursor(&conn, session_id)
            .map_err(HousekeepingError::Sqlite)?
            .unwrap_or((1, 0));
        let (current_len, next_exists) =
            read_commit_state(layout, session_id, segment_seq).map_err(HousekeepingError::Io)?;
        if !is_fully_committed(current_len, next_exists, committed_offset) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// How long a `pending_memory_candidate` may go without a review action
/// before it is eligible for expiry (spec 04 §6 `[SPEC: 30 days]`), in
/// milliseconds. See [`candidate_expiry_due`].
pub const CANDIDATE_EXPIRY_MS: i64 = 30 * 24 * 60 * 60 * 1_000;

/// The outcome of a candidate expiry sweep (spec 04 §6, T14-05) — the
/// candidate ids a real sweep transitioned to `expired` (or those a dry run
/// **would** transition).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CandidateExpirySweepReport {
    /// `pending_memory_candidate.candidate_id`s transitioned to `expired` (or,
    /// for a dry run, that would be), sorted for determinism.
    pub expired: Vec<String>,
    /// Still-`pending` candidates left in place (not yet past `expiry_ms`).
    pub retained: u64,
    /// Whether this was a dry run (nothing was actually transitioned).
    pub dry_run: bool,
}

impl CandidateExpirySweepReport {
    /// Whether the sweep expired (or would expire) nothing.
    pub fn is_empty(&self) -> bool {
        self.expired.is_empty()
    }
}

/// Whether a `pending` candidate has gone `expiry_ms` since `created_at`
/// without a review action (spec 04 §6 `[SPEC: 30 days]`). Pure, mirroring
/// [`session_gc_due`]'s shape exactly — there is no separate "last touched"
/// column on `pending_memory_candidate` (spec 03 §2.5), so `created_at` is
/// the only staleness signal a `pending` row carries.
pub fn candidate_expiry_due(now_ms: i64, expiry_ms: i64, created_at: i64) -> bool {
    now_ms.saturating_sub(created_at) >= expiry_ms
}

/// Transition every `pending` candidate older than `expiry_ms` to `expired`
/// (spec 04 §6). Async and DB-only, mirroring [`run_spool_session_sweep`]'s
/// shape minus its filesystem component — candidate expiry never touches
/// `StoreLayout`.
///
/// A candidate a concurrent [`crate::memory::approve_candidate`] or
/// [`crate::memory::reject_candidate`] moved out of `pending` between this
/// sweep's read pass and its own write attempt is retained, not treated as a
/// sweep failure: [`crate::memory::transition_candidate`]'s domain rejection
/// for that row is swallowed, matching the read-then-write race every other
/// sweep in this module already accepts.
pub async fn run_candidate_expiry_sweep(
    db: &StateDb,
    now_ms: i64,
    expiry_ms: i64,
    dry_run: bool,
) -> Result<CandidateExpirySweepReport, HousekeepingError> {
    let candidates = {
        let conn = db.open_read().map_err(HousekeepingError::Open)?;
        pending_candidate_ages(&conn).map_err(HousekeepingError::Sqlite)?
    };

    let mut expired = Vec::new();
    let mut retained: u64 = 0;

    for (candidate_id, created_at) in candidates {
        if !candidate_expiry_due(now_ms, expiry_ms, created_at) {
            retained += 1;
            continue;
        }

        if !dry_run {
            let outcome = db
                .writer()
                .transaction({
                    let candidate_id = candidate_id.clone();
                    move |tx| transition_candidate(tx, &candidate_id, CandidateState::Expired)
                })
                .await
                .map_err(HousekeepingError::Write)?;
            if outcome.is_err() {
                retained += 1;
                continue;
            }
        }
        expired.push(candidate_id);
    }

    expired.sort();
    Ok(CandidateExpirySweepReport {
        expired,
        retained,
        dry_run,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use local_rag_test_support::TempHome;

    /// A fresh `projection/`-style directory under an isolated temp home.
    fn proj_dir() -> (TempHome, std::path::PathBuf) {
        let home = TempHome::new().expect("temp home");
        let dir = home.join("projection");
        fs::create_dir_all(&dir).expect("mkdir projection");
        (home, dir)
    }

    /// Create `projection/<name>` with a file inside (a non-empty shard dir).
    fn shard(dir: &Path, name: &str) {
        let d = dir.join(name);
        fs::create_dir_all(&d).expect("mkdir shard");
        fs::write(d.join("segment.bin"), b"x").expect("write shard file");
    }

    fn live(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn orphan_removed_and_live_retained() {
        let (_home, dir) = proj_dir();
        shard(&dir, "wt-live");
        shard(&dir, "orphan");

        let report = sweep_orphan_shard_dirs(&dir, &live(&["wt-live"]), false).expect("sweep");

        assert_eq!(report.removed, vec!["orphan".to_string()]);
        assert!(dir.join("wt-live").is_dir(), "live shard retained");
        assert!(!dir.join("orphan").exists(), "orphan shard removed");
    }

    #[test]
    fn dry_run_reports_without_removing() {
        let (_home, dir) = proj_dir();
        shard(&dir, "orphan");

        let report = sweep_orphan_shard_dirs(&dir, &live(&[]), true).expect("sweep");

        assert_eq!(report.removed, vec!["orphan".to_string()]);
        assert!(report.dry_run);
        assert!(dir.join("orphan").is_dir(), "dry run must not delete");
    }

    #[test]
    fn repeated_sweep_is_idempotent() {
        let (_home, dir) = proj_dir();
        shard(&dir, "wt-live");
        shard(&dir, "orphan");
        let live = live(&["wt-live"]);

        let first = sweep_orphan_shard_dirs(&dir, &live, false).expect("first");
        assert_eq!(first.removed, vec!["orphan".to_string()]);

        let second = sweep_orphan_shard_dirs(&dir, &live, false).expect("second");
        assert!(second.is_empty(), "second sweep is a no-op: {second:?}");
        assert!(dir.join("wt-live").is_dir());
    }

    #[test]
    fn non_directory_entries_are_left_alone() {
        let (_home, dir) = proj_dir();
        fs::write(dir.join("stray.txt"), b"not a shard").expect("write stray");

        let report = sweep_orphan_shard_dirs(&dir, &live(&[]), false).expect("sweep");

        assert!(report.is_empty(), "a stray file is never an orphan shard");
        assert_eq!(report.retained, 1);
        assert!(dir.join("stray.txt").is_file(), "stray file untouched");
    }

    #[test]
    fn missing_projection_dir_is_empty() {
        let home = TempHome::new().expect("temp home");
        let missing = home.join("projection");
        let report = sweep_orphan_shard_dirs(&missing, &live(&[]), false).expect("sweep");
        assert!(report.is_empty());
        assert_eq!(report.retained, 0);
    }

    #[test]
    fn all_live_removes_nothing() {
        let (_home, dir) = proj_dir();
        shard(&dir, "wt-a");
        shard(&dir, "wt-b");

        let report = sweep_orphan_shard_dirs(&dir, &live(&["wt-a", "wt-b"]), false).expect("sweep");

        assert!(report.is_empty());
        assert_eq!(report.retained, 2);
        assert!(dir.join("wt-a").is_dir() && dir.join("wt-b").is_dir());
    }

    /// A non-UTF-8 directory name can never equal a UTF-8 `worktree_id`, so it is
    /// retained rather than deleted (the conservative branch). Best-effort: some
    /// filesystems (e.g. macOS APFS) reject non-UTF-8 names with `EILSEQ`; there the
    /// case cannot exist, so the test skips it. On filesystems that allow the name
    /// (e.g. Linux ext4, exercised in CI) it verifies the branch.
    #[cfg(unix)]
    #[test]
    fn non_utf8_dir_name_is_retained() {
        use std::os::unix::ffi::OsStrExt;

        let (_home, dir) = proj_dir();
        let bad = std::ffi::OsStr::from_bytes(&[0x66, 0x80, 0x66]); // "f\x80f", invalid UTF-8
        if fs::create_dir(dir.join(bad)).is_err() {
            // Filesystem forbids non-UTF-8 names — the branch is unreachable here.
            return;
        }

        let report = sweep_orphan_shard_dirs(&dir, &live(&[]), false).expect("sweep");

        assert!(report.is_empty(), "non-UTF-8 name is never an orphan match");
        assert_eq!(report.retained, 1);
        assert!(dir.join(bad).is_dir(), "non-UTF-8 dir retained");
    }

    // ---- D-007: grace-period shard destruction (spec 05 §8) ----

    fn clock(id: &str, state: WorktreeState, state_changed_at: i64) -> WorktreeStateClock {
        WorktreeStateClock {
            worktree_id: id.to_string(),
            state,
            state_changed_at,
        }
    }

    /// The full truth table of [`shard_destroy_due`]: state × elapsed budget.
    #[test]
    fn shard_destroy_due_truth_table() {
        let grace = 1_000i64;
        for state in [WorktreeState::Detached, WorktreeState::Removing] {
            let c = clock("wt", state, 5_000);
            assert!(
                !shard_destroy_due(&c, 5_999, grace),
                "{state:?} one ms short of the budget is not due"
            );
            assert!(
                shard_destroy_due(&c, 6_000, grace),
                "{state:?} exactly at the budget is due (>=, not >)"
            );
            assert!(
                shard_destroy_due(&c, 60_000, grace),
                "{state:?} long past the budget is due"
            );
        }
        // An active worktree is never due, no matter how old its stamp.
        let active = clock("wt", WorktreeState::Active, 0);
        assert!(!shard_destroy_due(&active, i64::MAX, grace));
    }

    /// A stamp in the future (clock skew) must never be due — the safe
    /// direction for a destructive sweep.
    #[test]
    fn a_future_stamp_is_never_due() {
        let c = clock("wt", WorktreeState::Removing, 10_000);
        assert!(!shard_destroy_due(&c, 9_000, 0));
        assert!(!shard_destroy_due(&c, 9_000, SHARD_DESTROY_GRACE_MS));
    }

    /// A zero grace budget means "destroy now", not "never".
    #[test]
    fn zero_grace_destroys_immediately() {
        let c = clock("wt", WorktreeState::Removing, 5_000);
        assert!(shard_destroy_due(&c, 5_000, 0));
    }

    #[test]
    fn expired_shard_ids_selects_only_due_worktrees() {
        let clocks = vec![
            clock("wt-active", WorktreeState::Active, 0),
            clock("wt-detached-fresh", WorktreeState::Detached, 9_000),
            clock("wt-detached-old", WorktreeState::Detached, 1_000),
            clock("wt-removing-old", WorktreeState::Removing, 0),
        ];
        let due = expired_shard_ids(&clocks, 10_000, 5_000);
        assert_eq!(
            due,
            ["wt-detached-old", "wt-removing-old"]
                .iter()
                .map(|s| s.to_string())
                .collect::<BTreeSet<_>>()
        );
    }

    #[test]
    fn spec_grace_is_seven_days() {
        assert_eq!(SHARD_DESTROY_GRACE_MS, 604_800_000);
    }

    #[test]
    fn expired_sweep_removes_only_doomed_dirs() {
        let (_home, dir) = proj_dir();
        shard(&dir, "wt-doomed");
        shard(&dir, "wt-keep");

        let doomed: BTreeSet<String> = ["wt-doomed"].iter().map(|s| s.to_string()).collect();
        let report = sweep_expired_shard_dirs(&dir, &doomed, false).expect("sweep");

        assert_eq!(report.removed, vec!["wt-doomed".to_string()]);
        assert_eq!(report.retained, 1);
        assert!(!dir.join("wt-doomed").exists());
        assert!(dir.join("wt-keep").is_dir());
    }

    #[test]
    fn expired_sweep_dry_run_reports_without_removing() {
        let (_home, dir) = proj_dir();
        shard(&dir, "wt-doomed");
        let doomed: BTreeSet<String> = ["wt-doomed"].iter().map(|s| s.to_string()).collect();

        let report = sweep_expired_shard_dirs(&dir, &doomed, true).expect("sweep");

        assert_eq!(report.removed, vec!["wt-doomed".to_string()]);
        assert!(report.dry_run);
        assert!(dir.join("wt-doomed").is_dir(), "dry run must not delete");
    }

    // ---- D-011: unreferenced per-model-space shard directories (spec 05 §8) ----

    /// Create `projection/<wt>/<space>` with a file inside.
    fn space_shard(dir: &Path, wt: &str, space: &str) {
        let d = dir.join(wt).join(space);
        fs::create_dir_all(&d).expect("mkdir space shard");
        fs::write(d.join("segment.bin"), b"x").expect("write shard file");
    }

    fn referenced(pairs: &[(&str, &[&str])]) -> BTreeMap<String, BTreeSet<String>> {
        pairs
            .iter()
            .map(|(wt, spaces)| {
                (
                    (*wt).to_string(),
                    spaces.iter().map(|s| (*s).to_string()).collect(),
                )
            })
            .collect()
    }

    /// The core case: after a worktree migrated `space-a → space-b`, the outgoing
    /// space's directory is reclaimed and the referenced one is kept.
    #[test]
    fn unreferenced_space_dir_removed_referenced_retained() {
        let (_home, dir) = proj_dir();
        space_shard(&dir, "wt", "space-a");
        space_shard(&dir, "wt", "space-b");

        let report =
            sweep_unreferenced_space_dirs(&dir, &referenced(&[("wt", &["space-b"])]), false)
                .expect("sweep");

        assert_eq!(report.removed, vec!["wt/space-a".to_string()]);
        assert!(!dir.join("wt").join("space-a").exists());
        assert!(dir.join("wt").join("space-b").is_dir());
    }

    /// Every column counts, not just `active`: a switch in flight has its target
    /// (and its outgoing `projected`) directory referenced, so a sweep racing a
    /// migration can never delete either side of the double buffer.
    #[test]
    fn a_switch_in_flight_keeps_both_buffers() {
        let (_home, dir) = proj_dir();
        space_shard(&dir, "wt", "space-a");
        space_shard(&dir, "wt", "space-b");
        space_shard(&dir, "wt", "space-dead");

        // active/projected = A (still serving), target = B (write-ahead committed).
        let report = sweep_unreferenced_space_dirs(
            &dir,
            &referenced(&[("wt", &["space-a", "space-b"])]),
            false,
        )
        .expect("sweep");

        assert_eq!(report.removed, vec!["wt/space-dead".to_string()]);
        assert!(dir.join("wt").join("space-a").is_dir(), "outgoing retained");
        assert!(dir.join("wt").join("space-b").is_dir(), "target retained");
    }

    /// A worktree root with **no projection state row** is skipped wholesale —
    /// this sweep has no opinion about it (the orphan sweep owns that case).
    /// Distinct from a row whose columns are all NULL, which does empty the root.
    #[test]
    fn a_root_without_a_row_is_skipped_but_an_empty_row_is_not() {
        let (_home, dir) = proj_dir();
        space_shard(&dir, "wt-no-row", "space-a");
        space_shard(&dir, "wt-empty-row", "space-a");

        let report =
            sweep_unreferenced_space_dirs(&dir, &referenced(&[("wt-empty-row", &[])]), false)
                .expect("sweep");

        assert_eq!(report.removed, vec!["wt-empty-row/space-a".to_string()]);
        assert!(
            dir.join("wt-no-row").join("space-a").is_dir(),
            "a root with no projection state row is left entirely alone"
        );
    }

    #[test]
    fn unreferenced_space_sweep_dry_run_and_idempotence() {
        let (_home, dir) = proj_dir();
        space_shard(&dir, "wt", "space-a");
        let live = referenced(&[("wt", &[])]);

        let dry = sweep_unreferenced_space_dirs(&dir, &live, true).expect("dry run");
        assert!(dry.dry_run);
        assert_eq!(dry.removed, vec!["wt/space-a".to_string()]);
        assert!(dir.join("wt").join("space-a").is_dir(), "dry run keeps it");

        let first = sweep_unreferenced_space_dirs(&dir, &live, false).expect("first");
        assert_eq!(first.removed, vec!["wt/space-a".to_string()]);

        let second = sweep_unreferenced_space_dirs(&dir, &live, false).expect("second");
        assert!(second.is_empty(), "second sweep is a no-op: {second:?}");
        assert!(
            dir.join("wt").is_dir(),
            "the root itself is never removed by this sweep"
        );
    }

    /// Stray files at either level are never candidates, and a missing
    /// `projection/` yields an empty report — the same conservative rules the two
    /// sibling sweeps follow.
    #[test]
    fn unreferenced_space_sweep_is_conservative() {
        let (_home, dir) = proj_dir();
        space_shard(&dir, "wt", "space-a");
        fs::write(dir.join("stray.txt"), b"x").expect("stray at root level");
        fs::write(dir.join("wt").join("stray.txt"), b"x").expect("stray inside a root");

        let report =
            sweep_unreferenced_space_dirs(&dir, &referenced(&[("wt", &["space-a"])]), false)
                .expect("sweep");
        assert!(report.is_empty(), "nothing unreferenced: {report:?}");
        assert!(dir.join("stray.txt").is_file());
        assert!(dir.join("wt").join("stray.txt").is_file());

        let home = TempHome::new().expect("temp home");
        let missing = sweep_unreferenced_space_dirs(
            &home.join("projection"),
            &referenced(&[("wt", &[])]),
            false,
        )
        .expect("sweep");
        assert!(missing.is_empty());
        assert_eq!(missing.retained, 0);
    }

    /// A `removing` row whose shard is already gone (a prior sweep, or a
    /// worktree that never had one) produces no repeated work.
    #[test]
    fn expired_sweep_is_idempotent() {
        let (_home, dir) = proj_dir();
        shard(&dir, "wt-doomed");
        let doomed: BTreeSet<String> = ["wt-doomed"].iter().map(|s| s.to_string()).collect();

        let first = sweep_expired_shard_dirs(&dir, &doomed, false).expect("first");
        assert_eq!(first.removed, vec!["wt-doomed".to_string()]);

        let second = sweep_expired_shard_dirs(&dir, &doomed, false).expect("second");
        assert!(second.is_empty(), "second sweep is a no-op: {second:?}");
    }
}
