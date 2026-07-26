//! T07-04 acceptance tests for validate-on-open and rebuild (spec 05 §6/§7,
//! plus D-004's deferred quarantine rotation, spec 05 §8).
//!
//! Fixture helpers mirror `crates/projection/tests/switch.rs` (integration test
//! binaries can't share code without a `mod` file; this duplicates that file's
//! small helper set, matching the existing `switch.rs`/`switch_faults.rs`
//! convention). Each test first drives one real `switch()` to establish a
//! genuine `active` tuple + a correctly-converged shard — rebuild has nothing
//! to validate before any switch has ever completed — then manipulates the
//! shard or the row to create a divergence.

use std::collections::HashSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use local_rag_core::identity::{Uuid, UuidSource, uuidv7_from};
use local_rag_core::paths::StoreLayout;
use local_rag_projection::{
    FakeProjectionStore, OpenOutcome, ProjectionStore, RebuildCause, RebuildError,
    RepresentationKind, ShardParams, VectorSource, open_and_validate, switch,
};
use local_rag_store::{
    DEFAULT_MODEL_SPACE_ID, GenerationState, NewContentBlob, NewFileRevision, NewOccurrence,
    NewParsedUnit, NewlineStyle, ProjectionStatus, SourceCompression, StateDb, UnitKind,
    WorktreeKind, allocate_generation, create_repository, create_worktree, insert_content_blob,
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
        uuidv7_from(3_000_000 + n, [0x11; 10])
    }
}

/// A test-only [`VectorSource`]: returns a fixed `DIMS`-wide vector for any
/// `(occurrence_id, kind)` not explicitly [`block`](FakeVectors::block)ed, and
/// records every key actually looked up. `Mutex`-backed (not `RefCell`) so it
/// is `Sync` — required by `switch`/`open_and_validate`'s
/// `&(dyn VectorSource + Send + Sync)` parameter (T09-02, `crates/projection::
/// manager` holds this reference across an `.await` inside a spawned task).
struct FakeVectors {
    blocked: Mutex<HashSet<(String, RepresentationKind)>>,
    calls: Mutex<Vec<(String, RepresentationKind)>>,
}

