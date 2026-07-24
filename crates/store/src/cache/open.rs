//! Open policy and open-time validation for `cache.sqlite` (spec 03 §4).
//!
//! The cache is a rebuildable, independently validated materialized view of the
//! canonical `state.sqlite` (03 §4, idea.md §2). It is bound to a specific store
//! via `cache_meta` and is **never migrated**: any incompatibility is resolved by
//! dropping and rebuilding, never by an in-place migration (13 §3). Losing the
//! cache loses nothing — it is reconstructible from state (03 §4.4 `[FIXED]`).
//!
//! Open-time validation implements the `[FIXED principle]` list of 03 §4.4 that
//! is relevant at this layer:
//!
//! 1. `cache_meta.store_instance_uuid` ≠ the store's → drop & recreate;
//! 2. `cache_meta.cache_schema_version` unsupported → drop & recreate;
//! 3. a corrupt/unreadable cache → drop & recreate ("rebuild on doubt").
//!
//! Per-worktree FTS *validity* (§4.4 step 3 — is the head fresh for the active
//! generation) and per-row `embedding_cache` checksums (step 4) are checked
//! lazily elsewhere, not here — [`super::embedding::verify_cached_embedding`]
//! (T11-02) is the per-row check step 4 names; cache-open only creates the
//! table, it never validates its rows. This build creates `cache_meta`,
//! `normalized_text_cache` (spec 03 §4.2, T03-04), `fts_doc`/`fts_occurrences`/
//! `fts_projection_head` (spec 03 §4.3, T08-01), and — as of T11-02 —
//! `embedding_cache` (spec 03 §4.2): the schema only; the generation
//! materializer (T08-02) and the (not-yet-built, T11-03) embedder populate
//! their respective tables. Adding a table means bumping
//! [`CACHE_SCHEMA_VERSION`] so an older cache is auto-rebuilt (§4.4 step 2).

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};

/// `busy_timeout` backstop in milliseconds (spec 03 §4). As with state, the real
/// serialization is the write queue; this only guards transient WAL contention.
const BUSY_TIMEOUT_MS: u64 = 5000;

/// The cache schema version this binary produces and understands (spec 03 §4.1).
///
/// Stored in `cache_meta.cache_schema_version`. Because the cache is never
/// migrated (13 §3), any stored value other than this is "unsupported" and forces
/// a drop & rebuild. Bump this whenever the cache DDL changes.
///
/// - `1`: `cache_meta` only (T01-05).
/// - `2`: adds `normalized_text_cache` (T03-04).
/// - `3`: adds `fts_doc`, `fts_occurrences` (FTS5), `fts_projection_head` (T08-01).
/// - `4`: adds `embedding_cache` (T11-02).
pub const CACHE_SCHEMA_VERSION: u32 = 4;

/// The `cache_meta` binding table (spec 03 §4.1). Created on every (re)build.
const CACHE_META_DDL: &str = "CREATE TABLE cache_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);";

/// The `normalized_text_cache` table (spec 03 §4.2). Created on every (re)build
/// alongside `cache_meta`. It holds the normalized text derived from
/// `state.sqlite`'s `source_blob` (recomputable via the code-storage normalize
/// layer, spec 06 §4); losing it loses nothing. `blob_id` is the content-derived
/// `content_blob.blob_id`; there is no checksum column — a row is validated by
/// recomputing its identity from the text (spec 03 §4.4-style "rebuild on doubt").
const NORMALIZED_TEXT_CACHE_DDL: &str = "\
CREATE TABLE normalized_text_cache (
  blob_id          TEXT PRIMARY KEY,
  normalized_text  TEXT NOT NULL,
  byte_size        INTEGER NOT NULL,
  created_at       INTEGER NOT NULL,
  last_used_at     INTEGER NOT NULL
);";

/// `fts_doc` (spec 03 §4.3, T08-01): the explicit rowid-correlation and
/// worktree/generation scoping side-table for `fts_occurrences`. An FTS5
/// virtual table has no native way to express "which worktree/generation does
/// this row belong to", so this table is both the join key
/// (`fts_rowid == fts_occurrences.rowid`) and the scope later validation/search
/// reads (06 §4, 09 §1) filter on.
const FTS_DOC_DDL: &str = "\
CREATE TABLE fts_doc (
  fts_rowid      INTEGER PRIMARY KEY,
  occurrence_id  TEXT NOT NULL UNIQUE,
  worktree_id    TEXT NOT NULL,
  generation_id  TEXT NOT NULL
);
CREATE INDEX fts_doc_by_wt ON fts_doc(worktree_id, generation_id);";

