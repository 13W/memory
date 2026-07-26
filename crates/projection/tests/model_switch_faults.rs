//! T11-05: the crash and concurrency halves of the model-axis card.
//!
//! Two properties spec 10 §4 states as `[FIXED]` and this file proves
//! executably:
//!
//! * *"until step 4 commits for a worktree, that worktree still runs A
//!   entirely"* — a kill between the shard write and the commit
//!   (`projection.switch.before_commit`, T07-05's seam) leaves the **old**
//!   space's shard fully intact, because each space owns its own directory
//!   (T11-05). The worktree is observably all-old or all-new, never half of
//!   each, and a retry converges;
//! * *"no global write barrier"* — one worktree migrating never blocks another
//!   worktree's own switch, since `L2` is per-worktree and no store-wide write
//!   lock exists at all (spec 02 §5).
//!
//! Serialized on a `tokio::sync::Mutex`: the failpoint registry is
//! process-global and held across `.await` here (the discipline D-005
//! established).
#![cfg(feature = "failpoints")]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use local_rag_core::identity::{Uuid, UuidSource, uuidv7_from};
use local_rag_core::paths::StoreLayout;
use local_rag_projection::{
    FakeProjectionStore, ProjectionStore, RepresentationKind, ShardParams, VectorSource, shard_dir,
    switch, switch_model_space,
};
use local_rag_store::{
    CoverageEntry, DEFAULT_MODEL_SPACE_ID, DistanceMetric, GenerationState, ModelSpaceState,
    NewContentBlob, NewFileRevision, NewOccurrence, NewParsedUnit, NewlineStyle, ProjectionStatus,
    RepresentationKey, SourceCompression, StateDb, UnitKind, WorktreeKind, WorktreeLockRegistry,
    allocate_generation, create_model_space, create_repository, create_worktree,
    insert_content_blob, insert_file_revision, insert_generation_file, insert_occurrence,
    insert_parsed_unit, insert_projection_state, occurrence_id, projection_state,
    recompute_coverage, register_representation, set_model_space_representation,
    transition_generation, transition_model_space, write_model_space_coverage,
};
use local_rag_test_support::{Action, TempHome, failpoint::global};
use tokio::sync::Mutex;

const NOW: i64 = 1_000;
const DIMS_A: usize = 3;
const DIMS_B: usize = 5;
const BEFORE_COMMIT: &str = "projection.switch.before_commit";

/// Serializes every test here against the process-global failpoint registry.
static SERIAL: Mutex<()> = Mutex::const_new(());

fn uuid(seed: u8) -> Uuid {
    let mut rand = [0u8; 10];
    rand[9] = seed;
    uuidv7_from(1000, rand)
}

fn space_a() -> Uuid {
    DEFAULT_MODEL_SPACE_ID.parse().expect("default space id")
}

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

struct WidthVectors {
    dimensions: usize,
}

impl VectorSource for WidthVectors {
    fn vector(&self, _occurrence_id: &str, _kind: RepresentationKind) -> Option<Vec<f32>> {
        Some(vec![0.5; self.dimensions])
    }
}

fn open_state() -> (TempHome, StoreLayout, Arc<StateDb>) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let db = Arc::new(StateDb::open(layout.state_db()).expect("open state.sqlite"));
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
            .expect("legal");
    }
    id
}

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
    .expect("establish on A");
    generation
}

