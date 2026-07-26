//! T11-05 acceptance tests: the production model axis (spec 10 §4 steps 4–6,
//! 05 §5/§8, 04 §3).
//!
//! Everything runs against a **real** registry — `create_model_space`,
//! `register_representation`, `set_model_space_representation`,
//! `write_model_space_coverage`, `transition_model_space` — rather than the raw
//! `INSERT INTO model_space` the older switch fixtures use, because the
//! preconditions under test (`active` + full coverage) are exactly what that
//! registry expresses.
//!
//! Deterministic: isolated [`TempHome`], fixed `now_ms` literals, ids from
//! `uuidv7_from` with pinned entropy, no network, no wall-clock sleeps.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use local_rag_core::identity::{Uuid, UuidSource, uuidv7_from};
use local_rag_core::paths::StoreLayout;
use local_rag_projection::{
    FakeProjectionStore, ModelSwitchError, ProjectionStore, RepresentationKind, ShardParams,
    VectorSource, dormant_migration_target, migrate_dormant_on_open, params_for_model_space,
    shard_dir, switch, switch_model_space,
};
use local_rag_store::{
    CoverageEntry, DEFAULT_MODEL_SPACE_ID, DistanceMetric, GenerationState, ModelSpaceState,
    NewContentBlob, NewFileRevision, NewOccurrence, NewParsedUnit, NewlineStyle, RepresentationKey,
    SourceCompression, StateDb, UnitKind, WorktreeKind, allocate_generation, create_model_space,
    create_repository, create_worktree, insert_content_blob, insert_file_revision,
    insert_generation_file, insert_occurrence, insert_parsed_unit, insert_projection_state,
    occurrence_id, projection_state, recompute_coverage, register_representation,
    set_default_model_space_id, set_model_space_representation, transition_generation,
    transition_model_space, write_model_space_coverage,
};
use local_rag_test_support::TempHome;

const NOW: i64 = 1_000;
/// Model space A's vector width; B deliberately differs (see [`DIMS_B`]).
const DIMS_A: usize = 3;
const DIMS_B: usize = 5;

fn uuid(seed: u8) -> Uuid {
    let mut rand = [0u8; 10];
    rand[9] = seed;
    uuidv7_from(1000, rand)
}

fn space_a() -> Uuid {
    DEFAULT_MODEL_SPACE_ID.parse().expect("default space id")
}

/// A `UuidSource` yielding a fresh deterministic id per call.
struct SeqUuidV7 {
    counter: AtomicU64,
}

impl SeqUuidV7 {
    fn new() -> Self {
        SeqUuidV7 {
            counter: AtomicU64::new(0),
        }
    }
}

impl UuidSource for SeqUuidV7 {
    fn next_uuid(&self) -> Uuid {
        let n = self.counter.fetch_add(1, Ordering::SeqCst);
        uuidv7_from(1_000_000 + n, [0xCD; 10])
    }
}

/// A `VectorSource` that answers every request with a vector of the width the
/// asking model space uses. Stands in for the real `CacheVectorSource` in the
/// tests that are about the *switch*, not about cache reads.
struct WidthVectors {
    dimensions: usize,
}

impl VectorSource for WidthVectors {
    fn vector(&self, _occurrence_id: &str, _kind: RepresentationKind) -> Option<Vec<f32>> {
        Some(vec![0.5; self.dimensions])
    }
}

fn open_state() -> (TempHome, StoreLayout, StateDb) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    (home, layout, db)
}

async fn worktree(db: &StateDb, seed: u8) -> Uuid {
    let repo = uuid(seed).to_string();
    let wt = uuid(seed.wrapping_add(100));
    let (r, w) = (repo, wt.to_string());
    db.writer()
        .transaction(move |tx| {
            create_repository(tx, &r, None, NOW)?;
            create_worktree(tx, &w, &r, WorktreeKind::Main, NOW)
        })
        .await
        .expect("create repo + worktree");
    let w = wt.to_string();
    db.writer()
        .transaction(move |tx| insert_projection_state(tx, &w, NOW))
        .await
        .expect("init projection state");
    wt
}

