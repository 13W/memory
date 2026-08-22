//! T01-02 acceptance tests for the `state.sqlite` open policy and bounded
//! writer (spec 02 §5, 03 §2/§3).
//!
//! All tests are deterministic: no network, no `$HOME` dependency (isolated
//! [`TempHome`]), and no wall-clock sleeps — the backpressure test gates the
//! writer thread with std channels and detects the full queue by polling the
//! blocked producer exactly once (no timers).

use local_rag_core::paths::StoreLayout;
use local_rag_store::{StateDb, WriteError};
use local_rag_test_support::TempHome;

/// A temporary store with an ensured directory tree; returns the home (kept
/// alive for cleanup) and an opened [`StateDb`] at the default capacity.
fn open_state(capacity: usize) -> (TempHome, StateDb) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let db = StateDb::open_with_capacity(layout.state_db(), capacity).expect("open state.sqlite");
    (home, db)
}

/// The normative connection pragmas are applied (spec 03 §2), read back through
/// the public write API.
#[tokio::test]
async fn state_pragmas_are_applied() {
    let (_home, db) = open_state(8);
    let (journal_mode, foreign_keys, synchronous, busy_timeout) = db
        .writer()
        .transaction(|tx| {
            let journal_mode: String = tx.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
            let foreign_keys: i64 = tx.query_row("PRAGMA foreign_keys", [], |r| r.get(0))?;
            let synchronous: i64 = tx.query_row("PRAGMA synchronous", [], |r| r.get(0))?;
            let busy_timeout: i64 = tx.query_row("PRAGMA busy_timeout", [], |r| r.get(0))?;
            Ok((journal_mode, foreign_keys, synchronous, busy_timeout))
        })
        .await
        .expect("read pragmas");

    assert_eq!(journal_mode.to_ascii_lowercase(), "wal");
    assert_eq!(foreign_keys, 1, "foreign_keys=ON");
    assert_eq!(synchronous, 2, "synchronous=FULL (2)");
    assert_eq!(busy_timeout, 5000);
}

/// D-027 (spec 12 §6 `[FIXED]` "files/segments 0600"): `state.sqlite` itself is
/// created at `0600`, not left at the process umask's default. Re-opening an
/// existing file widened by something else re-asserts `0600` too, the same
/// idempotent re-assert `ensure_dir` already gives the managed directories.
#[cfg(unix)]
#[tokio::test]
async fn state_db_open_creates_and_reasserts_state_sqlite_at_0600() {
    use std::os::unix::fs::PermissionsExt;

    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");

    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    let mode = std::fs::metadata(layout.state_db())
        .expect("stat state.sqlite")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "freshly created state.sqlite is 0600");
    drop(db);

    std::fs::set_permissions(layout.state_db(), std::fs::Permissions::from_mode(0o644))
        .expect("widen mode");
    let db2 = StateDb::open(layout.state_db()).expect("reopen state.sqlite");
    let mode = std::fs::metadata(layout.state_db())
        .expect("stat state.sqlite")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "reopening re-asserts 0600");
    drop(db2);
}

/// With `foreign_keys=ON`, a child referencing a missing parent is rejected and
/// nothing is written (the transaction rolls back).
#[tokio::test]
async fn foreign_keys_are_enforced() {
    let (_home, db) = open_state(8);
    db.writer()
        .transaction(|tx| {
            tx.execute_batch(
                "CREATE TABLE parent (id INTEGER PRIMARY KEY);
                 CREATE TABLE child  (id INTEGER PRIMARY KEY,
                                      parent_id INTEGER REFERENCES parent(id));",
            )
        })
        .await
        .expect("create schema");

    let result = db
        .writer()
        .transaction(|tx| {
            tx.execute("INSERT INTO child (id, parent_id) VALUES (1, 999)", [])
                .map(|_| ())
        })
        .await;
    assert!(
        matches!(result, Err(WriteError::Sqlite(_))),
        "FK violation must error, got {result:?}"
    );

    let count: i64 = db
        .writer()
        .transaction(|tx| tx.query_row("SELECT COUNT(*) FROM child", [], |r| r.get(0)))
        .await
        .expect("count child rows");
    assert_eq!(count, 0, "the rejected insert left no row");
}

/// A closure that errors mid-transaction rolls the whole transaction back; a
/// subsequent successful retry applies the row exactly once (idempotent).
#[tokio::test]
async fn closure_error_rolls_back_then_retry_is_idempotent() {
    let (_home, db) = open_state(8);
    db.writer()
        .transaction(|tx| tx.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY);"))
        .await
        .expect("create schema");

    // First statement succeeds, second violates the PK: the whole transaction
    // (including the first insert) must roll back.
    let result = db
        .writer()
        .transaction(|tx| {
            tx.execute("INSERT INTO t (id) VALUES (1)", [])?;
            tx.execute("INSERT INTO t (id) VALUES (1)", [])?; // duplicate PK → error
            Ok(())
        })
        .await;
    assert!(
        matches!(result, Err(WriteError::Sqlite(_))),
        "got {result:?}"
    );

    let count: i64 = db
        .writer()
        .transaction(|tx| tx.query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0)))
        .await
        .expect("count");
    assert_eq!(count, 0, "the failed transaction rolled back entirely");

    // Retry the write on its own — succeeds, row present exactly once.
    db.writer()
        .transaction(|tx| tx.execute("INSERT INTO t (id) VALUES (1)", []).map(|_| ()))
        .await
        .expect("retry insert");
    let count: i64 = db
        .writer()
        .transaction(|tx| tx.query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0)))
        .await
        .expect("count");
    assert_eq!(count, 1, "retry applied the row exactly once");
}