/// A kill between the shard write and the commit leaves the worktree fully on
/// the old space, and a retry converges fully onto the new one.
#[tokio::test(flavor = "multi_thread")]
async fn a_kill_before_commit_leaves_the_worktree_all_old_then_all_new() {
    let _serial = SERIAL.lock().await;
    let (_home, layout, db) = open_state();
    let store = FakeProjectionStore;
    let wt = worktree(&db, 1).await;
    let generation = establish_on_a(&db, &store, &layout, wt, 10).await;
    let b = active_space_b(&db, 200, DIMS_B as u32, 1).await;

    global().reset();
    global().register(BEFORE_COMMIT);
    global().arm(BEFORE_COMMIT, Action::Error).expect("armed");

    let err = switch_model_space(
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
    .expect_err("the crash point fires");
    assert!(err.to_string().contains(BEFORE_COMMIT), "{err}");
    global().disarm(BEFORE_COMMIT).expect("declared");

    // `state.sqlite` still names A as active — the worktree is all-old, exactly
    // as spec 10 §4's "until step 4 commits … still runs A entirely" requires —
    // with the interrupted switch detectable as `updating`.
    let read = db.open_read().expect("read");
    let row = projection_state(&read, &wt.to_string())
        .expect("row")
        .expect("exists");
    drop(read);
    assert_eq!(row.status, ProjectionStatus::Updating);
    assert_eq!(
        row.active_model_space_id.as_deref(),
        Some(space_a().to_string().as_str())
    );
    assert_eq!(
        row.target_model_space_id.as_deref(),
        Some(b.to_string().as_str())
    );

    // The old shard is untouched and still serviceable: the new space wrote into
    // its own directory (T11-05's layout), so nothing of A was overwritten.
    let old = store
        .open(
            &shard_dir(&layout, &wt, &space_a()),
            ShardParams::with_dimensions(DIMS_A),
        )
        .expect("open A's shard");
    assert_eq!(old.point_count().expect("count"), 1);
    let head = old.read_head().expect("head").expect("head present");
    assert_eq!(head.model_space_id, space_a());
    assert_eq!(head.generation_id, generation);

    // Retry converges: the worktree becomes all-new.
    let outcome = switch_model_space(
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
    .expect("retry")
    .expect("switched");
    assert_eq!(
        outcome.upserted, 0,
        "the shard already holds the target points from the killed attempt"
    );

    let read = db.open_read().expect("read");
    let row = projection_state(&read, &wt.to_string())
        .expect("row")
        .expect("exists");
    assert_eq!(row.status, ProjectionStatus::Clean);
    assert_eq!(
        row.active_model_space_id.as_deref(),
        Some(b.to_string().as_str())
    );
    assert_eq!(
        row.active_generation_id.as_deref(),
        Some(generation.to_string().as_str()),
        "the generation axis never moved"
    );
}

/// One worktree's migration never blocks another's switch: `L2` is per-worktree
/// and there is no store-wide write lock (spec 04 §3 `[FIXED]`, 02 §5).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_migration_does_not_block_another_worktree() {
    let _serial = SERIAL.lock().await;
    global().reset();

    let (_home, layout, db) = open_state();
    let store = Arc::new(FakeProjectionStore);
    let locks = Arc::new(WorktreeLockRegistry::new());

    let wt_a = worktree(&db, 2).await;
    let wt_b = worktree(&db, 3).await;
    establish_on_a(&db, &store, &layout, wt_a, 20).await;
    establish_on_a(&db, &store, &layout, wt_b, 21).await;
    let target = active_space_b(&db, 201, DIMS_B as u32, 1).await;
    let gen_b2 = allocate_ready(&db, &wt_b, 22).await;
    seed_occurrence(&db, &gen_b2, 22, "src/other.rs").await;

    // Hold worktree A's write lock for the whole migration, and run worktree B's
    // generation switch concurrently under its own lock.
    let migrate = {
        let (db, store, layout, locks) = (db.clone(), store.clone(), layout.clone(), locks.clone());
        tokio::spawn(async move {
            locks
                .write(
                    &wt_a.to_string(),
                    switch_model_space(
                        &db,
                        &*store,
                        &layout,
                        wt_a,
                        target,
                        &WidthVectors { dimensions: DIMS_B },
                        &SeqUuidV7::new(),
                        NOW + 1,
                    ),
                )
                .await
        })
    };

    let other = {
        let (db, store, layout, locks) = (db.clone(), store.clone(), layout.clone(), locks.clone());
        tokio::spawn(async move {
            locks
                .write(
                    &wt_b.to_string(),
                    switch(
                        &db,
                        &*store,
                        &shard_dir(&layout, &wt_b, &space_a()),
                        ShardParams::with_dimensions(DIMS_A),
                        wt_b,
                        gen_b2,
                        space_a(),
                        &WidthVectors { dimensions: DIMS_A },
                        &SeqUuidV7::new(),
                        NOW + 1,
                    ),
                )
                .await
        })
    };

    // Both finish well inside the budget; neither waits on the other, because no
    // lock is shared between them.
    let (a, b) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio::join!(migrate, other)
    })
    .await
    .expect("no store-wide barrier: both complete");
    a.expect("join a").expect("migration succeeded");
    b.expect("join b").expect("generation switch succeeded");

    let read = db.open_read().expect("read");
    let row_a = projection_state(&read, &wt_a.to_string())
        .expect("row")
        .expect("exists");
    let row_b = projection_state(&read, &wt_b.to_string())
        .expect("row")
        .expect("exists");

    assert_eq!(
        row_a.active_model_space_id.as_deref(),
        Some(target.to_string().as_str()),
        "worktree A moved on the model axis"
    );
    assert_eq!(
        row_b.active_model_space_id.as_deref(),
        Some(space_a().to_string().as_str()),
        "worktree B stayed on the old space"
    );
    assert_eq!(
        row_b.active_generation_id.as_deref(),
        Some(gen_b2.to_string().as_str()),
        "worktree B moved on the generation axis instead"
    );
    assert_eq!(row_a.status, ProjectionStatus::Clean);
    assert_eq!(row_b.status, ProjectionStatus::Clean);
}
