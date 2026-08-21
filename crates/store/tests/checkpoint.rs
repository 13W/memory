//! T15-01 acceptance tests for `StateWriter`/`CacheWriter::checkpoint` (spec
//! 02 §4.3's shutdown-time "flush WAL checkpoint"; spec 03 §3's
//! `PASSIVE`/`TRUNCATE` policy, adopted for `cache.sqlite` too per 03 §4's own
//! as-built note).

use std::path::{Path, PathBuf};

use local_rag_core::paths::StoreLayout;
use local_rag_store::{CacheDb, CheckpointMode, StateDb, insert_normalized_text};
use local_rag_test_support::TempHome;

fn open_state() -> (TempHome, StateDb) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    (home, db)
}

fn open_cache() -> (TempHome, CacheDb) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let db = CacheDb::open(layout.cache_db(), "uuid-a").expect("open cache.sqlite");
    (home, db)
}

/// Append a suffix to a path's file name (`state.sqlite` → `state.sqlite-wal`).
fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().expect("file name").to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

async fn write_rows(writer: &local_rag_store::StateWriter, n: i64) {
    writer
        .transaction(move |tx| {
            tx.execute_batch("CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY, v TEXT);")?;
            for i in 0..n {
                tx.execute(
                    "INSERT INTO t (id, v) VALUES (?1, ?2)",
                    rusqlite::params![i, "x".repeat(256)],
                )?;
            }
            Ok(())
        })
        .await
        .expect("seed rows");
}

#[tokio::test]
async fn state_passive_checkpoint_reports_frames_transferred() {
    let (_home, db) = open_state();
    write_rows(db.writer(), 500).await;

    let stats = db
        .writer()
        .checkpoint(CheckpointMode::Passive)
        .await
        .expect("passive checkpoint");
    assert!(!stats.busy, "no concurrent holder, must not be busy");
    assert!(
        stats.log_frames > 0,
        "the seeded writes must have logged frames"
    );
    assert!(
        stats.checkpointed_frames > 0,
        "a passive checkpoint with no concurrent reader must transfer every frame"
    );
}

/// The `TRUNCATE` mode reliably shrinks the `-wal` file (the load-bearing,
/// observable effect); its self-reported `checkpointed_frames` is not a
/// reliable success signal when it is the *first* checkpoint ever run on a
/// connection — confirmed by direct reproduction against this crate's pinned
/// (bundled) SQLite: `PRAGMA wal_checkpoint(TRUNCATE)` called first reports
/// `log=0, checkpointed=0` even though it does fully checkpoint and truncate
/// the file, while an identical `PASSIVE` call first reports the true count.
/// So this test asserts the file-size effect, not the counters, for
/// `TRUNCATE`; [`state_passive_checkpoint_reports_frames_transferred`] is what
/// proves the counters themselves are wired correctly.
#[tokio::test]
async fn state_truncate_checkpoint_shrinks_the_wal_file() {
    let (_home, db) = open_state();
    write_rows(db.writer(), 500).await;

    let wal_path = append_suffix(db.path(), "-wal");
    let before = std::fs::metadata(&wal_path).map(|m| m.len()).unwrap_or(0);
    assert!(before > 0, "writes must have grown the -wal file");

    let stats = db
        .writer()
        .checkpoint(CheckpointMode::Truncate)
        .await
        .expect("truncate checkpoint");
    assert!(!stats.busy, "no concurrent holder, must not be busy");

    let after = std::fs::metadata(&wal_path).map(|m| m.len()).unwrap_or(0);
    assert!(
        after < before,
        "TRUNCATE must shrink the -wal file: before={before} after={after}"
    );
}

/// See [`state_truncate_checkpoint_shrinks_the_wal_file`]'s doc comment: the
/// file-size effect is the reliable signal for a first-ever `TRUNCATE` on a
/// connection, not the self-reported frame counters.
#[tokio::test]
async fn cache_truncate_checkpoint_shrinks_the_wal_file() {
    let (_home, db) = open_cache();

    db.writer()
        .transaction(|tx| {
            for i in 0..500i64 {
                let text = "x".repeat(256);
                insert_normalized_text(tx, &format!("blob-{i}"), &text, text.len() as i64, 0)?;
            }
            Ok(())
        })
        .await
        .expect("seed cache rows");

    let wal_path = append_suffix(db.path(), "-wal");
    let before = std::fs::metadata(&wal_path).map(|m| m.len()).unwrap_or(0);
    assert!(before > 0, "writes must have grown the -wal file");

    let stats = db
        .writer()
        .checkpoint(CheckpointMode::Truncate)
        .await
        .expect("truncate checkpoint");
    assert!(!stats.busy, "no concurrent holder, must not be busy");

    let after = std::fs::metadata(&wal_path).map(|m| m.len()).unwrap_or(0);
    assert!(
        after < before,
        "TRUNCATE must shrink the -wal file: before={before} after={after}"
    );
}

