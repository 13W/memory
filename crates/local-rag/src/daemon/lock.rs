//! The store lock (spec 02 §2, §4.1) — L0 of the lock hierarchy (spec 02 §5).
//!
//! `store.lock` is a `flock`'d JSON file: `{instance_uuid, pid,
//! daemon_version, started_at}` (spec 02 §2), extended here with the
//! readiness marker step 4 of the startup sequence requires ("write
//! readiness marker into store.lock JSON").
//!
//! # Why the failure branch never reclaims
//!
//! `std::fs::File::lock`/`try_lock` (the same primitive
//! `crate::migrate::MigrationLock` already uses for L1) is advisory and
//! **auto-released when the holding process exits** — POSIX `flock` dies
//! with its owner. That means [`acquire`]'s "on failure" branch (spec 02
//! §4.1 step 1) is reached only when a **genuinely live** process still
//! holds the file open, or in the vanishingly narrow window where it dies a
//! few microseconds after our failed `try_lock`. A crashed prior daemon
//! cannot leave a "stale but still flock'd" lock file behind.
//!
//! That property is load-bearing, not just explanatory, and it decides the
//! whole branch (D-084): since a live holder is a *premise* of reaching it,
//! nothing found there can prove the owner dead — not an unreadable record,
//! not a mismatched pid, not a socket that will not answer. So the failure
//! branch reclaims nothing at all. It waits out a handover within a bounded
//! budget and otherwise refuses; recovery from a genuinely dead owner belongs
//! to the success branch, which simply overwrites whatever the record held.
//!
//! Getting this wrong is not a cosmetic bug: reclaiming unlinks the path the
//! live owner's `flock` is *not* attached to (the lock lives on the open file
//! description), so both instances end up believing they own the store. It
//! has now happened twice on the reporter's machine, through two different
//! doors. D-065 came through an unreadable record and was closed by refusing
//! that one case. D-084 came through the socket probe: a daemon in shutdown
//! stops accepting and unlinks its socket *first* (D-077) and releases the
//! lock *last*, so for the length of its drain it is alive, still writing,
//! and unreachable — indistinguishable from dead to any probe. Live capture,
//! `logs/daemon.2026-08-21.log`: `daemon stopping` at 11:53:47, a second
//! process logging `store lock acquired` at 11:54:02, and the first one only
//! reaching `daemon stopped` at 11:54:14. Twelve seconds of two daemons on
//! one canonical store. Refusing one door at a time is what let the second
//! one stay open, hence the rule rather than another special case.
//!
//! The store's UDS socket file has no such auto-cleanup: `run/daemon.sock`
//! left by a `SIGKILL`ed daemon is a plain filesystem entry that persists
//! until removed, and a fresh `UnixListener::bind` at the same path fails
//! until it is unlinked. This is the card's separate "stale socket" test
//! scenario, and it shows up on the **success** path of `try_lock` (we just
//! became the sole legitimate owner, so any leftover socket inode is
//! provably garbage — spec 02 §4.4's "orphan artifacts... cleaned at
//! startup"), not the failure path.

use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

use local_rag_core::paths::{PathError, StoreLayout, ensure_file_0600};
use local_rag_store::lock::{LockLevel, checked_scope_sync};
use serde::{Deserialize, Serialize};

/// The `store.lock` JSON shape (spec 02 §2), extended with the readiness
/// marker (spec 02 §4.1 step 4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreLockInfo {
    /// This daemon run's random identity (spec 02 §4.4: "Identity =
    /// `instance_uuid` (+ PID as advisory)").
    pub instance_uuid: String,
    /// The OS process id — advisory only, never sole identity (PID reuse).
    pub pid: u32,
    /// The daemon binary's version string.
    pub daemon_version: String,
    /// Wall-clock ms this instance acquired the lock.
    pub started_at: i64,
    /// Set by [`StoreLockGuard::mark_ready`] once the endpoint is bound
    /// (spec 02 §4.1 step 4).
    pub ready: bool,
    /// Wall-clock ms `ready` was set, if it has been.
    pub ready_at: Option<i64>,
    /// The bound endpoint path, if `ready`.
    pub socket_path: Option<String>,
}