async fn allocate_ready(db: &StateDb, worktree_id: &Uuid, gen_seed: u8) -> Uuid {
    let genr = uuid(gen_seed);
    let (w, g) = (worktree_id.to_string(), genr.to_string());
    db.writer()
        .transaction(move |tx| allocate_generation(tx, &w, &g, NOW).map(|_| ()))
        .await
        .expect("allocate generation");
    let g2 = genr.to_string();
    db.writer()
        .transaction(move |tx| transition_generation(tx, &g2, GenerationState::ProjectionReady))
        .await
        .expect("transition tx")
        .expect("legal");
    genr
}

/// One real occurrence with its whole FK chain.
async fn seed_occurrence(db: &StateDb, generation_id: &Uuid, seed: u8, path: &str) -> String {
    let gen_str = generation_id.to_string();
    let (fr, unit, blob) = (
        uuid(seed.wrapping_add(30)).to_string(),
        uuid(seed.wrapping_add(60)).to_string(),
        format!("{:064x}", seed as u128),
    );
    let occ = occurrence_id(&gen_str, path, &unit);
    let (g, p, f, u, b, o) = (
        gen_str.clone(),
        path.to_string(),
        fr.clone(),
        unit.clone(),
        blob.clone(),
        occ.clone(),
    );
    db.writer()
        .transaction(move |tx| {
            insert_file_revision(
                tx,
                &NewFileRevision {
                    file_revision_id: &f,
                    content_hash: &f,
                    parser_fingerprint: "fp",
                    source_blob: b"src",
                    compression: SourceCompression::None,
                    source_encoding: "utf-8",
                    newline_style: NewlineStyle::Lf,
                    source_size: 3,
                },
                NOW,
            )?;
            insert_content_blob(
                tx,
                &NewContentBlob {
                    blob_id: &b,
                    language: "rust",
                    algo_version: 1,
                    normalization_version: 1,
                },
                NOW,
            )?;
            insert_parsed_unit(
                tx,
                &NewParsedUnit {
                    unit_id: &u,
                    file_revision_id: &f,
                    unit_kind: UnitKind::Symbol,
                    syntax_locator: &format!("loc:{u}"),
                    blob_id: &b,
                    span_start: 0,
                    span_end: 3,
                    local_name: None,
                    kind: None,
                    parent_unit_id: None,
                },
            )?;
            insert_generation_file(tx, &g, &p, &p, &f)?;
            insert_occurrence(
                tx,
                &NewOccurrence {
                    occurrence_id: &o,
                    generation_id: &g,
                    normalized_path: &p,
                    unit_id: &u,
                    qualified_name: None,
                    context_hash: None,
                },
            )
        })
        .await
        .expect("seed occurrence");
    occ
}

/// Register `code_raw` at `dimensions` for `model_space_id` and mark it covered
/// for `expected` subjects — the shape `switch_model_space`'s preconditions read.
async fn provision_space(db: &StateDb, model_space_id: &Uuid, dimensions: u32, covered: u64) {
    let space = model_space_id.to_string();
    let representation_id = format!("{space}-code-raw");
    db.writer()
        .transaction(move |tx| {
            let id = register_representation(
                tx,
                &representation_id,
                &RepresentationKey {
                    kind: local_rag_store::RepresentationKind::CodeRaw,
                    representation_version: 1,
                    normalization_version: 1,
                    model_id: format!("model-{dimensions}"),
                    dimensions,
                    distance_metric: DistanceMetric::Cosine,
                },
                NOW,
            )?;
            set_model_space_representation(
                tx,
                &space,
                local_rag_store::RepresentationKind::CodeRaw,
                &id,
                true,
                NOW,
            )?;
            let mut counts = BTreeMap::new();
            counts.insert(
                local_rag_store::RepresentationKind::CodeRaw,
                CoverageEntry {
                    expected: covered,
                    ready: covered,
                    failed: 0,
                },
            );
            let coverage =
                recompute_coverage(&[local_rag_store::RepresentationKind::CodeRaw], &counts);
            write_model_space_coverage(tx, &space, &coverage, NOW)
        })
        .await
        .expect("provision model space");
}

