//! T09-01 acceptance tests for the lock hierarchy (spec 02 §5), mapped 1:1 to
//! the task card's five required scenarios.
//!
//! All tests are deterministic: no network, no `$HOME` dependency (isolated
//! [`TempHome`]), and no wall-clock sleeps — concurrency is proven with std
//! channels plus [`spawn_blocking`]/[`yield_now`], the same idiom already used
//! by `tests/state.rs`'s backpressure test; a regression to over-serialization
//! would hang rather than flake, caught by the job/CI-level timeout.
//!
//! [`spawn_blocking`]: tokio::task::spawn_blocking
//! [`yield_now`]: tokio::task::yield_now

use std::sync::{Arc, Mutex};

use local_rag_core::paths::StoreLayout;
use local_rag_store::lock::checked_scope_sync;
use local_rag_store::{
    CacheDb, CacheWriteError, LockLevel, StateDb, WorktreeLockRegistry, WriteError,
};
use local_rag_test_support::TempHome;

fn open_state(capacity: usize) -> (TempHome, StateDb) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let db = StateDb::open_with_capacity(layout.state_db(), capacity).expect("open state.sqlite");
    (home, db)
}

fn open_cache(capacity: usize) -> (TempHome, CacheDb) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let db = CacheDb::open_with_capacity(layout.cache_db(), "uuid-a", capacity)
        .expect("open cache.sqlite");
    (home, db)
}

/// **"allowed order succeeds"**: the real write path (spec 02 §5) —
/// `L2.write → L4a tx (write-ahead) → backend ops → L4a tx (commit)` — using
/// the real [`WorktreeLockRegistry`] and [`StateDb`], no synthetic levels.
#[tokio::test]
async fn real_write_path_order_succeeds() {
    let (_home, db) = open_state(8);
    let registry = WorktreeLockRegistry::new();

    db.writer()
        .transaction(|tx| tx.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY);"))
        .await
        .expect("schema");

    let writer = db.writer().clone();
    registry
        .write("wt-1", async move {
            // write-ahead
            writer
                .transaction(|tx| tx.execute("INSERT INTO t (id) VALUES (1)", []).map(|_| ()))
                .await
                .expect("write-ahead tx");
            // ...backend ops would run here, no lock held...
            // commit
            writer
                .transaction(|tx| tx.execute("INSERT INTO t (id) VALUES (2)", []).map(|_| ()))
                .await
                .expect("commit tx");
        })
        .await;

    let count: i64 = db
        .writer()
        .transaction(|tx| tx.query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0)))
        .await
        .expect("count");
    assert_eq!(count, 2, "both write-ahead and commit tx landed");
}

/// **"reverse acquisition fails fast in test"**: `L1` (rank 1) attempted while
/// already holding `L2.write` (rank 2) must panic.
#[tokio::test]
#[should_panic(expected = "spec 02 §5 requires strictly increasing rank")]
async fn reverse_order_panics_in_test() {
    let registry = WorktreeLockRegistry::new();
    registry
        .write("wt-1", async {
            checked_scope_sync(LockLevel::L1, || {});
        })
        .await;
}

/// **"separate worktrees progress concurrently"**: two different worktree ids'
/// `L2.write` locks must not serialize each other.
#[tokio::test]
async fn separate_worktrees_do_not_serialize() {
    let registry = Arc::new(WorktreeLockRegistry::new());

    let (entered_a_tx, entered_a_rx) = std::sync::mpsc::channel::<()>();
    let (proceed_a_tx, proceed_a_rx) = std::sync::mpsc::channel::<()>();
    let reg_a = registry.clone();
    let task_a = tokio::spawn(async move {
        reg_a
            .write("wt-1", async move {
                entered_a_tx.send(()).ok();
                tokio::task::spawn_blocking(move || proceed_a_rx.recv().ok())
                    .await
                    .ok();
            })
            .await;
    });

    let (entered_b_tx, entered_b_rx) = std::sync::mpsc::channel::<()>();
    let (proceed_b_tx, proceed_b_rx) = std::sync::mpsc::channel::<()>();
    let reg_b = registry.clone();
    let task_b = tokio::spawn(async move {
        reg_b
            .write("wt-2", async move {
                entered_b_tx.send(()).ok();
                tokio::task::spawn_blocking(move || proceed_b_rx.recv().ok())
                    .await
                    .ok();
            })
            .await;
    });

    // Both must be observed entered *without releasing either* — if the
    // implementation regressed to one global lock, the second `recv()` would
    // never fire while the first task still holds its (different) worktree's
    // lock and this test would hang rather than flake.
    tokio::task::spawn_blocking(move || entered_a_rx.recv().expect("A entered"))
        .await
        .expect("join A-entered wait");
    tokio::task::spawn_blocking(move || entered_b_rx.recv().expect("B entered"))
        .await
        .expect("join B-entered wait");

    proceed_a_tx.send(()).ok();
    proceed_b_tx.send(()).ok();
    task_a.await.expect("join A");
    task_b.await.expect("join B");
}