/// Many concurrent producers all commit; the single writer serializes them so
/// every row lands with no lost writes and no `SQLITE_BUSY` contention.
#[tokio::test]
async fn concurrent_producers_serialize() {
    let (_home, db) = open_state(8);
    db.writer()
        .transaction(|tx| tx.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY);"))
        .await
        .expect("create schema");

    const N: i64 = 64;
    let mut handles = Vec::new();
    for id in 0..N {
        let writer = db.writer().clone();
        handles.push(tokio::spawn(async move {
            writer
                .transaction(move |tx| {
                    tx.execute("INSERT INTO t (id) VALUES (?1)", [id])
                        .map(|_| ())
                })
                .await
        }));
    }
    for handle in handles {
        handle.await.expect("task join").expect("write committed");
    }

    let (count, min, max): (i64, i64, i64) = db
        .writer()
        .transaction(|tx| {
            tx.query_row("SELECT COUNT(*), MIN(id), MAX(id) FROM t", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
        })
        .await
        .expect("aggregate");
    assert_eq!(count, N, "every concurrent write landed");
    assert_eq!((min, max), (0, N - 1), "the full id range is present");
}

/// A full queue makes a producer wait on backpressure; cancelling that producer
/// (dropping its future) frees the slot cleanly, runs no partial write, and
/// leaves the store consistent and usable.
///
/// Determinism: the writer thread is gated with std channels (always safe off a
/// tokio runtime); "A has started" is awaited via [`spawn_blocking`]; and
/// backpressure is detected by polling C exactly once — no timers, no
/// wall-clock, no virtual-clock auto-advance.
///
/// [`spawn_blocking`]: tokio::task::spawn_blocking
#[tokio::test]
async fn queue_saturation_waits_then_cancels_cleanly() {
    use std::future::{Future, poll_fn};
    use std::task::Poll;

    let (_home, db) = open_state(1); // single slot → easy to saturate
    db.writer()
        .transaction(|tx| tx.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY);"))
        .await
        .expect("create schema");

    // std channels gate the writer thread (no tokio primitives run there).
    let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
    let (gate_tx, gate_rx) = std::sync::mpsc::channel::<()>();

    // Job A occupies the writer thread: it inserts id=1, signals that it has
    // started, then blocks on the gate before committing.
    let writer_a = db.writer().clone();
    let job_a = tokio::spawn(async move {
        writer_a
            .transaction(move |tx| {
                tx.execute("INSERT INTO t (id) VALUES (1)", [])?;
                started_tx.send(()).ok();
                gate_rx.recv().ok(); // hold the writer thread here
                Ok::<(), rusqlite::Error>(())
            })
            .await
    });

    // Wait (off the runtime) until A is actually executing: the queue has now
    // drained, so its single slot is free again.
    tokio::task::spawn_blocking(move || started_rx.recv().ok())
        .await
        .expect("join started-wait");

    // Job B fills the single queue slot but cannot run (writer busy with A).
    let writer_b = db.writer().clone();
    let job_b = tokio::spawn(async move {
        writer_b
            .transaction(|tx| tx.execute("INSERT INTO t (id) VALUES (2)", []).map(|_| ()))
            .await
    });
    // Deterministically wait until B has occupied the slot (queue depth metric).
    while db.writer().available_slots() > 0 {
        tokio::task::yield_now().await;
    }

    // Job C must now block on backpressure. Poll it exactly once: a full queue
    // means its very first `send` poll is `Pending`.
    let writer_c = db.writer().clone();
    let mut job_c = Box::pin(
        writer_c.transaction(|tx| tx.execute("INSERT INTO t (id) VALUES (3)", []).map(|_| ())),
    );
    let c_pending = poll_fn(|cx| Poll::Ready(job_c.as_mut().poll(cx).is_pending())).await;
    assert!(c_pending, "C must wait while the queue is full");

    // Cancel C — its send never occupied a slot, so no write is enqueued.
    drop(job_c);

    // Open the gate: A commits, then B runs and commits.
    gate_tx.send(()).ok();
    job_a.await.expect("join A").expect("A committed");
    job_b.await.expect("join B").expect("B committed");

    // Exactly rows {1, 2}: C never ran.
    let ids: Vec<i64> = db
        .writer()
        .transaction(|tx| {
            let mut stmt = tx.prepare("SELECT id FROM t ORDER BY id")?;
            let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })
        .await
        .expect("read ids");
    assert_eq!(ids, vec![1, 2], "cancelled C left no row");

    // The queue recovered: a fresh write still succeeds.
    db.writer()
        .transaction(|tx| tx.execute("INSERT INTO t (id) VALUES (4)", []).map(|_| ()))
        .await
        .expect("post-cancel write");
}

/// The read-only connection handed out for the read path cannot write; the crate
/// exposes no writable connection (all writes go through the queue).
#[tokio::test]
async fn read_only_connection_cannot_write() {
    let (_home, db) = open_state(8);
    db.writer()
        .transaction(|tx| tx.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY);"))
        .await
        .expect("create schema");

    let read = db.open_read().expect("open read-only connection");

    let write = read.execute("INSERT INTO t (id) VALUES (1)", []);
    assert!(write.is_err(), "a read-only connection must reject writes");

    // Reads still work.
    let count: i64 = read
        .query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))
        .expect("read count");
    assert_eq!(count, 0);
}

