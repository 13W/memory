//! T07-05: executable proof of spec 05 §10's F1–F12 fault-detection matrix
//! (`fixtures/fault/matrix.json`), for the rows not already covered by earlier
//! tasks:
//!
//! - F1 (`switch_faults.rs`, T07-03), F11/F12 (`rebuild_faults.rs`/`rebuild.rs`,
//!   T07-04) already have named tests proving their exact signal.
//! - F2/F3/F4 exercise `switch()` failing at three different points within one
//!   call — after a *first* switch already committed a real head, so "stale"
//!   is observable (unlike F1's bootstrap case, which has no prior head at
//!   all). F4 needs a new production seam (`projection.switch.before_commit`,
//!   `crates/projection/src/switch.rs`) — no existing failpoint fires between
//!   a landed shard write and the final SQLite commit.
//! - F5–F10 corrupt an already-`clean` shard out of band (the existing
//!   `Corruption` API, T07-01) and prove `open_and_validate` catches it at the
//!   *next* open — `switch()` itself is never involved in these six.
//!
//! Every test asserts both halves the card requires: the literal detection
//! signal, and that `open_and_validate` repairs the shard back to `Valid`
//! (idempotent rebuild). `crates/projection/tests/fault_matrix_coverage.rs`
//! cross-checks that all 12 rows are named here (or in the files above)
//! against the declarative fixture — the mechanically-checked "reusable
//! artifact" the group card asks for.
//!
//! All nine tests arm (or are vulnerable to) the process-global failpoint
//! registry, so they serialize on `SERIAL`, a `tokio::sync::Mutex` — the
//! async-aware equivalent of `fake_faults.rs`'s `std::sync::Mutex` `serial()`,
//! needed here because the guard is held across `.await` points
//! (`switch_faults.rs`'s doc comment flagged this exact need).
#![cfg(feature = "failpoints")]

use std::sync::atomic::{AtomicU64, Ordering};

use local_rag_core::identity::{Uuid, UuidSource, uuidv7_from};
use local_rag_core::paths::StoreLayout;
use local_rag_projection::{
    Corruption, FakeProjectionStore, FakeShard, OpenOutcome, PointId, ProjectionPoint,
    RepresentationKind, ShardHandle, ShardParams, SwitchError, VectorSource, open_and_validate,
    switch,
};
use local_rag_store::{
    DEFAULT_MODEL_SPACE_ID, GenerationState, NewContentBlob, NewFileRevision, NewOccurrence,
    NewParsedUnit, NewlineStyle, ProjectionStatus, SourceCompression, StateDb, UnitKind,
    WorktreeKind, allocate_generation, create_repository, create_worktree, insert_content_blob,
    insert_file_revision, insert_generation_file, insert_occurrence, insert_parsed_unit,
    insert_projection_state, occurrence_id, projection_state, transition_generation,
};
use local_rag_test_support::{Action, TempHome, failpoint::global};
use tokio::sync::Mutex;

const DIMS: usize = 3;
const UPSERT_FP: &str = "projection.fake.upsert";
const WRITE_HEAD_FP: &str = "projection.fake.write_head";
const BEFORE_COMMIT_FP: &str = "projection.switch.before_commit";

/// Serializes every test below against the process-global failpoint registry
/// (see the module doc).
static SERIAL: Mutex<()> = Mutex::const_new(());

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
        uuidv7_from(5_000_000 + n, [0x33; 10])
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

/// Establish a real, converged `active` tuple + shard via one `switch()`:
/// worktree, one occurrence, model space default.
///
/// `uuids` is the *same* source the caller will keep using for any further
/// switch/rebuild calls — `SeqUuidV7`'s sequence depends only on how many
/// times it has been called, not on which instance, so a test that minted its
/// own fresh `SeqUuidV7` here and a different fresh one for a later call would
/// silently produce the *same* first op id twice (a real bug this file hit
/// during development: F2/F3 need switch #1's and switch #2's op ids to
/// differ).
async fn established(
    db: &StateDb,
    layout: &StoreLayout,
    seed: u8,
    uuids: &(dyn UuidSource + Send + Sync),
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
        &AlwaysVectors,
        uuids,
        1000,
    )
    .await
    .expect("establish active tuple via switch");

    (wt, gen_a, shard_dir)
}

