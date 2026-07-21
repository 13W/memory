//! T05-01 acceptance tests for the generation lifecycle (spec 03 §2.1, 04 §1):
//! per-worktree monotone allocation, the legal state machine, the worktree
//! composite-FK seam, and the "exactly one active per worktree" app invariant.
//!
//! All tests are deterministic: an isolated [`TempHome`], fixed `now_ms` literals,
//! and ids minted from [`uuidv7_from`] with fixed entropy (no `SystemUuidV7`, so
//! no wall clock or `/dev/urandom`). Writer operations run through
//! [`StateWriter::transaction`]; reads use [`StateDb::open_read`].
//!
//! Pure state-machine coverage (round-trips, the full `check_transition` matrix,
//! corrupt-enum reads) lives in the `registry::generation` module's unit tests;
//! these exercise the DB operations, the schema constraints, and the concurrency
//! model.

use std::collections::BTreeSet;

use local_rag_core::identity::uuidv7_from;
use local_rag_core::paths::StoreLayout;
use local_rag_store::registry::{
    GenerationState, GenerationTransitionError, IllegalGenerationTransition, WorktreeKind,
    active_generations, allocate_generation, create_repository, create_worktree,
    current_generation, generation_state, set_current_generation, transition_generation,
};
use local_rag_store::{StateDb, WriteError};
use local_rag_test_support::TempHome;

/// A temporary store with an ensured tree and an opened [`StateDb`] (runs the
/// production migration set: registry v1, worktree v2, code v3).
fn open_state() -> (TempHome, StateDb) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    (home, db)
}

/// A distinct, deterministic UUIDv7 string keyed by `seed` (last entropy byte),
/// never touching the clock or entropy source.
fn uuid(seed: u8) -> String {
    let mut rand = [0u8; 10];
    rand[9] = seed;
    uuidv7_from(1000, rand).to_string()
}

/// Create a repository and one `active` main worktree under it, in one
/// transaction. Returns `worktree_id`.
async fn worktree(db: &StateDb, seed: u8) -> String {
    let repo = uuid(seed);
    let wt = uuid(seed.wrapping_add(100));
    let (repo0, wt0) = (repo.clone(), wt.clone());
    db.writer()
        .transaction(move |tx| {
            create_repository(tx, &repo0, None, 1000)?;
            create_worktree(tx, &wt0, &repo0, WorktreeKind::Main, 1000)
        })
        .await
        .expect("create repo + worktree");
    wt
}

/// Allocate one generation for `worktree_id` (born `building`); returns its id.
async fn allocate(db: &StateDb, worktree_id: &str, gen_seed: u8) -> String {
    let genr = uuid(gen_seed);
    let (w, g) = (worktree_id.to_string(), genr.clone());
    db.writer()
        .transaction(move |tx| allocate_generation(tx, &w, &g, 1000).map(|_| ()))
        .await
        .expect("allocate generation");
    genr
}

/// Convenience: transition and unwrap the outer (infrastructure) result, returning
/// the inner domain result.
async fn transition(
    db: &StateDb,
    generation_id: &str,
    to: GenerationState,
) -> Result<(), GenerationTransitionError> {
    let g = generation_id.to_string();
    db.writer()
        .transaction(move |tx| transition_generation(tx, &g, to))
        .await
        .expect("transition tx (infrastructure)")
}

/// Allocation assigns a per-worktree monotone number starting at 1, over all
/// states — retiring/failed rows keep their numbers (spec 03 §2.1, 04 §1).
#[tokio::test]
async fn allocation_is_monotone_per_worktree() {
    let (_home, db) = open_state();
    let wt = worktree(&db, 1).await;

    for (i, expected) in [(10_u8, 1_i64), (11, 2), (12, 3)] {
        let genr = uuid(i);
        let (w, g) = (wt.clone(), genr.clone());
        let number = db
            .writer()
            .transaction(move |tx| allocate_generation(tx, &w, &g, 1000))
            .await
            .expect("allocate");
        assert_eq!(number, expected, "monotone per-worktree number");

        // A fresh generation is born `building`.
        let read = db.open_read().expect("read conn");
        assert_eq!(
            generation_state(&read, &genr).expect("state"),
            Some(GenerationState::Building),
        );
    }
}

/// Numbers are scoped per worktree: two worktrees each start their own sequence
/// at 1 (spec 03 §2.1 `UNIQUE (worktree_id, generation_number)`).
#[tokio::test]
async fn allocation_numbers_are_per_worktree() {
    let (_home, db) = open_state();
    let (w1, w2) = (worktree(&db, 2).await, worktree(&db, 3).await);

    let allocate_number = |wt: String, genr: String| {
        let db = &db;
        async move {
            db.writer()
                .transaction(move |tx| allocate_generation(tx, &wt, &genr, 1000))
                .await
                .expect("allocate")
        }
    };

    assert_eq!(allocate_number(w1.clone(), uuid(20)).await, 1);
    assert_eq!(
        allocate_number(w2.clone(), uuid(21)).await,
        1,
        "w2 starts fresh"
    );
    assert_eq!(allocate_number(w1.clone(), uuid(22)).await, 2);
    assert_eq!(allocate_number(w2.clone(), uuid(23)).await, 2);
}

