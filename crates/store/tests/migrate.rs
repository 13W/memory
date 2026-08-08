//! T01-03 acceptance tests for the forward-only migration runner (spec 13 §3,
//! spec 02 §4.1/§5).
//!
//! The six card scenarios drive [`migrate::run`] directly with **synthetic**
//! migration sets so behavior is exercised without the real (empty) production
//! set. All tests are deterministic: isolated [`TempHome`], a fixed `now_ms`
//! literal for byte-stable `applied_at`, and — for concurrency — a
//! [`std::sync::Barrier`] gate instead of any wall-clock sleep.

use std::sync::{Arc, Barrier};

use local_rag_core::paths::StoreLayout;
use local_rag_store::migrate::{Migration, MigrationError, run};
use local_rag_store::rusqlite::Connection;
use local_rag_store::{
    StateDb, VersionDiagnosis, create_repository, rusqlite, worktree_state_clocks,
};
use local_rag_test_support::TempHome;

mod support;
use support::{raw_conn, temp_store};

// Synthetic migrations. Each creates a distinct table so "applied" is
// observable. `M1B` collides with `M1`'s version but carries different SQL (a
// different checksum) — the drift fixture. All are simple (non-destructive,
// SQL-only) so they exercise the T01-03 atomic apply path unchanged.
const M1: Migration = Migration::sql(1, "one", "CREATE TABLE t1 (x INTEGER);");
const M2: Migration = Migration::sql(2, "two", "CREATE TABLE t2 (x INTEGER);");
const M3: Migration = Migration::sql(3, "three", "CREATE TABLE t3 (x INTEGER);");
const M4: Migration = Migration::sql(4, "four", "CREATE TABLE t4 (x INTEGER);");
const M1B: Migration = Migration::sql(1, "one", "CREATE TABLE t1_altered (x INTEGER);");

// `&'static` sets so they can cross thread boundaries in the concurrency test.
const SET_12: &[Migration] = &[M1, M2];
const SET_123: &[Migration] = &[M1, M2, M3];
const SET_1234: &[Migration] = &[M1, M2, M3, M4];

/// All `schema_migrations` rows as `(version, name, checksum, applied_at)`.
fn migration_rows(conn: &Connection) -> Vec<(u32, String, String, i64)> {
    let mut stmt = conn
        .prepare(
            "SELECT version, name, checksum, applied_at FROM schema_migrations ORDER BY version",
        )
        .expect("prepare");
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)? as u32,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })
        .expect("query");
    rows.collect::<Result<Vec<_>, _>>().expect("collect rows")
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

/// empty → latest: a fresh store applies the whole set, records each row with
/// the correct checksum/name/timestamp, and creates every table.
#[test]
fn empty_to_latest_applies_all() {
    let (_home, layout) = temp_store();
    let mut conn = raw_conn(&layout.state_db());

    let report = run(&mut conn, SET_123, &layout.migration_lock(), 1000).expect("migrate");
    assert_eq!(report.applied, vec![1, 2, 3]);
    assert_eq!(report.store_version, 3);

    let rows = migration_rows(&conn);
    assert_eq!(rows.len(), 3);
    for (i, m) in SET_123.iter().enumerate() {
        assert_eq!(rows[i].0, m.version);
        assert_eq!(rows[i].1, m.name);
        assert_eq!(rows[i].2, m.checksum());
        assert_eq!(rows[i].3, 1000, "applied_at is the passed now_ms");
    }
    for t in ["t1", "t2", "t3"] {
        assert!(table_exists(&conn, t), "{t} created");
    }
    // Bootstrap created both framework tables too.
    assert!(table_exists(&conn, "schema_migrations"));
    assert!(table_exists(&conn, "store_settings"));
}

/// older → latest: applying a superset applies only the missing tail; the rows
/// already present keep their original `applied_at` (no re-apply).
#[test]
fn older_to_latest_applies_only_new() {
    let (_home, layout) = temp_store();
    let mut conn = raw_conn(&layout.state_db());

    run(&mut conn, SET_12, &layout.migration_lock(), 1000).expect("first migrate");
    let report = run(&mut conn, SET_1234, &layout.migration_lock(), 2000).expect("second migrate");
    assert_eq!(report.applied, vec![3, 4]);
    assert_eq!(report.store_version, 4);

    let rows = migration_rows(&conn);
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0].3, 1000, "v1 keeps its original applied_at");
    assert_eq!(rows[1].3, 1000, "v2 keeps its original applied_at");
    assert_eq!(rows[2].3, 2000, "v3 applied in the second run");
    assert_eq!(rows[3].3, 2000, "v4 applied in the second run");
}

