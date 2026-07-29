//! The single bounded writer for `state.sqlite` (spec 02 §5 L4a, 03 §3).
//!
//! SQLite has exactly one physical writer. All producers converge onto one
//! bounded [`mpsc`] queue feeding one writer that owns the sole writable
//! connection on a dedicated OS thread. A bounded queue means producers await
//! backpressure instead of growing memory without limit; cancelling a producer
//! (dropping the future) frees its slot cleanly and never runs a partial write.

use std::io;

use rusqlite::{Connection, Transaction};
use tokio::sync::{mpsc, oneshot};

/// A type-erased unit of work run on the writer thread against the owned
/// connection. Each job wraps one caller transaction plus its reply channel.
type Job = Box<dyn FnOnce(&mut Connection) + Send + 'static>;

/// An error from a queued write.
#[derive(Debug)]
#[non_exhaustive]
pub enum WriteError {
    /// The writer is gone: the queue closed (every [`StateWriter`] handle
    /// dropped, or the writer thread ended) before the transaction could run or
    /// reply. No partial write occurred.
    WriterGone,
    /// The transaction failed — `BEGIN`/`COMMIT` errored, or the closure
    /// returned an error. The transaction was rolled back; the store is
    /// unchanged.
    Sqlite(rusqlite::Error),
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriteError::WriterGone => {
                write!(f, "state writer is gone; the write queue is closed")
            }
            WriteError::Sqlite(e) => write!(f, "state transaction failed (rolled back): {e}"),
        }
    }
}

impl std::error::Error for WriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            WriteError::WriterGone => None,
            WriteError::Sqlite(e) => Some(e),
        }
    }
}

pub use crate::checkpoint::{CheckpointMode, CheckpointStats};

/// A cloneable handle to the single `state.sqlite` writer.
///
/// Cloning shares the same queue and writer thread; the thread lives until all
/// clones are dropped. Obtained from [`StateDb`](super::StateDb); never
/// constructed with a raw writable connection by callers.
#[derive(Debug, Clone)]
pub struct StateWriter {
    sender: mpsc::Sender<Job>,
}

impl StateWriter {
    /// Spawn the writer thread over `conn` with a bounded queue of `capacity`
    /// (clamped to at least 1, since a zero-capacity channel is invalid).
    pub(super) fn spawn(conn: Connection, capacity: usize) -> io::Result<Self> {
        let (sender, mut receiver) = mpsc::channel::<Job>(capacity.max(1));
        std::thread::Builder::new()
            .name("local-rag-state-writer".to_string())
            .spawn(move || {
                let mut conn = conn;
                // `blocking_recv` parks the OS thread (no runtime needed) until a
                // job arrives; returns `None` once every sender is dropped.
                while let Some(job) = receiver.blocking_recv() {
                    // spec 02 §5: "L4 queues are leaves" — mark this thread as
                    // already holding the hierarchy's topmost rank for the
                    // job's duration, so any lock acquisition attempted from
                    // inside `job` fails the strict-order check.
                    crate::lock::checked_scope_sync(crate::lock::LockLevel::L4a, || job(&mut conn));
                }
                // Queue closed → drop the owned connection. Any in-flight
                // transaction already committed or rolled back per job.
            })?;
        Ok(Self { sender })
    }

    /// The maximum depth of the bounded write queue.
    ///
    /// Queue depth is a metric (spec 02 §5); pair with [`available_slots`] to
    /// observe backpressure.
    ///
    /// [`available_slots`]: StateWriter::available_slots
    pub fn queue_capacity(&self) -> usize {
        self.sender.max_capacity()
    }

    /// Currently free slots in the write queue. `0` means the queue is full and
    /// further producers will await backpressure.
    pub fn available_slots(&self) -> usize {
        self.sender.capacity()
    }

