//! Forward-only migration runner for `state.sqlite` (spec 13 §3).
//!
//! Every migration is numbered, checksummed, and forward-only (spec 13 §3),
//! carrying forward-only SQL plus an optional ordered list of idempotent Rust
//! steps. At open the runner acquires the migration lock (L1, spec 02 §5),
//! bootstraps the framework's own bookkeeping tables (`schema_migrations`,
//! `store_settings`, `migration_progress`; spec 03 §2.1), refuses a store newer
//! than this binary supports (`INCOMPATIBLE_STORE`, spec 02 §6), verifies that
//! already-applied migrations have not drifted, then applies each pending
//! migration.
//!
//! The runner is parameterized by a `&[Migration]` so tests drive synthetic sets
//! deterministically; production passes [`ALL`]. Running migrations happens on
//! the raw writable connection **before** the bounded writer spawns (spec 02
//! §4.1: open → migrate under L1 → serve), so the write queue only ever carries
//! short runtime transactions.
//!
//! # Resumable / destructive mechanics (spec 13 §3, T01-04)
//!
//! A migration is either *simple* or *complex*:
//!
//! - A **simple** migration (`!destructive && steps.is_empty()`) applies in a
//!   single transaction — its SQL and the `schema_migrations` bookkeeping row
//!   commit atomically. A version row therefore exists iff its migration
//!   committed, so a crashed run resumes from `max(version)+1`.
//! - A **complex** migration (destructive and/or with Rust steps) is applied as
//!   an ordered list of **units**, each committed on its own so a crash resumes
//!   exactly. Progress is recorded per unit in `migration_progress`: a unit is
//!   committed iff its `(version, seq)` row exists. The unit order is
//!   `[backup?]  [sql?]  [steps…]`, and a final transaction inserts the
//!   `schema_migrations` row and clears the migration's progress rows atomically.
//!   Resume skips every unit whose progress row already exists (units must be
//!   idempotent; for SQL and Rust steps the enclosing transaction guarantees it).
//!
//! **Backup before destructive** (spec 13 §3): a `destructive` migration's first
//! unit copies `state.sqlite` via `VACUUM INTO` to
//! `<root>/backups/state-<version>-<now_ms>.sqlite` *before* any mutation. The
//! backup unit precedes SQL and steps, so re-taking it on resume is safe (no
//! mutation has run yet). `VACUUM` cannot run inside a transaction, so the
//! backup's progress row commits in a separate follow-up transaction; a crash
//! between the copy and that commit simply re-copies the still-pristine store.
//!
//! **Restore seam** (spec 13 §3 `[SPEC mechanics]`): backups live in the
//! directory returned by [`StoreLayout::backups_dir`](local_rag_core::paths::StoreLayout::backups_dir).
//! To roll back a bad destructive upgrade: stop the daemon, replace
//! `state.sqlite` (and drop its `-wal`/`-shm`) with the chosen
//! `backups/state-<version>-<ts>.sqlite`, then run the *previous* binary.
//! Forward-only migrations are never reversed in place.

mod lock;

use std::path::{Path, PathBuf};

use rusqlite::{Connection, Transaction, params};

use local_rag_core::hash::sha256_hex;
use local_rag_core::paths::{PathError, ensure_dir, ensure_file_0600};

use lock::MigrationLock;

