//! Fault-injection test for the write-ahead switch (spec 05 §5): "backend error
//! leaves detectable updating" (T07-03's fourth acceptance test). Reuses the
//! *existing* `projection.fake.upsert` failpoint from `crates/projection/src/fake.rs`
//! (T07-01) — no new failpoint names needed. Gated on `failpoints`.
//!
//! Unlike `fake_faults.rs` (several sync `#[test]`s in one binary, guarded by a
//! `serial()` mutex against the process-global failpoint registry), this file
//! has exactly one `#[tokio::test]` — a single test is trivially serial within
//! its own process, and holding a `std::sync::Mutex` guard across `.await`
//! would itself be a bug (`clippy::await_holding_lock`). If a later task adds
//! more failpoint-driven async tests here, reach for `tokio::sync::Mutex`
//! instead of this crate's sync guard idiom.
#![cfg(feature = "failpoints")]

use std::sync::atomic::{AtomicU64, Ordering};

use local_rag_core::identity::{Uuid, UuidSource, uuidv7_from};
use local_rag_core::paths::StoreLayout;
use local_rag_projection::{
    FakeProjectionStore, RepresentationKind, ShardParams, SwitchError, VectorSource, switch,
};
use local_rag_store::{
    DEFAULT_MODEL_SPACE_ID, GenerationState, NewContentBlob, NewFileRevision, NewOccurrence,
    NewParsedUnit, NewlineStyle, ProjectionStatus, SourceCompression, StateDb, UnitKind,
    WorktreeKind, allocate_generation, create_repository, create_worktree, current_generation,
    generation_state, insert_content_blob, insert_file_revision, insert_generation_file,
    insert_occurrence, insert_parsed_unit, insert_projection_state, occurrence_id,
    projection_state, transition_generation,
};
use local_rag_test_support::{Action, TempHome, failpoint::global};

const DIMS: usize = 3;
const UPSERT_FP: &str = "projection.fake.upsert";

fn params() -> ShardParams {
    ShardParams { dimensions: DIMS }
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
        uuidv7_from(2_000_000 + n, [0xEF; 10])
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

/// Spec 05 §5: "crash between 2-4 leaves `status='updating'`" — a backend error
/// during the desired-set reconcile must leave `state.sqlite` exactly where the
/// write-ahead left it, and must not touch the generation machine at all (the
/// commit transaction never runs).
#[tokio::test]
async fn backend_error_leaves_detectable_updating() {
    let (_home, layout, db) = open_state();
    let wt = worktree(&db, 1).await;
    init_projection(&db, &wt).await;
    let gen_a = allocate_ready(&db, &wt, 2).await;
    seed_occurrence(&db, &gen_a, 3, "a.rs").await;
    let ms = default_model_space();
    let shard_dir = layout.projection_shard(&wt.to_string());

    global().register(UPSERT_FP);
    global().arm(UPSERT_FP, Action::Error).expect("arm");
    let uuids = SeqUuidV7::new();
    let err = switch(
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
    .expect_err("injected backend failure");
    global().disarm(UPSERT_FP).expect("disarm");
    assert!(matches!(err, SwitchError::Backend(_)), "got {err:?}");

    // The write-ahead landed; the commit never ran.
    let read = db.open_read().expect("read");
    let row = projection_state(&read, &wt.to_string())
        .expect("row")
        .expect("exists");
    assert_eq!(
        row.status,
        ProjectionStatus::Updating,
        "detectable: status left exactly as the write-ahead set it"
    );
    assert_eq!(
        row.active_generation_id, None,
        "commit never activated the target"
    );
    assert_eq!(
        generation_state(&read, &gen_a.to_string()).expect("state"),
        Some(GenerationState::ProjectionReady),
        "the generation machine was never touched — commit did not run"
    );
    assert_eq!(
        current_generation(&read, &wt.to_string()).expect("cur"),
        None
    );

    // A clean retry (failpoint disarmed) now converges normally.
    let outcome = switch(
        &db,
        &FakeProjectionStore::new(),
        &shard_dir,
        params(),
        wt,
        gen_a,
        ms,
        &AlwaysVectors,
        &uuids,
        2000,
    )
    .await
    .expect("retry succeeds once the failpoint is disarmed");
    assert_eq!(
        outcome.upserted, 2,
        "both required-kind points for the one occurrence"
    );
}
