//! T02-03 acceptance tests for the worktree registry (spec 03 §2.1, 04 §7,
//! 01 §5).
//!
//! All tests are deterministic: an isolated [`TempHome`], fixed `now_ms`
//! literals, and ids minted from [`uuidv7_from`] with fixed entropy (no
//! `SystemUuidV7`, so no wall clock or `/dev/urandom`). Writer operations run
//! through [`StateWriter::transaction`]; reads use [`StateDb::open_read`].
//!
//! Pure state-machine coverage (round-trips, the full `check_transition` matrix)
//! lives in the `worktree` module's unit tests; these tests exercise the DB
//! operations and the schema invariants.

use local_rag_core::identity::domain::path_fingerprint;
use local_rag_core::identity::uuidv7_from;
use local_rag_core::paths::StoreLayout;
use local_rag_store::registry::{
    IllegalWorktreeTransition, WorktreeKind, WorktreeState, WorktreeTransitionError,
    create_repository, create_worktree, current_generation, current_worktree_path,
    find_worktrees_by_path_fingerprint, observe_worktree_path, transition_worktree_state,
    worktree_path_history, worktree_state,
};
use local_rag_store::rusqlite::Connection;
use local_rag_store::{StateDb, WriteError};
use local_rag_test_support::TempHome;

/// A temporary store with an ensured tree and an opened [`StateDb`] (which runs
/// the production migration set, including registry v1 and worktree v2).
fn open_state() -> (TempHome, StateDb) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    (home, db)
}

/// A distinct, deterministic UUIDv7 string. `seed` varies the last entropy byte
/// (never masked by version/variant stamping), so distinct seeds yield distinct
/// ids without touching the clock.
fn uuid(seed: u8) -> String {
    let mut rand = [0u8; 10];
    rand[9] = seed;
    uuidv7_from(1000, rand).to_string()
}

/// Create a repository and one `active` worktree of `kind` under it, in one
/// transaction. Returns `(repo_id, worktree_id)`.
async fn repo_with_worktree(db: &StateDb, seed: u8, kind: WorktreeKind) -> (String, String) {
    let repo = uuid(seed);
    let wt = uuid(seed.wrapping_add(100));
    let (repo0, wt0) = (repo.clone(), wt.clone());
    db.writer()
        .transaction(move |tx| {
            create_repository(tx, &repo0, None, 1000)?;
            create_worktree(tx, &wt0, &repo0, kind, 1000)
        })
        .await
        .expect("create repo + worktree");
    (repo, wt)
}

/// Insert a `generation` row directly (raw SQL): the generation builder and its
/// state machine are group 05 — here we only need rows to prove the worktree
/// composite-FK seam.
async fn insert_generation(db: &StateDb, generation_id: &str, worktree_id: &str, number: i64) {
    let (g, w) = (generation_id.to_string(), worktree_id.to_string());
    db.writer()
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO generation \
                   (generation_id, worktree_id, generation_number, state, created_at) \
                 VALUES (?1, ?2, ?3, 'active', 1000)",
                (&g, &w, number),
            )
            .map(|_| ())
        })
        .await
        .expect("insert generation");
}