/// D-081's safety premise, checked rather than assumed: cancelling the caller
/// that is *awaiting* a transaction does not cancel the transaction.
///
/// The daemon's shutdown now aborts background workers that overrun its budget.
/// That is only safe because a job handed to this writer belongs to the writer
/// thread: dropping the caller's future drops the receiver, never the queued
/// work. The queue is also FIFO, so the checkpoint that shutdown enqueues next
/// runs *after* whatever was already in flight — the store is never closed
/// mid-write.
///
/// Deterministic: the writer thread is gated with std channels, exactly like
/// this file's backpressure test. No timers, no sleeps. `multi_thread` because
/// the test itself blocks on those channels — on the default single-threaded
/// runtime that would starve the very task it is waiting for.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_caller_does_not_cancel_its_queued_transaction() {
    let (_home, db) = open_state(8);

    let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();

    let writer = db.writer().clone();
    let caller = tokio::spawn(async move {
        writer
            .transaction(move |tx| {
                // Tell the test the job is running on the writer thread, then
                // hold that thread until it says to go on.
                started_tx.send(()).expect("test is still listening");
                release_rx.recv().expect("test releases the writer");
                tx.execute_batch(
                    "CREATE TABLE IF NOT EXISTS cancelled_caller (id INTEGER PRIMARY KEY);\
                     INSERT INTO cancelled_caller (id) VALUES (1);",
                )?;
                Ok(())
            })
            .await
    });

    started_rx
        .recv()
        .expect("the job reached the writer thread");
    // The caller goes away mid-transaction — the shutdown-budget case.
    caller.abort();
    release_tx.send(()).expect("writer thread is waiting");

    // FIFO: this second job cannot run before the first one finishes, so
    // awaiting it is a barrier — no sleep needed.
    let landed: i64 = db
        .writer()
        .transaction(|tx| tx.query_row("SELECT count(*) FROM cancelled_caller", [], |r| r.get(0)))
        .await
        .expect("barrier transaction");
    assert_eq!(
        landed, 1,
        "the queued transaction is the writer thread's, not the caller's — cancelling the caller \
         must not lose a committed write"
    );
}

/// `D-092`: the writer's transaction owns the write lock from `BEGIN`, not from
/// whichever statement first writes.
///
/// The distinction is the whole defect. Under the `DEFERRED` default a job that
/// reads first — the normal shape; `apply_create` opens with
/// `find_by_idempotency_key` — is holding a read lock when it finally asks to
/// write, and SQLite answers that promotion with a bare `SQLITE_BUSY` rather
/// than calling the busy handler, because waiting there is how two connections
/// deadlock. All 5000 ms of `busy_timeout` go unspent and a foreign writer (the
/// TUI, the CLI) takes the write away from the daemon.
///
/// Deterministic by construction, not by load: the probe runs *inside* a
/// transaction that has touched nothing at all, so under `DEFERRED` there is no
/// lock yet and the foreign `BEGIN IMMEDIATE` sails through. `busy_timeout` on
/// the probe is zero so it answers now instead of waiting out the real
/// transaction.
#[tokio::test]
async fn the_writer_holds_the_write_lock_from_begin_not_from_its_first_write() {
    use std::time::Duration;

    use local_rag_store::rusqlite::{Connection, Error, ErrorCode};

    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let path = layout.state_db();
    let db = StateDb::open(path.clone()).expect("open state.sqlite");

    let probe_path = path.clone();
    let refused = db
        .writer()
        .transaction(move |_tx| {
            let other = Connection::open(&probe_path)?;
            other.busy_timeout(Duration::ZERO)?;
            match other.execute_batch("BEGIN IMMEDIATE") {
                Ok(()) => Ok(None),
                Err(Error::SqliteFailure(e, _)) => Ok(Some((e.code, e.extended_code))),
                Err(e) => Err(e),
            }
        })
        .await
        .expect("the probe itself must run");

    assert_eq!(
        refused,
        Some((ErrorCode::DatabaseBusy, 5)),
        "a foreign writer must find the lock already held (D-092); `None` means the writer began \
         DEFERRED and left the door open until its first write"
    );
}
