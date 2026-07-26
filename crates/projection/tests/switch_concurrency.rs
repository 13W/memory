//! T09-04 acceptance test for two-axis switch interleaving under real
//! concurrent load (spec 14 §4 "Two-axis interleaving": alternate generation
//! and model-space switches; assert serialization and correct final tuple).
//!
//! Fixture helpers duplicate `crates/projection/tests/switch.rs`'s own
//! (`params`, `default_model_space`, `SeqUuidV7`, `uuid`, `worktree`,
//! `init_projection`, `allocate_ready`, `seed_occurrence`,
//! `insert_model_space`) — integration test binaries can't share code without
//! a `mod` file, matching this repo's established per-file convention.
//! `open_state` deviates from `switch.rs`'s own (returns `Arc<StateDb>`, not a
//! bare `StateDb`) since several spawned tasks need shared ownership.
//!
//! **Correctness note driving the whole design:**
//! `GenerationState::check_transition` (`crates/store/src/registry/
//! generation.rs`) has no `Retiring → Active` edge — once a generation
//! retires it can never reactivate. So the generation axis cannot safely
//! "toggle" between two fixed values the way the model axis can (`commit_switch`
//! never state-transitions `model_space` rows at all). Each generation-axis
//! task below therefore owns its own dedicated, never-reused generation and
//! always moves *toward* it — a monotonic chain, correct under any
//! interleaving order — while model-axis tasks freely toggle between two
//! fixed, pre-established model spaces.
//!
//! Deterministic despite real concurrency: an isolated [`TempHome`], fixed
//! `now_ms` literals, and the actual property under test — L2.write
//! serialization — is what makes every switch's "read current, move one
//! axis" step race-free regardless of `tokio`'s scheduling. `no deadlock` is
//! an explicit assertable property (`tokio::time::timeout`), not left to an
//! external CI-level timeout.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use local_rag_core::identity::{Uuid, UuidSource, uuidv7_from};
use local_rag_core::paths::StoreLayout;
use local_rag_projection::{FakeProjectionStore, ShardParams, SwitchError, VectorSource, switch};
use local_rag_store::lock::WorktreeLockRegistry;
use local_rag_store::{
    DEFAULT_MODEL_SPACE_ID, GenerationState, NewContentBlob, NewFileRevision, NewOccurrence,
    NewParsedUnit, NewlineStyle, SourceCompression, StateDb, UnitKind, WorktreeKind,
    allocate_generation, create_repository, create_worktree, insert_content_blob,
    insert_file_revision, insert_generation_file, insert_occurrence, insert_parsed_unit,
    insert_projection_state, occurrence_id, projection_state, transition_generation,
};
use local_rag_test_support::TempHome;

const DIMS: usize = 3;

fn params() -> ShardParams {
    ShardParams::with_dimensions(DIMS)
}

fn default_model_space() -> Uuid {
    DEFAULT_MODEL_SPACE_ID
        .parse()
        .expect("default model space id parses")
}

/// A seeded, deterministic [`UuidSource`], `Arc`-shared across concurrent
/// tasks so `projection_op_id`s never collide.
struct SeqUuidV7 {
    counter: AtomicU64,
}

impl SeqUuidV7 {
    fn new() -> Self {
        Self {
            counter: AtomicU64::new(0),
        }
    }
}

impl UuidSource for SeqUuidV7 {
    fn next_uuid(&self) -> Uuid {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        uuidv7_from(5_000_000 + n, [0x44; 10])
    }
}

/// A stateless [`VectorSource`]: always returns a fixed `DIMS`-wide vector.
struct AlwaysVectors;

impl VectorSource for AlwaysVectors {
    fn vector(
        &self,
        _occurrence_id: &str,
        _kind: local_rag_projection::RepresentationKind,
    ) -> Option<Vec<f32>> {
        Some(vec![1.0, 0.0, 0.0])
    }
}

fn open_state() -> (TempHome, StoreLayout, Arc<StateDb>) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let db = Arc::new(StateDb::open(layout.state_db()).expect("open state.sqlite"));
    (home, layout, db)
}

