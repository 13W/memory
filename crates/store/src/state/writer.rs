//! The single bounded writer for `state.sqlite` (spec 02 §5 L4a, 03 §3).
//!
//! SQLite has exactly one physical writer. All producers converge onto one
//! bounded [`mpsc`] queue feeding one writer that owns the sole writable
//! connection on a dedicated OS thread. A bounded queue means producers await
//! backpressure instead of growing memory without limit; cancelling a producer
//! (dropping the future) frees its slot cleanly and never runs a partial write.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use rusqlite::{Connection, Transaction, TransactionBehavior, TransactionState};
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
    /// High-water mark of how long one queued transaction held the
    /// connection, in milliseconds (`D-094`). Shared with every clone so the
    /// number describes the queue, not a handle.
    longest_hold_ms: Arc<AtomicU64>,
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
        Ok(Self {
            sender,
            longest_hold_ms: Arc::new(AtomicU64::new(0)),
        })
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
            let longest = Arc::clone(&self.longest_hold_ms);
            let job: Job = Box::new(move |conn: &mut Connection| {
                let started = Instant::now();
                let outcome = run_transaction_with(conn, f, Access::Write);
                record_hold(&longest, started.elapsed());
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

    /// Run `f` in a transaction that is **not** allowed to write `main`.
    ///
    /// The opt-in half of `D-094`. [`transaction`](Self::transaction) opens
    /// `IMMEDIATE`, which is right for work that writes and wrong for work that
    /// does not: an `IMMEDIATE` transaction owns `main`'s write lock from
    /// `BEGIN` until commit, so a long read-only pass routed through this queue
    /// locks out every writer in every other process for its whole duration.
    /// That is not hypothetical — `retention::plan_sweep` is exactly such a
    /// pass (its scratch tables live in `temp.`, which is a separate database
    /// and takes no `main` lock), and on a live 60.9 GB store one 28-second
    /// `gc --dry-run` cost the daemon four failed writes, spaced by the very
    /// `busy_timeout` `D-092` had just made effective.
    ///
    /// Writing `temp.` is fine here; writing `main` is a bug, and a debug build
    /// says so rather than letting it pass — see `run_transaction_with`.
    /// `IMMEDIATE` stays the default precisely so that forgetting to opt in
    /// costs a held lock rather than a lost `busy_timeout`.
    pub async fn read_transaction<F, R>(&self, f: F) -> Result<R, WriteError>
    where
        F: FnOnce(&Transaction<'_>) -> rusqlite::Result<R> + Send + 'static,
        R: Send + 'static,
    {
        crate::lock::checked_scope_async(crate::lock::LockLevel::L4a, async move {
            let (resp_tx, resp_rx) = oneshot::channel::<Result<R, WriteError>>();
            let longest = Arc::clone(&self.longest_hold_ms);
            let job: Job = Box::new(move |conn: &mut Connection| {
                let started = Instant::now();
                let outcome = run_transaction_with(conn, f, Access::ReadOnly);
                record_hold(&longest, started.elapsed());
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

    /// The longest any single queued transaction has held this connection, in
    /// milliseconds (`D-094`).
    ///
    /// A metric in the same spirit as [`available_slots`](Self::available_slots)
    /// — spec 02 §5 already treats queue depth as one. Seconds here mean a
    /// caller is holding the write lock long enough to starve other processes,
    /// which is the shape `D-094` had and which nothing reported at the time.
    pub fn longest_hold_ms(&self) -> u64 {
        self.longest_hold_ms.load(Ordering::Relaxed)
    }

    /// Rewrite the database, returning every free page to the filesystem and
    /// converting the store to `auto_vacuum = INCREMENTAL` on the way
    /// (`X-012`).
    ///
    /// Heavy and exclusive: SQLite copies the live data into a fresh file, so
    /// this needs free disk for a second copy and, on a large store, many
    /// minutes. That is precisely why no background worker calls it — only
    /// `local-rag vacuum`, and only with no daemon holding the store.
    ///
    /// The pragma and the rewrite must share one connection: `auto_vacuum` can
    /// only change *during* a `VACUUM`, so the conversion is a side effect of
    /// this call and not a separate step a caller could forget.
    pub async fn vacuum(&self) -> Result<(), WriteError> {
        crate::lock::checked_scope_async(crate::lock::LockLevel::L4a, async move {
            let (resp_tx, resp_rx) = oneshot::channel::<Result<(), WriteError>>();
            let job: Job = Box::new(move |conn: &mut Connection| {
                let outcome = conn
                    .execute_batch("PRAGMA auto_vacuum = INCREMENTAL; VACUUM;")
                    .map_err(WriteError::Sqlite);
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

    /// Return at most `pages` free pages to the filesystem on the writer
    /// thread (`X-012`).
    ///
    /// Same shape and the same reason as [`checkpoint`](Self::checkpoint): the
    /// pragma refuses to run inside a transaction, and the only writable
    /// connection lives on this thread. Being one job on the same FIFO queue is
    /// what makes it safe to call from an idle poll — it can never interleave
    /// with a write, only follow one.
    ///
    /// Bounded by construction: `pages` is what caps a chunk, so this is a
    /// short job and never the multi-minute freeze a full `VACUUM` would be.
    pub async fn incremental_vacuum(&self, pages: u32) -> Result<u64, WriteError> {
        crate::lock::checked_scope_async(crate::lock::LockLevel::L4a, async move {
            let (resp_tx, resp_rx) = oneshot::channel::<Result<u64, WriteError>>();
            let job: Job = Box::new(move |conn: &mut Connection| {
                let outcome =
                    crate::vacuum::incremental_vacuum(conn, pages).map_err(WriteError::Sqlite);
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

/// Execute `f` in a fresh transaction: `BEGIN IMMEDIATE` → `f` → `COMMIT`,
/// rolling back on any error (the [`Transaction`] rolls back on drop by
/// default).
///
/// `IMMEDIATE`, not the `DEFERRED` default, and the difference is not a
/// preference (`D-092`). A deferred transaction takes its write lock lazily, so
/// a job that reads before it writes — which is the normal shape here;
/// `apply_create` opens with `find_by_idempotency_key` — holds a read lock at
/// the moment it asks to promote. SQLite refuses to invoke the busy handler on
/// that promotion, because waiting there is how two connections deadlock each
/// other (`sqlite3_busy_handler`: "If SQLite determines that invoking the busy
/// handler could result in a deadlock, it will go ahead and return
/// SQLITE_BUSY"). The caller then gets a bare `SQLITE_BUSY` — measured, extended
/// code 5, not 517 `BUSY_SNAPSHOT` — in under a fifth of a second, with all
/// 5000 ms of `busy_timeout` (spec 02 §5, 03 §2) unspent. Taking the write lock
/// at `BEGIN`, before any read lock exists, is what puts that backstop back in
/// play against a foreign writer such as the TUI or the CLI.
fn run_transaction_with<F, R>(conn: &mut Connection, f: F, access: Access) -> Result<R, WriteError>
where
    F: FnOnce(&Transaction<'_>) -> rusqlite::Result<R>,
{
    let behavior = match access {
        Access::Write => TransactionBehavior::Immediate,
        Access::ReadOnly => TransactionBehavior::Deferred,
    };
    let txn = conn
        .transaction_with_behavior(behavior)
        .map_err(WriteError::Sqlite)?;
    match f(&txn) {
        Ok(value) => {
            debug_assert!(
                access == Access::Write
                    || txn.transaction_state(Some("main")) != Ok(TransactionState::Write),
                "a read_transaction closure wrote `main`; it must take the \
                 IMMEDIATE path or stop writing (D-094)"
            );
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

/// How a queued transaction opens, and what it is then allowed to touch.
///
/// `D-094`: the distinction is load-bearing, not stylistic. `Write` opens
/// `IMMEDIATE` so `busy_timeout` actually applies (`D-092`); `ReadOnly` opens
/// `DEFERRED`, because a pass that never writes `main` must not hold `main`'s
/// write lock for its whole duration.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Access {
    Write,
    ReadOnly,
}

/// Record one transaction's hold time as a high-water mark (`D-094`).
///
/// Dependency-free on purpose: this crate carries no logging at all, and the
/// queue already publishes its occupancy as plain accessors.
fn record_hold(longest: &AtomicU64, elapsed: std::time::Duration) {
    let ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
    longest.fetch_max(ms, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch("CREATE TABLE probe (x)").expect("seed");
        conn
    }

    /// `D-094`: the guard is what keeps misclassification from being silent.
    ///
    /// A unit test rather than an integration one on purpose: the assertion
    /// fires on the writer thread, where `#[should_panic]` cannot see it, so
    /// the only place it can be observed directly is here, against the function
    /// that carries it.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "read_transaction closure wrote `main`")]
    fn a_read_only_transaction_that_writes_main_trips_the_guard() {
        let mut conn = open_conn();
        let _ = run_transaction_with(
            &mut conn,
            |tx| tx.execute_batch("INSERT INTO probe (x) VALUES (1)"),
            Access::ReadOnly,
        );
    }

    /// The same closure on the write path is exactly what the queue is for, so
    /// it must pass — the guard has to be about access, not about writing.
    #[test]
    fn the_same_write_on_the_write_path_is_fine() {
        let mut conn = open_conn();
        run_transaction_with(
            &mut conn,
            |tx| tx.execute_batch("INSERT INTO probe (x) VALUES (1)"),
            Access::Write,
        )
        .expect("a write on the write path commits");
    }

    /// A read-only pass that only reads is the case the opt-in exists for.
    #[test]
    fn a_read_only_transaction_that_only_reads_is_accepted() {
        let mut conn = open_conn();
        let count: i64 = run_transaction_with(
            &mut conn,
            |tx| tx.query_row("SELECT COUNT(*) FROM probe", [], |r| r.get(0)),
            Access::ReadOnly,
        )
        .expect("a read on the read-only path commits");
        assert_eq!(count, 0);
    }

    /// Writing `temp.` from a read-only pass is explicitly allowed — that is
    /// what `retention::setup_scratch` does, and `temp` is a separate database
    /// that takes no `main` lock.
    #[test]
    fn a_read_only_transaction_may_write_temp() {
        let mut conn = open_conn();
        run_transaction_with(
            &mut conn,
            |tx| {
                tx.execute_batch(
                    "CREATE TEMP TABLE scratch (x);\
                     INSERT INTO scratch (x) VALUES (1);",
                )
            },
            Access::ReadOnly,
        )
        .expect("temp writes take no `main` lock");
    }

    /// `D-094`'s metric: the high-water mark only ever grows.
    #[test]
    fn the_hold_metric_keeps_the_maximum() {
        let longest = AtomicU64::new(0);
        record_hold(&longest, std::time::Duration::from_millis(40));
        record_hold(&longest, std::time::Duration::from_millis(7));
        assert_eq!(
            longest.load(Ordering::Relaxed),
            40,
            "a later shorter hold must not lower it"
        );
    }
}