/// The signature of a migration's optional Rust step (spec 13 §3).
///
/// The step receives the unit's open transaction, performs idempotent work, and
/// returns. It MUST NOT commit — the runner owns commit and records the step's
/// progress row in the same transaction, so the step's effect and its checkpoint
/// are atomic.
pub type StepFn = fn(&Transaction<'_>) -> rusqlite::Result<()>;

/// One idempotent Rust step within a migration (spec 13 §3).
#[derive(Debug, Clone, Copy)]
pub struct MigrationStep {
    /// Human-readable label recorded in `migration_progress.label`.
    pub label: &'static str,
    /// The idempotent work, run inside (and committed by) the runner's unit
    /// transaction.
    pub run: StepFn,
}

/// One forward-only migration: a version, a name, forward-only SQL, an optional
/// destructive marker, and an optional ordered list of Rust steps (spec 13 §3).
///
/// Construct via [`Migration::sql`] and the [`destructive`](Migration::destructive)
/// / [`with_steps`](Migration::with_steps) builders. The identity for drift
/// detection is the SQL text (see [`checksum`](Migration::checksum)); Rust steps
/// are code, versioned by the binary, so they do not participate in the checksum.
#[derive(Debug, Clone, Copy)]
pub struct Migration {
    /// Strictly increasing version, contiguous from 1.
    pub version: u32,
    /// Human-readable name recorded in `schema_migrations.name`.
    pub name: &'static str,
    /// Forward-only SQL applied in one transaction (may be empty for a
    /// steps-only migration).
    pub sql: &'static str,
    /// Whether a pre-mutation `VACUUM INTO` backup is taken before applying this
    /// migration (spec 13 §3).
    pub destructive: bool,
    /// Ordered, idempotent Rust steps applied after the SQL, each checkpointed.
    pub steps: &'static [MigrationStep],
}

impl Migration {
    /// A simple, non-destructive, SQL-only migration.
    pub const fn sql(version: u32, name: &'static str, sql: &'static str) -> Self {
        Self {
            version,
            name,
            sql,
            destructive: false,
            steps: &[],
        }
    }

    /// Mark this migration destructive: a `VACUUM INTO` backup is taken before
    /// any mutation (spec 13 §3).
    pub const fn destructive(mut self) -> Self {
        self.destructive = true;
        self
    }

    /// Attach idempotent Rust steps, applied (and checkpointed) after the SQL.
    pub const fn with_steps(mut self, steps: &'static [MigrationStep]) -> Self {
        self.steps = steps;
        self
    }

    /// The SHA-256 hex checksum of the SQL text (drift detection).
    ///
    /// This is a namespacing/drift digest, deliberately **not** a spec 03 §1.2
    /// domain-separated content hash: migration checksums only detect that an
    /// already-applied migration's SQL was altered after the fact.
    pub fn checksum(&self) -> String {
        sha256_hex(self.sql.as_bytes())
    }

    /// Whether this migration applies in a single atomic transaction (the
    /// T01-03 fast path) rather than as checkpointed units.
    fn is_simple(&self) -> bool {
        !self.destructive && self.steps.is_empty()
    }
}

/// The canonical production migration set.
///
/// The framework tables (`schema_migrations`, `store_settings`,
/// `migration_progress`) are created by [bootstrap](run), not by a numbered
/// migration. Version 1 (T02-02) is the repository-side registry
/// (`registry::SCHEMA_V1`); version 2 (T02-03) is the worktree side —
/// `worktree`/`worktree_path`/`generation` (`registry::SCHEMA_V2`). Each
/// checksum is frozen once shipped (see [`Migration::checksum`]); later schema
/// changes are new entries here, never edits to an applied one.
pub const ALL: &[Migration] = &[
    Migration::sql(1, "registry", crate::registry::SCHEMA_V1),
    Migration::sql(2, "worktree", crate::registry::SCHEMA_V2),
];

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
    /// A SQLite call failed (bootstrap, reading history, a migration's SQL, a
    /// Rust step, or a bookkeeping insert). Any failing unit transaction is
    /// rolled back, so its progress/version row is absent and the store is left
    /// at the last committed unit/version.
    Sqlite(rusqlite::Error),
    /// The pre-mutation `VACUUM INTO` backup of a destructive migration failed.
    Backup(rusqlite::Error),
    /// Creating or securing the `backups/` directory or a backup file failed
    /// (e.g. wrong owner, or a non-file/non-dir at the path).
    BackupPath(PathError),
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
            MigrationError::Backup(e) => write!(f, "migration backup (VACUUM INTO) failed: {e}"),
            MigrationError::BackupPath(e) => write!(f, "migration backup path error: {e}"),
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
            MigrationError::Backup(e) => Some(e),
            MigrationError::BackupPath(e) => Some(e),
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
/// `schema_migrations.applied_at` / `migration_progress.done_at` and used in the
/// backup filename for every migration applied this run (spec 03 §1.1); passing
/// it in keeps the runner deterministic under test.
///
/// The pre-mutation backup directory is derived as `<lock dir>/backups`, i.e.
/// [`StoreLayout::backups_dir`](local_rag_core::paths::StoreLayout::backups_dir)
/// for any real store path (the lock, `state.sqlite`, and `backups/` are all
/// siblings under the store root).
///
/// The lock is acquired at entry (blocking) and released when this function
/// returns. Idempotent: calling again with the same set applies nothing and
/// returns an empty [`MigrationReport::applied`].
///
/// # Errors
///
/// Returns [`MigrationError`] on a malformed set, a lock failure, an
/// incompatible (newer) store, a checksum drift, rewritten history, a backup
/// failure, or any SQLite failure. A failing unit transaction rolls back,
/// leaving the store at the last committed unit/version.
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

    // `<root>/backups` — siblings of the lock/state files (used only by
    // destructive migrations, created on demand).
    let backups_dir = lock_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("backups");

    // Apply pending migrations, ascending.
    let mut applied = Vec::new();
    for m in migrations.iter().filter(|m| m.version > store_version) {
        if m.is_simple() {
            apply_simple(conn, m, now_ms)?;
        } else {
            apply_complex(conn, m, &backups_dir, now_ms)?;
        }
        applied.push(m.version);
    }

    Ok(MigrationReport {
        applied,
        store_version: binary_max,
    })
}

