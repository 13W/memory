//! Connection open policy for `state.sqlite` (spec 03 §2).
//!
//! Every connection applies the normative pragmas. `journal_mode=WAL` is a
//! database-level setting (persisted in the file header, set once and idempotent
//! thereafter); `foreign_keys`, `synchronous`, and `busy_timeout` are
//! per-connection and re-applied on each open.

use std::fmt;
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};

/// `busy_timeout` backstop in milliseconds (spec 03 §2). The real serialization
/// is the write queue; this only guards against transient WAL contention.
const BUSY_TIMEOUT_MS: u64 = 5000;

/// An error opening or configuring a `state.sqlite` connection.
#[derive(Debug)]
#[non_exhaustive]
pub enum OpenError {
    /// A SQLite call failed (open, pragma, etc.).
    Sqlite(rusqlite::Error),
    /// `PRAGMA journal_mode=WAL` did not take effect; the connection reported
    /// this mode instead. WAL is required for the durability/concurrency model.
    JournalMode(String),
    /// The writer thread could not be spawned (e.g. resource exhaustion).
    Spawn(std::io::Error),
    /// The migration framework failed while opening the store (spec 13 §3):
    /// incompatible/newer store, checksum drift, a lock failure, or a failing
    /// migration. Boxed to keep [`OpenError`] small.
    Migration(Box<crate::migrate::MigrationError>),
}

impl From<rusqlite::Error> for OpenError {
    fn from(e: rusqlite::Error) -> Self {
        OpenError::Sqlite(e)
    }
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OpenError::Sqlite(e) => write!(f, "sqlite error opening state store: {e}"),
            OpenError::JournalMode(mode) => {
                write!(
                    f,
                    "state store could not enable WAL journal mode (got {mode:?})"
                )
            }
            OpenError::Spawn(e) => write!(f, "could not spawn the state writer thread: {e}"),
            OpenError::Migration(e) => write!(f, "state store migration failed: {e}"),
        }
    }
}

impl std::error::Error for OpenError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            OpenError::Sqlite(e) => Some(e),
            OpenError::JournalMode(_) => None,
            OpenError::Spawn(e) => Some(e),
            OpenError::Migration(e) => Some(e),
        }
    }
}

/// Open (creating if absent) a **read-write** connection to `state.sqlite` with
/// the full open policy applied.
///
/// Crate-private on purpose: the only writable connection is the one the writer
/// task owns. No writable [`Connection`] is exposed on the public API (spec 02
/// §5: "direct write connections outside the queues are forbidden").
pub(super) fn open_state_rw(path: &Path) -> Result<Connection, OpenError> {
    // Default rusqlite flags: READ_WRITE | CREATE | URI | NO_MUTEX.
    let conn = Connection::open(path)?;
    apply_state_pragmas(&conn)?;
    Ok(conn)
}

/// Open a **read-only** connection to `state.sqlite`.
///
/// Opened `SQLITE_OPEN_READ_ONLY` with `query_only` as a second line of defence,
/// so writes fail with `SQLITE_READONLY`. Used by the search read leg (09).
pub(super) fn open_state_read_only(path: &Path) -> Result<Connection, OpenError> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_URI
        | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(path, flags)?;
    conn.busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS))?;
    // A read-only connection cannot write, but pin `query_only` too so the
    // intent is explicit and survives any future flag change.
    conn.pragma_update(None, "query_only", true)?;
    Ok(conn)
}

/// Apply the normative `state.sqlite` pragmas (spec 03 §2) to `conn`.
fn apply_state_pragmas(conn: &Connection) -> Result<(), OpenError> {
    // `journal_mode` returns the resulting mode, so set-and-verify via a query
    // rather than `pragma_update` (which is for non-returning pragmas).
    let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(OpenError::JournalMode(mode));
    }
    conn.pragma_update(None, "foreign_keys", true)?;
    conn.pragma_update(None, "synchronous", "FULL")?;
    conn.busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS))?;
    Ok(())
}