/// checksum drift: re-running a version with altered SQL is a hard error and
/// leaves the store untouched.
#[test]
fn checksum_drift_is_rejected() {
    let (_home, layout) = temp_store();
    let mut conn = raw_conn(&layout.state_db());

    run(&mut conn, &[M1], &layout.migration_lock(), 1000).expect("apply v1");
    let err = run(&mut conn, &[M1B], &layout.migration_lock(), 2000).expect_err("drift");
    match err {
        MigrationError::ChecksumDrift {
            version,
            expected,
            found,
            ..
        } => {
            assert_eq!(version, 1);
            assert_eq!(expected, M1B.checksum());
            assert_eq!(found, M1.checksum());
            assert_ne!(expected, found);
        }
        other => panic!("expected ChecksumDrift, got {other:?}"),
    }

    // Store unchanged: still one row carrying M1's checksum and timestamp.
    let rows = migration_rows(&conn);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].2, M1.checksum());
    assert_eq!(rows[0].3, 1000);
    // The altered migration's table was never created.
    assert!(!table_exists(&conn, "t1_altered"));
}

/// newer store: a binary whose set is older than the store refuses to touch it.
#[test]
fn newer_store_is_rejected() {
    let (_home, layout) = temp_store();
    let mut conn = raw_conn(&layout.state_db());

    run(&mut conn, SET_123, &layout.migration_lock(), 1000).expect("apply to v3");
    let err = run(&mut conn, SET_12, &layout.migration_lock(), 2000).expect_err("incompatible");
    match err {
        MigrationError::IncompatibleStore {
            store_version,
            binary_max_version,
        } => {
            assert_eq!(store_version, 3);
            assert_eq!(binary_max_version, 2);
        }
        other => panic!("expected IncompatibleStore, got {other:?}"),
    }

    // Store unchanged: still three rows.
    assert_eq!(migration_rows(&conn).len(), 3);
}

/// malformed set: a migration set that is not strictly-increasing-and-contiguous
/// -from-1 is a programming error caught up front by `validate_set`, before the
/// migration lock is taken or any framework table is bootstrapped.
#[test]
fn malformed_set_is_rejected() {
    let (_home, layout) = temp_store();
    let mut conn = raw_conn(&layout.state_db());

    // Does not start at version 1.
    let err = run(&mut conn, &[M2], &layout.migration_lock(), 1000).expect_err("not from 1");
    match err {
        MigrationError::MalformedSet { detail } => {
            assert!(
                detail.contains("expected version 1"),
                "unexpected detail: {detail}"
            );
        }
        other => panic!("expected MalformedSet, got {other:?}"),
    }

    // Contiguous-from-1 but with a gap (missing version 2).
    let err = run(&mut conn, &[M1, M3], &layout.migration_lock(), 1000).expect_err("gap at v2");
    match err {
        MigrationError::MalformedSet { detail } => {
            assert!(
                detail.contains("expected version 2"),
                "unexpected detail: {detail}"
            );
        }
        other => panic!("expected MalformedSet, got {other:?}"),
    }

    // Rejected before any write: validate_set runs ahead of bootstrap, so the
    // framework tables were never created on this store.
    assert!(
        !table_exists(&conn, "schema_migrations"),
        "no bootstrap on a malformed set"
    );
}

