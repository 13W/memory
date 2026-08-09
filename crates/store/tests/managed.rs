//! T20-01 acceptance tests for the daemon-managed indexing registry
//! (spec 03 §2.1, ADR-0009).
//!
//! All tests are deterministic: an isolated [`TempHome`], fixed `now_ms`
//! literals, and ids minted from [`uuidv7_from`] with fixed entropy (no
//! `SystemUuidV7`, so no wall clock or `/dev/urandom`). Writer operations run
//! through [`StateWriter::transaction`]; reads use [`StateDb::open_read`].
//! Pure per-statement coverage lives in the `managed` module's own unit
//! tests; these exercise the operations against the real migrated schema,
//! the real foreign key, and real transaction rollback.

use local_rag_core::identity::uuidv7_from;
use local_rag_core::paths::StoreLayout;
use local_rag_store::registry::{
    ManagedWorktree, WorktreeKind, create_repository, create_worktree, is_managed,
    managed_worktrees, register_managed_worktree, set_managed_enabled, unregister_managed_worktree,
};
use local_rag_store::rusqlite::{Connection, params};
use local_rag_store::{StateDb, WriteError};
use local_rag_test_support::TempHome;

/// A temporary store with an ensured tree and an opened [`StateDb`] (which
/// runs the production migration set, including registry v10 →
/// `managed_worktree`).
fn open_state() -> (TempHome, StateDb) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    (home, db)
}

/// A distinct, deterministic UUIDv7 string; `seed` varies the last entropy byte.
fn uuid(seed: u8) -> String {
    let mut rand = [0u8; 10];
    rand[9] = seed;
    uuidv7_from(1000, rand).to_string()
}

/// Create a repository and a `Main` worktree of it in one transaction,
/// returning the worktree's id.
async fn repo_with_worktree(db: &StateDb, seed: u8) -> String {
    let repo_id = uuid(seed);
    let worktree_id = uuid(seed.wrapping_add(100));
    let (r, w) = (repo_id.clone(), worktree_id.clone());
    db.writer()
        .transaction(move |tx| {
            create_repository(tx, &r, None, 1000)?;
            create_worktree(tx, &w, &r, WorktreeKind::Main, 1000)
        })
        .await
        .expect("create repository + worktree");
    worktree_id
}

/// Enroll `worktree_id` at `now_ms`.
async fn register(db: &StateDb, worktree_id: &str, now_ms: i64) {
    let id = worktree_id.to_string();
    db.writer()
        .transaction(move |tx| register_managed_worktree(tx, &id, now_ms))
        .await
        .expect("register managed worktree");
}

/// Count `managed_worktree` rows for `worktree_id` (0 or 1, PK-enforced).
fn managed_row_count(conn: &Connection, worktree_id: &str) -> i64 {
    conn.query_row(
        "SELECT count(*) FROM managed_worktree WHERE worktree_id = ?1",
        params![worktree_id],
        |r| r.get(0),
    )
    .expect("count managed rows")
}

#[tokio::test]
async fn register_round_trips_as_an_enabled_row() {
    let (_home, db) = open_state();
    let worktree_id = repo_with_worktree(&db, 1).await;
    register(&db, &worktree_id, 1000).await;

    let read = db.open_read().expect("read conn");
    assert_eq!(
        managed_worktrees(&read).expect("list"),
        vec![ManagedWorktree {
            worktree_id: worktree_id.clone(),
            enabled: true,
            registered_at: 1000,
            updated_at: 1000,
        }]
    );
    assert!(is_managed(&read, &worktree_id).expect("is_managed"));
}

#[tokio::test]
async fn repeated_register_keeps_one_row_and_bumps_updated_at() {
    let (_home, db) = open_state();
    let worktree_id = repo_with_worktree(&db, 2).await;

    register(&db, &worktree_id, 1000).await;
    register(&db, &worktree_id, 2000).await;

    let read = db.open_read().expect("read conn");
    assert_eq!(
        managed_row_count(&read, &worktree_id),
        1,
        "a repeated register must not create a duplicate row",
    );
    let row = &managed_worktrees(&read).expect("list")[0];
    assert_eq!(row.registered_at, 1000, "first enrollment is durable");
    assert_eq!(row.updated_at, 2000, "the latest write wins");
}

#[tokio::test]
async fn register_for_an_unknown_worktree_is_rejected_and_rolls_back() {
    let (_home, db) = open_state();
    let ghost = uuid(3); // never created as a worktree

    let g = ghost.clone();
    let result = db
        .writer()
        .transaction(move |tx| register_managed_worktree(tx, &g, 1000))
        .await;
    assert!(
        matches!(result, Err(WriteError::Sqlite(_))),
        "an unknown worktree_id must be rejected by the foreign key, got {result:?}",
    );

    let read = db.open_read().expect("read conn");
    assert_eq!(
        managed_row_count(&read, &ghost),
        0,
        "nothing is written on rejection",
    );
}

#[tokio::test]
async fn a_disabled_row_stays_visible_to_readers() {
    let (_home, db) = open_state();
    let worktree_id = repo_with_worktree(&db, 4).await;
    register(&db, &worktree_id, 1000).await;

    let id = worktree_id.clone();
    db.writer()
        .transaction(move |tx| set_managed_enabled(tx, &id, false, 3000))
        .await
        .expect("disable");

    let read = db.open_read().expect("read conn");
    let row = &managed_worktrees(&read).expect("list")[0];
    assert!(!row.enabled, "enabled=0 must be visible to readers");
    assert_eq!(row.updated_at, 3000);
    assert!(
        is_managed(&read, &worktree_id).expect("is_managed"),
        "a disabled row is still enrolled",
    );
}