/// The FTS5 virtual table (spec 03 §4.3, 09 §2). `unicode61 remove_diacritics 2`
/// is FTS5's built-in tokenizer; app-side preprocessing
/// ([`super::fts::tokenize_identifier`] and friends) does the code-aware
/// camelCase/snake_case/kebab-case/path/qualified-name splitting **before**
/// insert — this tokenizer only finishes plain Unicode word-boundary and
/// diacritic folding on the already-split text. Requires SQLite compiled with
/// `SQLITE_ENABLE_FTS5`, which this workspace's `bundled` rusqlite feature
/// already turns on unconditionally (no separate Cargo feature needed).
const FTS_OCCURRENCES_DDL: &str = "\
CREATE VIRTUAL TABLE fts_occurrences USING fts5(
  name, qualified_name, path, signature, body,
  tokenize = 'unicode61 remove_diacritics 2'
);";

/// `fts_projection_head` (spec 03 §4.3): the per-worktree validity proof for
/// the FTS view (06 §4's validation order). This task only creates the table;
/// it is populated by the generation materializer (T08-02) and read by
/// per-search/validate-on-open checks (T08-03).
const FTS_PROJECTION_HEAD_DDL: &str = "\
CREATE TABLE fts_projection_head (
  worktree_id            TEXT PRIMARY KEY,
  generation_id          TEXT NOT NULL,
  lexical_schema_version INTEGER NOT NULL,
  tokenizer_version      INTEGER NOT NULL,
  occurrence_count       INTEGER NOT NULL,
  manifest_hash          TEXT NOT NULL,
  updated_at             INTEGER NOT NULL
);";

/// `embedding_cache` (spec 03 §4.2, T11-02). `subject_hash` is domain-separated
/// per `subject_kind` ([`super::embedding::SubjectKind`], spec 03 §1.2);
/// `representation_id` is a **logical** FK (`state.representation`, a different
/// database — never a real SQLite `FOREIGN KEY`, since `foreign_keys=OFF` here
/// and there is no writable cross-DB `ATTACH`, spec 03 §1.4). `WITHOUT ROWID` is
/// intentional: the composite primary key is already unique and total-order
/// comparable, so a shadow rowid would be pure overhead. There is no checksum
/// column on `normalized_text_cache`'s pattern here — unlike that table, this
/// row's primary key (`subject_hash`) is derived from the *subject*, never from
/// `vector_f32`, so a bit-flip in the vector bytes would be undetectable by
/// re-deriving the key; `checksum` (`H` over the stored little-endian vector
/// bytes) is the integrity check instead, verified lazily on read
/// ([`super::embedding::verify_cached_embedding`]).
const EMBEDDING_CACHE_DDL: &str = "\
CREATE TABLE embedding_cache (
  subject_kind       TEXT NOT NULL CHECK
    (subject_kind IN ('content_blob','occurrence_context','memory_entry')),
  subject_hash       TEXT NOT NULL,
  representation_id  TEXT NOT NULL,
  dimensions         INTEGER NOT NULL,
  vector_f32         BLOB NOT NULL,
  byte_size          INTEGER NOT NULL,
  checksum           TEXT NOT NULL,
  created_at         INTEGER NOT NULL,
  last_used_at       INTEGER NOT NULL,
  PRIMARY KEY (subject_kind, subject_hash, representation_id)
) WITHOUT ROWID;";

/// An error opening, validating, or rebuilding a `cache.sqlite` connection.
#[derive(Debug)]
#[non_exhaustive]
pub enum CacheOpenError {
    /// A SQLite call failed (open, pragma, DDL, or seed).
    Sqlite(rusqlite::Error),
    /// `PRAGMA journal_mode=WAL` did not take effect; the connection reported
    /// this mode instead. WAL is required for the concurrency model.
    JournalMode(String),
    /// The cache writer thread could not be spawned (e.g. resource exhaustion).
    Spawn(io::Error),
    /// Removing a stale cache file (`cache.sqlite`/`-wal`/`-shm`) while rebuilding
    /// failed for a reason other than "not found".
    Recreate(io::Error),
}

impl From<rusqlite::Error> for CacheOpenError {
    fn from(e: rusqlite::Error) -> Self {
        CacheOpenError::Sqlite(e)
    }
}

impl fmt::Display for CacheOpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CacheOpenError::Sqlite(e) => write!(f, "sqlite error opening cache store: {e}"),
            CacheOpenError::JournalMode(mode) => {
                write!(
                    f,
                    "cache store could not enable WAL journal mode (got {mode:?})"
                )
            }
            CacheOpenError::Spawn(e) => write!(f, "could not spawn the cache writer thread: {e}"),
            CacheOpenError::Recreate(e) => {
                write!(
                    f,
                    "could not remove a stale cache file while rebuilding: {e}"
                )
            }
        }
    }
}