/// concurrent migrator exclusion: two migrators race on the same store; L1
/// (flock) serializes them so both return `Ok` and the set is applied exactly
/// once. If L1 were broken, both would read store_version 0 and the loser's
/// `INSERT version=1` would hit the PK, turning its run into `Err`.
#[test]
fn concurrent_migrators_apply_exactly_once() {
    let (_home, layout) = temp_store();
    let state_path = layout.state_db();
    let lock_path = layout.migration_lock();

    // Pre-initialise the store to WAL in this thread so the two workers don't
    // race on SQLite's *one-time* journal-mode switch (which needs an exclusive
    // header rewrite and can return SQLITE_BUSY under contention). Opening an
    // already-WAL database is a no-op, so the only race left is the one under
    // test: both migrators contending on the migration lock (L1). (D-001.)
    drop(raw_conn(&state_path));

    let barrier = Arc::new(Barrier::new(2));

    let handles: Vec<_> = (0..2)
        .map(|_| {
            let barrier = Arc::clone(&barrier);
            let state_path = state_path.clone();
            let lock_path = lock_path.clone();
            std::thread::spawn(move || {
                let mut conn = raw_conn(&state_path);
                barrier.wait(); // maximize overlap
                run(&mut conn, SET_123, &lock_path, 1000)
            })
        })
        .collect();

    let results: Vec<_> = handles
        .into_iter()
        .map(|h| h.join().expect("join"))
        .collect();
    assert!(
        results.iter().all(|r| r.is_ok()),
        "both migrators must succeed, got {results:?}"
    );

    // Exactly one migrator applied all three; the other applied none.
    let mut applied_lens: Vec<usize> = results
        .iter()
        .map(|r| r.as_ref().unwrap().applied.len())
        .collect();
    applied_lens.sort_unstable();
    assert_eq!(applied_lens, vec![0, 3]);

    // Final state: exactly versions {1,2,3}, one row each (no double-apply).
    let conn = raw_conn(&state_path);
    let rows = migration_rows(&conn);
    assert_eq!(
        rows.iter().map(|r| r.0).collect::<Vec<_>>(),
        vec![1, 2, 3],
        "exactly one row per version"
    );
}

/// repeated open no-op: re-running the same set applies nothing and does not
/// rewrite the existing rows' timestamps.
#[test]
fn repeated_run_is_noop() {
    let (_home, layout) = temp_store();
    let mut conn = raw_conn(&layout.state_db());

    run(&mut conn, SET_12, &layout.migration_lock(), 1000).expect("first migrate");
    let report = run(&mut conn, SET_12, &layout.migration_lock(), 2000).expect("second migrate");
    assert!(report.applied.is_empty(), "nothing applied on re-run");
    assert_eq!(report.store_version, 2);

    let rows = migration_rows(&conn);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].3, 1000, "v1 timestamp unchanged");
    assert_eq!(rows[1].3, 1000, "v2 timestamp unchanged");
}