/// Apply a simple (non-destructive, SQL-only) migration in one transaction — the
/// SQL and its `schema_migrations` row commit atomically (the T01-03 fast path).
fn apply_simple(conn: &mut Connection, m: &Migration, now_ms: i64) -> Result<(), MigrationError> {
    let tx = conn.transaction().map_err(MigrationError::Sqlite)?;
    tx.execute_batch(m.sql).map_err(MigrationError::Sqlite)?;
    record_migration(&tx, m, now_ms)?;
    tx.commit().map_err(MigrationError::Sqlite)?;
    Ok(())
}

/// Apply a complex (destructive and/or stepped) migration as checkpointed units,
/// resuming from the last committed unit (spec 13 §3).
fn apply_complex(
    conn: &mut Connection,
    m: &Migration,
    backups_dir: &Path,
    now_ms: i64,
) -> Result<(), MigrationError> {
    // Unit order: [backup?] [sql?] [steps…]. `seq` is assigned only to units
    // that actually run, so it is stable for a given migration definition.
    let mut seq: i64 = 0;

    // Unit: pre-mutation backup (destructive only). Runs before any mutation, so
    // re-taking it on resume is safe.
    if m.destructive {
        if !progress_has(conn, m.version, seq)? {
            take_backup(conn, m.version, backups_dir, now_ms)?;
            // `VACUUM` cannot run inside a transaction, so the copy already
            // happened above; record its checkpoint in its own transaction.
            record_progress(conn, m.version, seq, "backup", now_ms)?;
        }
        // Injection seam (feature-gated, zero-cost otherwise): model a hard
        // crash immediately after the backup checkpoint durably commits.
        #[cfg(feature = "failpoints")]
        local_rag_test_support::fail_point!("migrate:after_backup");
        seq += 1;
    }

    // Unit: forward-only SQL (if any).
    if !m.sql.trim().is_empty() {
        if !progress_has(conn, m.version, seq)? {
            let tx = conn.transaction().map_err(MigrationError::Sqlite)?;
            tx.execute_batch(m.sql).map_err(MigrationError::Sqlite)?;
            insert_progress(&tx, m.version, seq, "sql", now_ms)?;
            tx.commit().map_err(MigrationError::Sqlite)?;
        }
        seq += 1;
    }

    // Units: idempotent Rust steps, in order.
    for step in m.steps {
        if !progress_has(conn, m.version, seq)? {
            let tx = conn.transaction().map_err(MigrationError::Sqlite)?;
            (step.run)(&tx).map_err(MigrationError::Sqlite)?;
            insert_progress(&tx, m.version, seq, step.label, now_ms)?;
            tx.commit().map_err(MigrationError::Sqlite)?;
        }
        seq += 1;
    }

    // Finalize: record the migration and clear its progress atomically.
    let tx = conn.transaction().map_err(MigrationError::Sqlite)?;
    record_migration(&tx, m, now_ms)?;
    tx.execute(
        "DELETE FROM migration_progress WHERE version = ?1",
        params![m.version],
    )
    .map_err(MigrationError::Sqlite)?;
    tx.commit().map_err(MigrationError::Sqlite)?;
    Ok(())
}