fn current_path_count(conn: &Connection, worktree_id: &str) -> i64 {
    conn.query_row(
        "SELECT count(*) FROM worktree_path WHERE worktree_id = ?1 AND is_current = 1",
        [worktree_id],
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

/// A convenience wrapper: transition and unwrap the outer (infrastructure)
/// [`Result`], returning the inner domain [`Result`].
async fn transition(
    db: &StateDb,
    worktree_id: &str,
    to: WorktreeState,
) -> Result<(), WorktreeTransitionError> {
    let w = worktree_id.to_string();
    db.writer()
        .transaction(move |tx| transition_worktree_state(tx, &w, to))
        .await
        .expect("transition tx (infrastructure)")
}

/// The worktree id is a UUID; it never equals and is never derived from the
/// path fingerprint, which is stored only as a lookup accelerator (spec 01 §5).
#[tokio::test]
async fn worktree_id_is_not_and_does_not_define_path_fingerprint() {
    let (_home, db) = open_state();
    let (_repo, wt) = repo_with_worktree(&db, 1, WorktreeKind::Main).await;

    let canonical = "/work/proj".to_string();
    let display = "/work/Proj".to_string();
    let fp = path_fingerprint(&canonical);

    // The id is entirely independent of the fingerprint.
    assert_ne!(wt, fp, "worktree_id is not the path fingerprint");
    assert!(wt.contains('-'), "worktree_id is a UUID: {wt}");
    assert_eq!(fp.len(), 64, "fingerprint is a BLAKE3 hex digest");
    assert!(fp.bytes().all(|b| b.is_ascii_hexdigit()));

    let (wt0, c0, d0, fp0) = (wt.clone(), canonical.clone(), display.clone(), fp.clone());
    db.writer()
        .transaction(move |tx| observe_worktree_path(tx, &wt0, &c0, &d0, &fp0, 1000))
        .await
        .expect("observe path");

    let read = db.open_read().expect("read conn");

    // The fingerprint is stored, verbatim, only in worktree_path.path_fingerprint.
    let stored_fp: String = read
        .query_row(
            "SELECT path_fingerprint FROM worktree_path WHERE worktree_id = ?1",
            [&wt],
            |r| r.get(0),
        )
        .expect("read fingerprint");
    assert_eq!(stored_fp, fp);

    // The display spelling is preserved (identity never depends on it, 03 §1.3).
    let stored_display: String = read
        .query_row(
            "SELECT display_path FROM worktree_path WHERE worktree_id = ?1",
            [&wt],
            |r| r.get(0),
        )
        .expect("read display");
    assert_eq!(stored_display, display);

    // Looking a fingerprint up yields the durable UUID, not the fingerprint.
    let found = find_worktrees_by_path_fingerprint(&read, &fp).expect("find by fp");
    assert_eq!(found, vec![wt.clone()], "fp is a lookup key → the UUID");
    assert!(
        !found.contains(&fp),
        "the fingerprint is never itself an id"
    );
}

/// `worktree` carries no path-derived identity column: exactly the §2.1 columns,
/// and no `canonical_path`/path column (spec 01 §5 / 03 §2.1).
#[tokio::test]
async fn worktree_has_no_path_derived_identity_column() {
    let (_home, db) = open_state();
    let read = db.open_read().expect("read conn");

    let mut stmt = read
        .prepare("SELECT name FROM pragma_table_info('worktree')")
        .expect("prepare table_info");
    let mut columns: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .expect("query columns")
        .collect::<Result<_, _>>()
        .expect("collect columns");
    columns.sort();

    let mut expected = vec![
        "worktree_id".to_string(),
        "repo_id".to_string(),
        "kind".to_string(),
        "current_generation_id".to_string(),
        "state".to_string(),
        "created_at".to_string(),
        "last_seen_at".to_string(),
    ];
    expected.sort();
    assert_eq!(columns, expected, "exactly the seven §2.1 columns");
    for forbidden in [
        "canonical_path",
        "observed_path",
        "path",
        "path_fingerprint",
    ] {
        assert!(
            !columns.iter().any(|c| c == forbidden),
            "no path-derived column `{forbidden}` on worktree",
        );
    }
}

/// The composite FK proves a worktree's current generation belongs to THAT
/// worktree (spec 03 §2.1): pointing at another worktree's generation is
/// rejected; pointing at its own succeeds.
#[tokio::test]
async fn composite_fk_rejects_cross_worktree_current_generation() {
    let (_home, db) = open_state();

    // One repo, two worktrees, one active generation each.
    let repo = uuid(2);
    let (w1, w2) = (uuid(20), uuid(21));
    let (repo0, w1a, w2a) = (repo.clone(), w1.clone(), w2.clone());
    db.writer()
        .transaction(move |tx| {
            create_repository(tx, &repo0, None, 1000)?;
            create_worktree(tx, &w1a, &repo0, WorktreeKind::Main, 1000)?;
            create_worktree(tx, &w2a, &repo0, WorktreeKind::Linked, 1000)
        })
        .await
        .expect("create repo + two worktrees");

    let (g1, g2) = (uuid(30), uuid(31));
    insert_generation(&db, &g1, &w1, 1).await;
    insert_generation(&db, &g2, &w2, 1).await;

    // Own generation: accepted.
    let (w1b, g1b) = (w1.clone(), g1.clone());
    db.writer()
        .transaction(move |tx| local_rag_store::set_current_generation(tx, &w1b, &g1b))
        .await
        .expect("set own generation");

    // Cross-worktree generation: the composite FK rejects (g2 belongs to w2).
    let (w1c, g2c) = (w1.clone(), g2.clone());
    let result = db
        .writer()
        .transaction(move |tx| local_rag_store::set_current_generation(tx, &w1c, &g2c))
        .await;
    assert!(
        matches!(result, Err(WriteError::Sqlite(_))),
        "pointing w1 at w2's generation must be rejected, got {result:?}",
    );

    // The rejected UPDATE rolled back: w1 still points at its own generation.
    let read = db.open_read().expect("read conn");
    assert_eq!(
        current_generation(&read, &w1).expect("current generation"),
        Some(g1.clone()),
        "w1's current generation is unchanged",
    );
}

/// Observing paths keeps exactly one current, and switching moves it to the last
/// observed path (spec 03 §2.1).
#[tokio::test]
async fn observe_sets_and_switches_single_current() {
    let (_home, db) = open_state();
    let (_repo, wt) = repo_with_worktree(&db, 3, WorktreeKind::Main).await;
    let (a, b) = ("/work/a".to_string(), "/work/b".to_string());

    for (path, now) in [(&a, 1000_i64), (&b, 2000), (&a, 3000)] {
        let (wt_n, p_n) = (wt.clone(), path.clone());
        let fp = path_fingerprint(path);
        db.writer()
            .transaction(move |tx| observe_worktree_path(tx, &wt_n, &p_n, &p_n, &fp, now))
            .await
            .expect("observe");
    }

    let read = db.open_read().expect("read conn");
    assert_eq!(
        current_path_count(&read, &wt),
        1,
        "exactly one current path"
    );
    assert_eq!(
        current_worktree_path(&read, &wt).expect("current path"),
        Some(a.clone()),
        "the last observed path is current",
    );
}

/// The `worktree_path_current` partial unique index rejects a forced second
/// current path (spec 03 §2.1).
#[tokio::test]
async fn partial_unique_index_rejects_two_current() {
    let (_home, db) = open_state();
    let (_repo, wt) = repo_with_worktree(&db, 4, WorktreeKind::Main).await;

    let (wt0, fp) = (wt.clone(), path_fingerprint("/work/a"));
    db.writer()
        .transaction(move |tx| observe_worktree_path(tx, &wt0, "/work/a", "/work/a", &fp, 1000))
        .await
        .expect("observe A");

    // Force a second current row directly — the index must reject it.
    let wt1 = wt.clone();
    let result = db
        .writer()
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO worktree_path \
                   (worktree_id, observed_canonical_path, display_path, path_fingerprint, \
                    is_current, first_seen_at, last_seen_at) \
                 VALUES (?1, '/work/b', '/work/b', 'fp', 1, 2000, 2000)",
                [&wt1],
            )
            .map(|_| ())
        })
        .await;
    assert!(
        matches!(result, Err(WriteError::Sqlite(_))),
        "a second current path must be rejected, got {result:?}",
    );

    let read = db.open_read().expect("read conn");
    assert_eq!(
        current_path_count(&read, &wt),
        1,
        "still exactly one current"
    );
    assert_eq!(
        current_worktree_path(&read, &wt).expect("current path"),
        Some("/work/a".to_string()),
        "the rejected insert left A current",
    );
}