impl std::error::Error for CacheOpenError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CacheOpenError::Sqlite(e) => Some(e),
            CacheOpenError::JournalMode(_) => None,
            CacheOpenError::Spawn(e) => Some(e),
            CacheOpenError::Recreate(e) => Some(e),
        }
    }
}

/// What happened when the cache was opened.
///
/// - [`Created`](CacheOpenOutcome::Created): no cache existed, a fresh bound cache
///   was built.
/// - [`Reused`](CacheOpenOutcome::Reused): an existing cache was valid and bound
///   to the same store; its rows were preserved.
/// - [`Recreated`](CacheOpenOutcome::Recreated): an existing cache was invalid
///   (wrong store UUID, unsupported schema version, or corrupt) and was dropped
///   and rebuilt empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheOpenOutcome {
    /// No prior cache; a fresh bound cache was created.
    Created,
    /// An existing valid, correctly-bound cache was reused as-is.
    Reused,
    /// An incompatible/corrupt cache was dropped and rebuilt empty.
    Recreated,
}

/// Open (creating if absent) a **read-write** connection to `cache.sqlite` with
/// the cache open policy applied (spec 03 §4).
///
/// Crate-private on purpose: the only writable connection is the one the cache
/// writer task owns. No writable [`Connection`] is exposed on the public API
/// (spec 02 §5: "direct write connections outside the queues are forbidden").
pub(super) fn open_cache_rw(path: &Path) -> Result<Connection, CacheOpenError> {
    // Default rusqlite flags: READ_WRITE | CREATE | URI | NO_MUTEX.
    let conn = Connection::open(path)?;
    apply_cache_pragmas(&conn)?;
    Ok(conn)
}

/// Open a **read-only** connection to `cache.sqlite`.
///
/// Opened `SQLITE_OPEN_READ_ONLY` with `query_only` as a second line of defence.
/// Read-only cross-database `ATTACH` for ad-hoc queries is permitted here (03
/// §1.4); writable cross-DB `ATTACH` is not. Used by the search read leg (09).
pub(super) fn open_cache_read_only(path: &Path) -> Result<Connection, CacheOpenError> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_URI
        | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(path, flags)?;
    conn.busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS))?;
    conn.pragma_update(None, "query_only", true)?;
    Ok(conn)
}

/// Apply the normative `cache.sqlite` pragmas (spec 03 §4) to `conn`.
///
/// Differs from state (03 §2) in two ways: `foreign_keys=OFF` (no FKs into another
/// database; internal integrity is via heads) and `synchronous=NORMAL` (a loss
/// just rebuilds, so full durability is not the budget priority `[SPEC]`).
fn apply_cache_pragmas(conn: &Connection) -> Result<(), CacheOpenError> {
    // `journal_mode` returns the resulting mode, so set-and-verify via a query.
    let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(CacheOpenError::JournalMode(mode));
    }
    conn.pragma_update(None, "foreign_keys", false)?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS))?;
    Ok(())
}

/// Open the cache at `path`, validate its binding against `store_instance_uuid`,
/// and rebuild it if missing/incompatible/corrupt (spec 03 §4.4).
///
/// Returns the bound read-write [`Connection`] the writer will own, plus the
/// [`CacheOpenOutcome`]. `now_ms` seeds `cache_meta.created_at` on a (re)build.
///
/// # Crash safety
///
/// A rebuild is delete-then-create: stale files are removed, a fresh database is
/// created, and `cache_meta` (schema + binding rows) is written in a single
/// transaction. If the process dies mid-rebuild, the next open again finds a
/// missing/unbound cache and rebuilds — the operation is idempotent and converges
/// to a valid bound cache. `state.sqlite` is never touched (a distinct file, no
/// `ATTACH`), so a lost cache loses nothing (03 §4.4 `[FIXED]`).
pub(super) fn open_and_bind(
    path: &Path,
    store_instance_uuid: &str,
    now_ms: i64,
) -> Result<(Connection, CacheOpenOutcome), CacheOpenError> {
    let existed = path.exists();

    // Fast path: an existing, valid, correctly-bound cache is reused untouched.
    if let Some(conn) = inspect_existing(path, store_instance_uuid) {
        return Ok((conn, CacheOpenOutcome::Reused));
    }

    // Rebuild path: drop any stale files, then build a fresh bound cache.
    recreate(path)?;
    let mut conn = open_cache_rw(path)?;
    // Failpoint seam: a hard kill here leaves a fresh but *unbound* cache file
    // (no `cache_meta` rows). Proves the next open detects it and rebuilds.
    #[cfg(feature = "failpoints")]
    local_rag_test_support::fail_point!("cache:after_delete");
    seed_binding(&mut conn, store_instance_uuid, now_ms)?;

    let outcome = if existed {
        CacheOpenOutcome::Recreated
    } else {
        CacheOpenOutcome::Created
    };
    Ok((conn, outcome))
}

