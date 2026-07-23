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
//!   §6)** → group 13 / T13-05 (needs the `spool_import_cursor` table).
//!
//! # Idempotence & resume
//!
//! The sweep is a pure function of the current filesystem and the live worktree
//! set: re-running recomputes the set from the live database and finds no orphans
//! (they are gone), so an interruption between deletions is healed by simply running
//! again. Triggering it at startup and periodically is the daemon's job (group 15);
//! this module ships the idempotent, dry-run-capable sweep it will call.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::Path;

use local_rag_core::paths::StoreLayout;

use crate::registry::{WorktreeState, WorktreeStateClock, all_worktree_ids, worktree_state_clocks};
use crate::state::{OpenError, StateDb};

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
    /// Reading the live worktree set failed.
    Sqlite(rusqlite::Error),
    /// A filesystem operation (enumerate or remove) failed.
    Io(io::Error),
}

impl std::fmt::Display for HousekeepingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HousekeepingError::Open(e) => {
                write!(f, "housekeeping could not open the state store: {e}")
            }
            HousekeepingError::Sqlite(e) => {
                write!(f, "housekeeping could not read the worktree set: {e}")
            }
            HousekeepingError::Io(e) => write!(f, "housekeeping filesystem sweep failed: {e}"),
        }
    }
}

impl std::error::Error for HousekeepingError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            HousekeepingError::Open(e) => Some(e),
            HousekeepingError::Sqlite(e) => Some(e),
            HousekeepingError::Io(e) => Some(e),
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