    /// Run `f` inside a single transaction on the writer thread.
    ///
    /// The closure receives a [`Transaction`]; returning `Ok` commits, returning
    /// `Err` (or a panic-free early return) rolls back and surfaces
    /// [`WriteError::Sqlite`]. `send().await` applies backpressure when the queue
    /// is full; if this future is cancelled *while still waiting on a full
    /// queue*, its slot is released and no work is enqueued. (Once the job is
    /// enqueued, it runs to completion even if the caller stops awaiting; only
    /// the result is discarded.) Because the commit happens before the reply is
    /// sent, a returned `Ok` means the data is durably committed.
    pub async fn transaction<F, R>(&self, f: F) -> Result<R, WriteError>
    where
        F: FnOnce(&Transaction<'_>) -> rusqlite::Result<R> + Send + 'static,
        R: Send + 'static,
    {
        crate::lock::checked_scope_async(crate::lock::LockLevel::L4a, async move {
            let (resp_tx, resp_rx) = oneshot::channel::<Result<R, WriteError>>();
            let job: Job = Box::new(move |conn: &mut Connection| {
                let outcome = run_transaction(conn, f);
                // The caller may have been cancelled (dropped `resp_rx`); ignore.
                let _ = resp_tx.send(outcome);
            });
            // Backpressure point: pends while the bounded queue is full.
            self.sender
                .send(job)
                .await
                .map_err(|_| WriteError::WriterGone)?;
            // Writer dropped the reply channel without sending → thread ended.
            resp_rx.await.unwrap_or(Err(WriteError::WriterGone))
        })
        .await
    }

    /// Run a `PRAGMA wal_checkpoint` on the writer thread (spec 02 §4.3's
    /// shutdown-time "flush WAL checkpoint"; spec 03 §3's checkpoint policy).
    ///
    /// Unlike [`transaction`](Self::transaction), the closure this dispatches
    /// runs directly against `&mut Connection`, never wrapped in a
    /// [`Transaction`] — SQLite refuses `wal_checkpoint` while a transaction
    /// is open, and no other path to the raw connection exists outside this
    /// writer thread (spec 02 §5: writable connections never leave the
    /// queue). Still one job on the same queue, so it never races an
    /// in-flight write.
    pub async fn checkpoint(&self, mode: CheckpointMode) -> Result<CheckpointStats, WriteError> {
        crate::lock::checked_scope_async(crate::lock::LockLevel::L4a, async move {
            let (resp_tx, resp_rx) = oneshot::channel::<Result<CheckpointStats, WriteError>>();
            let job: Job = Box::new(move |conn: &mut Connection| {
                let outcome = run_checkpoint(conn, mode);
                let _ = resp_tx.send(outcome);
            });
            self.sender
                .send(job)
                .await
                .map_err(|_| WriteError::WriterGone)?;
            resp_rx.await.unwrap_or(Err(WriteError::WriterGone))
        })
        .await
    }
}

/// Execute `f` in a fresh transaction: `BEGIN` → `f` → `COMMIT`, rolling back on
/// any error (the [`Transaction`] rolls back on drop by default).
fn run_transaction<F, R>(conn: &mut Connection, f: F) -> Result<R, WriteError>
where
    F: FnOnce(&Transaction<'_>) -> rusqlite::Result<R>,
{
    let txn = conn.transaction().map_err(WriteError::Sqlite)?;
    match f(&txn) {
        Ok(value) => {
            txn.commit().map_err(WriteError::Sqlite)?;
            Ok(value)
        }
        Err(e) => {
            // Explicit rollback for clarity; drop would do the same.
            drop(txn);
            Err(WriteError::Sqlite(e))
        }
    }
}

fn run_checkpoint(
    conn: &mut Connection,
    mode: CheckpointMode,
) -> Result<CheckpointStats, WriteError> {
    conn.query_row(mode.pragma(), [], |row| {
        Ok(CheckpointStats {
            busy: row.get::<_, i64>(0)? != 0,
            log_frames: row.get(1)?,
            checkpointed_frames: row.get(2)?,
        })
    })
    .map_err(WriteError::Sqlite)
}