/// End-to-end: `StateDb::open` bootstraps the framework tables and applies the
/// real production set (registry migration v1 T02-02, worktree migration v2
/// T02-03, code migration v3 T03-01, projection migration v4 T07-02,
/// representation migration v6 T11-01), and a second open is a clean no-op.
#[test]
fn state_db_open_bootstraps_and_is_idempotent() {
    let (_home, layout) = temp_store();

    let db = StateDb::open(layout.state_db()).expect("first open");
    {
        let read = db.open_read().expect("read conn");
        let tables: i64 = read
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' \
                 AND name IN ('schema_migrations','store_settings')",
                [],
                |r| r.get(0),
            )
            .expect("count framework tables");
        assert_eq!(tables, 2, "bootstrap created both framework tables");
        // The production set applies v1 (repository side) …
        for t in ["repository", "repository_path", "repo_settings"] {
            assert!(table_exists(&read, t), "registry table {t} created");
        }
        // … v2 (worktree side) …
        for t in ["worktree", "worktree_path", "generation"] {
            assert!(table_exists(&read, t), "worktree table {t} created");
        }
        // … v3 (code storage: content-shared §2.3 + generation membership §2.4) …
        for t in [
            "file_revision",
            "content_blob",
            "parsed_unit",
            "generation_file",
            "skipped_file",
            "generation_unit_occurrence",
            "unresolved_reference",
            "resolved_graph_edge",
        ] {
            assert!(table_exists(&read, t), "code table {t} created");
        }
        // … v4 (projection deployment state + minimal model registry, §2.2) …
        for t in ["model_space", "worktree_projection_state"] {
            assert!(table_exists(&read, t), "projection table {t} created");
        }
        // … and v6 (representation registry, T11-01, §2.2).
        for t in ["representation", "model_space_representation"] {
            assert!(table_exists(&read, t), "representation table {t} created");
        }
        // … and v7 (spool-derived observation ledger, T13-04, §2.5 subset).
        for t in [
            "observation_envelope",
            "observation_path",
            "observation_payload",
            "spool_import_cursor",
        ] {
            assert!(table_exists(&read, t), "observation table {t} created");
        }
        // … and v9 (durable memory, T14-01, the remainder of §2.5).
        for t in [
            "memory_entry",
            "memory_evidence",
            "pending_memory_candidate",
            "candidate_evidence",
            "processing_cursor",
            "consolidation_run",
            "audit_event",
        ] {
            assert!(table_exists(&read, t), "memory table {t} created");
        }
        // … and v10 (daemon-managed indexing registry, T20-01, §2.1).
        assert!(
            table_exists(&read, "managed_worktree"),
            "managed_worktree table created"
        );
        // The v4 seed: the default model space is `active` and pointed at by
        // `store_settings.default_model_space_id` (spec 04 §3).
        let default_id: String = read
            .query_row(
                "SELECT value FROM store_settings WHERE key = 'default_model_space_id'",
                [],
                |r| r.get(0),
            )
            .expect("default_model_space_id seeded");
        let (name, state): (String, String) = read
            .query_row(
                "SELECT display_name, state FROM model_space WHERE model_space_id = ?1",
                [&default_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("default model space row");
        assert_eq!(name, "default", "default model space display_name");
        assert_eq!(state, "active", "default model space MUST be active");

        // Recorded as exactly ten rows: (1,"registry"), (2,"worktree"),
        // (3,"code"), (4,"projection"), (5,"worktree_state_clock"),
        // (6,"representation"), (7,"observation"),
        // (8,"observation_redaction_version"), (9,"memory"),
        // (10,"managed_worktree").
        let rows = migration_rows(&read);
        assert_eq!(
            rows.len(),
            10,
            "the production set is [v1,v2,v3,v4,v5,v6,v7,v8,v9,v10] at T20-01"
        );
        assert_eq!(rows[0].0, 1);
        assert_eq!(rows[0].1, "registry");
        assert_eq!(rows[1].0, 2);
        assert_eq!(rows[1].1, "worktree");
        assert_eq!(rows[2].0, 3);
        assert_eq!(rows[2].1, "code");
        assert_eq!(rows[3].0, 4);
        assert_eq!(rows[3].1, "projection");
        assert_eq!(rows[4].0, 5);
        assert_eq!(rows[4].1, "worktree_state_clock");
        assert_eq!(rows[5].0, 6);
        assert_eq!(rows[5].1, "representation");
        assert_eq!(rows[6].0, 7);
        assert_eq!(rows[6].1, "observation");
        assert_eq!(rows[7].0, 8);
        assert_eq!(rows[7].1, "observation_redaction_version");
        assert_eq!(rows[8].0, 9);
        assert_eq!(rows[8].1, "memory");
        assert_eq!(rows[9].0, 10);
        assert_eq!(rows[9].1, "managed_worktree");
    }
    drop(db);

    // Reopen on the same store: succeeds and records no new migrations.
    let db2 = StateDb::open(layout.state_db()).expect("second open");
    let read = db2.open_read().expect("read conn");
    let applied: i64 = read
        .query_row("SELECT count(*) FROM schema_migrations", [], |r| r.get(0))
        .expect("count migrations");
    assert_eq!(applied, 10, "reopen adds no new migration rows");
}

/// D-007: migration 5 adds `worktree.state_changed_at` and backfills every
/// pre-existing row from `last_seen_at`, so no row is left with a zero clock
/// that would make its shard instantly eligible for grace destruction
/// (spec 05 §8).
///
/// Applied against a store first migrated to version 4 only — the real upgrade
/// path — rather than a fresh full-set store, which would never exercise the
/// backfill (a fresh store has no rows to backfill).
#[tokio::test]
async fn migration_5_adds_and_backfills_the_worktree_state_clock() {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");

    // 1) Bring the store up to version 4 only, and seed a worktree there.
    let up_to_v4: Vec<_> = local_rag_store::ALL
        .iter()
        .filter(|m| m.version <= 4)
        .copied()
        .collect();
    {
        let mut conn = rusqlite::Connection::open(layout.state_db()).expect("raw conn");
        conn.pragma_update(None, "journal_mode", "WAL")
            .expect("wal");
        let report =
            run(&mut conn, &up_to_v4, &layout.migration_lock(), 1000).expect("migrate to v4");
        assert_eq!(report.store_version, 4);

        let tx = conn.transaction().expect("tx");
        create_repository(&tx, "repo-1", None, 1000).expect("repo");
        create_worktree_v4(&tx, "wt-1", "repo-1", 1000, 7777);
        tx.commit().expect("commit");
    }

    // 2) Opening with the full production set applies migration 5.
    let db = StateDb::open(layout.state_db()).expect("open applies v5");
    let read = db.open_read().expect("read conn");

    let clocks = worktree_state_clocks(&read).expect("read clocks");
    assert_eq!(clocks.len(), 1);
    assert_eq!(clocks[0].worktree_id, "wt-1");
    assert_eq!(
        clocks[0].state_changed_at, 7777,
        "the pre-existing row is backfilled from last_seen_at, not left at 0"
    );
}

/// D-019: migration 8 adds `observation_envelope.redaction_version` with
/// **no** backfill — unlike migration 5's `state_changed_at`, `NULL` is the
/// correct value for a pre-existing row (it predates the column entirely, so
/// no scanner version can honestly be attributed to it), not something that
/// would make anything incorrectly eligible for anything else.
///
/// Applied against a store first migrated to version 7 only — the real
/// upgrade path — with an envelope row inserted using the pre-migration
/// column list, so the assertion actually exercises "an old row gets NULL",
/// not merely "a fresh row can store a value".
#[tokio::test]
async fn migration_8_adds_the_envelope_redaction_version_column_with_no_backfill() {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");

    // 1) Bring the store up to version 7 only, and seed an envelope row using
    // the pre-migration-8 column list (no `redaction_version` column exists
    // yet at this point).
    let up_to_v7: Vec<_> = local_rag_store::ALL
        .iter()
        .filter(|m| m.version <= 7)
        .copied()
        .collect();
    {
        let mut conn = rusqlite::Connection::open(layout.state_db()).expect("raw conn");
        conn.pragma_update(None, "journal_mode", "WAL")
            .expect("wal");
        let report =
            run(&mut conn, &up_to_v7, &layout.migration_lock(), 1000).expect("migrate to v7");
        assert_eq!(report.store_version, 7);

        conn.execute(
            "INSERT INTO observation_envelope \
               (observation_id, source_event_id, dedup_key, payload_hash, event_type, \
                evidence_kind, trust, session_id) \
             VALUES ('obs-pre-v8', 'st:sess-1:x:1', NULL, 'deadbeef', 'Stop', \
                     'model_claim', 'low', 'sess-1')",
            [],
        )
        .expect("insert v7-shaped envelope row");
    }

    // 2) Opening with the full production set applies migration 8.
    let db = StateDb::open(layout.state_db()).expect("open applies v8");
    let read = db.open_read().expect("read conn");

    let redaction_version: Option<i64> = read
        .query_row(
            "SELECT redaction_version FROM observation_envelope WHERE observation_id = 'obs-pre-v8'",
            [],
            |r| r.get(0),
        )
        .expect("read redaction_version");
    assert_eq!(
        redaction_version, None,
        "a pre-existing row is left NULL, never backfilled to a fabricated version"
    );
}

/// T20-01: migration 10 adds `managed_worktree` to an **existing** v9 store —
/// the real forward-only upgrade path, not merely a fresh full-set open.
///
/// Applied against a store first migrated to version 9 only, with a real
/// `worktree` row already present, so the assertion exercises "an existing
/// store gains the table and can immediately enroll a pre-existing
/// worktree", which is what a user upgrading into daemon-managed indexing
/// actually does. Purely additive: nothing pre-existing needs a backfill
/// (unlike migration 5).
#[tokio::test]
async fn migration_10_adds_managed_worktree_to_an_existing_v9_store() {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");

    // 1) Bring the store up to version 9 only, and seed a real worktree using
    // the raw connection (no managed_worktree table exists yet at this point).
    let up_to_v9: Vec<_> = local_rag_store::ALL
        .iter()
        .filter(|m| m.version <= 9)
        .copied()
        .collect();
    {
        let mut conn = rusqlite::Connection::open(layout.state_db()).expect("raw conn");
        conn.pragma_update(None, "journal_mode", "WAL")
            .expect("wal");
        conn.pragma_update(None, "foreign_keys", true)
            .expect("fk on");
        let report =
            run(&mut conn, &up_to_v9, &layout.migration_lock(), 1000).expect("migrate to v9");
        assert_eq!(report.store_version, 9);
        assert!(
            !table_exists(&conn, "managed_worktree"),
            "managed_worktree must not exist before migration 10"
        );

        conn.execute(
            "INSERT INTO repository (repo_id, created_at, last_seen_at) \
             VALUES ('repo-pre-v10', 1000, 1000)",
            [],
        )
        .expect("seed repository");
        conn.execute(
            "INSERT INTO worktree \
               (worktree_id, repo_id, kind, current_generation_id, state, created_at, \
                last_seen_at, state_changed_at) \
             VALUES ('wt-pre-v10', 'repo-pre-v10', 'main', NULL, 'active', 1000, 1000, 1000)",
            [],
        )
        .expect("seed worktree");
    }

    // 2) Opening with the full production set applies migration 10, and the
    // pre-existing worktree can be enrolled immediately.
    let db = StateDb::open(layout.state_db()).expect("open applies v10");
    let read = db.open_read().expect("read conn");
    assert!(table_exists(&read, "managed_worktree"));

    db.writer()
        .transaction(|tx| {
            local_rag_store::registry::register_managed_worktree(tx, "wt-pre-v10", 2000)
        })
        .await
        .expect("enroll the pre-existing worktree");

    let read = db.open_read().expect("read conn");
    let rows = local_rag_store::registry::managed_worktrees(&read).expect("list");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].worktree_id, "wt-pre-v10");
}

