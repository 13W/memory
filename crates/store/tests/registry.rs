//! T02-02 acceptance tests for the repository registry (spec 03 §2.1, 01 §5,
//! 12 §7).
//!
//! All tests are deterministic: an isolated [`TempHome`], fixed `now_ms`
//! literals, and `repo_id`s minted from [`uuidv7_from`] with fixed entropy (no
//! `SystemUuidV7`, so no wall clock or `/dev/urandom`). Writer operations run
//! through [`StateWriter::transaction`]; reads use [`StateDb::open_read`].

use local_rag_core::identity::remote;
use local_rag_core::identity::uuidv7_from;
use local_rag_core::paths::StoreLayout;
use local_rag_store::registry::{
    PathObservation, all_repository_ids, create_repository, current_path,
    find_repositories_by_remote, find_repository_by_path, observe_repository_path, path_history,
};
use local_rag_store::rusqlite::Connection;
use local_rag_store::{StateDb, WriteError};
use local_rag_test_support::TempHome;

/// A temporary store with an ensured tree and an opened [`StateDb`] (which runs
/// the production migration set, including registry v1).
fn open_state() -> (TempHome, StateDb) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    (home, db)
}

/// A distinct, deterministic UUIDv7 string. `seed` varies the last entropy byte
/// (never masked by the version/variant stamping), so distinct seeds yield
/// distinct ids without touching the clock.
fn repo_id(seed: u8) -> String {
    let mut rand = [0u8; 10];
    rand[9] = seed;
    uuidv7_from(1000, rand).to_string()
}

/// Count a repository's current (`is_current = 1`) path rows.
fn current_count(conn: &Connection, repo_id: &str) -> i64 {
    conn.query_row(
        "SELECT count(*) FROM repository_path WHERE repo_id = ?1 AND is_current = 1",
        [repo_id],
        |r| r.get(0),
    )
    .expect("count current rows")
}

fn table_exists(conn: &Connection, name: &str) -> bool {
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name = ?1",
            [name],
            |r| r.get(0),
        )
        .expect("query sqlite_master");
    n == 1
}

/// create + observe A, then observe B, then observe A again: after each step
/// exactly one path is current and it is the one just observed (the one-current
/// constraint holds across switches).
#[tokio::test]
async fn observe_sets_and_switches_single_current() {
    let (_home, db) = open_state();
    let id = repo_id(1);
    let (a, b) = ("/work/proj-a".to_string(), "/work/proj-b".to_string());

    // create + first observe compose into one transaction (the discovery flow).
    let id0 = id.clone();
    let a0 = a.clone();
    db.writer()
        .transaction(move |tx| {
            create_repository(tx, &id0, None, 1000)?;
            observe_repository_path(tx, &id0, &a0, 1000)
        })
        .await
        .expect("create + observe A");

    for (path, now) in [(&b, 2000_i64), (&a, 3000_i64)] {
        let id_n = id.clone();
        let path_n = path.clone();
        db.writer()
            .transaction(move |tx| observe_repository_path(tx, &id_n, &path_n, now))
            .await
            .expect("observe switch");
    }

    let read = db.open_read().expect("read conn");
    assert_eq!(current_count(&read, &id), 1, "exactly one current path");
    assert_eq!(
        current_path(&read, &id).expect("current_path"),
        Some(a.clone()),
        "the last observed path is current",
    );
}

/// The `repository_path_current` partial unique index rejects a second current
/// path: after a repo has current path A, a raw insert of B with `is_current=1`
/// (bypassing the clear-then-set of `observe_repository_path`) violates the
/// index, and A remains the sole current path.
#[tokio::test]
async fn partial_unique_index_rejects_two_current() {
    let (_home, db) = open_state();
    let id = repo_id(2);

    let id0 = id.clone();
    db.writer()
        .transaction(move |tx| {
            create_repository(tx, &id0, None, 1000)?;
            observe_repository_path(tx, &id0, "/work/a", 1000)
        })
        .await
        .expect("create + observe A");

    // Force a second current row directly — the index must reject it.
    let id1 = id.clone();
    let result = db
        .writer()
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO repository_path \
                   (repo_id, observed_path, is_current, first_seen_at, last_seen_at) \
                 VALUES (?1, '/work/b', 1, 2000, 2000)",
                [&id1],
            )
            .map(|_| ())
        })
        .await;
    assert!(
        matches!(result, Err(WriteError::Sqlite(_))),
        "a second current path must be rejected, got {result:?}"
    );

    let read = db.open_read().expect("read conn");
    assert_eq!(current_count(&read, &id), 1, "still exactly one current");
    assert_eq!(
        current_path(&read, &id).expect("current_path"),
        Some("/work/a".to_string()),
        "the rejected insert left A current",
    );
}