/// Create model space B, provision it, and drive it to `active`.
async fn active_space_b(db: &StateDb, seed: u8, dimensions: u32, covered: u64) -> Uuid {
    let id = uuid(seed);
    let (i, name) = (id.to_string(), format!("space-{seed}"));
    db.writer()
        .transaction(move |tx| create_model_space(tx, &i, &name, NOW))
        .await
        .expect("create model space");
    provision_space(db, &id, dimensions, covered).await;
    for to in [ModelSpaceState::ProjectionReady, ModelSpaceState::Active] {
        let (i, required) = (
            id.to_string(),
            vec![local_rag_store::RepresentationKind::CodeRaw],
        );
        db.writer()
            .transaction(move |tx| transition_model_space(tx, &i, to, &required, NOW))
            .await
            .expect("transition tx")
            .expect("legal transition");
    }
    id
}

/// Bring `worktree_id` onto space A with one occurrence, through the real switch.
async fn establish_on_a(
    db: &StateDb,
    store: &FakeProjectionStore,
    layout: &StoreLayout,
    worktree_id: Uuid,
    gen_seed: u8,
) -> Uuid {
    let generation = allocate_ready(db, &worktree_id, gen_seed).await;
    seed_occurrence(db, &generation, gen_seed, "src/lib.rs").await;
    provision_space(db, &space_a(), DIMS_A as u32, 1).await;

    switch(
        db,
        store,
        &shard_dir(layout, &worktree_id, &space_a()),
        ShardParams::with_dimensions(DIMS_A),
        worktree_id,
        generation,
        space_a(),
        &WidthVectors { dimensions: DIMS_A },
        &SeqUuidV7::new(),
        NOW,
    )
    .await
    .expect("establish on space A");
    generation
}

fn active_space(db: &StateDb, worktree_id: &Uuid) -> Option<String> {
    let read = db.open_read().expect("read");
    projection_state(&read, &worktree_id.to_string())
        .expect("row")
        .and_then(|r| r.active_model_space_id)
}

/// A model-axis switch keeps the generation, moves only the space, and lands the
/// new points in the space's own directory (spec 10 §4 step 4).
#[tokio::test]
async fn model_axis_switch_moves_only_the_model_space() {
    let (_home, layout, db) = open_state();
    let store = FakeProjectionStore;
    let wt = worktree(&db, 1).await;
    let generation = establish_on_a(&db, &store, &layout, wt, 10).await;
    let b = active_space_b(&db, 200, DIMS_B as u32, 1).await;

    let outcome = switch_model_space(
        &db,
        &store,
        &layout,
        wt,
        b,
        &WidthVectors { dimensions: DIMS_B },
        &SeqUuidV7::new(),
        NOW + 1,
    )
    .await
    .expect("model switch")
    .expect("a real switch happened");
    assert_eq!(outcome.upserted, 1, "the single occurrence is re-projected");

    let read = db.open_read().expect("read");
    let row = projection_state(&read, &wt.to_string())
        .expect("row")
        .expect("exists");
    assert_eq!(
        row.active_model_space_id.as_deref(),
        Some(b.to_string().as_str())
    );
    assert_eq!(
        row.active_generation_id.as_deref(),
        Some(generation.to_string().as_str()),
        "the generation axis must not move"
    );
    assert_eq!(row.status, local_rag_store::ProjectionStatus::Clean);

    // A repeat is a no-op rather than an error (the dormant path may race it).
    assert!(
        switch_model_space(
            &db,
            &store,
            &layout,
            wt,
            b,
            &WidthVectors { dimensions: DIMS_B },
            &SeqUuidV7::new(),
            NOW + 2,
        )
        .await
        .expect("second call")
        .is_none()
    );
}