/// A version-4-shaped `worktree` INSERT: the column list migration 5 has not
/// added yet, with an explicit `last_seen_at` the backfill can be observed
/// through. (`create_worktree` itself writes the post-migration column list, so
/// it cannot be used against a version-4 store.)
// ---------------------------------------------------------------------------
// T16-03: `StateDb::diagnose_versions` — read-only, never bootstraps/applies.
// ---------------------------------------------------------------------------

#[test]
fn diagnose_versions_reports_not_initialized_when_state_sqlite_is_absent() {
    let (_home, layout) = temp_store();
    // `temp_store()` only ensures the directory tree; `state.sqlite` itself
    // was never created.
    let diagnosis = StateDb::diagnose_versions(&layout.state_db(), SET_123).expect("diagnose");
    assert!(matches!(diagnosis, VersionDiagnosis::NotInitialized));
    assert!(
        !layout.state_db().exists(),
        "diagnose_versions must not create the file it is diagnosing"
    );
}

#[test]
fn diagnose_versions_reports_missing_bookkeeping_on_a_non_sqlite_file() {
    let (_home, layout) = temp_store();
    std::fs::write(layout.state_db(), b"not a sqlite database").expect("seed garbage file");

    let diagnosis = StateDb::diagnose_versions(&layout.state_db(), SET_123).expect("diagnose");
    assert!(matches!(diagnosis, VersionDiagnosis::MissingBookkeeping));
}

