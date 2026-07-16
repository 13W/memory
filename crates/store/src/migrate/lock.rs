//! The migration lock (L1, spec 02 §5): an exclusive advisory file lock on
//! `<root>/migration.lock`, held only while migrations run and released before
//! normal operation begins.
//!
//! Uses `std::fs::File::lock` (stabilized in Rust 1.89, available at the pinned
//! toolchain / MSRV 1.96) rather than a raw `libc::flock`: it is safe (no
//! `unsafe`), needs no new dependency, and maps to `flock` on unix and
//! `LockFileEx` on Windows through one code path. The lock is advisory and
//! per-open-file-description, so two independent opens of the same path — even
//! within one process — genuinely contend.

use std::fs::{File, OpenOptions};
use std::path::Path;

use local_rag_core::paths::ensure_file_0600;

use super::MigrationError;

/// An RAII guard holding the exclusive migration lock.
///
/// The lock is released when the guard is dropped (explicitly via `unlock`, and
/// again implicitly when the file handle closes).
#[derive(Debug)]
pub(super) struct MigrationLock {
    file: File,
}

impl MigrationLock {
    /// Ensure the `0600` lock file exists (owner-verified via
    /// [`ensure_file_0600`]), open it read-write, and take an **exclusive**
    /// advisory lock, blocking until it is granted.
    ///
    /// Blocking (not `try_lock`) is deliberate: the card requires two concurrent
    /// migrators to both succeed — the loser waits, then observes the store
    /// already at the latest version and applies nothing. Refusing to serve
    /// while a migration runs (`MIGRATION_IN_PROGRESS`, spec 02 §6) is a
    /// daemon-protocol concern (T15), not this runner's.
    pub(super) fn acquire(path: &Path) -> Result<Self, MigrationError> {
        ensure_file_0600(path).map_err(MigrationError::LockPath)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(MigrationError::Lock)?;
        file.lock().map_err(MigrationError::Lock)?;
        Ok(Self { file })
    }
}

impl Drop for MigrationLock {
    fn drop(&mut self) {
        // Best-effort explicit release; closing the handle also releases it.
        let _ = self.file.unlock();
    }
}