/// D-083: why the `-wal` file can grow without bound while the daemon runs.
///
/// SQLite can only transfer frames a reader no longer needs, so **one** open
/// read transaction stops the checkpointer dead — and reports nothing wrong
/// while doing it. This pins the exact signature measured on the owner's
/// store, where `state.sqlite-wal` reached 324 GB against a 41 GB database:
/// `busy = false` (nothing is contending) and `checkpointed_frames = 0`
/// (nothing moves) while `log_frames` keeps climbing.
///
/// The bound therefore cannot come from checkpointing harder; it comes from
/// checkpointing at a moment when no reader is holding a snapshot — which is
/// what `daemon::indexing::worktree_task` now does at the end of every
/// indexing cycle.
#[tokio::test]
async fn a_held_read_transaction_freezes_the_checkpoint_without_reporting_busy() {
    let (home, db) = open_state();
    write_rows(db.writer(), 1).await;

    let reader = db.open_read().expect("read conn");
    reader.execute_batch("BEGIN").expect("begin read tx");
    // The snapshot is taken by the first read inside the transaction, not by
    // `BEGIN` itself (SQLite's `BEGIN` is deferred).
    let _: i64 = reader
        .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
        .expect("take the snapshot");

    // Distinct ids: `write_rows` always starts at 0, and the snapshot above
    // needs rows to already exist.
    db.writer()
        .transaction(|tx| {
            for i in 1_000..3_000i64 {
                tx.execute(
                    "INSERT INTO t (id, v) VALUES (?1, ?2)",
                    rusqlite::params![i, "x".repeat(256)],
                )?;
            }
            Ok(())
        })
        .await
        .expect("write under the held snapshot");

    let first = db
        .writer()
        .checkpoint(CheckpointMode::Passive)
        .await
        .expect("passive checkpoint");
    assert!(
        !first.busy,
        "the checkpointer is not blocked by a lock -- that is what makes this \
         failure mode invisible: {first:?}"
    );
    assert!(
        first.checkpointed_frames < first.log_frames,
        "everything up to the reader's mark transfers, nothing past it: {first:?}"
    );

    // The signature that matters is not "some frames stayed" but "the number
    // never moves again". More writes, another checkpoint: the log grows, the
    // transferred count is frozen at exactly the reader's mark. On the owner's
    // store that number sat at 206 516 for tens of minutes while the log went
    // from 6.3 to 10 million frames.
    db.writer()
        .transaction(|tx| {
            for i in 3_000..5_000i64 {
                tx.execute(
                    "INSERT INTO t (id, v) VALUES (?1, ?2)",
                    rusqlite::params![i, "x".repeat(256)],
                )?;
            }
            Ok(())
        })
        .await
        .expect("write more under the same snapshot");
    let second = db
        .writer()
        .checkpoint(CheckpointMode::Passive)
        .await
        .expect("passive checkpoint again");
    assert!(
        second.log_frames > first.log_frames,
        "the log kept growing: {first:?} -> {second:?}"
    );
    assert_eq!(
        second.checkpointed_frames, first.checkpointed_frames,
        "and not one further frame moved: {first:?} -> {second:?}"
    );

    // Release the snapshot: the same call now drains the log.
    reader.execute_batch("ROLLBACK").expect("end read tx");
    drop(reader);
    let stats = db
        .writer()
        .checkpoint(CheckpointMode::Truncate)
        .await
        .expect("truncate checkpoint");
    assert!(!stats.busy, "{stats:?}");
    assert_eq!(
        stats.checkpointed_frames, stats.log_frames,
        "with no reader the whole log transfers: {stats:?}"
    );
    let wal = append_suffix(db.path(), "-wal");
    assert_eq!(
        std::fs::metadata(&wal).expect("wal metadata").len(),
        0,
        "TRUNCATE returns the disk, which is the half PASSIVE never does"
    );

    drop(home);
}