/// Moving the current path retains history: the prior row survives with
/// `is_current = 0` and its original `first_seen_at` (spec 03 §2.1).
#[tokio::test]
async fn path_history_retained_across_move() {
    let (_home, db) = open_state();
    let (_repo, wt) = repo_with_worktree(&db, 5, WorktreeKind::Main).await;

    for (path, now) in [("/old/loc", 1000_i64), ("/new/loc", 2000)] {
        let (wt_n, p_n, fp) = (wt.clone(), path.to_string(), path_fingerprint(path));
        db.writer()
            .transaction(move |tx| observe_worktree_path(tx, &wt_n, &p_n, &p_n, &fp, now))
            .await
            .expect("observe");
    }

    let read = db.open_read().expect("read conn");
    let history = worktree_path_history(&read, &wt).expect("history");
    assert_eq!(history.len(), 2, "both observed paths retained");
    assert_eq!(history[0].observed_canonical_path, "/old/loc");
    assert!(!history[0].is_current, "old path no longer current");
    assert_eq!(history[0].first_seen_at, 1000, "old first_seen_at intact");
    assert_eq!(history[1].observed_canonical_path, "/new/loc");
    assert!(history[1].is_current);
    assert_eq!(current_path_count(&read, &wt), 1, "exactly one current");
}