/// The git remote fingerprint is a hint, not identity (spec 12 §7): two distinct
/// repositories may carry the same fingerprint, and both are found by it.
#[tokio::test]
async fn same_remote_maps_to_two_repositories() {
    let (_home, db) = open_state();
    let fp = remote::fingerprint("git@github.com:org/repo.git");
    let (id_a, id_b) = (repo_id(3), repo_id(4));

    for (id, path) in [(&id_a, "/clone/one"), (&id_b, "/clone/two")] {
        let id_n = id.clone();
        let fp_n = fp.clone();
        let path_n = path.to_string();
        db.writer()
            .transaction(move |tx| {
                create_repository(tx, &id_n, Some(&fp_n), 1000)?;
                observe_repository_path(tx, &id_n, &path_n, 1000)
            })
            .await
            .expect("create repo with shared remote");
    }

    let read = db.open_read().expect("read conn");
    let mut found = find_repositories_by_remote(&read, &fp).expect("find by remote");
    found.sort();
    let mut expected = vec![id_a, id_b];
    expected.sort();
    assert_eq!(
        found, expected,
        "both repos share the one remote fingerprint"
    );
}

/// Moving the current path retains history: the prior path row survives with
/// `is_current = 0` and its original `first_seen_at`.
#[tokio::test]
async fn path_history_retained_across_move() {
    let (_home, db) = open_state();
    let id = repo_id(5);

    let id0 = id.clone();
    db.writer()
        .transaction(move |tx| {
            create_repository(tx, &id0, None, 1000)?;
            observe_repository_path(tx, &id0, "/old/loc", 1000)
        })
        .await
        .expect("create + observe old");

    let id1 = id.clone();
    db.writer()
        .transaction(move |tx| observe_repository_path(tx, &id1, "/new/loc", 2000))
        .await
        .expect("observe new");

    let read = db.open_read().expect("read conn");
    let history = path_history(&read, &id).expect("path history");
    assert_eq!(history.len(), 2, "both observed paths retained");
    assert_eq!(
        history[0],
        PathObservation {
            observed_path: "/old/loc".to_string(),
            is_current: false,
            first_seen_at: 1000,
            last_seen_at: 1000,
        },
        "the old path is retained, no longer current, first_seen_at intact",
    );
    assert_eq!(
        history[1],
        PathObservation {
            observed_path: "/new/loc".to_string(),
            is_current: true,
            first_seen_at: 2000,
            last_seen_at: 2000,
        },
    );
    assert_eq!(
        current_count(&read, &id),
        1,
        "exactly one current after move"
    );
}

/// Re-observing the current path is idempotent: no duplicate row, still current,
/// `first_seen_at` preserved and `last_seen_at` refreshed.
#[tokio::test]
async fn observe_is_idempotent_under_retry() {
    let (_home, db) = open_state();
    let id = repo_id(6);

    let id0 = id.clone();
    db.writer()
        .transaction(move |tx| {
            create_repository(tx, &id0, None, 1000)?;
            observe_repository_path(tx, &id0, "/loc", 1000)
        })
        .await
        .expect("create + observe @1000");

    let id1 = id.clone();
    db.writer()
        .transaction(move |tx| observe_repository_path(tx, &id1, "/loc", 2000))
        .await
        .expect("re-observe @2000");

    let read = db.open_read().expect("read conn");
    let history = path_history(&read, &id).expect("path history");
    assert_eq!(history.len(), 1, "no duplicate row on retry");
    assert_eq!(
        history[0],
        PathObservation {
            observed_path: "/loc".to_string(),
            is_current: true,
            first_seen_at: 1000,
            last_seen_at: 2000,
        },
        "first_seen_at preserved, last_seen_at bumped, still current",
    );
}