#[test]
fn diagnose_versions_reports_applied_with_empty_pending_on_a_fresh_store() {
    let (_home, layout) = temp_store();
    let mut conn = raw_conn(&layout.state_db());
    run(&mut conn, SET_123, &layout.migration_lock(), 1000).expect("migrate to v3");
    drop(conn);

    let diagnosis = StateDb::diagnose_versions(&layout.state_db(), SET_123).expect("diagnose");
    match diagnosis {
        VersionDiagnosis::Applied(report) => {
            assert_eq!(report.store_version, 3);
            assert_eq!(report.binary_max_version, 3);
            assert!(report.pending.is_empty());
        }
        other => panic!("expected Applied, got {other:?}"),
    }
}

#[test]
fn diagnose_versions_reports_pending_when_the_binary_set_has_new_migrations() {
    let (_home, layout) = temp_store();
    let mut conn = raw_conn(&layout.state_db());
    run(&mut conn, SET_12, &layout.migration_lock(), 1000).expect("migrate to v2");
    drop(conn);

    // Diagnose with the full 4-migration set: 2 are still pending.
    let diagnosis = StateDb::diagnose_versions(&layout.state_db(), SET_1234).expect("diagnose");
    match diagnosis {
        VersionDiagnosis::Applied(report) => {
            assert_eq!(report.store_version, 2);
            assert_eq!(report.binary_max_version, 4);
            assert_eq!(report.pending, vec![3, 4]);
        }
        other => panic!("expected Applied, got {other:?}"),
    }

    // The diagnosis itself never applied anything: a real `run()` afterward
    // still finds versions 3/4 pending, not already-applied.
    let mut conn = raw_conn(&layout.state_db());
    let report = run(&mut conn, SET_1234, &layout.migration_lock(), 2000).expect("real run");
    assert_eq!(
        report.applied,
        vec![3, 4],
        "diagnose_versions applied nothing on its own"
    );
}

