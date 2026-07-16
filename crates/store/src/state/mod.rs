//! `state.sqlite` open policy and the single bounded writer.
//!
//! [`StateDb::open`] applies the normative connection pragmas (spec 03 §2) and
//! spawns the one writer task that owns the sole writable connection. Callers
//! never touch that connection directly; they submit short transactions through
//! [`StateWriter::transaction`] (spec 02 §5: "direct write connections outside
//! the queues are forbidden"). Reads go through [`StateDb::open_read`], a
//! read-only connection that physically cannot write.

mod open;
mod writer;

pub use open::OpenError;
pub use writer::{StateWriter, WriteError};

use std::path::{Path, PathBuf};

use rusqlite::Connection;

/// Default depth of the bounded write queue (spec 03 §3: "numbers `[SPEC]`").
///
/// Large enough that bursty reconcile/consolidation batches rarely block, small
/// enough that a stalled writer applies backpressure before unbounded memory
/// growth. Queue depth is a metric (spec 02 §5); tune later against real load.
pub const DEFAULT_WRITE_QUEUE_CAPACITY: usize = 64;

/// A handle to the canonical `state.sqlite` store.
///
/// Owns the [`StateWriter`] (the single write path) and hands out read-only
/// connections. The physical writer runs on a dedicated OS thread that owns the
/// only writable [`Connection`]; that thread stays alive until every
/// [`StateWriter`] handle is dropped (its channel closes). Graceful drain/join
/// on shutdown is the daemon's concern (spec 02 §4.3, T15) — here the thread is
/// detached, which is safe because a killed writer only ever loses an
/// uncommitted transaction (rolled back on the next open).
#[derive(Debug)]
pub struct StateDb {
    path: PathBuf,
    writer: StateWriter,
}

impl StateDb {
    /// Open (creating if absent) `state.sqlite` at `path` with the default write
    /// queue capacity ([`DEFAULT_WRITE_QUEUE_CAPACITY`]).
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, OpenError> {
        Self::open_with_capacity(path, DEFAULT_WRITE_QUEUE_CAPACITY)
    }

    /// Open `state.sqlite` with an explicit write-queue `capacity`.
    ///
    /// A small capacity makes backpressure easy to exercise in tests; the
    /// daemon uses [`StateDb::open`].
    pub fn open_with_capacity(
        path: impl Into<PathBuf>,
        capacity: usize,
    ) -> Result<Self, OpenError> {
        let path = path.into();
        let mut conn = open::open_state_rw(&path)?;
        // Run the migration framework under the migration lock (L1) before the
        // bounded writer exists (spec 02 §4.1: open → migrate → serve). The lock
        // file is a sibling of `state.sqlite`, so this path is byte-identical to
        // `StoreLayout::migration_lock()` for any real store path.
        let lock_path = path.with_file_name("migration.lock");
        crate::migrate::run(
            &mut conn,
            crate::migrate::ALL,
            &lock_path,
            crate::clock::system_now_ms(),
        )
        .map_err(|e| OpenError::Migration(Box::new(e)))?;
        let writer = writer::StateWriter::spawn(conn, capacity).map_err(OpenError::Spawn)?;
        Ok(Self { path, writer })
    }

    /// The single write path into `state.sqlite`.
    pub fn writer(&self) -> &StateWriter {
        &self.writer
    }

    /// Open a fresh **read-only** connection to `state.sqlite`.
    ///
    /// The connection is opened `SQLITE_OPEN_READ_ONLY`, so any write attempt
    /// fails with `SQLITE_READONLY`. This is the read leg of the search pipeline
    /// (spec 02 §5); it never contends with the writer. Assumes the store's
    /// writer keeps the database live (true whenever the owning [`StateDb`] is
    /// alive), so the WAL is readable.
    pub fn open_read(&self) -> Result<Connection, OpenError> {
        open::open_state_read_only(&self.path)
    }

    /// The path to the underlying `state.sqlite` file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}