impl StoreLockInfo {
    fn fresh(instance_uuid: &str, pid: u32, daemon_version: &str, now_ms: i64) -> Self {
        StoreLockInfo {
            instance_uuid: instance_uuid.to_string(),
            pid,
            daemon_version: daemon_version.to_string(),
            started_at: now_ms,
            ready: false,
            ready_at: None,
            socket_path: None,
        }
    }
}

/// An RAII guard holding the exclusive store lock (L0).
///
/// The OS `flock` releases when `file` closes — on an explicit [`release`],
/// on [`Drop`], or (safe by construction, spec 02 §4.3) on the process being
/// killed outright.
///
/// [`release`]: StoreLockGuard::release
#[derive(Debug)]
pub struct StoreLockGuard {
    file: File,
    info: StoreLockInfo,
}

impl StoreLockGuard {
    /// The lock JSON currently on record for this instance.
    pub fn info(&self) -> &StoreLockInfo {
        &self.info
    }

    /// Rewrite `store.lock`'s JSON through the **same** open, `flock`'d
    /// handle (spec 02 §4.1 step 4) — never close/reopen, which would risk a
    /// window with no lock held at all mid-swap.
    pub fn mark_ready(&mut self, now_ms: i64, socket_path: &Path) -> io::Result<()> {
        self.info.ready = true;
        self.info.ready_at = Some(now_ms);
        self.info.socket_path = Some(socket_path.display().to_string());
        write_info(&mut self.file, &self.info)
    }

    /// Release the lock (spec 02 §4.3 shutdown: "release lock"): best-effort
    /// unlink `store.lock` **if the path still names this guard's own file**,
    /// then drop the handle (releasing the OS `flock`). Prefer this at the end
    /// of an orderly shutdown sequence over a bare `drop` — both release the
    /// `flock`, but only this leaves nothing behind for the next `acquire` to
    /// treat as (harmlessly) stale.
    ///
    /// The identity check is D-084's second half. `flock` lives on the open
    /// file description, the record lives at a path, and the two can come
    /// apart: anything that unlinks and recreates `store.lock` while this
    /// guard is alive leaves us holding a lock on an inode the path no longer
    /// names. Unlinking by path then deletes a *live* successor's record, and
    /// the daemon after that finds no file at all and acquires cleanly
    /// alongside both. That is the third daemon in D-084's live capture, 57
    /// seconds after the second. With D-084's reclaim gone the divergence
    /// should be unreachable, which is exactly why the check is cheap enough
    /// to keep: D-065's lesson is that this file's identity is what other
    /// processes rely on.
    pub fn release(self, layout: &StoreLayout) {
        let path = layout.store_lock();
        if self.still_owns_path(&path) {
            let _ = std::fs::remove_file(&path);
        }
        drop(self);
    }

    /// Whether `path` still resolves to the very file this guard holds open.
    #[cfg(unix)]
    fn still_owns_path(&self, path: &Path) -> bool {
        use std::os::unix::fs::MetadataExt;
        match (self.file.metadata(), std::fs::metadata(path)) {
            (Ok(mine), Ok(named)) => mine.dev() == named.dev() && mine.ino() == named.ino(),
            // Our own handle is unreadable, or nothing is at the path: either
            // way there is nothing of ours left to unlink.
            _ => false,
        }
    }

    #[cfg(not(unix))]
    fn still_owns_path(&self, _path: &Path) -> bool {
        // No inode identity to compare against off POSIX; keep the previous
        // unconditional behaviour rather than invent a weaker check.
        true
    }
}

impl Drop for StoreLockGuard {
    fn drop(&mut self) {
        // Best-effort explicit unlock; closing the handle also releases it
        // (mirrors `crate::migrate::MigrationLock`'s own `Drop`).
        let _ = self.file.unlock();
    }
}