/// Concurrent allocations for the same worktree all succeed with distinct
/// monotone numbers — the read-compute-write `MAX + 1` is race-free because the
/// single global writer (spec 03 §3) serializes it, and `UNIQUE (worktree_id,
/// generation_number)` is the tripwire that would catch any regression. The
/// per-task number assignment is nondeterministic, so only the *set* is asserted.
#[tokio::test]
async fn concurrent_allocation_yields_unique_numbers() {
    let (_home, db) = open_state();
    let wt = worktree(&db, 4).await;

    const N: u8 = 24;
    let mut handles = Vec::new();
    for i in 0..N {
        // Distinct generation ids; `i` varies a non-masked entropy byte.
        let genr = {
            let mut rand = [0u8; 10];
            rand[0] = 0xA0;
            rand[9] = i;
            uuidv7_from(1000, rand).to_string()
        };
        let writer = (*db.writer()).clone();
        let w = wt.clone();
        handles.push(tokio::spawn(async move {
            writer
                .transaction(move |tx| allocate_generation(tx, &w, &genr, 1000))
                .await
        }));
    }

    let mut numbers = BTreeSet::new();
    for h in handles {
        let number = h
            .await
            .expect("join")
            .expect("allocation never hits UNIQUE");
        assert!(numbers.insert(number), "number {number} allocated twice");
    }

    let expected: BTreeSet<i64> = (1..=i64::from(N)).collect();
    assert_eq!(
        numbers, expected,
        "numbers are exactly {{1..=N}}, no gaps/dupes"
    );
}

/// Allocating for an unknown worktree is rejected by the `generation.worktree_id`
/// foreign key; the transaction rolls back (spec 03 §2.1).
#[tokio::test]
async fn allocate_unknown_worktree_rejected() {
    let (_home, db) = open_state();
    let (ghost_wt, genr) = (uuid(5), uuid(55));

    let (w, g) = (ghost_wt.clone(), genr.clone());
    let result = db
        .writer()
        .transaction(move |tx| allocate_generation(tx, &w, &g, 1000))
        .await;
    assert!(
        matches!(result, Err(WriteError::Sqlite(_))),
        "unknown worktree rejected by FK, got {result:?}",
    );

    // Nothing was written.
    let read = db.open_read().expect("read conn");
    assert_eq!(generation_state(&read, &genr).expect("state"), None);
}

/// An illegal transition returns a typed domain error and leaves state unchanged
/// (spec 04 §1; the transaction commits a no-op because no write ran).
#[tokio::test]
async fn illegal_transition_is_typed_error_and_rolls_back() {
    let (_home, db) = open_state();
    let wt = worktree(&db, 6).await;
    let genr = allocate(&db, &wt, 60).await; // born `building`

    // building → active is illegal: it must pass through `projection_ready`.
    assert_eq!(
        transition(&db, &genr, GenerationState::Active).await,
        Err(GenerationTransitionError::Illegal(
            IllegalGenerationTransition {
                from: GenerationState::Building,
                to: GenerationState::Active,
            }
        )),
    );

    // The rejected transition left the state at `building`.
    let read = db.open_read().expect("read conn");
    assert_eq!(
        generation_state(&read, &genr).expect("state"),
        Some(GenerationState::Building),
        "state unchanged after the rejected transition",
    );
}

/// Transitioning a generation that does not exist is a typed `UnknownGeneration`
/// (not an infrastructure error).
#[tokio::test]
async fn unknown_generation_transition_is_typed_error() {
    let (_home, db) = open_state();
    let ghost = uuid(7); // never allocated
    assert_eq!(
        transition(&db, &ghost, GenerationState::ProjectionReady).await,
        Err(GenerationTransitionError::UnknownGeneration),
    );
}

/// The full happy-path walk `building → projection_ready → active → retiring`
/// succeeds through the guarded transition (spec 04 §1).
#[tokio::test]
async fn happy_path_walk_succeeds() {
    let (_home, db) = open_state();
    let wt = worktree(&db, 8).await;
    let genr = allocate(&db, &wt, 80).await;

    for to in [
        GenerationState::ProjectionReady,
        GenerationState::Active,
        GenerationState::Retiring,
    ] {
        assert_eq!(transition(&db, &genr, to).await, Ok(()), "→ {to:?}");
        let read = db.open_read().expect("read conn");
        assert_eq!(generation_state(&read, &genr).expect("state"), Some(to));
    }
}