/// Two worktrees migrate independently: moving one leaves the other entirely on
/// the old space, shard included (spec 04 §3 `[FIXED]`, no global barrier).
#[tokio::test]
async fn two_worktrees_migrate_independently() {
    let (_home, layout, db) = open_state();
    let store = FakeProjectionStore;
    let wt_a = worktree(&db, 2).await;
    let wt_b = worktree(&db, 3).await;
    establish_on_a(&db, &store, &layout, wt_a, 20).await;
    establish_on_a(&db, &store, &layout, wt_b, 21).await;
    let b = active_space_b(&db, 201, DIMS_B as u32, 1).await;

    switch_model_space(
        &db,
        &store,
        &layout,
        wt_a,
        b,
        &WidthVectors { dimensions: DIMS_B },
        &SeqUuidV7::new(),
        NOW + 1,
    )
    .await
    .expect("migrate the first worktree")
    .expect("switched");

    assert_eq!(
        active_space(&db, &wt_a).as_deref(),
        Some(b.to_string().as_str())
    );
    assert_eq!(
        active_space(&db, &wt_b).as_deref(),
        Some(space_a().to_string().as_str()),
        "the untouched worktree keeps running the old space"
    );

    // Its shard is still there, under the old space's directory, and still
    // holds its point — "until step 4 commits for a worktree, that worktree
    // still runs A entirely" (spec 10 §4 `[FIXED]`).
    let old = store
        .open(
            &shard_dir(&layout, &wt_b, &space_a()),
            ShardParams::with_dimensions(DIMS_A),
        )
        .expect("open the untouched shard");
    assert_eq!(old.point_count().expect("count"), 1);

    // ... and migrating it afterwards works just as well.
    switch_model_space(
        &db,
        &store,
        &layout,
        wt_b,
        b,
        &WidthVectors { dimensions: DIMS_B },
        &SeqUuidV7::new(),
        NOW + 2,
    )
    .await
    .expect("migrate the second worktree")
    .expect("switched");
    assert_eq!(
        active_space(&db, &wt_b).as_deref(),
        Some(b.to_string().as_str())
    );
}

/// A space with different `dimensions` gets its own shard directory; the old
/// shard is never widened in place (spec 10 §4 step 2 `[FIXED]`).
#[tokio::test]
async fn different_dimensions_never_reuse_the_old_shard() {
    let (_home, layout, db) = open_state();
    let store = FakeProjectionStore;
    let wt = worktree(&db, 4).await;
    establish_on_a(&db, &store, &layout, wt, 30).await;
    let b = active_space_b(&db, 202, DIMS_B as u32, 1).await;

    let dir_a = shard_dir(&layout, &wt, &space_a());
    let dir_b = shard_dir(&layout, &wt, &b);
    assert_ne!(dir_a, dir_b, "each model space owns a directory");

    // Params are derived from the registry, not from a store-wide constant —
    // both axes of them: `dimensions` and (T12-02) `distance_metric`, which is
    // `cosine` for every representation this fixture registers.
    let read = db.open_read().expect("read");
    assert_eq!(
        params_for_model_space(&read, &space_a()).expect("params A"),
        ShardParams {
            dimensions: DIMS_A,
            distance_metric: DistanceMetric::Cosine,
        }
    );
    assert_eq!(
        params_for_model_space(&read, &b).expect("params B"),
        ShardParams {
            dimensions: DIMS_B,
            distance_metric: DistanceMetric::Cosine,
        }
    );
    drop(read);

    switch_model_space(
        &db,
        &store,
        &layout,
        wt,
        b,
        &WidthVectors { dimensions: DIMS_B },
        &SeqUuidV7::new(),
        NOW + 1,
    )
    .await
    .expect("migrate")
    .expect("switched");

    assert!(
        dir_a.is_dir(),
        "the old shard's files survive the migration"
    );
    assert!(dir_b.is_dir(), "the new space wrote its own shard");

    // The old shard still answers at its own width, untouched.
    let old = store
        .open(&dir_a, ShardParams::with_dimensions(DIMS_A))
        .expect("open old");
    assert_eq!(old.point_count().expect("count"), 1);

    // And the new shard refuses a vector of the old width — the widths are
    // physically separated, not merely conventionally.
    let new = store
        .open(&dir_b, ShardParams::with_dimensions(DIMS_B))
        .expect("open new");
    let any_id = new
        .point_ids()
        .expect("ids")
        .next()
        .expect("the migrated point");
    let err = new
        .upsert(&[local_rag_projection::ProjectionPoint {
            point_id: any_id,
            vector: vec![0.5; DIMS_A],
        }])
        .expect_err("a 3-wide vector cannot enter a 5-wide shard");
    assert!(
        matches!(
            err,
            local_rag_projection::ProjectionError::DimensionMismatch {
                expected: DIMS_B,
                actual: DIMS_A
            }
        ),
        "{err}"
    );
}