/// Why [`acquire`] could not establish this instance as the store's owner.
#[derive(Debug)]
#[non_exhaustive]
pub enum StoreLockError {
    /// A live daemon instance already holds the store (spec 02 §4.1/§6:
    /// `STORE_LOCKED`).
    Locked {
        /// The lock info identifying the live owner, for diagnostics.
        owner: StoreLockInfo,
    },
    /// The lock file path could not be created/verified as a private,
    /// owner-verified `0600` file.
    LockPath(PathError),
    /// An I/O error acquiring, reading, or writing the lock file.
    Io(io::Error),
}

impl std::fmt::Display for StoreLockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreLockError::Locked { owner } => write!(
                f,
                "store is locked by pid {} (instance {})",
                owner.pid, owner.instance_uuid
            ),
            StoreLockError::LockPath(e) => write!(f, "store lock path error: {e}"),
            StoreLockError::Io(e) => write!(f, "store lock i/o error: {e}"),
        }
    }
}

impl std::error::Error for StoreLockError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StoreLockError::LockPath(e) => Some(e),
            StoreLockError::Io(e) => Some(e),
            StoreLockError::Locked { .. } => None,
        }
    }
}

/// How long [`acquire`] keeps retrying a `WouldBlock` before it refuses
/// (D-084).
///
/// Not a `[SPEC]` number — spec 02 §4.1 names the mechanism, not a budget for
/// this wait. Chosen, not derived: it has to sit comfortably inside the
/// proxy's own `connect_or_spawn` budget (`local_rag_proxy::connect::
/// DEFAULT_BACKOFF.total_budget_ms`, 20 s), because that is what is waiting on
/// the far end while a spawned daemon tries to take over a store the outgoing
/// one has not finished releasing. Same precedent as
/// [`crate::daemon::probe::LIVENESS_PROBE_TIMEOUT_MS`] for an internal bounded
/// wait with no normative number behind it.
pub const LOCK_HANDOVER_BUDGET_MS: u64 = 10_000;

/// How often [`acquire`] re-tries inside [`LOCK_HANDOVER_BUDGET_MS`]. Short
/// enough that a handover is imperceptible, long enough that a genuinely busy
/// store is not polled thousands of times for nothing.
const HANDOVER_POLL_MS: u64 = 25;

/// Acquire the store lock (spec 02 §4.1 step 1), waiting out a handover from
/// an outgoing owner within a bounded budget.
///
/// Wrapped in [`checked_scope_sync`] against [`LockLevel::L0`] — this is what
/// makes `L0` a real participant in the strict lock-order check (spec 02
/// §5), the same instrumentation `crate::migrate::MigrationLock` already
/// carries for `L1`; the doc comment on `LockLevel::L0` names this task as
/// the one that would do it.
///
/// Algorithm:
/// 1. `try_lock` (non-blocking).
/// 2. On success: this instance is the sole owner. Best-effort remove any
///    orphaned socket file left by a crashed prior owner, write fresh lock
///    info, and return.
/// 3. On `WouldBlock`: **never reclaim** (D-084). Retry step 1 until
///    `handover_budget` is spent, then [`StoreLockError::Locked`], naming the
///    owner when the record can be read and [`unknown_owner`] when it cannot
///    (D-065).
///
/// Step 3 has no liveness check at all any more, and that is the point.
/// Reaching it proves a live process holds the `flock` *at this instant* —
/// POSIX releases it when its holder exits, so a dead owner cannot hold one
/// (this module's own header). Death is therefore unprovable in this branch no
/// matter what the record says, and every attempt to prove it anyway has
/// unlinked a live owner's lock file: D-065 through an unreadable record,
/// D-084 through a socket probe that a *shutting-down* owner legitimately
/// fails — it stops accepting and removes its socket first (`daemon::shutdown`
/// step 1, D-077) and releases the lock last, so for the length of its drain
/// it is alive, writing, and unreachable. Recovery from a genuinely dead owner
/// belongs to step 2, which simply overwrites whatever the record held.
///
/// The budget replaces what the reclaim was really being used for. A daemon
/// spawned during someone else's drain used to steal the store; now it waits
/// for the incumbent to finish and takes the lock legitimately, or refuses.
pub fn acquire(
    layout: &StoreLayout,
    instance_uuid: &str,
    pid: u32,
    daemon_version: &str,
    now_ms: i64,
    handover_budget: Duration,
) -> Result<StoreLockGuard, StoreLockError> {
    checked_scope_sync(LockLevel::L0, || {
        acquire_inner(
            layout,
            instance_uuid,
            pid,
            daemon_version,
            now_ms,
            handover_budget,
        )
    })
}