/// The composite FK proves a worktree's current generation belongs to THAT
/// worktree (spec 03 §2.1), exercised end-to-end through the T05-01 primitives:
/// pointing at another worktree's generation is rejected; pointing at its own
/// succeeds.
#[tokio::test]
async fn cross_worktree_current_generation_rejected() {
    let (_home, db) = open_state();

    // One repo, two worktrees.
    let repo = uuid(9);
    let (w1, w2) = (uuid(90), uuid(91));
    let (repo0, w1a, w2a) = (repo.clone(), w1.clone(), w2.clone());
    db.writer()
        .transaction(move |tx| {
            create_repository(tx, &repo0, None, 1000)?;
            create_worktree(tx, &w1a, &repo0, WorktreeKind::Main, 1000)?;
            create_worktree(tx, &w2a, &repo0, WorktreeKind::Linked, 1000)
        })
        .await
        .expect("create repo + two worktrees");

    // Allocate one generation per worktree and drive each to `active`.
    let g1 = allocate(&db, &w1, 92).await;
    let g2 = allocate(&db, &w2, 93).await;
    for g in [&g1, &g2] {
        assert_eq!(
            transition(&db, g, GenerationState::ProjectionReady).await,
            Ok(())
        );
        assert_eq!(transition(&db, g, GenerationState::Active).await, Ok(()));
    }

    // Own generation: accepted.
    let (w1b, g1b) = (w1.clone(), g1.clone());
    db.writer()
        .transaction(move |tx| set_current_generation(tx, &w1b, &g1b))
        .await
        .expect("set own generation");

    // Cross-worktree generation: the composite FK rejects (g2 belongs to w2).
    let (w1c, g2c) = (w1.clone(), g2.clone());
    let result = db
        .writer()
        .transaction(move |tx| set_current_generation(tx, &w1c, &g2c))
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

/// The "exactly one active per worktree" app invariant (spec 04 §1) is upheld by
/// *sequencing* the switch — retire the outgoing active before promoting the
/// incoming one, in one transaction — and observed via [`active_generations`],
/// which never returns `retiring`/`failed` (`[FIXED]` routing). A negative control
/// proves the invariant is real: skipping the retire leaves two `active` rows.
#[tokio::test]
async fn one_active_invariant() {
    let (_home, db) = open_state();
    let wt = worktree(&db, 11).await;

    // Generation N: allocate and activate.
    let g_old = allocate(&db, &wt, 110).await;
    assert_eq!(
        transition(&db, &g_old, GenerationState::ProjectionReady).await,
        Ok(())
    );
    assert_eq!(
        transition(&db, &g_old, GenerationState::Active).await,
        Ok(())
    );

    // Generation N+1: allocate and bring to projection_ready.
    let g_new = allocate(&db, &wt, 111).await;
    assert_eq!(
        transition(&db, &g_new, GenerationState::ProjectionReady).await,
        Ok(())
    );

    let read = db.open_read().expect("read conn");
    assert_eq!(
        active_generations(&read, &wt).expect("active"),
        vec![g_old.clone()],
        "before the switch, exactly N is active",
    );
    drop(read);

    // Positive: a well-sequenced switch retires N, then activates N+1, in one tx.
    let (go, gn) = (g_old.clone(), g_new.clone());
    db.writer()
        .transaction(move |tx| {
            let retire = transition_generation(tx, &go, GenerationState::Retiring)?;
            assert!(retire.is_ok(), "retire N legal: {retire:?}");
            let promote = transition_generation(tx, &gn, GenerationState::Active)?;
            assert!(promote.is_ok(), "promote N+1 legal: {promote:?}");
            Ok(())
        })
        .await
        .expect("switch tx");

    let read = db.open_read().expect("read conn");
    assert_eq!(
        active_generations(&read, &wt).expect("active"),
        vec![g_new.clone()],
        "after the switch, exactly N+1 is active (retiring N is not routed)",
    );
    drop(read);

    // Negative control: allocate N+2 and activate it WITHOUT retiring N+1. The DB
    // does not forbid this — the invariant lives in the switch's sequencing, not a
    // constraint — so `active_generations` now returns two ids.
    let g_third = allocate(&db, &wt, 112).await;
    assert_eq!(
        transition(&db, &g_third, GenerationState::ProjectionReady).await,
        Ok(())
    );
    assert_eq!(
        transition(&db, &g_third, GenerationState::Active).await,
        Ok(())
    );

    let read = db.open_read().expect("read conn");
    let actives = active_generations(&read, &wt).expect("active");
    assert_eq!(
        actives.len(),
        2,
        "skipping the retire leaves two active rows — the invariant is procedural, got {actives:?}",
    );
    assert!(actives.contains(&g_new) && actives.contains(&g_third));
}
