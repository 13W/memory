//! The store lock (spec 02 §2, §4.1) — L0 of the lock hierarchy (spec 02 §5).
//!
//! `store.lock` is a `flock`'d JSON file: `{instance_uuid, pid,
//! daemon_version, started_at}` (spec 02 §2), extended here with the
//! readiness marker step 4 of the startup sequence requires ("write
//! readiness marker into store.lock JSON").
//!
//! # Why the recovery algorithm has two independent branches
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

use local_rag_core::paths::{PathError, StoreLayout, ensure_file_0600};
use local_rag_store::lock::{LockLevel, checked_scope_sync};
use serde::{Deserialize, Serialize};

use super::probe::{LivenessOutcome, LivenessProbe};

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
    /// unlink `store.lock`, then drop the handle (releasing the OS `flock`).
    /// Prefer this at the end of an orderly shutdown sequence over a bare
    /// `drop` — both release the `flock`, but only this leaves nothing
    /// behind for the next `acquire` to treat as (harmlessly) stale.
    pub fn release(self, layout: &StoreLayout) {
        let _ = std::fs::remove_file(layout.store_lock());
        drop(self);
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

/// Acquire the store lock (spec 02 §4.1 step 1), recovering from a stale
/// prior owner exactly once.
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
/// 3. On `WouldBlock`: read the existing lock JSON (a parse failure is
///    treated identically to "stale, unknown owner" — never a hard error,
///    since a torn write from a crash is exactly the scenario this recovery
///    exists for) and run `probe` against its `(pid, instance_uuid)`.
///    - [`LivenessOutcome::Alive`] → [`StoreLockError::Locked`].
///    - [`LivenessOutcome::Stale`] → best-effort remove both `store.lock`
///      and the socket file, retry step 1 **exactly once**. A second
///      `WouldBlock` (someone else won the race) is `Locked`, not a further
///      retry.
pub fn acquire(
    layout: &StoreLayout,
    instance_uuid: &str,
    pid: u32,
    daemon_version: &str,
    now_ms: i64,
    probe: &dyn LivenessProbe,
) -> Result<StoreLockGuard, StoreLockError> {
    checked_scope_sync(LockLevel::L0, || {
        acquire_inner(layout, instance_uuid, pid, daemon_version, now_ms, probe)
    })
}

fn acquire_inner(
    layout: &StoreLayout,
    instance_uuid: &str,
    pid: u32,
    daemon_version: &str,
    now_ms: i64,
    probe: &dyn LivenessProbe,
) -> Result<StoreLockGuard, StoreLockError> {
    let path = layout.store_lock();
    ensure_file_0600(&path).map_err(StoreLockError::LockPath)?;

    match try_acquire(&path, instance_uuid, pid, daemon_version, now_ms) {
        Ok(guard) => {
            // Sole legitimate owner now: any leftover socket is provably
            // garbage from a crashed prior owner (spec 02 §4.4).
            let _ = std::fs::remove_file(layout.socket_path());
            Ok(guard)
        }
        Err(TryAcquireError::Io(e)) => Err(StoreLockError::Io(e)),
        Err(TryAcquireError::WouldBlock) => {
            let owner = read_lock_info(&path);
            let alive = owner.as_ref().is_some_and(|o| is_owner_alive(o, probe));
            if alive {
                // `owner` is `Some` whenever `alive` is true.
                return Err(StoreLockError::Locked {
                    owner: owner.expect("alive implies a parsed owner"),
                });
            }

            // Stale (dead PID, mismatched instance, unreachable socket, or an
            // unparseable lock file): reclaim and retry exactly once.
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(layout.socket_path());
            ensure_file_0600(&path).map_err(StoreLockError::LockPath)?;

            match try_acquire(&path, instance_uuid, pid, daemon_version, now_ms) {
                Ok(guard) => {
                    let _ = std::fs::remove_file(layout.socket_path());
                    Ok(guard)
                }
                Err(TryAcquireError::Io(e)) => Err(StoreLockError::Io(e)),
                Err(TryAcquireError::WouldBlock) => Err(StoreLockError::Locked {
                    owner: read_lock_info(&path).unwrap_or_else(unknown_owner),
                }),
            }
        }
    }
}

/// Whether `owner` — a lock file we could parse — is still alive.
///
/// `ready == false` gets a **different** check than `ready == true`, not the
/// full [`LivenessProbe`]: an owner mid-startup (between acquiring `store.lock`
/// at step 1 and binding the endpoint at step 4 — most commonly, an actual
/// migration in progress, spec 02 §4.1 step 2, which can legitimately take a
/// while on a large store) has genuinely bound no socket yet. Running the
/// socket half of the probe against it would always fail — indistinguishable
/// from a truly dead owner — and wrongly reclaim the lock out from under a
/// live, still-starting daemon (whose OS `flock` is never actually released;
/// the reclaiming instance would just take a fresh lock on the same *path*,
/// leaving two daemons each convinced they alone own the store). A `ready:
/// false` record is inherently recent (this instance is actively starting up
/// right now), so PID reuse in that narrow window is not the realistic threat
/// the full probe exists to guard against — trusting `pid_exists` alone here
/// is deliberate, not a shortcut. Once `ready == true`, the full probe (PID
/// **and** a matching socket greeting) applies exactly as documented on
/// [`acquire`].
fn is_owner_alive(owner: &StoreLockInfo, probe: &dyn LivenessProbe) -> bool {
    if !owner.ready {
        return local_rag_core::process::pid_exists(owner.pid);
    }
    probe.check(owner.pid, &owner.instance_uuid) == LivenessOutcome::Alive
}

/// A placeholder for the pathological case where the retry's contender also
/// left an unparseable lock file — still a real, live conflict (we did lose
/// the race), just one we cannot name precisely.
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
/// that already holds the `flock` (truncate-and-rewrite, not create/rename —
/// this file's identity, not its content, is what other processes rely on).
fn write_info(file: &mut File, info: &StoreLockInfo) -> io::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    serde_json::to_writer(&mut *file, info)?;
    file.flush()?;
    file.sync_all()
}

/// Best-effort read of an existing `store.lock`'s JSON. `None` on any I/O or
/// parse failure — [`acquire_inner`] treats that identically to "stale,
/// unknown owner".
fn read_lock_info(path: &Path) -> Option<StoreLockInfo> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}
