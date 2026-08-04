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