fn uuid(seed: u16) -> Uuid {
    let mut rand = [0u8; 10];
    rand[8] = (seed >> 8) as u8;
    rand[9] = (seed & 0xff) as u8;
    uuidv7_from(3000, rand)
}

async fn worktree(db: &StateDb, seed: u16) -> Uuid {
    let repo = uuid(seed).to_string();
    let wt = uuid(seed.wrapping_add(100));
    let (r, w) = (repo, wt.to_string());
    db.writer()
        .transaction(move |tx| {
            create_repository(tx, &r, None, 1000)?;
            create_worktree(tx, &w, &r, WorktreeKind::Main, 1000)
        })
        .await
        .expect("create repo + worktree");
    wt
}

async fn init_projection(db: &StateDb, worktree_id: &Uuid) {
    let w = worktree_id.to_string();
    db.writer()
        .transaction(move |tx| insert_projection_state(tx, &w, 1000))
        .await
        .expect("init projection state");
    // The default model space must declare what it requires now that the
    // expected point set joins the registry (T11-05).
    register_code_representations(db, &default_model_space()).await;
}

async fn allocate_ready(db: &StateDb, worktree_id: &Uuid, gen_seed: u16) -> Uuid {
    let genr = uuid(gen_seed);
    let (w, g) = (worktree_id.to_string(), genr.to_string());
    db.writer()
        .transaction(move |tx| allocate_generation(tx, &w, &g, 1000).map(|_| ()))
        .await
        .expect("allocate generation");
    let g2 = genr.to_string();
    db.writer()
        .transaction(move |tx| transition_generation(tx, &g2, GenerationState::ProjectionReady))
        .await
        .expect("transition tx (infra)")
        .expect("building -> projection_ready is legal");
    genr
}

