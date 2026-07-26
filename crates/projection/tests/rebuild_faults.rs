//! Fault-injection test for rebuild (spec 05 §10 F11: "crash during rebuild ->
//! `status='rebuilding'` -> rebuild restarts"). Reuses the *existing*
//! `projection.fake.upsert` failpoint from `crates/projection/src/fake.rs`
//! (T07-01) — no new failpoint names needed. Gated on `failpoints`.
#![cfg(feature = "failpoints")]

use std::sync::atomic::{AtomicU64, Ordering};

use local_rag_core::identity::{Uuid, UuidSource, uuidv7_from};
use local_rag_core::paths::StoreLayout;
use local_rag_projection::{
    FakeProjectionStore, OpenOutcome, ProjectionStore, RebuildError, RepresentationKind,
    ShardParams, VectorSource, open_and_validate, switch,
};
use local_rag_store::{
    DEFAULT_MODEL_SPACE_ID, GenerationState, NewContentBlob, NewFileRevision, NewOccurrence,
    NewParsedUnit, NewlineStyle, ProjectionStatus, SourceCompression, StateDb, UnitKind,
    WorktreeKind, allocate_generation, create_repository, create_worktree, insert_content_blob,
    insert_file_revision, insert_generation_file, insert_occurrence, insert_parsed_unit,
    insert_projection_state, projection_state, transition_generation,
};
use local_rag_test_support::{Action, TempHome, failpoint::global};

const DIMS: usize = 3;
const UPSERT_FP: &str = "projection.fake.upsert";

fn params() -> ShardParams {
    ShardParams::with_dimensions(DIMS)
}

fn default_model_space() -> Uuid {
    DEFAULT_MODEL_SPACE_ID
        .parse()
        .expect("default model space id parses")
}

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
        uuidv7_from(4_000_000 + n, [0x22; 10])
    }
}

struct AlwaysVectors;

impl VectorSource for AlwaysVectors {
    fn vector(&self, _occurrence_id: &str, _kind: RepresentationKind) -> Option<Vec<f32>> {
        Some(vec![1.0, 0.0, 0.0])
    }
}

fn open_state() -> (TempHome, StoreLayout, StateDb) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    (home, layout, db)
}

fn uuid(seed: u8) -> Uuid {
    let mut rand = [0u8; 10];
    rand[9] = seed;
    uuidv7_from(1000, rand)
}

async fn worktree(db: &StateDb, seed: u8) -> Uuid {
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

async fn allocate_ready(db: &StateDb, worktree_id: &Uuid, gen_seed: u8) -> Uuid {
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

async fn seed_occurrence(db: &StateDb, generation_id: &Uuid, seed: u8, path: &str) -> String {
    let gen_str = generation_id.to_string();
    let revision = uuid(seed).to_string();
    let blob = uuid(seed.wrapping_add(30)).to_string();
    let unit = uuid(seed.wrapping_add(40)).to_string();
    let occ = local_rag_store::occurrence_id(&gen_str, path, &unit);
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

/// spec 05 §10 F11: a backend failure mid-rebuild leaves `status='rebuilding'`;
/// calling `open_and_validate` again restarts and converges.
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

#[tokio::test]
async fn crash_during_rebuild_leaves_rebuilding_and_retry_converges() {
    let (_home, layout, db) = open_state();
    let wt = worktree(&db, 1).await;
    init_projection(&db, &wt).await;
    let gen_a = allocate_ready(&db, &wt, 2).await;
    seed_occurrence(&db, &gen_a, 3, "a.rs").await;
    let ms = default_model_space();
    let shard_dir = layout.projection_shard(&wt.to_string());
    let quarantine_dir = layout.quarantine_dir();
    let uuids = SeqUuidV7::new();

    switch(
        &db,
        &FakeProjectionStore::new(),
        &shard_dir,
        params(),
        wt,
        gen_a,
        ms,
        &AlwaysVectors,
        &uuids,
        1000,
    )
    .await
    .expect("establish active tuple");

    // Force a divergence so open_and_validate attempts a rebuild: write a head
    // claiming a bogus op id.
    {
        let shard = FakeProjectionStore::new()
            .open(&shard_dir, params())
            .expect("reopen");
        let bogus_op: Uuid = "0000000e-0000-7000-8000-00000000ffff".parse().unwrap();
        let ids: Vec<local_rag_projection::PointId> = shard.point_ids().expect("ids").collect();
        let bogus_head = local_rag_projection::head(wt, gen_a, ms, bogus_op, &ids);
        shard.write_head(&bogus_head).expect("write bogus head");
    }

    global().register(UPSERT_FP);
    global().arm(UPSERT_FP, Action::Error).expect("arm");
    let err = open_and_validate(
        &db,
        &FakeProjectionStore::new(),
        &shard_dir,
        &quarantine_dir,
        params(),
        wt,
        &AlwaysVectors,
        &uuids,
        2000,
    )
    .await
    .expect_err("injected backend failure mid-rebuild");
    global().disarm(UPSERT_FP).expect("disarm");
    assert!(matches!(err, RebuildError::Backend(_)), "got {err:?}");

    let read = db.open_read().expect("read");
    let row = projection_state(&read, &wt.to_string())
        .expect("row")
        .expect("exists");
    assert_eq!(
        row.status,
        ProjectionStatus::Rebuilding,
        "F11: status left exactly as begin_rebuild set it"
    );
    drop(read);

    // Retry (failpoint disarmed) restarts and converges.
    let outcome = open_and_validate(
        &db,
        &FakeProjectionStore::new(),
        &shard_dir,
        &quarantine_dir,
        params(),
        wt,
        &AlwaysVectors,
        &uuids,
        3000,
    )
    .await
    .expect("retry converges");
    let OpenOutcome::Rebuilt(rebuilt) = outcome else {
        panic!("expected a rebuild on the restart, got {outcome:?}");
    };
    assert_eq!(rebuilt.point_count, 2, "both required-kind points restored");

    let outcome2 = open_and_validate(
        &db,
        &FakeProjectionStore::new(),
        &shard_dir,
        &quarantine_dir,
        params(),
        wt,
        &AlwaysVectors,
        &uuids,
        4000,
    )
    .await
    .expect("final open is valid");
    assert_eq!(outcome2, OpenOutcome::Valid);
}
