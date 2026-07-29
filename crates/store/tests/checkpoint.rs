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