#[test]
fn diagnose_versions_reports_fault_on_checksum_drift() {
    let (_home, layout) = temp_store();
    let mut conn = raw_conn(&layout.state_db());
    run(&mut conn, &[M1], &layout.migration_lock(), 1000).expect("apply v1");
    drop(conn);

    let diagnosis = StateDb::diagnose_versions(&layout.state_db(), &[M1B]).expect("diagnose");
    match diagnosis {
        VersionDiagnosis::Fault(MigrationError::ChecksumDrift { version, .. }) => {
            assert_eq!(version, 1);
        }
        other => panic!("expected Fault(ChecksumDrift), got {other:?}"),
    }
}

#[test]
fn diagnose_versions_reports_fault_on_incompatible_newer_store() {
    let (_home, layout) = temp_store();
    let mut conn = raw_conn(&layout.state_db());
    run(&mut conn, SET_123, &layout.migration_lock(), 1000).expect("apply to v3");
    drop(conn);

    let diagnosis = StateDb::diagnose_versions(&layout.state_db(), SET_12).expect("diagnose");
    match diagnosis {
        VersionDiagnosis::Fault(MigrationError::IncompatibleStore {
            store_version,
            binary_max_version,
        }) => {
            assert_eq!(store_version, 3);
            assert_eq!(binary_max_version, 2);
        }
        other => panic!("expected Fault(IncompatibleStore), got {other:?}"),
    }
}

#[test]
fn diagnose_versions_never_touches_the_migration_lock_file() {
    let (_home, layout) = temp_store();
    let mut conn = raw_conn(&layout.state_db());
    run(&mut conn, SET_123, &layout.migration_lock(), 1000).expect("apply to v3");
    drop(conn);

    // `run()` itself creates `migration.lock` (via `MigrationLock::acquire`'s
    // own `ensure_file_0600`) as a side effect of the *real* migration above —
    // this test isolates whether `diagnose_versions` touches it *again*, not
    // whether it exists at all.
    assert!(
        layout.migration_lock().exists(),
        "sanity: the real run() above did create migration.lock"
    );
    let before = std::fs::metadata(layout.migration_lock())
        .expect("stat before")
        .modified()
        .expect("mtime before");

    StateDb::diagnose_versions(&layout.state_db(), SET_123).expect("diagnose");

    let after = std::fs::metadata(layout.migration_lock())
        .expect("stat after")
        .modified()
        .expect("mtime after");
    assert_eq!(
        before, after,
        "diagnose_versions must never touch migration.lock"
    );
}

fn create_worktree_v4(
    tx: &rusqlite::Transaction<'_>,
    worktree_id: &str,
    repo_id: &str,
    created_at: i64,
    last_seen_at: i64,
) {
    tx.execute(
        "INSERT INTO worktree \
           (worktree_id, repo_id, kind, current_generation_id, state, created_at, last_seen_at) \
         VALUES (?1, ?2, 'main', NULL, 'active', ?3, ?4)",
        rusqlite::params![worktree_id, repo_id, created_at, last_seen_at],
    )
    .expect("insert v4-shaped worktree row");
}