/// Re-observing the current path is idempotent: no duplicate row, still current,
/// `first_seen_at` preserved and `last_seen_at`/`display`/`fp` refreshed (spec
/// 03 §2.1; retry safety per CLAUDE.md).
#[tokio::test]
async fn observe_is_idempotent_under_retry() {
    let (_home, db) = open_state();
    let (_repo, wt) = repo_with_worktree(&db, 6, WorktreeKind::Main).await;

    let (wt0, fp0) = (wt.clone(), path_fingerprint("/loc"));
    db.writer()
        .transaction(move |tx| observe_worktree_path(tx, &wt0, "/loc", "/Loc", &fp0, 1000))
        .await
        .expect("observe @1000");
    let (wt1, fp1) = (wt.clone(), path_fingerprint("/loc"));
    db.writer()
        .transaction(move |tx| observe_worktree_path(tx, &wt1, "/loc", "/LOC", &fp1, 2000))
        .await
        .expect("re-observe @2000");

    let read = db.open_read().expect("read conn");
    let history = worktree_path_history(&read, &wt).expect("history");
    assert_eq!(history.len(), 1, "no duplicate row on retry");
    let row = &history[0];
    assert_eq!(row.observed_canonical_path, "/loc");
    assert_eq!(row.first_seen_at, 1000, "first_seen preserved");
    assert_eq!(row.last_seen_at, 2000, "last_seen bumped");
    assert_eq!(row.display_path, "/LOC", "display refreshed");
    assert!(row.is_current);
}

/// Detach then reattach retains the worktree id (spec 04 §7): the same durable
/// UUID survives active → detached → active, even across a path move.
#[tokio::test]
async fn detach_and_reattach_retains_worktree_id() {
    let (_home, db) = open_state();
    let (_repo, wt) = repo_with_worktree(&db, 7, WorktreeKind::Main).await;

    let (wt0, fp0) = (wt.clone(), path_fingerprint("/old"));
    db.writer()
        .transaction(move |tx| observe_worktree_path(tx, &wt0, "/old", "/old", &fp0, 1000))
        .await
        .expect("observe old path");

    // Path no longer resolves → detach.
    assert_eq!(transition(&db, &wt, WorktreeState::Detached).await, Ok(()));
    {
        let read = db.open_read().expect("read conn");
        assert_eq!(
            worktree_state(&read, &wt).expect("state"),
            Some(WorktreeState::Detached),
        );
    }

    // Reattach at a new location (the id is unchanged), then back to active.
    let (wt1, fp1) = (wt.clone(), path_fingerprint("/new"));
    db.writer()
        .transaction(move |tx| observe_worktree_path(tx, &wt1, "/new", "/new", &fp1, 2000))
        .await
        .expect("observe new path");
    assert_eq!(transition(&db, &wt, WorktreeState::Active).await, Ok(()));

    let read = db.open_read().expect("read conn");
    assert_eq!(
        worktree_state(&read, &wt).expect("state"),
        Some(WorktreeState::Active),
        "back to active",
    );
    // The id resolves to the same, single worktree row.
    let count: i64 = read
        .query_row(
            "SELECT count(*) FROM worktree WHERE worktree_id = ?1",
            [&wt],
            |r| r.get(0),
        )
        .expect("count worktree rows");
    assert_eq!(count, 1, "the same durable worktree id survived");
    assert_eq!(
        worktree_path_history(&read, &wt).expect("history").len(),
        2,
        "both paths retained across detach/reattach",
    );
}

