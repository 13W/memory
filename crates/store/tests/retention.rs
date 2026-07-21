//! T06-01 integration tests for pin-root calculation (spec 06 §5): the DB-facing
//! readers over real `generation` rows, worktree isolation of the "last `K`"
//! window, and agreement between [`pinned_generation_roots`] and the pure
//! [`mark_pins`] on the same data.
//!
//! The pure policy matrix (state roots, K/T boundaries, lease expiry, union,
//! determinism) lives in the `retention` module's unit tests; these exercise the
//! schema-backed path.
//!
//! All tests are deterministic: an isolated [`TempHome`], fixed `now_ms`/`created_at`
//! literals, and ids from [`uuidv7_from`] with fixed entropy (no wall clock or
//! entropy source). Writes flow through [`StateWriter::transaction`]; reads use
//! [`StateDb::open_read`].

use std::collections::BTreeSet;

use local_rag_core::config::StorageConfig;
use local_rag_core::identity::uuidv7_from;
use local_rag_core::paths::StoreLayout;
use local_rag_store::registry::{
    GenerationState, WorktreeKind, allocate_generation, create_repository, create_worktree,
    transition_generation,
};
use local_rag_store::{
    ExternalPins, JobLease, RetentionParams, StateDb, generation_meta_for_worktree, mark_pins,
    pinned_generation_roots,
};
use local_rag_test_support::TempHome;

/// A temporary store with an ensured tree and an opened [`StateDb`] (production
/// migration set: registry v1, worktree v2, code v3).
fn open_state() -> (TempHome, StateDb) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    (home, db)
}

/// A distinct, deterministic UUIDv7 string keyed by `seed`, never touching the
/// clock or entropy source.
fn uuid(seed: u8) -> String {
    let mut rand = [0u8; 10];
    rand[9] = seed;
    uuidv7_from(1000, rand).to_string()
}

/// Create a repository and one `active` main worktree under it; returns `worktree_id`.
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

/// Allocate one generation for `worktree_id` (born `building`) with an explicit
/// `created_at`; returns its id.
async fn allocate_at(db: &StateDb, worktree_id: &str, gen_seed: u8, created_at: i64) -> String {
    let genr = uuid(gen_seed);
    let (w, g) = (worktree_id.to_string(), genr.clone());
    db.writer()
        .transaction(move |tx| allocate_generation(tx, &w, &g, created_at).map(|_| ()))
        .await
        .expect("allocate generation");
    genr
}

/// Drive a freshly-allocated (`building`) generation to `target` through legal
/// transitions (spec 04 §1): `building → projection_ready → active → retiring`, or
/// `building → failed`.
async fn drive_to(db: &StateDb, generation_id: &str, target: GenerationState) {
    let path: &[GenerationState] = match target {
        GenerationState::Building => &[],
        GenerationState::ProjectionReady => &[GenerationState::ProjectionReady],
        GenerationState::Active => &[GenerationState::ProjectionReady, GenerationState::Active],
        GenerationState::Retiring => &[
            GenerationState::ProjectionReady,
            GenerationState::Active,
            GenerationState::Retiring,
        ],
        GenerationState::Failed => &[GenerationState::Failed],
    };
    for &to in path {
        let g = generation_id.to_string();
        db.writer()
            .transaction(move |tx| transition_generation(tx, &g, to))
            .await
            .expect("transition tx (infrastructure)")
            .expect("legal transition");
    }
}

/// `pinned_generation_roots` over real rows must equal the pure `mark_pins` on the
/// same loaded metadata, and it must include the unconditional state roots.
#[tokio::test]
async fn db_roots_match_pure_mark_and_cover_state_roots() {
    let (_home, db) = open_state();
    let wt = worktree(&db, 1).await;

    // building, projection_ready, active, retiring, failed — one of each.
    let g_build = allocate_at(&db, &wt, 10, 1000).await;
    let g_ready = allocate_at(&db, &wt, 11, 1000).await;
    drive_to(&db, &g_ready, GenerationState::ProjectionReady).await;
    let g_active = allocate_at(&db, &wt, 12, 1000).await;
    drive_to(&db, &g_active, GenerationState::Active).await;
    let g_retiring = allocate_at(&db, &wt, 13, 1000).await;
    drive_to(&db, &g_retiring, GenerationState::Retiring).await;
    let g_failed = allocate_at(&db, &wt, 14, 1000).await;
    drive_to(&db, &g_failed, GenerationState::Failed).await;

    let params = RetentionParams {
        keep_last_k: 0,
        window_ms: 0,
    };
    let ext = ExternalPins::default();
    let now = 2000;

    let read = db.open_read().expect("read conn");
    let roots = pinned_generation_roots(&read, &wt, &params, &ext, now).expect("roots");

    // building + projection_ready + active are pinned; retiring (K=0/window=0) and
    // failed are not.
    let expected: BTreeSet<String> =
        BTreeSet::from([g_build.clone(), g_ready.clone(), g_active.clone()]);
    assert_eq!(roots.generations, expected);
    assert!(!roots.generations.contains(&g_retiring));
    assert!(!roots.generations.contains(&g_failed));

    // The DB path equals the pure path on the same loaded rows.
    let meta = generation_meta_for_worktree(&read, &wt).expect("meta");
    assert_eq!(roots, mark_pins(&meta, &params, &ext, now));
}