/// Step 0's preconditions are checked before anything is written.
#[tokio::test]
async fn ineligible_or_uncovered_targets_are_refused_before_the_write_ahead() {
    let (_home, layout, db) = open_state();
    let store = FakeProjectionStore;
    let wt = worktree(&db, 5).await;
    establish_on_a(&db, &store, &layout, wt, 40).await;

    // (a) A space that is still `building` is not a legal target.
    let building = uuid(203);
    let (i, name) = (building.to_string(), "space-building".to_string());
    db.writer()
        .transaction(move |tx| create_model_space(tx, &i, &name, NOW))
        .await
        .expect("create");
    provision_space(&db, &building, DIMS_B as u32, 1).await;
    let err = switch_model_space(
        &db,
        &store,
        &layout,
        wt,
        building,
        &WidthVectors { dimensions: DIMS_B },
        &SeqUuidV7::new(),
        NOW + 1,
    )
    .await
    .expect_err("a building space is not eligible");
    assert!(
        matches!(
            err,
            ModelSwitchError::NotEligible {
                state: ModelSpaceState::Building,
                ..
            }
        ),
        "{err}"
    );

    // (b) An `active` space whose coverage is short is refused too.
    let uncovered = active_space_b(&db, 204, DIMS_B as u32, 1).await;
    let (i, coverage) = (uncovered.to_string(), {
        let mut counts = BTreeMap::new();
        counts.insert(
            local_rag_store::RepresentationKind::CodeRaw,
            CoverageEntry {
                expected: 7,
                ready: 2,
                failed: 5,
            },
        );
        recompute_coverage(&[local_rag_store::RepresentationKind::CodeRaw], &counts)
    });
    db.writer()
        .transaction(move |tx| write_model_space_coverage(tx, &i, &coverage, NOW))
        .await
        .expect("degrade coverage");
    let err = switch_model_space(
        &db,
        &store,
        &layout,
        wt,
        uncovered,
        &WidthVectors { dimensions: DIMS_B },
        &SeqUuidV7::new(),
        NOW + 2,
    )
    .await
    .expect_err("incomplete coverage is refused");
    assert!(
        matches!(err, ModelSwitchError::IncompleteCoverage { .. }),
        "{err}"
    );

    // Neither refusal touched the projection row: still cleanly on A.
    let read = db.open_read().expect("read");
    let row = projection_state(&read, &wt.to_string())
        .expect("row")
        .expect("exists");
    assert_eq!(row.status, local_rag_store::ProjectionStatus::Clean);
    assert_eq!(
        row.active_model_space_id.as_deref(),
        Some(space_a().to_string().as_str())
    );
    assert!(row.target_model_space_id.is_none());
}