/// Observing a path for an unknown repository is rejected by the foreign key,
/// and no `repository_path` row is written.
#[tokio::test]
async fn observe_unknown_repo_is_rejected() {
    let (_home, db) = open_state();
    let ghost = repo_id(7); // never created

    let ghost0 = ghost.clone();
    let result = db
        .writer()
        .transaction(move |tx| observe_repository_path(tx, &ghost0, "/nowhere", 1000))
        .await;
    assert!(
        matches!(result, Err(WriteError::Sqlite(_))),
        "observe on an unknown repo must be rejected, got {result:?}"
    );

    let read = db.open_read().expect("read conn");
    let n: i64 = read
        .query_row("SELECT count(*) FROM repository_path", [], |r| r.get(0))
        .expect("count paths");
    assert_eq!(n, 0, "the rejected observe left no path row");
}

/// The stored `git_remote_fingerprint` is the domain hash (64 lowercase hex),
/// not the raw or normalized URL.
#[tokio::test]
async fn remote_fingerprint_stored_is_the_hash() {
    let (_home, db) = open_state();
    let id = repo_id(8);
    let url = "git@github.com:org/repo.git";
    let fp = remote::fingerprint(url);

    let id0 = id.clone();
    let fp0 = fp.clone();
    db.writer()
        .transaction(move |tx| create_repository(tx, &id0, Some(&fp0), 1000))
        .await
        .expect("create repo");

    let read = db.open_read().expect("read conn");
    let stored: String = read
        .query_row(
            "SELECT git_remote_fingerprint FROM repository WHERE repo_id = ?1",
            [&id],
            |r| r.get(0),
        )
        .expect("read fingerprint");
    assert_eq!(stored, fp);
    assert_eq!(stored.len(), 64, "a BLAKE3 hex digest");
    assert!(stored.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_ne!(stored, url, "not the raw URL");
    assert_ne!(
        stored,
        remote::normalize_remote_url(url),
        "not the normalized URL"
    );
}

/// A repository may have no remote (a local, non-git-remote checkout): the
/// fingerprint is NULL.
#[tokio::test]
async fn null_remote_fingerprint_allowed() {
    let (_home, db) = open_state();
    let id = repo_id(9);

    let id0 = id.clone();
    db.writer()
        .transaction(move |tx| create_repository(tx, &id0, None, 1000))
        .await
        .expect("create repo without remote");

    let read = db.open_read().expect("read conn");
    let is_null: bool = read
        .query_row(
            "SELECT git_remote_fingerprint IS NULL FROM repository WHERE repo_id = ?1",
            [&id],
            |r| r.get(0),
        )
        .expect("read fingerprint nullability");
    assert!(is_null, "a repo without a remote stores NULL");
}

/// The v1 migration produces exactly the repository-side schema: the three
/// tables, the current-path partial unique index, and a single recorded
/// migration row.
#[tokio::test]
async fn migration_produces_exact_registry_schema() {
    let (_home, db) = open_state();
    let read = db.open_read().expect("read conn");

    for t in ["repository", "repository_path", "repo_settings"] {
        assert!(table_exists(&read, t), "table {t} exists");
    }

    // The current-path index is a UNIQUE partial index on is_current = 1.
    let index_sql: String = read
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='index' AND name='repository_path_current'",
            [],
            |r| r.get(0),
        )
        .expect("read index sql");
    assert!(index_sql.contains("UNIQUE"), "index is UNIQUE: {index_sql}");
    assert!(
        index_sql.contains("WHERE is_current = 1"),
        "index is partial on is_current = 1: {index_sql}",
    );

    // Applied migrations: (1,"registry") — this task — plus (2,"worktree"),
    // (3,"code"), (4,"projection"), (5,"worktree_state_clock"),
    // (6,"representation"), (7,"observation"),
    // (8,"observation_redaction_version"), (9,"memory"),
    // (10,"managed_worktree"), (11,"consolidation_run_failure_tracking"), and
    // (12,"consolidation_run_context_overflow_tracking"), appended by later
    // groups.
    let mut stmt = read
        .prepare("SELECT version, name FROM schema_migrations ORDER BY version")
        .expect("prepare migration rows");
    let rows: Vec<(i64, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .expect("query migration rows")
        .collect::<Result<_, _>>()
        .expect("collect migration rows");
    assert_eq!(
        rows,
        vec![
            (1, "registry".to_string()),
            (2, "worktree".to_string()),
            (3, "code".to_string()),
            (4, "projection".to_string()),
            (5, "worktree_state_clock".to_string()),
            (6, "representation".to_string()),
            (7, "observation".to_string()),
            (8, "observation_redaction_version".to_string()),
            (9, "memory".to_string()),
            (10, "managed_worktree".to_string()),
            (11, "consolidation_run_failure_tracking".to_string()),
            (
                12,
                "consolidation_run_context_overflow_tracking".to_string()
            ),
        ],
        "the production set is [v1 registry, v2 worktree, v3 code, v4 projection, \
         v5 worktree_state_clock, v6 representation, v7 observation, \
         v8 observation_redaction_version, v9 memory, v10 managed_worktree, \
         v11 consolidation_run_failure_tracking, \
         v12 consolidation_run_context_overflow_tracking] at D-058",
    );
}

/// `repository` has no `canonical_path` column: `repository_path` is the single
/// source of the current path (spec 01 §5 / 03 §2.1).
#[tokio::test]
async fn repository_has_no_canonical_path_column() {
    let (_home, db) = open_state();
    let read = db.open_read().expect("read conn");

    let mut stmt = read
        .prepare("SELECT name FROM pragma_table_info('repository')")
        .expect("prepare table_info");
    let mut columns: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .expect("query columns")
        .collect::<Result<_, _>>()
        .expect("collect columns");
    columns.sort();

    let mut expected = vec![
        "repo_id".to_string(),
        "git_remote_fingerprint".to_string(),
        "created_at".to_string(),
        "last_seen_at".to_string(),
    ];
    expected.sort();
    assert_eq!(columns, expected, "exactly the four §2.1 columns");
    assert!(
        !columns.iter().any(|c| c == "canonical_path"),
        "no path-derived identity column on repository",
    );
}

/// `find_repository_by_path` matches only the current path: after a move, the
/// old path no longer resolves, but it is still present in history.
#[tokio::test]
async fn find_repository_by_current_path_returns_current_repo() {
    let (_home, db) = open_state();
    let id = repo_id(10);

    let id0 = id.clone();
    db.writer()
        .transaction(move |tx| {
            create_repository(tx, &id0, None, 1000)?;
            observe_repository_path(tx, &id0, "/loc/a", 1000)
        })
        .await
        .expect("create + observe A");

    {
        let read = db.open_read().expect("read conn");
        assert_eq!(
            find_repository_by_path(&read, "/loc/a").expect("find A"),
            Some(id.clone()),
            "A resolves while current",
        );
    }

    // Move current to B.
    let id1 = id.clone();
    db.writer()
        .transaction(move |tx| observe_repository_path(tx, &id1, "/loc/b", 2000))
        .await
        .expect("observe B");

    let read = db.open_read().expect("read conn");
    assert_eq!(
        find_repository_by_path(&read, "/loc/a").expect("find A after move"),
        None,
        "A no longer current ⇒ not resolved",
    );
    assert_eq!(
        find_repository_by_path(&read, "/loc/b").expect("find B"),
        Some(id.clone()),
        "B is now current",
    );
    // But A remains in history.
    let history = path_history(&read, &id).expect("history");
    assert!(
        history.iter().any(|p| p.observed_path == "/loc/a"),
        "A retained in history",
    );
}

/// `all_repository_ids` is empty on a fresh store and, once populated, returns
/// every `repo_id` in ascending order regardless of insertion order (T15-07,
/// mirrors `worktree::all_worktree_ids`'s own acceptance test shape).
#[tokio::test]
async fn all_repository_ids_lists_every_repo_ascending() {
    let (_home, db) = open_state();

    {
        let read = db.open_read().expect("read conn");
        assert_eq!(
            all_repository_ids(&read).expect("empty store"),
            Vec::<String>::new(),
        );
    }

    let (id_a, id_b, id_c) = (repo_id(20), repo_id(21), repo_id(22));
    let mut expected = vec![id_a.clone(), id_b.clone(), id_c.clone()];
    expected.sort();

    // Insert in a different order than the expected sorted output, to prove
    // the result is genuinely ordered by the query, not by insertion order.
    for id in [&id_b, &id_c, &id_a] {
        let id = id.clone();
        db.writer()
            .transaction(move |tx| create_repository(tx, &id, None, 1000))
            .await
            .expect("create repository");
    }

    let read = db.open_read().expect("read conn");
    assert_eq!(all_repository_ids(&read).expect("all ids"), expected);
}