/// If a valid, correctly-bound cache already exists at `path`, return its open
/// read-write connection; otherwise return `None` (missing, wrong store UUID,
/// unsupported schema version, or corrupt/unreadable — all "rebuild on doubt").
///
/// On the `None` paths any opened connection is dropped here, before the caller
/// removes the file, so no live handle blocks the rebuild.
fn inspect_existing(path: &Path, store_instance_uuid: &str) -> Option<Connection> {
    if !path.exists() {
        return None;
    }
    // A corrupt header may still open lazily and only fail on first query; either
    // way, an open error means "rebuild".
    let conn = open_cache_rw(path).ok()?;
    match read_binding(&conn) {
        Some((uuid, version)) if uuid == store_instance_uuid && version == CACHE_SCHEMA_VERSION => {
            Some(conn)
        }
        // Mismatch, missing rows, or unreadable (missing table / `SQLITE_NOTADB`).
        _ => {
            drop(conn);
            None
        }
    }
}

/// Read the `(store_instance_uuid, cache_schema_version)` binding from
/// `cache_meta`. Any missing row, missing table, unparsable version, or read
/// error yields `None` (treated as "rebuild").
fn read_binding(conn: &Connection) -> Option<(String, u32)> {
    let uuid: String = conn
        .query_row(
            "SELECT value FROM cache_meta WHERE key = 'store_instance_uuid'",
            [],
            |row| row.get(0),
        )
        .ok()?;
    let version_text: String = conn
        .query_row(
            "SELECT value FROM cache_meta WHERE key = 'cache_schema_version'",
            [],
            |row| row.get(0),
        )
        .ok()?;
    let version = version_text.parse::<u32>().ok()?;
    Some((uuid, version))
}

/// Create the cache schema (`cache_meta`, `normalized_text_cache`, `fts_doc`,
/// `fts_occurrences`, `fts_projection_head`, `embedding_cache`) and seed the
/// binding rows in one transaction (all-or-nothing: a crash before commit
/// leaves the fresh file unbound, so the next open rebuilds it).
fn seed_binding(
    conn: &mut Connection,
    store_instance_uuid: &str,
    now_ms: i64,
) -> Result<(), CacheOpenError> {
    let tx = conn.transaction()?;
    tx.execute_batch(CACHE_META_DDL)?;
    tx.execute_batch(NORMALIZED_TEXT_CACHE_DDL)?;
    tx.execute_batch(FTS_DOC_DDL)?;
    tx.execute_batch(FTS_OCCURRENCES_DDL)?;
    tx.execute_batch(FTS_PROJECTION_HEAD_DDL)?;
    tx.execute_batch(EMBEDDING_CACHE_DDL)?;
    let rows: [(&str, String); 3] = [
        ("store_instance_uuid", store_instance_uuid.to_string()),
        ("cache_schema_version", CACHE_SCHEMA_VERSION.to_string()),
        ("created_at", now_ms.to_string()),
    ];
    {
        let mut stmt = tx.prepare("INSERT INTO cache_meta (key, value) VALUES (?1, ?2)")?;
        for (key, value) in &rows {
            stmt.execute((*key, value.as_str()))?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Remove a stale cache database and its WAL sidecars, ignoring "not found".
///
/// On POSIX an unlink while a prior connection is still open is tolerated (the
/// old inode lives until that connection closes; a fresh file is created next).
/// In production there is a single opener under the store lock, so no concurrent
/// handle exists at all.
fn recreate(path: &Path) -> Result<(), CacheOpenError> {
    for file in sidecar_files(path) {
        match std::fs::remove_file(&file) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(CacheOpenError::Recreate(e)),
        }
    }
    Ok(())
}

/// `cache.sqlite` plus its `-wal`/`-shm` sidecars.
fn sidecar_files(path: &Path) -> [PathBuf; 3] {
    [
        path.to_path_buf(),
        append_suffix(path, "-wal"),
        append_suffix(path, "-shm"),
    ]
}

/// Append a suffix to a path's file name (`cache.sqlite` → `cache.sqlite-wal`).
fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}