/// Copy `state.sqlite` to `<backups_dir>/state-<version>-<now_ms>.sqlite` via
/// `VACUUM INTO`, before any destructive mutation (spec 13 §3).
///
/// The directory is ensured `0700` and the backup file `0600`. Any stale file at
/// the target (from a prior crashed attempt) is removed first; this is safe
/// because the backup unit precedes all mutation, so the store is still
/// pre-change.
fn take_backup(
    conn: &Connection,
    version: u32,
    backups_dir: &Path,
    now_ms: i64,
) -> Result<(), MigrationError> {
    ensure_dir(backups_dir).map_err(MigrationError::BackupPath)?;
    let path = backup_path(backups_dir, version, now_ms);
    if path.exists() {
        std::fs::remove_file(&path).map_err(|e| {
            MigrationError::BackupPath(PathError::Io {
                path: path.clone(),
                source: e,
            })
        })?;
    }
    // `VACUUM INTO` takes a filename expression; build a safely single-quoted
    // string literal (path may contain quotes on exotic homes).
    let escaped = path.to_string_lossy().replace('\'', "''");
    conn.execute_batch(&format!("VACUUM INTO '{escaped}'"))
        .map_err(MigrationError::Backup)?;
    // Enforce 0600 on the freshly created copy (VACUUM writes with the umask).
    ensure_file_0600(&path).map_err(MigrationError::BackupPath)?;
    Ok(())
}

/// The backup file path for `version` at `now_ms` (spec 13 §3).
fn backup_path(backups_dir: &Path, version: u32, now_ms: i64) -> PathBuf {
    backups_dir.join(format!("state-{version}-{now_ms}.sqlite"))
}

/// Insert the `schema_migrations` row for `m` (used by both apply paths).
fn record_migration(
    tx: &Transaction<'_>,
    m: &Migration,
    now_ms: i64,
) -> Result<(), MigrationError> {
    tx.execute(
        "INSERT INTO schema_migrations (version, name, checksum, applied_at) \
         VALUES (?1, ?2, ?3, ?4)",
        params![m.version, m.name, m.checksum(), now_ms],
    )
    .map_err(MigrationError::Sqlite)?;
    Ok(())
}

/// Whether unit `seq` of `version` has a committed progress row.
fn progress_has(conn: &Connection, version: u32, seq: i64) -> Result<bool, MigrationError> {
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM migration_progress WHERE version = ?1 AND seq = ?2",
            params![version, seq],
            |r| r.get(0),
        )
        .map_err(MigrationError::Sqlite)?;
    Ok(n > 0)
}

/// Insert a progress row inside an existing transaction (SQL/step units).
fn insert_progress(
    tx: &Transaction<'_>,
    version: u32,
    seq: i64,
    label: &str,
    now_ms: i64,
) -> Result<(), MigrationError> {
    tx.execute(
        "INSERT INTO migration_progress (version, seq, label, done_at) \
         VALUES (?1, ?2, ?3, ?4)",
        params![version, seq, label, now_ms],
    )
    .map_err(MigrationError::Sqlite)?;
    Ok(())
}

/// Record a progress row in its own transaction (the backup unit, whose work is
/// the non-transactional `VACUUM INTO`).
fn record_progress(
    conn: &mut Connection,
    version: u32,
    seq: i64,
    label: &str,
    now_ms: i64,
) -> Result<(), MigrationError> {
    let tx = conn.transaction().map_err(MigrationError::Sqlite)?;
    insert_progress(&tx, version, seq, label, now_ms)?;
    tx.commit().map_err(MigrationError::Sqlite)?;
    Ok(())
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
/// `schema_migrations` (and its progress requires `migration_progress`) to
/// already exist. They are created unconditionally on every open, outside the
/// numbered set.
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
         );
         CREATE TABLE IF NOT EXISTS migration_progress (
           version  INTEGER NOT NULL,
           seq      INTEGER NOT NULL,
           label    TEXT NOT NULL,
           done_at  INTEGER NOT NULL,
           PRIMARY KEY (version, seq)
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