fn acquire_inner(
    layout: &StoreLayout,
    instance_uuid: &str,
    pid: u32,
    daemon_version: &str,
    now_ms: i64,
    handover_budget: Duration,
) -> Result<StoreLockGuard, StoreLockError> {
    let path = layout.store_lock();
    let deadline = Instant::now() + handover_budget;

    loop {
        // Inside the loop, not before it: an outgoing owner unlinks the path
        // on its way out (`StoreLockGuard::release`), so between two of our
        // attempts the file can legitimately stop existing. `ensure_file_0600`
        // is `create_new`, so re-running it costs one `EEXIST` in the common
        // case and recreates the file in that one.
        ensure_file_0600(&path).map_err(StoreLockError::LockPath)?;

        match try_acquire(&path, instance_uuid, pid, daemon_version, now_ms) {
            Ok(guard) => {
                // Sole legitimate owner now: any leftover socket is provably
                // garbage from a crashed prior owner (spec 02 §4.4).
                let _ = std::fs::remove_file(layout.socket_path());
                return Ok(guard);
            }
            // The path was unlinked between `ensure_file_0600` and the open —
            // the same handover race, one step later. Retry rather than
            // reporting an I/O failure for a file we are about to recreate.
            Err(TryAcquireError::Io(e))
                if e.kind() == io::ErrorKind::NotFound && Instant::now() < deadline => {}
            Err(TryAcquireError::Io(e)) => return Err(StoreLockError::Io(e)),
            Err(TryAcquireError::WouldBlock) => {
                if Instant::now() >= deadline {
                    // D-065: name the owner when the record can be read, and
                    // stand in for it when it cannot — never treat an
                    // unreadable record as an absent one.
                    return Err(StoreLockError::Locked {
                        owner: read_lock_info_settling(&path).unwrap_or_else(unknown_owner),
                    });
                }
            }
        }

        std::thread::sleep(Duration::from_millis(HANDOVER_POLL_MS));
    }
}

/// A placeholder for a live conflict we cannot name: the holder's record was
/// still unreadable after [`read_lock_info_settling`] gave up (D-065). We did
/// lose the race either way — this stands in for the owner in the resulting
/// [`StoreLockError::Locked`], never for an owner we believe to be dead.
///
/// `pid: 0` is the sentinel for "unnamed": no daemon ever runs as pid 0, and
/// `main`'s startup message keys off it to drop the parenthetical rather than
/// print a pid that means nothing. `ready: false` is honest for the same
/// reason the record was unreadable in the first place — the owner is still
/// starting up — and lands the caller on spec 02 §6's `MIGRATION_IN_PROGRESS`,
/// whose "retry shortly" is exactly the right advice here.
fn unknown_owner() -> StoreLockInfo {
    StoreLockInfo {
        instance_uuid: "<unknown>".to_string(),
        pid: 0,
        daemon_version: "<unknown>".to_string(),
        started_at: 0,
        ready: false,
        ready_at: None,
        socket_path: None,
    }
}

enum TryAcquireError {
    WouldBlock,
    Io(io::Error),
}

fn try_acquire(
    path: &Path,
    instance_uuid: &str,
    pid: u32,
    daemon_version: &str,
    now_ms: i64,
) -> Result<StoreLockGuard, TryAcquireError> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(TryAcquireError::Io)?;
    match file.try_lock() {
        Ok(()) => {
            let info = StoreLockInfo::fresh(instance_uuid, pid, daemon_version, now_ms);
            write_info(&mut file, &info).map_err(TryAcquireError::Io)?;
            Ok(StoreLockGuard { file, info })
        }
        Err(TryLockError::WouldBlock) => Err(TryAcquireError::WouldBlock),
        Err(TryLockError::Error(e)) => Err(TryAcquireError::Io(e)),
    }
}