/// The "last `K`" window is per worktree: each worktree keeps its own K retiring
/// generations, and one worktree's pins never include another's.
#[tokio::test]
async fn last_k_is_isolated_per_worktree() {
    let (_home, db) = open_state();
    let wt_a = worktree(&db, 1).await;
    let wt_b = worktree(&db, 2).await;

    // Three retiring generations in each worktree (numbers 1,2,3 per worktree).
    let mut a_ids = Vec::new();
    for seed in [20, 21, 22] {
        let g = allocate_at(&db, &wt_a, seed, 1000).await;
        drive_to(&db, &g, GenerationState::Retiring).await;
        a_ids.push(g);
    }
    let mut b_ids = Vec::new();
    for seed in [30, 31, 32] {
        let g = allocate_at(&db, &wt_b, seed, 1000).await;
        drive_to(&db, &g, GenerationState::Retiring).await;
        b_ids.push(g);
    }

    let params = RetentionParams {
        keep_last_k: 2,
        window_ms: 0,
    };
    let ext = ExternalPins::default();
    let now = 2000;
    let read = db.open_read().expect("read conn");

    let roots_a = pinned_generation_roots(&read, &wt_a, &params, &ext, now).expect("roots a");
    // Worktree A keeps its own last two (numbers 2 and 3 → seeds 21, 22).
    assert_eq!(
        roots_a.generations,
        BTreeSet::from([a_ids[1].clone(), a_ids[2].clone()])
    );
    // None of worktree B's generations leak into A's pins.
    for b in &b_ids {
        assert!(!roots_a.generations.contains(b), "B gen {b} leaked into A");
    }

    let roots_b = pinned_generation_roots(&read, &wt_b, &params, &ext, now).expect("roots b");
    assert_eq!(
        roots_b.generations,
        BTreeSet::from([b_ids[1].clone(), b_ids[2].clone()])
    );
}

/// The window `T` is applied against `created_at` over real rows: with K=0, only
/// retiring generations created within the window are pinned.
#[tokio::test]
async fn window_over_created_at_from_real_rows() {
    let (_home, db) = open_state();
    let wt = worktree(&db, 1).await;

    // Two retiring generations, one born old, one born recent.
    let g_old = allocate_at(&db, &wt, 40, 1_000).await;
    drive_to(&db, &g_old, GenerationState::Retiring).await;
    let g_recent = allocate_at(&db, &wt, 41, 9_500).await;
    drive_to(&db, &g_recent, GenerationState::Retiring).await;

    let params = RetentionParams {
        keep_last_k: 0,
        window_ms: 1_000, // now=10_000 → floor=9_000
    };
    let now = 10_000;
    let read = db.open_read().expect("read conn");
    let roots =
        pinned_generation_roots(&read, &wt, &params, &ExternalPins::default(), now).expect("roots");

    assert_eq!(roots.generations, BTreeSet::from([g_recent]));
    assert!(!roots.generations.contains(&g_old));
}

/// A non-expired lease pins an otherwise-unretained retiring generation over the
/// DB path; the default (empty) `ExternalPins` leaves it unpinned.
#[tokio::test]
async fn lease_pins_over_db_path() {
    let (_home, db) = open_state();
    let wt = worktree(&db, 1).await;

    let g = allocate_at(&db, &wt, 50, 1_000).await;
    drive_to(&db, &g, GenerationState::Retiring).await;

    let params = RetentionParams {
        keep_last_k: 0,
        window_ms: 0,
    };
    let now = 5_000;
    let read = db.open_read().expect("read conn");

    // Empty external pins: nothing keeps the retiring generation.
    let bare =
        pinned_generation_roots(&read, &wt, &params, &ExternalPins::default(), now).expect("bare");
    assert!(bare.generations.is_empty());

    // A live lease pins it.
    let leased = ExternalPins {
        leases: vec![JobLease {
            generation_id: g.clone(),
            lease_until_ms: now + 1,
        }],
        ..ExternalPins::default()
    };
    let roots = pinned_generation_roots(&read, &wt, &params, &leased, now).expect("leased");
    assert_eq!(roots.generations, BTreeSet::from([g]));
}

/// A worktree with no generations pins nothing; `from_storage_config` wiring works
/// end-to-end (defaults keep nothing when there are no retiring rows).
#[tokio::test]
async fn empty_worktree_pins_nothing() {
    let (_home, db) = open_state();
    let wt = worktree(&db, 1).await;

    let params = RetentionParams::from_storage_config(&StorageConfig::default());
    let read = db.open_read().expect("read conn");
    let roots =
        pinned_generation_roots(&read, &wt, &params, &ExternalPins::default(), 10_000).expect("r");
    assert!(roots.generations.is_empty());
    assert!(roots.referenced_file_revisions.is_empty());
}