async fn seed_occurrence(db: &StateDb, generation_id: &Uuid, seed: u16, path: &str) -> String {
    let gen_str = generation_id.to_string();
    let revision = uuid(seed).to_string();
    let blob = uuid(seed.wrapping_add(30)).to_string();
    let unit = uuid(seed.wrapping_add(40)).to_string();
    let occ = occurrence_id(&gen_str, path, &unit);
    let (rev, b, u, g, p, occ2) = (revision, blob, unit, gen_str, path.to_string(), occ.clone());
    db.writer()
        .transaction(move |tx| {
            insert_file_revision(
                tx,
                &NewFileRevision {
                    file_revision_id: &rev,
                    content_hash: &rev,
                    parser_fingerprint: "fp",
                    source_blob: b"hello\n",
                    compression: SourceCompression::None,
                    source_encoding: "utf-8",
                    newline_style: NewlineStyle::Lf,
                    source_size: 6,
                },
                1000,
            )?;
            insert_content_blob(
                tx,
                &NewContentBlob {
                    blob_id: &b,
                    language: "rust",
                    algo_version: 1,
                    normalization_version: 1,
                },
                1000,
            )?;
            insert_parsed_unit(
                tx,
                &NewParsedUnit {
                    unit_id: &u,
                    file_revision_id: &rev,
                    unit_kind: UnitKind::Symbol,
                    syntax_locator: "fn:main",
                    blob_id: &b,
                    span_start: 0,
                    span_end: 6,
                    local_name: Some("main"),
                    kind: Some("fn"),
                    parent_unit_id: None,
                },
            )?;
            insert_generation_file(tx, &g, &p, &p, &rev)?;
            insert_occurrence(
                tx,
                &NewOccurrence {
                    occurrence_id: &occ2,
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

async fn insert_model_space(db: &StateDb, id: &Uuid) {
    let i = id.to_string();
    db.writer()
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO model_space (model_space_id, display_name, state, created_at, updated_at) \
                 VALUES (?1, ?2, 'active', 1000, 1000)",
                local_rag_store::rusqlite::params![i, format!("space-{i}")],
            )
            .map(|_| ())
        })
        .await
        .expect("insert model space");
    register_code_representations(db, id).await;
}

/// Run one generation-axis switch: always moves toward `my_gen` (a dedicated,
/// never-reused generation for this task), preserving whatever model space is
/// current at the moment this task actually acquires `L2.write`.
#[allow(clippy::too_many_arguments)]
async fn generation_axis_task(
    db: Arc<StateDb>,
    locks: Arc<WorktreeLockRegistry>,
    shard_dir: std::path::PathBuf,
    wt: Uuid,
    my_gen: Uuid,
    uuids: Arc<SeqUuidV7>,
    order: Arc<Mutex<Vec<(String, String)>>>,
    now_ms: i64,
) -> Result<(), SwitchError> {
    let wt_str = wt.to_string();
    locks
        .write(&wt_str, async {
            let current_ms = {
                let read = db.open_read().expect("read conn");
                let row = projection_state(&read, &wt_str)
                    .expect("read projection state")
                    .expect("projection state row exists");
                row.active_model_space_id.expect("active model space set")
            };
            let target_ms: Uuid = current_ms.parse().expect("valid model space uuid");
            let outcome = switch(
                &db,
                &FakeProjectionStore::new(),
                &shard_dir,
                params(),
                wt,
                my_gen,
                target_ms,
                &AlwaysVectors,
                uuids.as_ref(),
                now_ms,
            )
            .await;
            if outcome.is_ok() {
                order
                    .lock()
                    .expect("order mutex poisoned")
                    .push((my_gen.to_string(), target_ms.to_string()));
            }
            outcome.map(|_| ())
        })
        .await
}

/// Run one model-axis switch: reads the current tuple fresh under the lock
/// and toggles to whichever of `{ms_default, ms_b}` is NOT current, keeping
/// the generation exactly as read.
#[allow(clippy::too_many_arguments)]
async fn model_axis_task(
    db: Arc<StateDb>,
    locks: Arc<WorktreeLockRegistry>,
    shard_dir: std::path::PathBuf,
    wt: Uuid,
    ms_default: Uuid,
    ms_b: Uuid,
    uuids: Arc<SeqUuidV7>,
    order: Arc<Mutex<Vec<(String, String)>>>,
    now_ms: i64,
) -> Result<(), SwitchError> {
    let wt_str = wt.to_string();
    locks
        .write(&wt_str, async {
            let (current_gen, current_ms) = {
                let read = db.open_read().expect("read conn");
                let row = projection_state(&read, &wt_str)
                    .expect("read projection state")
                    .expect("projection state row exists");
                (
                    row.active_generation_id.expect("active generation set"),
                    row.active_model_space_id.expect("active model space set"),
                )
            };
            let target_gen: Uuid = current_gen.parse().expect("valid generation uuid");
            let target_ms = if current_ms == ms_default.to_string() {
                ms_b
            } else {
                ms_default
            };
            let outcome = switch(
                &db,
                &FakeProjectionStore::new(),
                &shard_dir,
                params(),
                wt,
                target_gen,
                target_ms,
                &AlwaysVectors,
                uuids.as_ref(),
                now_ms,
            )
            .await;
            if outcome.is_ok() {
                order
                    .lock()
                    .expect("order mutex poisoned")
                    .push((target_gen.to_string(), target_ms.to_string()));
            }
            outcome.map(|_| ())
        })
        .await
}

/// **"axes serialize and final tuple deterministic"** (spec 14 §4, card
/// bullet 2): 3 generation-axis + 3 model-axis switch tasks, spawned to race
/// on real OS threads, each reading current state fresh under `L2.write`
/// before choosing its one-axis move. Every switch must succeed (proving
/// serialization prevented any stale-current race), and the final DB tuple
/// must equal whichever switch actually landed last.
/// Register the two code representations (`code_raw`, `code_context`) as
/// `required` for `model_space_id`.
///
/// T11-05 replaced `expected::REQUIRED_REPRESENTATION_KINDS`'s hardcoded pair
/// with a real `model_space_representation` join, so a fixture's model space now
/// has to declare what it requires. Registering exactly that pair keeps every
/// pre-existing expectation in this file (2 points per occurrence) unchanged.
async fn register_code_representations(db: &StateDb, model_space_id: &Uuid) {
    let space = model_space_id.to_string();
    db.writer()
        .transaction(move |tx| {
            for (i, kind) in [
                local_rag_store::RepresentationKind::CodeRaw,
                local_rag_store::RepresentationKind::CodeContext,
            ]
            .into_iter()
            .enumerate()
            {
                let representation_id = format!("{space}-repr-{i}");
                let id = local_rag_store::register_representation(
                    tx,
                    &representation_id,
                    &local_rag_store::RepresentationKey {
                        kind,
                        representation_version: 1,
                        normalization_version: 1,
                        model_id: format!("test-model-{space}"),
                        dimensions: DIMS as u32,
                        distance_metric: local_rag_store::DistanceMetric::Cosine,
                    },
                    1000,
                )?;
                local_rag_store::set_model_space_representation(tx, &space, kind, &id, true, 1000)?;
            }
            Ok(())
        })
        .await
        .expect("register code representations");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_axis_switches_serialize_to_a_deterministic_final_tuple() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let (_home, layout, db) = open_state();
        let wt = worktree(&db, 10).await;
        init_projection(&db, &wt).await;

        // Bootstrap: establish (gen_boot, ms_default) synchronously before any
        // concurrent task runs.
        let gen_boot = allocate_ready(&db, &wt, 20).await;
        seed_occurrence(&db, &gen_boot, 21, "boot.rs").await;
        let ms_default = default_model_space();
        let shard_dir = layout.projection_shard(&wt.to_string());
        let uuids = Arc::new(SeqUuidV7::new());
        switch(
            &db,
            &FakeProjectionStore::new(),
            &shard_dir,
            params(),
            wt,
            gen_boot,
            ms_default,
            &AlwaysVectors,
            uuids.as_ref(),
            500,
        )
        .await
        .expect("bootstrap switch");

        // 3 dedicated generations, one per generation-axis task — never reused,
        // since Retiring -> Active is illegal (see module docs).
        let gen_a = allocate_ready(&db, &wt, 30).await;
        seed_occurrence(&db, &gen_a, 31, "a.rs").await;
        let gen_b = allocate_ready(&db, &wt, 40).await;
        seed_occurrence(&db, &gen_b, 41, "b.rs").await;
        let gen_c = allocate_ready(&db, &wt, 50).await;
        seed_occurrence(&db, &gen_c, 51, "c.rs").await;

        let ms_b = uuid(60);
        insert_model_space(&db, &ms_b).await;

        let locks = Arc::new(WorktreeLockRegistry::new());
        let order: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));

        let mut handles = Vec::new();
        for my_gen in [gen_a, gen_b, gen_c] {
            handles.push(tokio::spawn(generation_axis_task(
                db.clone(),
                locks.clone(),
                shard_dir.clone(),
                wt,
                my_gen,
                uuids.clone(),
                order.clone(),
                1000,
            )));
        }
        for _ in 0..3 {
            handles.push(tokio::spawn(model_axis_task(
                db.clone(),
                locks.clone(),
                shard_dir.clone(),
                wt,
                ms_default,
                ms_b,
                uuids.clone(),
                order.clone(),
                1000,
            )));
        }

        for h in handles {
            h.await
                .expect("join task")
                .expect("every switch must succeed — a failure proves a serialization bug");
        }

        let final_row = {
            let read = db.open_read().expect("read conn");
            projection_state(&read, &wt.to_string())
                .expect("read projection state")
                .expect("row exists")
        };
        let final_tuple = (
            final_row.active_generation_id.expect("active generation"),
            final_row.active_model_space_id.expect("active model space"),
        );

        let recorded = order.lock().expect("order mutex poisoned");
        let last = recorded
            .last()
            .expect("at least one concurrent switch recorded");
        assert_eq!(
            &final_tuple, last,
            "final DB tuple must equal whichever switch actually landed last"
        );
        assert_eq!(
            recorded.len(),
            6,
            "all 6 concurrent switches must have succeeded and recorded"
        );
    })
    .await
    .expect("must not deadlock");
}