/// Overwrite the lock file's content with `info`'s JSON, through the handle
/// that already holds the `flock` (rewrite in place, not create/rename — this
/// file's identity, not its content, is what other processes rely on).
///
/// The rewrite deliberately never shrinks the file *before* writing (D-065).
/// `flock` is advisory, so readers — [`read_lock_info`], and the diagnostic
/// [`read_store_lock_file`] behind `local-rag status`/`doctor` — read the path
/// without contending for the lock and can land mid-rewrite. A leading
/// `set_len(0)` gave them a window in which the file was empty, indistinguishable
/// from a torn write. Instead: one write covering at least the previous length
/// (short records padded with trailing spaces, which `serde_json` accepts), then
/// truncate to the record's true length. A concurrent reader therefore sees the
/// old record, or the new one, never an empty file and never the new record's
/// head glued to the old one's tail. The padding never survives on disk.
fn write_info(file: &mut File, info: &StoreLockInfo) -> io::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    let json = serde_json::to_vec(info)?;
    let previous_len = file.metadata()?.len();
    let buf = padded_record(&json, previous_len);
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&buf)?;
    file.flush()?;
    file.set_len(json.len() as u64)?;
    file.sync_all()
}

/// `json`, padded with trailing spaces so the write covers `previous_len`
/// bytes — the pure half of [`write_info`], split out so its behaviour is
/// testable without racing a real reader against a real writer.
///
/// Trailing whitespace is valid JSON, so a reader that catches the padded
/// state still parses the record it is meant to see. When the new record is at
/// least as long as the old one there is nothing to cover and the bytes are
/// returned unchanged.
fn padded_record(json: &[u8], previous_len: u64) -> Vec<u8> {
    let mut buf = json.to_vec();
    let previous_len = usize::try_from(previous_len).unwrap_or(usize::MAX);
    if previous_len > buf.len() {
        buf.resize(previous_len, b' ');
    }
    buf
}

/// Best-effort read of an existing `store.lock`'s JSON. `None` on any I/O or
/// parse failure.
fn read_lock_info(path: &Path) -> Option<StoreLockInfo> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// How many times [`read_lock_info_settling`] reads before giving up, and how
/// long it waits between attempts.
///
/// Not `[SPEC]` numbers: spec 02 §4.1 names the *mechanism* but no budget for
/// this internal read, so these are chosen and documented as chosen — the same
/// precedent [`LIVENESS_PROBE_TIMEOUT_MS`](super::probe::LIVENESS_PROBE_TIMEOUT_MS)
/// sets for its own internal bounded wait. Together they bound the wait at 8 ms,
/// which is only ever paid on the one branch that finds an unreadable record.
const OWNER_READ_ATTEMPTS: u32 = 5;
const OWNER_READ_RETRY_MS: u64 = 2;

/// [`read_lock_info`], retried a bounded number of times before concluding the
/// record cannot be read (D-065).
///
/// Only ever called from [`acquire_inner`]'s `WouldBlock` branch, where a live
/// process provably holds the `flock`. Two things can make its record
/// unreadable there, and both are transient by construction:
///
/// - the owner acquired the lock microseconds ago and has not written its
///   record yet (the file [`ensure_file_0600`] just created is empty);
/// - the owner is rewriting the record right now ([`mark_ready`] via
///   [`write_info`]).
///
/// Either way the bytes appear almost immediately, so a handful of reads
/// spread over a few milliseconds turns "some daemon holds this store" into a
/// message that names the actual `pid`/`instance_uuid`. Returning `None` is
/// therefore rare, and is *still* a live conflict — never grounds for a
/// reclaim.
///
/// Blocking sleeps are fine here: [`acquire`] runs inside
/// `tokio::task::spawn_blocking` (see `daemon::lifecycle`), never on an async
/// executor thread.
///
/// [`mark_ready`]: StoreLockGuard::mark_ready
fn read_lock_info_settling(path: &Path) -> Option<StoreLockInfo> {
    for attempt in 0..OWNER_READ_ATTEMPTS {
        if let Some(info) = read_lock_info(path) {
            return Some(info);
        }
        if attempt + 1 < OWNER_READ_ATTEMPTS {
            std::thread::sleep(std::time::Duration::from_millis(OWNER_READ_RETRY_MS));
        }
    }
    None
}