/// An illegal transition returns a typed domain error and leaves state unchanged
/// (spec 04 §7; the transaction commits a no-op because no write ran).
#[tokio::test]
async fn illegal_transition_is_typed_error() {
    let (_home, db) = open_state();
    let (_repo, wt) = repo_with_worktree(&db, 8, WorktreeKind::Main).await;

    // active → removing is legal (terminal).
    assert_eq!(transition(&db, &wt, WorktreeState::Removing).await, Ok(()));

    // removing → active is illegal: a typed rejection, not a coercion.
    assert_eq!(
        transition(&db, &wt, WorktreeState::Active).await,
        Err(WorktreeTransitionError::Illegal(
            IllegalWorktreeTransition {
                from: WorktreeState::Removing,
                to: WorktreeState::Active,
            }
        )),
    );

    let read = db.open_read().expect("read conn");
    assert_eq!(
        worktree_state(&read, &wt).expect("state"),
        Some(WorktreeState::Removing),
        "state unchanged after the rejected transition",
    );
}

/// Transitioning a worktree that does not exist is a typed `UnknownWorktree`
/// (not an infrastructure error).
#[tokio::test]
async fn unknown_worktree_transition_is_typed_error() {
    let (_home, db) = open_state();
    let ghost = uuid(9); // never created
    assert_eq!(
        transition(&db, &ghost, WorktreeState::Detached).await,
        Err(WorktreeTransitionError::UnknownWorktree),
    );
}

/// Creating a worktree for an unknown repository is rejected by the foreign key,
/// leaving no row (spec 03 §2.1).
#[tokio::test]
async fn create_worktree_unknown_repo_rejected() {
    let (_home, db) = open_state();
    let (ghost_repo, wt) = (uuid(10), uuid(110));

    let (repo0, wt0) = (ghost_repo.clone(), wt.clone());
    let result = db
        .writer()
        .transaction(move |tx| create_worktree(tx, &wt0, &repo0, WorktreeKind::Main, 1000))
        .await;
    assert!(
        matches!(result, Err(WriteError::Sqlite(_))),
        "create on an unknown repo must be rejected, got {result:?}",
    );

    let read = db.open_read().expect("read conn");
    let n: i64 = read
        .query_row("SELECT count(*) FROM worktree", [], |r| r.get(0))
        .expect("count worktrees");
    assert_eq!(n, 0, "the rejected create left no worktree row");
}

/// The v2 migration produces exactly the worktree-side schema: the three tables,
/// the current-path partial unique index, the fingerprint lookup index, and the
/// composite FK into `generation` (spec 03 §2.1).
#[tokio::test]
async fn migration_produces_exact_worktree_schema() {
    let (_home, db) = open_state();
    let read = db.open_read().expect("read conn");

    for t in ["worktree", "worktree_path", "generation"] {
        assert!(table_exists(&read, t), "table {t} exists");
    }

    // The current-path index is a UNIQUE partial index on is_current = 1.
    let index_sql: String = read
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='index' AND name='worktree_path_current'",
            [],
            |r| r.get(0),
        )
        .expect("read current-index sql");
    assert!(
        index_sql.contains("UNIQUE"),
        "current index UNIQUE: {index_sql}"
    );
    assert!(
        index_sql.contains("WHERE is_current = 1"),
        "current index is partial: {index_sql}",
    );

    // The fingerprint lookup index exists.
    let fp_index: i64 = read
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='index' AND name='worktree_path_fp'",
            [],
            |r| r.get(0),
        )
        .expect("count fp index");
    assert_eq!(fp_index, 1, "worktree_path_fp lookup index exists");

    // The composite FK on worktree targets generation(generation_id, worktree_id).
    let fk_count: i64 = read
        .query_row(
            "SELECT count(*) FROM pragma_foreign_key_list('worktree') \
             WHERE \"table\" = 'generation'",
            [],
            |r| r.get(0),
        )
        .expect("read worktree FKs");
    assert_eq!(fk_count, 2, "the two-column composite FK into generation");
}