/// A worktree whose space went `retiring` migrates to the default at its next
/// open (spec 05 §8 `[FIXED]`), and a healthy worktree does not.
#[tokio::test]
async fn a_dormant_worktree_migrates_to_the_default_on_open() {
    let (_home, layout, db) = open_state();
    let store = FakeProjectionStore;
    let wt = worktree(&db, 6).await;
    establish_on_a(&db, &store, &layout, wt, 50).await;
    let b = active_space_b(&db, 205, DIMS_B as u32, 1).await;

    // Nothing due while the worktree's space is healthy.
    let read = db.open_read().expect("read");
    assert_eq!(
        dormant_migration_target(&read, &wt).expect("target"),
        None,
        "a healthy worktree is not migrated behind the user's back"
    );
    drop(read);
    assert!(
        migrate_dormant_on_open(
            &db,
            &store,
            &layout,
            wt,
            &WidthVectors { dimensions: DIMS_B },
            &SeqUuidV7::new(),
            NOW + 1,
        )
        .await
        .expect("no-op")
        .is_none()
    );

    // Move the default to B and retire A — spec 10 §4 steps 5 and 6.
    let (i, required) = (
        b.to_string(),
        vec![local_rag_store::RepresentationKind::CodeRaw],
    );
    db.writer()
        .transaction(move |tx| set_default_model_space_id(tx, &i, NOW))
        .await
        .expect("set default tx")
        .expect("B is active, so it may be the default");
    let (i, req) = (space_a().to_string(), required);
    db.writer()
        .transaction(move |tx| transition_model_space(tx, &i, ModelSpaceState::Retiring, &req, NOW))
        .await
        .expect("retire tx")
        .expect("active -> retiring is legal");

    // Now the open path owes a migration, and performs it.
    let read = db.open_read().expect("read");
    assert_eq!(
        dormant_migration_target(&read, &wt).expect("target"),
        Some(b)
    );
    drop(read);
    let outcome = migrate_dormant_on_open(
        &db,
        &store,
        &layout,
        wt,
        &WidthVectors { dimensions: DIMS_B },
        &SeqUuidV7::new(),
        NOW + 2,
    )
    .await
    .expect("dormant migration")
    .expect("it migrated");
    assert_eq!(outcome.upserted, 1);
    assert_eq!(
        active_space(&db, &wt).as_deref(),
        Some(b.to_string().as_str())
    );

    // Idempotent: a second open has nothing to do.
    assert!(
        migrate_dormant_on_open(
            &db,
            &store,
            &layout,
            wt,
            &WidthVectors { dimensions: DIMS_B },
            &SeqUuidV7::new(),
            NOW + 3,
        )
        .await
        .expect("second open")
        .is_none()
    );
}

/// The default pointer only ever names an `active` space (spec 04 §3 `[FIXED]`).
#[tokio::test]
async fn the_default_pointer_refuses_a_non_active_space() {
    let (_home, _layout, db) = open_state();
    let building = uuid(206);
    let (i, name) = (building.to_string(), "space-building".to_string());
    db.writer()
        .transaction(move |tx| create_model_space(tx, &i, &name, NOW))
        .await
        .expect("create");

    let i = building.to_string();
    let refused = db
        .writer()
        .transaction(move |tx| set_default_model_space_id(tx, &i, NOW))
        .await
        .expect("tx ran")
        .expect_err("a building space cannot be the default");
    assert!(
        matches!(
            refused,
            local_rag_store::DefaultModelSpaceError::NotActive {
                state: ModelSpaceState::Building,
                ..
            }
        ),
        "{refused}"
    );

    let missing = db
        .writer()
        .transaction(move |tx| set_default_model_space_id(tx, "nope", NOW))
        .await
        .expect("tx ran")
        .expect_err("an unknown space cannot be the default");
    assert!(
        matches!(
            missing,
            local_rag_store::DefaultModelSpaceError::Unknown { .. }
        ),
        "{missing}"
    );

    // The pointer still names the seeded default.
    let read = db.open_read().expect("read");
    assert_eq!(
        local_rag_store::default_model_space_id(&read).expect("read default"),
        Some(DEFAULT_MODEL_SPACE_ID.to_string())
    );
}