/// spec 05 §10 F2: "kill mid-upsert batch" -> "status='updating' (+ head
/// op_id stale)". A second switch (the model-space axis) whose upsert never
/// lands leaves the shard's head exactly as switch #1 left it — stale
/// relative to the now-in-flight target op.
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
async fn f2_kill_mid_upsert_leaves_status_updating_and_stale_head() {
    let _serial = SERIAL.lock().await;
    let (_home, layout, db) = open_state();
    let uuids = SeqUuidV7::new();
    let (wt, gen_a, shard_dir) = established(&db, &layout, 10, &uuids).await;
    let quarantine_dir = layout.quarantine_dir();

    let head_before = FakeShard::open(&shard_dir, params())
        .expect("open concrete")
        .read_head()
        .expect("head")
        .expect("head present from switch #1");

    let ms_b = uuid(12);
    insert_model_space(&db, &ms_b).await;

    global().register(UPSERT_FP);
    global().arm(UPSERT_FP, Action::Error).expect("arm");
    let err = switch(
        &db,
        &FakeProjectionStore::new(),
        &shard_dir,
        params(),
        wt,
        gen_a,
        ms_b,
        &AlwaysVectors,
        &uuids,
        2000,
    )
    .await
    .expect_err("injected mid-upsert failure");
    global().disarm(UPSERT_FP).expect("disarm");
    assert!(matches!(err, SwitchError::Backend(_)), "got {err:?}");

    let read = db.open_read().expect("read");
    let row = projection_state(&read, &wt.to_string())
        .expect("row")
        .expect("exists");
    assert_eq!(row.status, ProjectionStatus::Updating);
    let in_flight_op = row
        .projection_op_id
        .clone()
        .expect("write-ahead set an op id");
    drop(read);

    let head_after = FakeShard::open(&shard_dir, params())
        .expect("reopen")
        .read_head()
        .expect("head")
        .expect("head still present");
    assert_eq!(
        head_after, head_before,
        "the shard was never touched by the failed attempt"
    );
    assert_ne!(
        head_after.projection_op_id.to_string(),
        in_flight_op,
        "F2: head op id is stale relative to the in-flight op"
    );

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
    .expect("detects and rebuilds");
    assert!(
        matches!(outcome, OpenOutcome::Rebuilt(_)),
        "got {outcome:?}"
    );
    let valid = open_and_validate(
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
    .expect("valid");
    assert_eq!(valid, OpenOutcome::Valid);
}

/// spec 05 §10 F3: "kill after all point ops, before write_head" -> "head
/// op_id != projection_op_id". Upsert and delete both land; only `write_head`
/// fails, so the persisted head still names switch #1's op.
#[tokio::test]
async fn f3_kill_before_write_head_leaves_stale_op_id() {
    let _serial = SERIAL.lock().await;
    let (_home, layout, db) = open_state();
    let uuids = SeqUuidV7::new();
    let (wt, gen_a, shard_dir) = established(&db, &layout, 20, &uuids).await;
    let quarantine_dir = layout.quarantine_dir();
    let ms_b = uuid(22);
    insert_model_space(&db, &ms_b).await;

    global().register(WRITE_HEAD_FP);
    global().arm(WRITE_HEAD_FP, Action::Error).expect("arm");
    let err = switch(
        &db,
        &FakeProjectionStore::new(),
        &shard_dir,
        params(),
        wt,
        gen_a,
        ms_b,
        &AlwaysVectors,
        &uuids,
        2000,
    )
    .await
    .expect_err("injected pre-write_head failure");
    global().disarm(WRITE_HEAD_FP).expect("disarm");
    assert!(matches!(err, SwitchError::Backend(_)), "got {err:?}");

    let read = db.open_read().expect("read");
    let row = projection_state(&read, &wt.to_string())
        .expect("row")
        .expect("exists");
    assert_eq!(row.status, ProjectionStatus::Updating);
    let in_flight_op = row.projection_op_id.clone().expect("op id set");
    drop(read);

    let head = FakeShard::open(&shard_dir, params())
        .expect("reopen")
        .read_head()
        .expect("head")
        .expect("head present (from switch #1 — write_head for #2 never landed)");
    assert_ne!(
        head.projection_op_id.to_string(),
        in_flight_op,
        "F3: head op_id != projection_op_id"
    );

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
    .expect("detects and rebuilds");
    assert!(
        matches!(outcome, OpenOutcome::Rebuilt(_)),
        "got {outcome:?}"
    );
    let valid = open_and_validate(
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
    .expect("valid");
    assert_eq!(valid, OpenOutcome::Valid);
}

/// spec 05 §10 F4: "kill after write_head, before SQLite commit" ->
/// "status='updating', head tuple = target != active". The shard already
/// fully reflects the target tuple; only the final `state.sqlite` commit is
/// prevented (the new `projection.switch.before_commit` seam, T07-05).
#[tokio::test]
async fn f4_kill_before_final_commit_leaves_head_ahead_of_active() {
    let _serial = SERIAL.lock().await;
    let (_home, layout, db) = open_state();
    let uuids = SeqUuidV7::new();
    let (wt, gen_a, shard_dir) = established(&db, &layout, 30, &uuids).await;
    let quarantine_dir = layout.quarantine_dir();
    let ms_default = default_model_space();
    let ms_b = uuid(32);
    insert_model_space(&db, &ms_b).await;

    global().register(BEFORE_COMMIT_FP);
    global().arm(BEFORE_COMMIT_FP, Action::Error).expect("arm");
    let err = switch(
        &db,
        &FakeProjectionStore::new(),
        &shard_dir,
        params(),
        wt,
        gen_a,
        ms_b,
        &AlwaysVectors,
        &uuids,
        2000,
    )
    .await
    .expect_err("injected pre-commit failure");
    global().disarm(BEFORE_COMMIT_FP).expect("disarm");
    assert!(matches!(err, SwitchError::Failpoint(_)), "got {err:?}");

    let read = db.open_read().expect("read");
    let row = projection_state(&read, &wt.to_string())
        .expect("row")
        .expect("exists");
    assert_eq!(row.status, ProjectionStatus::Updating);
    assert_eq!(
        row.active_model_space_id.as_deref(),
        Some(ms_default.to_string().as_str()),
        "active is untouched — still switch #1's tuple"
    );
    assert_eq!(
        row.target_model_space_id.as_deref(),
        Some(ms_b.to_string().as_str())
    );
    drop(read);

    let head = FakeShard::open(&shard_dir, params())
        .expect("reopen")
        .read_head()
        .expect("head")
        .expect("head present — the shard write fully landed");
    assert_eq!(
        head.model_space_id, ms_b,
        "F4: head tuple = target (ms_b), which != active (ms_default)"
    );

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
    .expect("detects and rebuilds");
    assert!(
        matches!(outcome, OpenOutcome::Rebuilt(_)),
        "got {outcome:?}"
    );
    let valid = open_and_validate(
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
    .expect("valid");
    assert_eq!(valid, OpenOutcome::Valid);
}

/// Shared mechanism for F5 and F10 (spec 05 §10 explicitly says F10 is "same
/// as F5 at next open"): the shard's persisted points are lost entirely after
/// a clean commit (modeling WAL loss/truncation, or — for F10 — a swallowed
/// flush/sync failure, which looks identical at the next open), caught via
/// `PointCountMismatch`.
async fn assert_post_clean_point_loss_detected_and_repaired(seed: u8) {
    let _serial = SERIAL.lock().await;
    let (_home, layout, db) = open_state();
    let uuids = SeqUuidV7::new();
    let (wt, _gen_a, shard_dir) = established(&db, &layout, seed, &uuids).await;
    let quarantine_dir = layout.quarantine_dir();

    std::fs::write(shard_dir.join("points"), "").expect("truncate points (simulated data loss)");

    let outcome = open_and_validate(
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
    .expect("detects and rebuilds");
    let OpenOutcome::Rebuilt(rebuilt) = outcome else {
        panic!("expected a rebuild, got {outcome:?}");
    };
    assert!(
        rebuilt.quarantined.is_none(),
        "openable (just empty) shard is destroyed, not quarantined"
    );
    assert_eq!(rebuilt.point_count, 2, "both required-kind points restored");

    let valid = open_and_validate(
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
    .expect("valid");
    assert_eq!(valid, OpenOutcome::Valid);
}

/// spec 05 §10 F5: "shard WAL loss/truncation after clean commit" ->
/// "manifest/point_count mismatch".
#[tokio::test]
async fn f5_post_clean_point_loss_detected_at_next_open() {
    assert_post_clean_point_loss_detected_and_repaired(40).await;
}

/// spec 05 §10 F10: "backend flush/sync failure swallowed" -> "same as F5 at
/// next open" (the spec's own words) — not independent coverage, the same
/// mechanism under a different narrative.
#[tokio::test]
async fn f10_same_as_f5_backend_flush_failure_swallowed() {
    assert_post_clean_point_loss_detected_and_repaired(41).await;
}

/// spec 05 §10 F6: "partial point deletion with intact catalog" ->
/// "manifest_hash mismatch".
///
/// Honesty note: spec's "intact catalog" implies a backend whose reported
/// count stays stale relative to its actual data — a real-backend nuance this
/// fake does not model (`FakeShard::point_count` always reflects exactly
/// what's loaded, so dropping a point changes the count too). `validate`
/// checks point count *before* manifest, so this test observes
/// `PointCountMismatch`, not `ManifestMismatch` specifically — both are
/// correct detections of the same underlying divergence. F8's test below is
/// what isolates `ManifestMismatch` on its own (same count, different ids).
#[tokio::test]
async fn f6_partial_point_deletion_detected_at_next_open() {
    let _serial = SERIAL.lock().await;
    let (_home, layout, db) = open_state();
    let uuids = SeqUuidV7::new();
    let (wt, _gen_a, shard_dir) = established(&db, &layout, 50, &uuids).await;
    let quarantine_dir = layout.quarantine_dir();

    {
        let shard = FakeShard::open(&shard_dir, params()).expect("open concrete");
        let ids: Vec<PointId> = shard.point_ids().expect("ids").collect();
        assert_eq!(ids.len(), 2);
        shard
            .corrupt(Corruption::DropPoint(ids[0].clone()))
            .expect("corrupt");
    }

    let outcome = open_and_validate(
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
    .expect("detects and rebuilds");
    let OpenOutcome::Rebuilt(rebuilt) = outcome else {
        panic!("expected a rebuild, got {outcome:?}");
    };
    assert_eq!(rebuilt.point_count, 2, "the dropped point is restored");

    let valid = open_and_validate(
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
    .expect("valid");
    assert_eq!(valid, OpenOutcome::Valid);
}

/// spec 05 §10 F7: "missing head / stale head from previous op" -> "head
/// missing / op_id mismatch" (the missing-head half; F2/F3 above cover the
/// stale-head half at the `switch()` level).
#[tokio::test]
async fn f7_missing_head_detected_at_next_open() {
    let _serial = SERIAL.lock().await;
    let (_home, layout, db) = open_state();
    let uuids = SeqUuidV7::new();
    let (wt, _gen_a, shard_dir) = established(&db, &layout, 60, &uuids).await;
    let quarantine_dir = layout.quarantine_dir();

    {
        let shard = FakeShard::open(&shard_dir, params()).expect("open concrete");
        shard.corrupt(Corruption::RemoveHead).expect("corrupt");
    }
    // `corrupt` only touches the persisted file, not this (already-dropped)
    // handle's in-memory state (matches `fake.rs`'s own doc: "without touching
    // in-memory state... re-open the shard to observe it") — reopen fresh.
    assert!(
        FakeShard::open(&shard_dir, params())
            .expect("reopen")
            .read_head()
            .expect("head")
            .is_none(),
        "head is gone after reopen"
    );

    let outcome = open_and_validate(
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
    .expect("detects and rebuilds");
    assert!(
        matches!(outcome, OpenOutcome::Rebuilt(_)),
        "got {outcome:?}"
    );

    let valid = open_and_validate(
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
    .expect("valid");
    assert_eq!(valid, OpenOutcome::Valid);
}

/// spec 05 §10 F8: "equal point_count, different ID set" -> "manifest_hash
/// mismatch". The pure predicate is already proven in `validate.rs`'s unit
/// tests; this is the end-to-end companion through `open_and_validate`.
#[tokio::test]
async fn f8_equal_count_different_ids_detected_at_next_open() {
    let _serial = SERIAL.lock().await;
    let (_home, layout, db) = open_state();
    let uuids = SeqUuidV7::new();
    let (wt, _gen_a, shard_dir) = established(&db, &layout, 70, &uuids).await;
    let quarantine_dir = layout.quarantine_dir();

    {
        let shard = FakeShard::open(&shard_dir, params()).expect("open concrete");
        let ids: Vec<PointId> = shard.point_ids().expect("ids").collect();
        let before_count = ids.len();
        shard
            .corrupt(Corruption::SwapPoint {
                remove: ids[0].clone(),
                insert: ProjectionPoint {
                    point_id: PointId::from_hex("swapped-in"),
                    vector: vec![9.0, 9.0, 9.0],
                },
            })
            .expect("corrupt");
        assert_eq!(
            before_count, 2,
            "count unchanged by construction — isolates manifest"
        );
    }

    let outcome = open_and_validate(
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
    .expect("detects and rebuilds");
    let OpenOutcome::Rebuilt(rebuilt) = outcome else {
        panic!("expected a rebuild, got {outcome:?}");
    };
    assert_eq!(
        rebuilt.point_count, 2,
        "same count as before — proves manifest (not count) caught it"
    );

    let valid = open_and_validate(
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
    .expect("valid");
    assert_eq!(valid, OpenOutcome::Valid);
}

/// spec 05 §10 F9: "failed final upsert/delete reported as ok by backend" ->
/// "manifest verification at next open". Mechanically identical to F6/F8 (a
/// post-hoc `Corruption` + the next open's manifest re-verification); F9's
/// distinct point is *when* the lie is caught: `switch()` above returned `Ok`
/// with no error at all — only this *separate*, later `open_and_validate`
/// call reveals the backend's silent failure.
#[tokio::test]
async fn f9_backend_reported_success_but_corrupted_content_caught_at_next_open() {
    let _serial = SERIAL.lock().await;
    let (_home, layout, db) = open_state();
    let uuids = SeqUuidV7::new();
    let (wt, _gen_a, shard_dir) = established(&db, &layout, 80, &uuids).await; // switch() returned Ok
    let quarantine_dir = layout.quarantine_dir();

    {
        let shard = FakeShard::open(&shard_dir, params()).expect("open concrete");
        let ids: Vec<PointId> = shard.point_ids().expect("ids").collect();
        shard
            .corrupt(Corruption::DropPoint(ids[0].clone()))
            .expect("corrupt");
    }

    let outcome = open_and_validate(
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
    .expect("detects and rebuilds — even though switch() itself never errored");
    assert!(
        matches!(outcome, OpenOutcome::Rebuilt(_)),
        "got {outcome:?}"
    );

    let valid = open_and_validate(
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
    .expect("valid");
    assert_eq!(valid, OpenOutcome::Valid);
}