#[tokio::test]
async fn listing_is_ordered_by_worktree_id() {
    let (_home, db) = open_state();
    let (a, b, c) = (
        repo_with_worktree(&db, 10).await,
        repo_with_worktree(&db, 11).await,
        repo_with_worktree(&db, 12).await,
    );
    // Register in an order that does not match sorted order.
    register(&db, &c, 1000).await;
    register(&db, &a, 1000).await;
    register(&db, &b, 1000).await;

    let read = db.open_read().expect("read conn");
    let mut expected = vec![a, b, c];
    expected.sort();
    let actual: Vec<String> = managed_worktrees(&read)
        .expect("list")
        .into_iter()
        .map(|r| r.worktree_id)
        .collect();
    assert_eq!(actual, expected);
}

#[tokio::test]
async fn unregister_removes_only_the_target_and_is_idempotent() {
    let (_home, db) = open_state();
    let a = repo_with_worktree(&db, 20).await;
    let b = repo_with_worktree(&db, 21).await;
    register(&db, &a, 1000).await;
    register(&db, &b, 1000).await;

    let target = a.clone();
    let removed_first = db
        .writer()
        .transaction(move |tx| unregister_managed_worktree(tx, &target))
        .await
        .expect("unregister");
    assert!(removed_first);

    let target = a.clone();
    let removed_second = db
        .writer()
        .transaction(move |tx| unregister_managed_worktree(tx, &target))
        .await
        .expect("unregister again");
    assert!(
        !removed_second,
        "a second unregister is a no-op, not an error"
    );

    let read = db.open_read().expect("read conn");
    assert!(!is_managed(&read, &a).expect("is_managed a"));
    assert!(
        is_managed(&read, &b).expect("is_managed b"),
        "sibling row untouched"
    );
}

#[tokio::test]
async fn set_enabled_on_an_unregistered_worktree_writes_nothing() {
    let (_home, db) = open_state();
    let worktree_id = repo_with_worktree(&db, 30).await; // created, never registered

    let id = worktree_id.clone();
    let matched = db
        .writer()
        .transaction(move |tx| set_managed_enabled(tx, &id, true, 1000))
        .await
        .expect("toggle");
    assert!(!matched, "no managed row exists to match");

    let read = db.open_read().expect("read conn");
    assert_eq!(
        managed_row_count(&read, &worktree_id),
        0,
        "toggling an unregistered worktree must not implicitly enroll it",
    );
}

#[tokio::test]
async fn enrolling_a_brand_new_worktree_is_a_single_transaction() {
    let (_home, db) = open_state();
    let repo_id = uuid(40);
    let worktree_id = uuid(41);

    let (r, w) = (repo_id.clone(), worktree_id.clone());
    db.writer()
        .transaction(move |tx| {
            create_repository(tx, &r, None, 1000)?;
            create_worktree(tx, &w, &r, WorktreeKind::Main, 1000)?;
            register_managed_worktree(tx, &w, 1000)
        })
        .await
        .expect("enroll a brand-new worktree in one transaction");

    let read = db.open_read().expect("read conn");
    assert!(is_managed(&read, &worktree_id).expect("is_managed"));
}

#[tokio::test]
async fn enrollment_survives_reopening_the_store() {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");

    let worktree_id = {
        let db = StateDb::open(layout.state_db()).expect("first open");
        let worktree_id = repo_with_worktree(&db, 50).await;
        register(&db, &worktree_id, 1000).await;
        worktree_id
    };

    let db2 = StateDb::open(layout.state_db()).expect("reopen");
    let read = db2.open_read().expect("read conn");
    assert!(is_managed(&read, &worktree_id).expect("is_managed after reopen"));
}

#[tokio::test]
async fn the_version_10_checksum_matches_the_frozen_migration_and_survives_reopen() {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");

    let expected = local_rag_store::ALL
        .iter()
        .find(|m| m.version == 10)
        .expect("v10 in ALL")
        .checksum();

    let checksum = {
        let db = StateDb::open(layout.state_db()).expect("first open");
        let read = db.open_read().expect("read conn");
        let checksum: String = read
            .query_row(
                "SELECT checksum FROM schema_migrations WHERE version = 10",
                [],
                |r| r.get(0),
            )
            .expect("read v10 checksum");
        assert_eq!(
            checksum, expected,
            "recorded checksum matches the frozen SQL"
        );
        checksum
    };

    let db2 = StateDb::open(layout.state_db()).expect("reopen");
    let read2 = db2.open_read().expect("read conn 2");
    let checksum2: String = read2
        .query_row(
            "SELECT checksum FROM schema_migrations WHERE version = 10",
            [],
            |r| r.get(0),
        )
        .expect("read v10 checksum again");
    assert_eq!(
        checksum2, checksum,
        "checksum is byte-identical across reopen"
    );
    let count: i64 = read2
        .query_row("SELECT count(*) FROM schema_migrations", [], |r| r.get(0))
        .expect("count migrations");
    assert_eq!(count, 10, "reopen adds no new migration rows");
}