/// **"same worktree writers serialize"**: two `L2.write` acquisitions for the
/// *same* worktree id run strictly sequentially.
#[tokio::test]
async fn same_worktree_writers_serialize() {
    let registry = Arc::new(WorktreeLockRegistry::new());
    let order = Arc::new(Mutex::new(Vec::<&'static str>::new()));

    let (entered_a_tx, entered_a_rx) = std::sync::mpsc::channel::<()>();
    let (proceed_a_tx, proceed_a_rx) = std::sync::mpsc::channel::<()>();
    let reg_a = registry.clone();
    let order_a = order.clone();
    let task_a = tokio::spawn(async move {
        reg_a
            .write("wt-1", async move {
                entered_a_tx.send(()).ok();
                tokio::task::spawn_blocking(move || proceed_a_rx.recv().ok())
                    .await
                    .ok();
                order_a.lock().expect("order mutex").push("A");
            })
            .await;
    });
    tokio::task::spawn_blocking(move || entered_a_rx.recv().expect("A entered"))
        .await
        .expect("join A-entered wait");

    // B targets the SAME worktree id; it must not be able to enter its body
    // (send on `entered_b_tx`) while A still holds the lock.
    let (entered_b_tx, entered_b_rx) = std::sync::mpsc::channel::<()>();
    let reg_b = registry.clone();
    let order_b = order.clone();
    let task_b = tokio::spawn(async move {
        reg_b
            .write("wt-1", async move {
                entered_b_tx.send(()).ok();
                order_b.lock().expect("order mutex").push("B");
            })
            .await;
    });
    // Let B's task be polled at least once: it will block acquiring the real
    // `RwLock` (held by A) and suspend before ever reaching its body.
    tokio::task::yield_now().await;
    assert_eq!(
        entered_b_rx.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty),
        "B must not enter while A holds the same worktree's write lock"
    );

    proceed_a_tx.send(()).ok();
    task_a.await.expect("join A");

    tokio::task::spawn_blocking(move || entered_b_rx.recv().expect("B entered after A released"))
        .await
        .expect("join B-entered wait");
    task_b.await.expect("join B");

    assert_eq!(*order.lock().expect("order mutex"), vec!["A", "B"]);
}

/// **"queue callback cannot acquire locks"** (`state.sqlite` / L4a): a job
/// closure that attempts another lock acquisition panics the writer thread —
/// observed as `WriterGone`, including on a follow-up call, proving the
/// thread actually died rather than hanging.
#[tokio::test]
async fn state_writer_job_cannot_acquire_another_lock() {
    let (_home, db) = open_state(8);

    db.writer()
        .transaction(|tx| tx.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY);"))
        .await
        .expect("baseline job");

    let first = db
        .writer()
        .transaction(|_tx| {
            checked_scope_sync(LockLevel::L1, || {});
            Ok(())
        })
        .await;
    assert!(
        matches!(first, Err(WriteError::WriterGone)),
        "got {first:?}"
    );

    let second = db
        .writer()
        .transaction(|tx| tx.execute_batch("CREATE TABLE u (id INTEGER PRIMARY KEY);"))
        .await;
    assert!(
        matches!(second, Err(WriteError::WriterGone)),
        "the writer thread must have actually terminated, got {second:?}"
    );
}

/// The `cache.sqlite` (L4b) counterpart of
/// [`state_writer_job_cannot_acquire_another_lock`].
#[tokio::test]
async fn cache_writer_job_cannot_acquire_another_lock() {
    let (_home, db) = open_cache(8);

    db.writer()
        .transaction(|tx| tx.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY);"))
        .await
        .expect("baseline job");

    let first = db
        .writer()
        .transaction(|_tx| {
            checked_scope_sync(LockLevel::L1, || {});
            Ok(())
        })
        .await;
    assert!(
        matches!(first, Err(CacheWriteError::WriterGone)),
        "got {first:?}"
    );

    let second = db
        .writer()
        .transaction(|tx| tx.execute_batch("CREATE TABLE u (id INTEGER PRIMARY KEY);"))
        .await;
    assert!(
        matches!(second, Err(CacheWriteError::WriterGone)),
        "the writer thread must have actually terminated, got {second:?}"
    );
}
