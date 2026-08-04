//! T17-04 acceptance test: the documented backup/restore mechanic (spec 13 §3
//! `[SPEC mechanics]`, `migrate/mod.rs`'s own "Restore seam" doc comment) is
//! exercised end to end, not just backup *creation* (already covered by
//! `migrate_resumable.rs`).
//!
//! None of the real released migrations (`local_rag_store::migrate::ALL`) is
//! destructive yet (`migrate_fixtures.rs::no_released_migration_is_destructive_or_stepped_yet`),
//! so there is nothing real to restore from — this drives a synthetic
//! destructive set, the same shape `migrate_resumable.rs` already uses for its
//! own backup tests.

use local_rag_store::migrate::{Migration, MigrationStep, run};
use local_rag_store::rusqlite::{Connection, Transaction};

mod support;
use support::{raw_conn, remove_sqlite_file_and_sidecars, temp_store};

const SEED_V1: Migration = Migration::sql(
    1,
    "seed",
    "CREATE TABLE data(x INTEGER); INSERT INTO data(x) VALUES (42);",
);

fn wipe(tx: &Transaction<'_>) -> local_rag_store::rusqlite::Result<()> {
    tx.execute("DELETE FROM data", [])?;
    Ok(())
}

const WIPE_STEPS: &[MigrationStep] = &[MigrationStep {
    label: "wipe",
    run: wipe,
}];
const WIPE_V2: Migration = Migration::sql(2, "wipe", "")
    .destructive()
    .with_steps(WIPE_STEPS);

/// What "the previous binary" knows: only migration 1 exists yet.
const OLD_BINARY_SET: &[Migration] = &[SEED_V1];
/// What "the new binary" ships: the destructive wipe has been added.
const FULL_SET: &[Migration] = &[SEED_V1, WIPE_V2];

fn data_values(conn: &Connection) -> Vec<i64> {
    let mut stmt = conn
        .prepare("SELECT x FROM data ORDER BY x")
        .expect("prepare");
    let rows = stmt.query_map([], |r| r.get::<_, i64>(0)).expect("query");
    rows.map(|r| r.expect("row")).collect()
}

/// The documented rollback procedure recovers the old binary's data and
/// leaves a store the old binary can reopen cleanly — and one that can still
/// forward-migrate normally afterward, so an operator's restore does not
/// strand the store off the upgrade path.
#[test]
fn restoring_a_pre_destructive_backup_recovers_the_old_binarys_data() {
    let (_home, layout) = temp_store();
    let state_path = layout.state_db();

    // 1) A single upgrade run from empty applies v1 then v2: the destructive
    // v2's backup unit runs after v1 has committed, so it captures the
    // pre-wipe (v1) data.
    {
        let mut conn = raw_conn(&state_path);
        run(&mut conn, FULL_SET, &layout.migration_lock(), 1_000).expect("migrate to v2");
        assert!(data_values(&conn).is_empty(), "v2 wiped the live data");
    }
    let backup = layout.backups_dir().join("state-2-1000.sqlite");
    assert!(backup.is_file(), "backup at {}", backup.display());

    // 2) The documented manual procedure: stop the daemon (connection above is
    // already dropped), replace state.sqlite (dropping -wal/-shm) with the
    // chosen backup.
    remove_sqlite_file_and_sidecars(&state_path);
    std::fs::copy(&backup, &state_path).expect("restore backup over state.sqlite");

    // 3) Run the *previous* binary (a migration set that only knows v1)
    // against the restored file: no IncompatibleStore, and the data is back.
    {
        let mut conn = raw_conn(&state_path);
        let report = run(&mut conn, OLD_BINARY_SET, &layout.migration_lock(), 2_000)
            .expect("the old binary opens the restored store cleanly");
        assert!(
            report.applied.is_empty(),
            "the restored store is already at v1; nothing to (re-)apply"
        );
        assert_eq!(report.store_version, 1);
        assert_eq!(
            data_values(&conn),
            vec![42],
            "the pre-wipe data is back after restore"
        );
    }

    // 4) The restored store is not stranded: retrying the real upgrade
    // afterward still forward-migrates cleanly.
    {
        let mut conn = raw_conn(&state_path);
        let report = run(&mut conn, FULL_SET, &layout.migration_lock(), 3_000)
            .expect("the restored store still forward-migrates on retry");
        assert_eq!(report.applied, vec![2]);
        assert!(data_values(&conn).is_empty(), "wipe re-applied on retry");
    }
}