/// Once no worktree references the outgoing space, its cache rows stop being
/// pinned — spec 10 §4 step 6, via T11-04's `protected_model_space_ids`.
#[tokio::test]
async fn the_outgoing_space_stops_being_pinned_once_nothing_references_it() {
    let (_home, layout, db) = open_state();
    let store = FakeProjectionStore;
    let wt = worktree(&db, 7).await;
    establish_on_a(&db, &store, &layout, wt, 60).await;
    let b = active_space_b(&db, 207, DIMS_B as u32, 1).await;

    let read = db.open_read().expect("read");
    let protected = local_rag_store::protected_model_space_ids(&read).expect("protected");
    assert!(
        protected.contains(&space_a().to_string()),
        "the space a worktree is on must be pinned"
    );
    drop(read);

    switch_model_space(
        &db,
        &store,
        &layout,
        wt,
        b,
        &WidthVectors { dimensions: DIMS_B },
        &SeqUuidV7::new(),
        NOW + 1,
    )
    .await
    .expect("migrate")
    .expect("switched");

    // Move the default off A and retire it; only then is nothing referencing it.
    let (i, required) = (
        b.to_string(),
        vec![local_rag_store::RepresentationKind::CodeRaw],
    );
    db.writer()
        .transaction(move |tx| set_default_model_space_id(tx, &i, NOW))
        .await
        .expect("tx")
        .expect("legal");
    let (i, req) = (space_a().to_string(), required);
    db.writer()
        .transaction(move |tx| transition_model_space(tx, &i, ModelSpaceState::Retiring, &req, NOW))
        .await
        .expect("tx")
        .expect("legal");

    let read = db.open_read().expect("read");
    let protected = local_rag_store::protected_model_space_ids(&read).expect("protected");
    assert!(
        !protected.contains(&space_a().to_string()),
        "a retiring space nothing references is evictable: {protected:?}"
    );
    assert!(
        protected.contains(&b.to_string()),
        "the new space is pinned"
    );
}

/// `params_for_model_space` refuses a space with no `code_raw` representation
/// rather than inventing a width.
#[tokio::test]
async fn a_space_without_code_raw_cannot_size_a_shard() {
    let (_home, _layout, db) = open_state();
    let empty = uuid(208);
    let (i, name) = (empty.to_string(), "space-empty".to_string());
    db.writer()
        .transaction(move |tx| create_model_space(tx, &i, &name, NOW))
        .await
        .expect("create");

    let read = db.open_read().expect("read");
    let err = params_for_model_space(&read, &empty).expect_err("no code_raw representation");
    assert!(
        matches!(err, ModelSwitchError::NoShardParams { .. }),
        "{err}"
    );
}

/// The shard directory of one space never nests inside another's, and both nest
/// under the worktree's single root (spec 05 §8's "same shard directory").
#[tokio::test]
async fn shard_directories_nest_under_one_worktree_root() {
    let (_home, layout, db) = open_state();
    let wt = worktree(&db, 8).await;
    let b = uuid(209);

    let root = layout.projection_shard(&wt.to_string());
    let dir_a = shard_dir(&layout, &wt, &space_a());
    let dir_b = shard_dir(&layout, &wt, &b);

    assert!(dir_a.starts_with(&root) && dir_b.starts_with(&root));
    assert_ne!(dir_a, dir_b);
    assert!(!dir_a.starts_with(&dir_b) && !dir_b.starts_with(&dir_a));
}

/// A worktree with no active generation has nothing to re-project.
#[tokio::test]
async fn a_worktree_without_an_active_generation_is_refused() {
    let (_home, layout, db) = open_state();
    let store = FakeProjectionStore;
    let wt = worktree(&db, 9).await;
    let b = active_space_b(&db, 210, DIMS_B as u32, 0).await;

    let err = switch_model_space(
        &db,
        &store,
        &layout,
        wt,
        b,
        &WidthVectors { dimensions: DIMS_B },
        &SeqUuidV7::new(),
        NOW,
    )
    .await
    .expect_err("no active generation");
    assert!(matches!(err, ModelSwitchError::NoActiveGeneration), "{err}");

    // The dormant path agrees: nothing to migrate before an initial projection.
    let read = db.open_read().expect("read");
    assert_eq!(dormant_migration_target(&read, &wt).expect("target"), None);
}
