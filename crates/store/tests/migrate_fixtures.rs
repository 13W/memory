//! T17-04 acceptance tests: "migration tests run on fixture stores of every
//! prior released schema version" (spec 13 §3 `[FIXED]`).
//!
//! CLAUDE.md forbids committing generated SQLite files, so these fixtures are
//! not binary artifacts on disk — they are built on the fly from the real,
//! frozen production migration set (`local_rag_store::migrate::ALL`) via
//! `support::build_store_at_version`. This is trustworthy specifically because
//! [`local_rag_store::migrate::Migration::checksum`] freezes each entry's SQL
//! once shipped (`ALL`'s own doc comment: "later schema changes are new
//! entries here, never edits to an applied one"): `&ALL[..n]` for a historical
//! `n` is byte-identical to what the real release at that version produced —
//! it is the single source of truth, not a second, separately-maintained copy
//! of it. The synthetic `M1..M4` sets in `migrate.rs`/`migrate_resumable.rs`
//! remain useful for exercising the engine's *mechanics* in isolation; these
//! tests instead exercise the real, as-shipped chain end to end.

use local_rag_store::create_repository;
use local_rag_store::migrate::run;

mod support;
use support::{build_store_at_version, temp_store};

/// Every released schema version (`ALL[..1]`, `ALL[..2]`, … `ALL[..ALL.len()]`)
/// migrates cleanly to head: a store frozen at any prior release opens under
/// the current binary, applies exactly the expected tail, and lands at the
/// current `store_version` with nothing left pending.
#[test]
fn every_released_schema_version_migrates_cleanly_to_head() {
    let all = local_rag_store::migrate::ALL;
    for n in 1..=all.len() {
        let (_home, layout) = temp_store();
        let mut conn = build_store_at_version(&layout, n, 1_000);

        let report = run(&mut conn, all, &layout.migration_lock(), 2_000)
            .unwrap_or_else(|e| panic!("migrate fixture at v{n} forward to head: {e}"));

        let expected_tail: Vec<u32> = ((n as u32 + 1)..=all.len() as u32).collect();
        assert_eq!(
            report.applied, expected_tail,
            "fixture at v{n} must apply exactly the versions after it"
        );
        assert_eq!(
            report.store_version,
            all.len() as u32,
            "fixture at v{n} must reach head"
        );

        // A further run is a clean no-op: the fixture is not just reachable
        // once, it is a stable resting state for the real chain.
        let report2 = run(&mut conn, all, &layout.migration_lock(), 3_000)
            .unwrap_or_else(|e| panic!("re-run at head from fixture v{n}: {e}"));
        assert!(
            report2.applied.is_empty(),
            "fixture at v{n}: re-run at head applies nothing"
        );
    }
}

/// A real row seeded at the very first released schema version survives the
/// entire real migration chain to head — proving data continuity through the
/// actual as-shipped sequence, not merely that the engine's generic
/// crash/resume mechanics work on a synthetic set (already covered by
/// `migrate_resumable.rs`).
#[test]
fn a_row_seeded_at_the_first_released_version_survives_migration_to_head() {
    let (_home, layout) = temp_store();
    let mut conn = build_store_at_version(&layout, 1, 1_000);

    {
        let tx = conn.transaction().expect("tx");
        create_repository(&tx, "repo-fixture-v1", None, 1_000).expect("seed repository row");
        tx.commit().expect("commit seed");
    }

    let all = local_rag_store::migrate::ALL;
    let report =
        run(&mut conn, all, &layout.migration_lock(), 2_000).expect("migrate v1 fixture to head");
    assert_eq!(report.store_version, all.len() as u32);

    let seen: String = conn
        .query_row(
            "SELECT repo_id FROM repository WHERE repo_id = 'repo-fixture-v1'",
            [],
            |r| r.get(0),
        )
        .expect("the v1-seeded repository row is still present at head");
    assert_eq!(seen, "repo-fixture-v1");
}

/// Tripwire: every released migration is currently simple (non-destructive, no
/// Rust steps). The engine's checkpoint/backup machinery is already fully and
/// generically exercised against synthetic complex migrations
/// (`migrate_resumable.rs`); a crash-at-checkpoint test against a *real*
/// fixture has nothing to interrupt today and would add no coverage. The day
/// this assertion breaks — the first real destructive/stepped migration ships
/// — a genuine crash-at-checkpoint test against the real `ALL` chain becomes
/// both meaningful and required alongside it.
#[test]
fn no_released_migration_is_destructive_or_stepped_yet() {
    assert!(
        local_rag_store::migrate::ALL
            .iter()
            .all(|m| !m.destructive && m.steps.is_empty()),
        "a real destructive/stepped migration shipped: add a crash-at-checkpoint \
         test against the real ALL chain (see this test's doc comment)"
    );
}

