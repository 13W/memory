//! Forward-only migration runner for `state.sqlite` (spec 13 §3).
//!
//! Every migration is numbered, checksummed, and forward-only (spec 13 §3). At
//! open the runner acquires the migration lock (L1, spec 02 §5), bootstraps the
//! framework's own bookkeeping tables (`schema_migrations`, `store_settings`;
//! spec 03 §2.1), refuses a store newer than this binary supports
//! (`INCOMPATIBLE_STORE`, spec 02 §6), verifies that already-applied migrations
//! have not drifted, then applies each pending migration in its own transaction.
//!
//! The runner is parameterized by a `&[Migration]` so tests drive synthetic sets
//! deterministically; production passes [`ALL`]. Running migrations happens on
//! the raw writable connection **before** the bounded writer spawns (spec 02
//! §4.1: open → migrate under L1 → serve), so the write queue only ever carries
//! short runtime transactions.
//!
//! Out of scope here (T01-04): resumable per-step checkpoints, destructive
//! markers, and `VACUUM INTO` backups. The seams for those are called out inline
//! (one transaction per migration is already the crash-resume checkpoint; the
//! optional Rust step and destructive backup hook attach to the apply loop).

mod lock;

use std::path::Path;

use rusqlite::{Connection, params};

use local_rag_core::hash::sha256_hex;
use local_rag_core::paths::PathError;

use lock::MigrationLock;

/// One forward-only migration: a version, a name, and its SQL (spec 13 §3).
///
/// T01-04 will grow this with an optional Rust step and a destructive marker;
/// today the identity of a migration is exactly `(version, name, sql)` and its
/// [`checksum`](Migration::checksum) is over the SQL text.
#[derive(Debug, Clone, Copy)]
pub struct Migration {
    /// Strictly increasing version, contiguous from 1.
    pub version: u32,
    /// Human-readable name recorded in `schema_migrations.name`.
    pub name: &'static str,
    /// Forward-only SQL applied in one transaction.
    pub sql: &'static str,
}

impl Migration {
    /// The SHA-256 hex checksum of the SQL text (drift detection).
    ///
    /// This is a namespacing/drift digest, deliberately **not** a spec 03 §1.2
    /// domain-separated content hash: migration checksums only detect that an
    /// already-applied migration's SQL was altered after the fact.
    pub fn checksum(&self) -> String {
        sha256_hex(self.sql.as_bytes())
    }
}

/// The canonical production migration set.
///
/// Empty at T01-03: the framework tables are created by [bootstrap](run), not by
/// a numbered migration, and the first real schema migration (registry DDL)
/// lands in T02-02. An empty set is well-defined — bootstrap runs and nothing is
/// applied.
pub const ALL: &[Migration] = &[];

/// The outcome of a [`run`], for callers and tests to assert idempotency.
#[derive(Debug, Clone)]
pub struct MigrationReport {
    /// Versions applied during *this* run, ascending. Empty means a no-op.
    pub applied: Vec<u32>,
    /// The maximum applied version after the run (`0` if only bootstrap ran).
    pub store_version: u32,
}

/// An error from the migration runner.
#[derive(Debug)]
#[non_exhaustive]
pub enum MigrationError {
    /// A SQLite call failed (bootstrap, reading history, a migration's SQL, or a
    /// bookkeeping insert). Any failing migration transaction is rolled back, so
    /// its version row is absent and the store is left at the prior version.
    Sqlite(rusqlite::Error),
    /// Opening or locking the migration lock file failed.
    Lock(std::io::Error),
    /// Creating/verifying the `0600` migration lock file path failed (e.g. wrong
    /// owner, or a non-file at the path).
    LockPath(PathError),
    /// The store's schema is newer than this binary supports (spec 13 §3); the
    /// binary refuses to touch it. Maps to `INCOMPATIBLE_STORE` (spec 02 §6).
    IncompatibleStore {
        /// The maximum version recorded in the store.
        store_version: u32,
        /// The maximum version this binary knows how to apply.
        binary_max_version: u32,
    },
    /// An already-applied migration's recorded checksum diverges from this
    /// binary's SQL for that version — a migration was altered after being
    /// applied. A hard error; forward-only migrations are immutable once shipped.
    ChecksumDrift {
        /// The version whose checksum diverged.
        version: u32,
        /// The name recorded in the store for that version.
        name: String,
        /// The checksum this binary computes for that version.
        expected: String,
        /// The checksum recorded in the store.
        found: String,
    },
    /// The store records an applied version `≤ binary_max` that this binary's set
    /// does not contain — rewritten migration history. Belt-and-suspenders; a
    /// contiguous set cannot trigger it under normal operation.
    UnknownAppliedVersion {
        /// The applied version missing from this binary's set.
        version: u32,
    },
    /// The provided migration set is not strictly increasing and contiguous from
    /// 1 — a programming error in the set definition, caught before any write.
    MalformedSet {
        /// What was wrong with the set.
        detail: String,
    },
}

impl std::fmt::Display for MigrationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MigrationError::Sqlite(e) => write!(f, "migration sqlite error: {e}"),
            MigrationError::Lock(e) => write!(f, "could not acquire the migration lock: {e}"),
            MigrationError::LockPath(e) => write!(f, "migration lock file error: {e}"),
            MigrationError::IncompatibleStore {
                store_version,
                binary_max_version,
            } => write!(
                f,
                "store schema version {store_version} is newer than this binary supports \
                 (max {binary_max_version}); upgrade the binary"
            ),
            MigrationError::ChecksumDrift {
                version,
                name,
                expected,
                found,
            } => write!(
                f,
                "migration {version} ({name}) checksum drift: expected {expected}, \
                 store has {found}"
            ),
            MigrationError::UnknownAppliedVersion { version } => write!(
                f,
                "store records applied migration {version} unknown to this binary \
                 (rewritten history)"
            ),
            MigrationError::MalformedSet { detail } => {
                write!(f, "malformed migration set: {detail}")
            }
        }
    }
}

