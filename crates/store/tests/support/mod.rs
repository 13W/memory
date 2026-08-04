//! Shared fixtures for the migration-framework test suite (T01-03/T01-04/T17-04).
//!
//! Every migration test needs the same isolated store + raw connection shape;
//! centralized here so `migrate.rs`, `migrate_resumable.rs`, `migrate_restore.rs`
//! and `migrate_fixtures.rs` do not each carry their own copy. Shared across
//! multiple `tests/*.rs` binaries via `mod support;`; each one only uses part of
//! this module's surface, so `dead_code` is unavoidable per-binary — suppressed
//! at the module level rather than item by item (same precedent as
//! `crates/local-rag/tests/support/mod.rs`).
#![allow(dead_code)]

use std::path::Path;
use std::time::Duration;

use local_rag_core::paths::StoreLayout;
use local_rag_store::migrate::run;
use local_rag_store::rusqlite::Connection;
use local_rag_test_support::TempHome;

/// A temp store with an ensured tree; returns the home (kept alive for cleanup)
/// and its [`StoreLayout`].
pub fn temp_store() -> (TempHome, StoreLayout) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    (home, layout)
}

/// A raw read-write connection to `path` (WAL + `synchronous=FULL`), standing in
/// for the connection `StateDb::open` would hand the runner. `synchronous=FULL`
/// is load-bearing for the crash-injection tests in `migrate_resumable.rs`
/// (only a durably-flushed commit survives a real `SIGABRT`); it is harmless
/// extra durability for every other consumer of this helper.
pub fn raw_conn(path: &Path) -> Connection {
    let conn = Connection::open(path).expect("open state db");
    conn.busy_timeout(Duration::from_secs(5))
        .expect("busy_timeout");
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")
        .expect("pragmas");
    conn
}

/// Append a suffix to a path's file name (`state.sqlite` → `state.sqlite-wal`).
pub fn append(path: &Path, suffix: &str) -> std::path::PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    std::path::PathBuf::from(name)
}

/// Remove a SQLite main file and its `-wal`/`-shm` sidecars, tolerating any of
/// the three already being absent (spec 13 §3's documented restore procedure:
/// "replace `state.sqlite` (and drop its `-wal`/`-shm`)").
pub fn remove_sqlite_file_and_sidecars(path: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(append(path, suffix));
    }
}

/// Migrate a fresh store at `layout` through real released schema version `n`
/// (i.e. `local_rag_store::migrate::ALL[..n]`) and return the open connection.
///
/// Building fixtures this way — rather than committing `.sqlite` files, which
/// CLAUDE.md forbids — is trustworthy specifically because
/// [`local_rag_store::migrate::Migration::checksum`] freezes each entry's SQL
/// once shipped (see `ALL`'s own doc comment): `&ALL[..n]` for a historical `n`
/// is byte-identical to what the real release at that version produced, not a
/// second, separately-maintained encoding of it.
pub fn build_store_at_version(layout: &StoreLayout, n: usize, now_ms: i64) -> Connection {
    let all = local_rag_store::migrate::ALL;
    assert!(
        n <= all.len(),
        "n={n} exceeds the number of released migrations ({})",
        all.len()
    );
    let mut conn = raw_conn(&layout.state_db());
    let report = run(&mut conn, &all[..n], &layout.migration_lock(), now_ms)
        .unwrap_or_else(|e| panic!("migrate to released schema version {n}: {e}"));
    assert_eq!(report.store_version, n as u32);
    conn
}