/// T21-13: migration 15 does not merely reshape `memory_text_normalization` —
/// it decides, row by row, what each v14 state *means* under the English canon.
/// A migration that silently kept or silently dropped the wrong ones would leave
/// the queue and `doctor`'s exit code reading columns that no longer say what
/// they used to, so the mapping is pinned here rather than inferred.
///
/// `build_store_at_version` produces empty tables, so the three v14 states are
/// seeded by hand against a real v14 store and asserted after the real forward
/// chain runs.
#[test]
fn migration_15_carries_english_and_failed_rows_and_drops_pending_translations() {
    let all = local_rag_store::migrate::ALL;
    let (_home, layout) = temp_store();
    let mut conn = build_store_at_version(&layout, 14, 1_000);

    // A memory_entry per row: the table's FK is ON DELETE CASCADE, so an
    // orphaned normalization row could never exist in the first place.
    for id in ["m-english", "m-failed", "m-ready"] {
        conn.execute(
            "INSERT INTO memory_entry \
               (memory_id, kind, state, scope_kind, scope_owner_id, text, confidence, \
                importance, entry_version, created_at, updated_at) \
             VALUES (?1, 'fact', 'active', 'global', 'global', ?2, 0.6, 0.5, 1, 1000, 1000)",
            local_rag_store::rusqlite::params![id, format!("text of {id}")],
        )
        .expect("seed memory_entry");
    }
    let seed = |conn: &local_rag_store::rusqlite::Connection,
                id: &str,
                status: &str,
                text: Option<&str>| {
        conn.execute(
            "INSERT INTO memory_text_normalization \
               (memory_id, status, source_text_sha256, normalized_text, source_language, \
                normalizer_model_id, prompt_version, normalizer_version, attempt_count, \
                last_error, next_attempt_at, created_at, updated_at) \
             VALUES (?1, ?2, 'sha-of-the-text', ?3, 'ru', 'a-model', 1, 1, 5, \
                     'the envelope tore', 4242, 1000, 1000)",
            local_rag_store::rusqlite::params![id, status, text],
        )
        .expect("seed normalization row");
    };
    seed(&conn, "m-english", "skipped", None);
    seed(&conn, "m-failed", "failed", None);
    seed(&conn, "m-ready", "ready", Some("the English variant"));

    run(&mut conn, all, &layout.migration_lock(), 2_000).expect("migrate v14 fixture to head");

    let mut stmt = conn
        .prepare("SELECT memory_id, status, source_text FROM memory_text_normalization ORDER BY memory_id")
        .expect("prepare");
    let rows: Vec<(String, String, Option<String>)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("collect");

    assert_eq!(
        rows,
        vec![
            // `skipped` meant "already English"; it still does, under the name
            // the canon made accurate. Dropping it would make the detector
            // re-examine the entry on every tick forever.
            ("m-english".to_string(), "english".to_string(), None),
            // The retry bookkeeping survives intact — T21-16 releases these by
            // bumping the normalizer version, which is the owner's decision.
            ("m-failed".to_string(), "failed".to_string(), None),
        ],
        "`ready` rows are dropped on purpose: their English text was never \
         installed as canon, v15 has no state for a translation waiting to be \
         installed, and T21-16 replaces the translator that produced them",
    );

    let (attempts, last_error, next_attempt): (i64, Option<String>, Option<i64>) = conn
        .query_row(
            "SELECT attempt_count, last_error, next_attempt_at \
             FROM memory_text_normalization WHERE memory_id = 'm-failed'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("read the carried-over failure");
    assert_eq!(attempts, 5, "the dead-letter's attempt count must survive");
    assert_eq!(last_error.as_deref(), Some("the envelope tore"));
    assert_eq!(next_attempt, Some(4242));

    // The staleness basis is carried across under its new name, not lost.
    let canon_sha: String = conn
        .query_row(
            "SELECT canon_text_sha256 FROM memory_text_normalization WHERE memory_id = 'm-english'",
            [],
            |r| r.get(0),
        )
        .expect("read canon_text_sha256");
    assert_eq!(canon_sha, "sha-of-the-text");

    // The rebuild must leave the FK intact, or a later purge would orphan rows.
    let violations: i64 = conn
        .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })
        .expect("foreign_key_check");
    assert_eq!(
        violations, 0,
        "the rebuilt table kept its FK to memory_entry"
    );
}