impl FakeVectors {
    fn new() -> Self {
        Self {
            blocked: Mutex::new(HashSet::new()),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn block(&self, occurrence_id: &str, kind: RepresentationKind) {
        self.blocked
            .lock()
            .expect("fake vectors mutex poisoned")
            .insert((occurrence_id.to_string(), kind));
    }

    fn calls(&self) -> Vec<(String, RepresentationKind)> {
        self.calls
            .lock()
            .expect("fake vectors mutex poisoned")
            .clone()
    }
}

impl VectorSource for FakeVectors {
    fn vector(&self, occurrence_id: &str, kind: RepresentationKind) -> Option<Vec<f32>> {
        self.calls
            .lock()
            .expect("fake vectors mutex poisoned")
            .push((occurrence_id.to_string(), kind));
        if self
            .blocked
            .lock()
            .expect("fake vectors mutex poisoned")
            .contains(&(occurrence_id.to_string(), kind))
        {
            None
        } else {
            Some(vec![1.0, 0.0, 0.0])
        }
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

/// Establish a real, converged `active` tuple + shard via one `switch()`:
/// worktree, one occurrence, model space default. Returns the worktree/
/// generation ids and the shard directory.
async fn established(
    db: &StateDb,
    layout: &StoreLayout,
    seed: u8,
) -> (Uuid, Uuid, std::path::PathBuf) {
    let wt = worktree(db, seed).await;
    init_projection(db, &wt).await;
    let gen_a = allocate_ready(db, &wt, seed.wrapping_add(1)).await;
    seed_occurrence(db, &gen_a, seed.wrapping_add(2), "a.rs").await;
    let ms = default_model_space();
    let shard_dir = layout.projection_shard(&wt.to_string());

    switch(
        db,
        &FakeProjectionStore::new(),
        &shard_dir,
        params(),
        wt,
        gen_a,
        ms,
        &FakeVectors::new(),
        &SeqUuidV7::new(),
        1000,
    )
    .await
    .expect("establish active tuple via switch");

    (wt, gen_a, shard_dir)
}

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
async fn bootstrap_before_any_switch_is_no_active_tuple() {
    let (_home, layout, db) = open_state();
    let wt = worktree(&db, 1).await;
    init_projection(&db, &wt).await;
    let shard_dir = layout.projection_shard(&wt.to_string());
    let quarantine_dir = layout.quarantine_dir();
    let vectors = FakeVectors::new();
    let uuids = SeqUuidV7::new();

    let outcome = open_and_validate(
        &db,
        &FakeProjectionStore::new(),
        &shard_dir,
        &quarantine_dir,
        params(),
        wt,
        &vectors,
        &uuids,
        2000,
    )
    .await
    .expect("open_and_validate");
    assert_eq!(outcome, OpenOutcome::NoActiveTuple);
    assert!(!shard_dir.exists(), "the shard is never touched");
    assert!(vectors.calls().is_empty());
}

#[tokio::test]
async fn valid_shard_stays_valid_and_second_open_is_a_true_no_op() {
    let (_home, layout, db) = open_state();
    let (wt, _gen_a, shard_dir) = established(&db, &layout, 10).await;
    let quarantine_dir = layout.quarantine_dir();
    let vectors = FakeVectors::new();
    let uuids = SeqUuidV7::new();

    for attempt in 0..2 {
        let outcome = open_and_validate(
            &db,
            &FakeProjectionStore::new(),
            &shard_dir,
            &quarantine_dir,
            params(),
            wt,
            &vectors,
            &uuids,
            2000 + attempt,
        )
        .await
        .expect("open_and_validate");
        assert_eq!(outcome, OpenOutcome::Valid, "attempt {attempt}");
    }
    assert!(
        vectors.calls().is_empty(),
        "a valid shard never needs a vector lookup"
    );
}

#[tokio::test]
async fn unopenable_shard_is_quarantined_and_rebuilt() {
    let (_home, layout, db) = open_state();
    let (wt, _gen_a, shard_dir) = established(&db, &layout, 20).await;
    let quarantine_dir = layout.quarantine_dir();

    // Corrupt the persisted points file directly (mirrors
    // `fake_shard.rs::open_on_corrupt_points_file_errors` — no failpoints
    // feature needed).
    std::fs::write(shard_dir.join("points"), "0a\tnothex\n").expect("corrupt points file");

    let vectors = FakeVectors::new();
    let uuids = SeqUuidV7::new();
    let outcome = open_and_validate(
        &db,
        &FakeProjectionStore::new(),
        &shard_dir,
        &quarantine_dir,
        params(),
        wt,
        &vectors,
        &uuids,
        2000,
    )
    .await
    .expect("open_and_validate");

    let OpenOutcome::Rebuilt(rebuilt) = outcome else {
        panic!("expected a rebuild, got {outcome:?}");
    };
    let quarantined = rebuilt
        .quarantined
        .expect("unopenable shard is quarantined, not destroyed");
    assert!(
        quarantined.exists(),
        "the corrupt shard dir was moved aside"
    );
    assert!(
        quarantined.join("points").exists(),
        "the corrupt directory's contents were preserved for diagnostics"
    );

    // The fresh shard is valid.
    let outcome2 = open_and_validate(
        &db,
        &FakeProjectionStore::new(),
        &shard_dir,
        &quarantine_dir,
        params(),
        wt,
        &vectors,
        &uuids,
        3000,
    )
    .await
    .expect("second open_and_validate");
    assert_eq!(outcome2, OpenOutcome::Valid);
}

#[tokio::test]
async fn quarantine_rotation_keeps_at_most_two() {
    let (_home, layout, db) = open_state();
    let (wt, _gen_a, shard_dir) = established(&db, &layout, 30).await;
    let quarantine_dir = layout.quarantine_dir();
    let vectors = FakeVectors::new();
    let uuids = SeqUuidV7::new();

    for round in 0..3u32 {
        std::fs::write(shard_dir.join("points"), "0a\tnothex\n").expect("corrupt points file");
        let outcome = open_and_validate(
            &db,
            &FakeProjectionStore::new(),
            &shard_dir,
            &quarantine_dir,
            params(),
            wt,
            &vectors,
            &uuids,
            2000 + i64::from(round),
        )
        .await
        .expect("open_and_validate");
        assert!(matches!(outcome, OpenOutcome::Rebuilt(_)), "round {round}");
    }

    let prefix = format!("{wt}-");
    let remaining: Vec<_> = std::fs::read_dir(&quarantine_dir)
        .expect("read quarantine dir")
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n.starts_with(&prefix))
        })
        .collect();
    assert_eq!(
        remaining.len(),
        local_rag_projection::QUARANTINE_RETENTION,
        "exactly QUARANTINE_RETENTION entries survive 3 quarantine events"
    );
}

#[tokio::test]
async fn stale_head_tuple_triggers_rebuild() {
    let (_home, layout, db) = open_state();
    let (wt, gen_a, shard_dir) = established(&db, &layout, 40).await;
    let quarantine_dir = layout.quarantine_dir();

    // Write a head claiming a bogus op id — an `OpIdMismatch` divergence,
    // without needing the failpoints-gated `Corruption` API.
    {
        let shard = FakeProjectionStore::new()
            .open(&shard_dir, params())
            .expect("reopen");
        let bogus_op: Uuid = "0000000e-0000-7000-8000-00000000ffff".parse().unwrap();
        let ids: Vec<local_rag_projection::PointId> = shard.point_ids().expect("ids").collect();
        let bogus_head =
            local_rag_projection::head(wt, gen_a, default_model_space(), bogus_op, &ids);
        shard.write_head(&bogus_head).expect("write bogus head");
    }

    let vectors = FakeVectors::new();
    let uuids = SeqUuidV7::new();
    let outcome = open_and_validate(
        &db,
        &FakeProjectionStore::new(),
        &shard_dir,
        &quarantine_dir,
        params(),
        wt,
        &vectors,
        &uuids,
        2000,
    )
    .await
    .expect("open_and_validate");
    match outcome {
        OpenOutcome::Rebuilt(r) => assert!(
            r.quarantined.is_none(),
            "openable shard is destroyed, not quarantined"
        ),
        other => panic!("expected a rebuild, got {other:?}"),
    }

    let outcome2 = open_and_validate(
        &db,
        &FakeProjectionStore::new(),
        &shard_dir,
        &quarantine_dir,
        params(),
        wt,
        &vectors,
        &uuids,
        3000,
    )
    .await
    .expect("second open_and_validate");
    assert_eq!(outcome2, OpenOutcome::Valid);
}

#[tokio::test]
async fn missing_vector_during_rebuild_leaves_status_rebuilding_and_no_partial_shard() {
    let (_home, layout, db) = open_state();
    let (wt, gen_a, shard_dir) = established(&db, &layout, 50).await;
    let quarantine_dir = layout.quarantine_dir();

    // Force a divergence (bogus op id, as above) so a rebuild is attempted.
    {
        let shard = FakeProjectionStore::new()
            .open(&shard_dir, params())
            .expect("reopen");
        let bogus_op: Uuid = "0000000e-0000-7000-8000-00000000ffff".parse().unwrap();
        let ids: Vec<local_rag_projection::PointId> = shard.point_ids().expect("ids").collect();
        let bogus_head =
            local_rag_projection::head(wt, gen_a, default_model_space(), bogus_op, &ids);
        shard.write_head(&bogus_head).expect("write bogus head");
    }

    let vectors = FakeVectors::new();
    // Block the code_raw vector of the single real occurrence this fixture
    // seeded (`established(.., 50)` -> `seed_occurrence(.., 52, "a.rs")` ->
    // `unit = uuid(52 + 40)`, matching that helper's own seed derivation).
    let real_occ = occurrence_id(&gen_a.to_string(), "a.rs", &uuid(92).to_string());
    vectors.block(&real_occ, RepresentationKind::CodeRaw);

    let uuids = SeqUuidV7::new();
    let err = open_and_validate(
        &db,
        &FakeProjectionStore::new(),
        &shard_dir,
        &quarantine_dir,
        params(),
        wt,
        &vectors,
        &uuids,
        2000,
    )
    .await
    .expect_err("missing vector aborts the rebuild");
    assert!(
        matches!(
            err,
            RebuildError::MissingVector {
                representation_kind: RepresentationKind::CodeRaw,
                ..
            }
        ),
        "got {err:?}"
    );

    let read = db.open_read().expect("read");
    let row = projection_state(&read, &wt.to_string())
        .expect("row")
        .expect("exists");
    assert_eq!(
        row.status,
        ProjectionStatus::Rebuilding,
        "never clean with a partial expected set"
    );

    // The shard was destroyed and recreated fresh but never got a head or any
    // points written (the loop errors before any shard write).
    let shard = FakeProjectionStore::new()
        .open(&shard_dir, params())
        .expect("reopen fresh shard");
    assert_eq!(shard.point_count().expect("count"), 0);
    assert!(shard.read_head().expect("head").is_none());
}

#[tokio::test]
async fn cause_is_recorded_for_diagnostics() {
    // A quick check that RebuildCause's Display text is distinct for the two
    // branches (used as `last_error`), so a diagnostic reading the row can
    // tell a corruption apart from an ordinary divergence.
    let unopenable = RebuildCause::Unopenable.to_string();
    let divergent =
        RebuildCause::Divergent(local_rag_projection::Divergence::HeadMissing).to_string();
    assert_ne!(unopenable, divergent);
    assert!(unopenable.contains("corruption"));
    assert!(divergent.contains("divergence"));
}