/// The three states `store.lock` can be found in by a pure read (T16-03,
/// `local-rag doctor`'s "lock" check). Unlike [`acquire`], this never
/// contends for the `flock` and never writes anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreLockFileState {
    /// No `store.lock` file exists at all.
    Absent,
    /// A file exists, but its content is not valid [`StoreLockInfo`] JSON — a
    /// torn write from a crash, or something else entirely at the path.
    Corrupt,
    /// Successfully parsed. A live daemon holding this lock is the expected,
    /// healthy case — not itself a fault; the caller decides what to do with
    /// it (`local-rag status`'s own live socket probe, or `doctor`'s simpler
    /// `pid_exists` check).
    Parsed(StoreLockInfo),
}

/// Read-only inspection of `store.lock` — the read-side counterpart to
/// [`acquire`]'s own best-effort [`read_lock_info`], but distinguishing
/// "absent" from "corrupt" rather than collapsing both into "stale, unknown
/// owner" the way [`acquire_inner`]'s recovery path deliberately does (the
/// right choice for recovery, not for a diagnostic that wants to say which
/// one it actually found).
pub fn read_store_lock_file(layout: &StoreLayout) -> StoreLockFileState {
    match std::fs::read(layout.store_lock()) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => StoreLockFileState::Absent,
        Err(_) => StoreLockFileState::Corrupt,
        Ok(bytes) => match serde_json::from_slice::<StoreLockInfo>(&bytes) {
            Ok(info) => StoreLockFileState::Parsed(info),
            Err(_) => StoreLockFileState::Corrupt,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> StoreLockInfo {
        StoreLockInfo::fresh("instance-a", 4242, "9.9.9", 1_000)
    }

    /// The shrinking rewrite — a long `ready: true` record replaced by a short
    /// fresh one — is the case that used to leave readers an empty file
    /// (D-065). The padded buffer must cover every byte the old record
    /// occupied, and must still parse as the new record.
    #[test]
    fn a_shorter_record_is_padded_to_cover_the_previous_one() {
        let json = serde_json::to_vec(&record()).expect("serialize");
        let previous_len = json.len() as u64 + 40;

        let buf = padded_record(&json, previous_len);

        assert_eq!(buf.len() as u64, previous_len, "must cover the old record");
        assert!(buf.ends_with(b" "), "the tail must be padding: {buf:?}");
        let parsed: StoreLockInfo =
            serde_json::from_slice(&buf).expect("trailing whitespace is valid JSON");
        assert_eq!(parsed, record(), "padding must not change what is read");
    }

    /// Growing (or exactly matching) rewrites have nothing to cover, so the
    /// bytes go out unchanged — no padding is invented for its own sake.
    #[test]
    fn a_record_at_least_as_long_as_the_previous_one_is_not_padded() {
        let json = serde_json::to_vec(&record()).expect("serialize");

        assert_eq!(padded_record(&json, 0), json, "first write, nothing before");
        assert_eq!(padded_record(&json, json.len() as u64), json, "exact match");
        assert_eq!(padded_record(&json, 1), json, "previous record was shorter");
    }

    /// `read_lock_info_settling` is a bounded loop, not an unbounded wait: an
    /// absent file yields `None` rather than hanging, and does so without any
    /// dependency on how fast the machine runs.
    #[test]
    fn an_unreadable_record_settles_to_none_rather_than_waiting_forever() {
        // A path under a directory that does not exist: every read fails
        // immediately, so the loop runs its full course. Nothing is created,
        // so the test touches no filesystem state at all.
        let path = Path::new("/nonexistent-local-rag-d065/never-written.lock");
        assert_eq!(read_lock_info_settling(path), None);
    }
}
