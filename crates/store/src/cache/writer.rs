//! The single bounded writer for `cache.sqlite` (spec 02 §5 L4b, 03 §4).
//!
//! Physically independent of the `state.sqlite` writer (L4a): a separate bounded
//! [`mpsc`] queue feeding its own writer task that owns the sole writable cache
//! connection on a dedicated OS thread. Keeping the two queues distinct is what
//! makes the cross-database rule (03 §1.4) structural — a cache transaction and a
//! state transaction can never share one physical write path, let alone one
//! `ATTACH`ed transaction. A bounded queue means producers await backpressure
//! instead of growing memory without limit; cancelling a producer (dropping the
//! future) frees its slot cleanly and never runs a partial write.

use std::io;

use rusqlite::{Connection, Transaction};
use tokio::sync::{mpsc, oneshot};

/// A type-erased unit of work run on the cache writer thread against the owned
/// connection. Each job wraps one caller transaction plus its reply channel.
type Job = Box<dyn FnOnce(&mut Connection) + Send + 'static>;

/// An error from a queued cache write.
#[derive(Debug)]
#[non_exhaustive]
pub enum CacheWriteError {
    /// The writer is gone: the queue closed (every [`CacheWriter`] handle
    /// dropped, or the writer thread ended) before the transaction could run or
    /// reply. No partial write occurred.
    WriterGone,
    /// The transaction failed — `BEGIN`/`COMMIT` errored, or the closure
    /// returned an error. The transaction was rolled back; the cache is
    /// unchanged.
    Sqlite(rusqlite::Error),
}

impl std::fmt::Display for CacheWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CacheWriteError::WriterGone => {
                write!(f, "cache writer is gone; the write queue is closed")
            }
            CacheWriteError::Sqlite(e) => {
                write!(f, "cache transaction failed (rolled back): {e}")
            }
        }
    }
}

impl std::error::Error for CacheWriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CacheWriteError::WriterGone => None,
            CacheWriteError::Sqlite(e) => Some(e),
        }
    }
}

/// A cloneable handle to the single `cache.sqlite` writer.
///
/// Cloning shares the same queue and writer thread; the thread lives until all
/// clones are dropped. Obtained from [`CacheDb`](super::CacheDb); never
/// constructed with a raw writable connection by callers.
#[derive(Debug, Clone)]
pub struct CacheWriter {
    sender: mpsc::Sender<Job>,
}

impl CacheWriter {
    /// Spawn the cache writer thread over `conn` with a bounded queue of
    /// `capacity` (clamped to at least 1, since a zero-capacity channel is
    /// invalid).
    pub(super) fn spawn(conn: Connection, capacity: usize) -> io::Result<Self> {
        let (sender, mut receiver) = mpsc::channel::<Job>(capacity.max(1));
        std::thread::Builder::new()
            .name("local-rag-cache-writer".to_string())
            .spawn(move || {
                let mut conn = conn;
                // `blocking_recv` parks the OS thread (no runtime needed) until a
                // job arrives; returns `None` once every sender is dropped.
                while let Some(job) = receiver.blocking_recv() {
                    job(&mut conn);
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
    /// [`available_slots`]: CacheWriter::available_slots
    pub fn queue_capacity(&self) -> usize {
        self.sender.max_capacity()
    }

    /// Currently free slots in the write queue. `0` means the queue is full and
    /// further producers will await backpressure.
    pub fn available_slots(&self) -> usize {
        self.sender.capacity()
    }

    /// Run `f` inside a single transaction on the cache writer thread.
    ///
    /// The closure receives a [`Transaction`]; returning `Ok` commits, returning
    /// `Err` rolls back and surfaces [`CacheWriteError::Sqlite`]. `send().await`
    /// applies backpressure when the queue is full; if this future is cancelled
    /// *while still waiting on a full queue*, its slot is released and no work is
    /// enqueued. (Once the job is enqueued, it runs to completion even if the
    /// caller stops awaiting; only the result is discarded.) Because the commit
    /// happens before the reply is sent, a returned `Ok` means the data is
    /// committed — durable only up to `synchronous=NORMAL` (03 §4: a loss just
    /// forces a rebuild, never state data loss).
    pub async fn transaction<F, R>(&self, f: F) -> Result<R, CacheWriteError>
    where
        F: FnOnce(&Transaction<'_>) -> rusqlite::Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let (resp_tx, resp_rx) = oneshot::channel::<Result<R, CacheWriteError>>();
        let job: Job = Box::new(move |conn: &mut Connection| {
            let outcome = run_transaction(conn, f);
            // The caller may have been cancelled (dropped `resp_rx`); ignore.
            let _ = resp_tx.send(outcome);
        });
        // Backpressure point: pends while the bounded queue is full.
        self.sender
            .send(job)
            .await
            .map_err(|_| CacheWriteError::WriterGone)?;
        // Writer dropped the reply channel without sending → thread ended.
        resp_rx.await.unwrap_or(Err(CacheWriteError::WriterGone))
    }
}

/// Execute `f` in a fresh transaction: `BEGIN` → `f` → `COMMIT`, rolling back on
/// any error (the [`Transaction`] rolls back on drop by default).
fn run_transaction<F, R>(conn: &mut Connection, f: F) -> Result<R, CacheWriteError>
where
    F: FnOnce(&Transaction<'_>) -> rusqlite::Result<R>,
{
    let txn = conn.transaction().map_err(CacheWriteError::Sqlite)?;
    match f(&txn) {
        Ok(value) => {
            txn.commit().map_err(CacheWriteError::Sqlite)?;
            Ok(value)
        }
        Err(e) => {
            // Explicit rollback for clarity; drop would do the same.
            drop(txn);
            Err(CacheWriteError::Sqlite(e))
        }
    }
}