impl std::error::Error for MigrationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            MigrationError::Sqlite(e) => Some(e),
            MigrationError::Lock(e) => Some(e),
            MigrationError::LockPath(e) => Some(e),
            _ => None,
        }
    }
}

/// Bootstrap the framework tables and apply every pending migration in
/// `migrations`, under the migration lock at `lock_path` (spec 02 §5 L1).
///
/// `now_ms` is the wall-clock Unix-millisecond timestamp written to
/// `schema_migrations.applied_at` for every migration applied this run (spec 03
/// §1.1); passing it in keeps the runner deterministic under test.
///
/// The lock is acquired at entry (blocking) and released when this function
/// returns. Idempotent: calling again with the same set applies nothing and
/// returns an empty [`MigrationReport::applied`].
///
/// # Errors
///
/// Returns [`MigrationError`] on a malformed set, a lock failure, an
/// incompatible (newer) store, a checksum drift, rewritten history, or any
/// SQLite failure. A failing migration transaction rolls back, leaving the store
/// at the previously applied version.
pub fn run(
    conn: &mut Connection,
    migrations: &[Migration],
    lock_path: &Path,
    now_ms: i64,
) -> Result<MigrationReport, MigrationError> {
    validate_set(migrations)?;
    let binary_max = migrations.last().map(|m| m.version).unwrap_or(0);

    // L1: exclusive with normal operation, held for the rest of this call.
    let _guard = MigrationLock::acquire(lock_path)?;

    bootstrap(conn)?;

    let applied_history = read_applied(conn)?;
    let store_version = applied_history
        .iter()
        .map(|(v, _, _)| *v)
        .max()
        .unwrap_or(0);

    // Compatibility: refuse a store newer than we support (spec 13 §3).
    if store_version > binary_max {
        return Err(MigrationError::IncompatibleStore {
            store_version,
            binary_max_version: binary_max,
        });
    }

    // Drift / history: every already-applied version must exist in our set with
    // a matching checksum (forward-only migrations are immutable once shipped).
    for (version, name, found) in &applied_history {
        match migrations.iter().find(|m| m.version == *version) {
            Some(m) => {
                let expected = m.checksum();
                if expected != *found {
                    return Err(MigrationError::ChecksumDrift {
                        version: *version,
                        name: name.clone(),
                        expected,
                        found: found.clone(),
                    });
                }
            }
            None => {
                return Err(MigrationError::UnknownAppliedVersion { version: *version });
            }
        }
    }

    // Apply pending migrations, ascending, one transaction each. One tx per
    // migration is the crash-resume checkpoint: with WAL + synchronous=FULL a
    // version row exists iff its migration committed, so a re-run resumes from
    // max(version)+1. (T01-04 adds per-step checkpoints and destructive backups.)
    let mut applied = Vec::new();
    for m in migrations.iter().filter(|m| m.version > store_version) {
        let tx = conn.transaction().map_err(MigrationError::Sqlite)?;
        tx.execute_batch(m.sql).map_err(MigrationError::Sqlite)?;
        // [T01-04 seam] an optional Rust step for this migration runs here,
        // inside the same transaction as its SQL and the bookkeeping insert.
        tx.execute(
            "INSERT INTO schema_migrations (version, name, checksum, applied_at) \
             VALUES (?1, ?2, ?3, ?4)",
            params![m.version, m.name, m.checksum(), now_ms],
        )
        .map_err(MigrationError::Sqlite)?;
        tx.commit().map_err(MigrationError::Sqlite)?;
        applied.push(m.version);
    }

    Ok(MigrationReport {
        applied,
        store_version: binary_max,
    })
}

/// Verify the set is strictly increasing and contiguous from 1.
fn validate_set(migrations: &[Migration]) -> Result<(), MigrationError> {
    for (i, m) in migrations.iter().enumerate() {
        let expected = i as u32 + 1;
        if m.version != expected {
            return Err(MigrationError::MalformedSet {
                detail: format!(
                    "expected version {expected} at position {i}, found {}",
                    m.version
                ),
            });
        }
    }
    Ok(())
}

/// Create the framework's own bookkeeping tables (spec 03 §2.1) idempotently.
///
/// These cannot be numbered migrations: recording a migration requires
/// `schema_migrations` to already exist. They are created unconditionally on
/// every open, outside the numbered set.
fn bootstrap(conn: &mut Connection) -> Result<(), MigrationError> {
    let tx = conn.transaction().map_err(MigrationError::Sqlite)?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
           version     INTEGER PRIMARY KEY,
           name        TEXT NOT NULL,
           checksum    TEXT NOT NULL,
           applied_at  INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS store_settings (
           key   TEXT PRIMARY KEY,
           value TEXT NOT NULL
         );",
    )
    .map_err(MigrationError::Sqlite)?;
    tx.commit().map_err(MigrationError::Sqlite)?;
    Ok(())
}

/// Read the applied migration history as `(version, name, checksum)`, ascending.
fn read_applied(conn: &Connection) -> Result<Vec<(u32, String, String)>, MigrationError> {
    let mut stmt = conn
        .prepare("SELECT version, name, checksum FROM schema_migrations ORDER BY version")
        .map_err(MigrationError::Sqlite)?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)? as u32,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
        .map_err(MigrationError::Sqlite)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(MigrationError::Sqlite)
}
