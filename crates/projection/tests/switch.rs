//! T07-03 acceptance tests for the desired-set write-ahead switch (spec 05 §5).
//!
//! All tests are deterministic: an isolated [`TempHome`], fixed `now_ms`
//! literals, and a seeded [`SeqUuidV7`] (a local `UuidSource` double —
//! `test-support` is deliberately dependency-free of this, mirroring
//! `crates/index/tests/reconcile.rs`). Fixtures build a **real** worktree +
//! generation + occurrence chain through `local-rag-store`'s own writers (no
//! parser/reconcile pipeline needed — `crates/store/tests/code.rs`'s
//! `seed_revision`/`seed_unit` pattern, folded into one `seed_occurrence`
//! helper).

use std::collections::HashSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use local_rag_core::identity::{Uuid, UuidSource, uuidv7_from};
use local_rag_core::paths::StoreLayout;
use local_rag_projection::{
    FakeProjectionStore, PointId, ProjectionPoint, ProjectionStore, RepresentationKind,
    ShardParams, SwitchError, VectorSource, expected_points, switch,
};
use local_rag_store::{
    DEFAULT_MODEL_SPACE_ID, GenerationState, NewContentBlob, NewFileRevision, NewOccurrence,
    NewParsedUnit, NewlineStyle, ProjectionStatus, SourceCompression, StateDb, UnitKind,
    WorktreeKind, allocate_generation, create_repository, create_worktree, current_generation,
    generation_state, insert_content_blob, insert_file_revision, insert_generation_file,
    insert_occurrence, insert_parsed_unit, insert_projection_state, occurrence_id,
    projection_state, transition_generation,
};
use local_rag_test_support::TempHome;

const DIMS: usize = 3;

fn params() -> ShardParams {
    ShardParams { dimensions: DIMS }
}

fn default_model_space() -> Uuid {
    DEFAULT_MODEL_SPACE_ID
        .parse()
        .expect("default model space id parses")
}

/// A seeded, deterministic [`UuidSource`] (mirrors `crates/index/tests/reconcile.rs`).
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
        uuidv7_from(1_000_000 + n, [0xCD; 10])
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

    /// Make `vector()` return `None` for this key (models a coverage gap).
    fn block(&self, occurrence_id: &str, kind: RepresentationKind) {
        self.blocked
            .lock()
            .expect("fake vectors mutex poisoned")
            .insert((occurrence_id.to_string(), kind));
    }

    /// Every `(occurrence_id, kind)` key looked up so far.
    fn calls(&self) -> HashSet<(String, RepresentationKind)> {
        self.calls
            .lock()
            .expect("fake vectors mutex poisoned")
            .iter()
            .cloned()
            .collect()
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

/// A temporary store with an ensured tree, an opened [`StateDb`], and the
/// [`StoreLayout`] used to derive real shard directories
/// (`layout.projection_shard(&worktree_id.to_string())`, spec 05 §2).
fn open_state() -> (TempHome, StoreLayout, StateDb) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    (home, layout, db)
}

/// A distinct, deterministic UUIDv7 keyed by `seed`.
fn uuid(seed: u8) -> Uuid {
    let mut rand = [0u8; 10];
    rand[9] = seed;
    uuidv7_from(1000, rand)
}

/// Create a repository and one `active` main worktree; returns `worktree_id`.
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

/// Initialize a `clean`, empty projection state row for `worktree_id`.
async fn init_projection(db: &StateDb, worktree_id: &Uuid) {
    let w = worktree_id.to_string();
    db.writer()
        .transaction(move |tx| insert_projection_state(tx, &w, 1000))
        .await
        .expect("init projection state");
}

/// Allocate a generation and drive it straight to `projection_ready` (mirroring
/// what the real generation builder, T05-03, does before a switch is ever
/// attempted — `commit_switch`'s pre-flight requires this).
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

/// Seed one real occurrence in `generation_id` at `path` (a full
/// `file_revision` + `content_blob` + `parsed_unit` + `generation_file` +
/// `generation_unit_occurrence` chain, satisfying every FK), returning the
/// deterministic `occurrence_id`.
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

/// Raw-insert an extra `active` model space (mirrors
/// `crates/store/tests/projection_state.rs`'s `insert_model_space`).
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
}

#[tokio::test]
async fn add_change_delete_point_sets() {
    let (_home, layout, db) = open_state();
    let wt = worktree(&db, 1).await;
    init_projection(&db, &wt).await;
    let gen_a = allocate_ready(&db, &wt, 2).await;
    seed_occurrence(&db, &gen_a, 3, "a.rs").await;
    seed_occurrence(&db, &gen_a, 4, "b.rs").await;
    let ms = default_model_space();
    let shard_dir = layout.projection_shard(&wt.to_string());

    let read = db.open_read().expect("read");
    let expected = expected_points(&read, &wt, &gen_a, &ms).expect("expected");
    drop(read);
    assert_eq!(expected.len(), 4, "2 occurrences x 2 required kinds");

    // Pre-populate the shard: one already-correct point (kept as-is) plus two
    // stale leftovers unrelated to the expected set.
    {
        let shard = FakeProjectionStore::new()
            .open(&shard_dir, params())
            .expect("pre-populate open");
        shard
            .upsert(&[
                ProjectionPoint {
                    point_id: expected[0].point_id.clone(),
                    vector: vec![1.0, 0.0, 0.0],
                },
                ProjectionPoint {
                    point_id: PointId::from_hex("stale-1"),
                    vector: vec![9.0, 9.0, 9.0],
                },
                ProjectionPoint {
                    point_id: PointId::from_hex("stale-2"),
                    vector: vec![9.0, 9.0, 9.0],
                },
            ])
            .expect("pre-populate upsert");
    }

    let vectors = FakeVectors::new();
    let uuids = SeqUuidV7::new();
    let outcome = switch(
        &db,
        &FakeProjectionStore::new(),
        &shard_dir,
        params(),
        wt,
        gen_a,
        ms,
        &vectors,
        &uuids,
        1000,
    )
    .await
    .expect("switch");

    assert_eq!(
        outcome.upserted, 3,
        "3 of the 4 expected points were missing"
    );
    assert_eq!(outcome.deleted, 2, "the 2 stale points are removed");

    let shard = FakeProjectionStore::new()
        .open(&shard_dir, params())
        .expect("reopen");
    let mut on_disk: Vec<String> = shard
        .point_ids()
        .expect("ids")
        .map(|id| id.as_str().to_string())
        .collect();
    on_disk.sort();
    let mut expected_ids: Vec<String> = expected
        .iter()
        .map(|p| p.point_id.as_str().to_string())
        .collect();
    expected_ids.sort();
    assert_eq!(
        on_disk, expected_ids,
        "shard converges to exactly the expected set"
    );
}

#[tokio::test]
async fn unchanged_vectors_not_recomputed() {
    let (_home, layout, db) = open_state();
    let wt = worktree(&db, 5).await;
    init_projection(&db, &wt).await;
    let gen_a = allocate_ready(&db, &wt, 6).await;
    seed_occurrence(&db, &gen_a, 7, "a.rs").await;
    seed_occurrence(&db, &gen_a, 8, "b.rs").await;
    let ms = default_model_space();
    let shard_dir = layout.projection_shard(&wt.to_string());

    let read = db.open_read().expect("read");
    let expected = expected_points(&read, &wt, &gen_a, &ms).expect("expected");
    drop(read);
    assert_eq!(expected.len(), 4);

    // Pre-populate 2 of the 4 expected points ("already correct" — e.g. left
    // over from an earlier partial attempt at this exact target).
    {
        let shard = FakeProjectionStore::new()
            .open(&shard_dir, params())
            .expect("pre-populate open");
        shard
            .upsert(&[
                ProjectionPoint {
                    point_id: expected[0].point_id.clone(),
                    vector: vec![1.0, 0.0, 0.0],
                },
                ProjectionPoint {
                    point_id: expected[1].point_id.clone(),
                    vector: vec![1.0, 0.0, 0.0],
                },
            ])
            .expect("pre-populate upsert");
    }

    let vectors = FakeVectors::new();
    let uuids = SeqUuidV7::new();
    switch(
        &db,
        &FakeProjectionStore::new(),
        &shard_dir,
        params(),
        wt,
        gen_a,
        ms,
        &vectors,
        &uuids,
        1000,
    )
    .await
    .expect("switch");

    let called = vectors.calls();
    let expected_missing: HashSet<(String, RepresentationKind)> = expected[2..]
        .iter()
        .map(|p| (p.occurrence_id.clone(), p.representation_kind))
        .collect();
    assert_eq!(
        called, expected_missing,
        "only the missing points' vectors were looked up — the 2 already-present \
         points were never even queried"
    );
}

#[tokio::test]
async fn retry_from_arbitrary_partial_fake_set_converges_and_is_idempotent() {
    let (_home, layout, db) = open_state();
    let wt = worktree(&db, 10).await;
    init_projection(&db, &wt).await;
    let gen_a = allocate_ready(&db, &wt, 11).await;
    seed_occurrence(&db, &gen_a, 12, "a.rs").await;
    seed_occurrence(&db, &gen_a, 13, "b.rs").await;
    let ms = default_model_space();
    let shard_dir = layout.projection_shard(&wt.to_string());

    // Model a crash mid-reconcile: an unrelated point present, no head at all —
    // "kill mid-upsert batch, before write_head" (spec 05 §10 F2-shaped state),
    // with nothing yet known about the real target's point ids.
    {
        let shard = FakeProjectionStore::new()
            .open(&shard_dir, params())
            .expect("open");
        shard
            .upsert(&[ProjectionPoint {
                point_id: PointId::from_hex("leftover"),
                vector: vec![9.0, 9.0, 9.0],
            }])
            .expect("partial upsert");
    }

    let vectors = FakeVectors::new();
    let uuids = SeqUuidV7::new();
    let outcome = switch(
        &db,
        &FakeProjectionStore::new(),
        &shard_dir,
        params(),
        wt,
        gen_a,
        ms,
        &vectors,
        &uuids,
        2000,
    )
    .await
    .expect("switch converges from arbitrary partial state");
    assert_eq!(outcome.upserted, 4, "all 4 expected points were missing");
    assert_eq!(outcome.deleted, 1, "the leftover point is removed");

    // Retry: calling switch() again with the identical target is a true no-op
    // reconcile — proving "no command-log replay" (the shard's current content
    // is the only history consulted).
    let outcome2 = switch(
        &db,
        &FakeProjectionStore::new(),
        &shard_dir,
        params(),
        wt,
        gen_a,
        ms,
        &vectors,
        &uuids,
        3000,
    )
    .await
    .expect("second switch (idempotent retry)");
    assert_eq!(outcome2.upserted, 0);
    assert_eq!(outcome2.deleted, 0);
    assert_ne!(
        outcome.projection_op_id, outcome2.projection_op_id,
        "each switch mints a fresh op id"
    );
}

#[tokio::test]
async fn final_tuple_is_exact() {
    let (_home, layout, db) = open_state();
    let wt = worktree(&db, 20).await;
    init_projection(&db, &wt).await;
    let gen_a = allocate_ready(&db, &wt, 21).await;
    seed_occurrence(&db, &gen_a, 22, "a.rs").await;
    let ms = default_model_space();
    let shard_dir = layout.projection_shard(&wt.to_string());

    let vectors = FakeVectors::new();
    let uuids = SeqUuidV7::new();
    let outcome = switch(
        &db,
        &FakeProjectionStore::new(),
        &shard_dir,
        params(),
        wt,
        gen_a,
        ms,
        &vectors,
        &uuids,
        5000,
    )
    .await
    .expect("switch");

    let read = db.open_read().expect("read");
    let row = projection_state(&read, &wt.to_string())
        .expect("read row")
        .expect("row exists");
    assert_eq!(row.status, ProjectionStatus::Clean);
    assert_eq!(
        row.active_generation_id.as_deref(),
        Some(gen_a.to_string().as_str())
    );
    assert_eq!(
        row.active_model_space_id.as_deref(),
        Some(ms.to_string().as_str())
    );
    assert_eq!(row.projected_generation_id, row.active_generation_id);
    assert_eq!(row.projected_model_space_id, row.active_model_space_id);
    assert_eq!(row.target_generation_id, None);
    assert_eq!(row.target_model_space_id, None);
    assert_eq!(
        row.projection_op_id.as_deref(),
        Some(outcome.projection_op_id.to_string().as_str())
    );

    let shard = FakeProjectionStore::new()
        .open(&shard_dir, params())
        .expect("reopen");
    let head_on_disk = shard.read_head().expect("head").expect("head present");
    assert_eq!(
        head_on_disk, outcome.head,
        "the written head survives reopen byte-for-byte"
    );
}

#[tokio::test]
async fn generation_transitions_in_the_same_commit() {
    let (_home, layout, db) = open_state();
    let wt = worktree(&db, 30).await;
    init_projection(&db, &wt).await;
    let gen_a = allocate_ready(&db, &wt, 31).await;
    seed_occurrence(&db, &gen_a, 32, "a.rs").await;
    let ms = default_model_space();
    let shard_dir = layout.projection_shard(&wt.to_string());
    let vectors = FakeVectors::new();
    let uuids = SeqUuidV7::new();

    switch(
        &db,
        &FakeProjectionStore::new(),
        &shard_dir,
        params(),
        wt,
        gen_a,
        ms,
        &vectors,
        &uuids,
        1000,
    )
    .await
    .expect("first switch");

    {
        let read = db.open_read().expect("read");
        assert_eq!(
            generation_state(&read, &gen_a.to_string()).expect("state"),
            Some(GenerationState::Active)
        );
        assert_eq!(
            current_generation(&read, &wt.to_string()).expect("cur"),
            Some(gen_a.to_string())
        );
    }

    // A second switch to a new generation (model space unchanged) retires gen_a
    // in the same commit that activates gen_b.
    let gen_b = allocate_ready(&db, &wt, 33).await;
    seed_occurrence(&db, &gen_b, 34, "a.rs").await;
    switch(
        &db,
        &FakeProjectionStore::new(),
        &shard_dir,
        params(),
        wt,
        gen_b,
        ms,
        &vectors,
        &uuids,
        2000,
    )
    .await
    .expect("second switch");

    let read = db.open_read().expect("read");
    assert_eq!(
        generation_state(&read, &gen_a.to_string()).expect("state a"),
        Some(GenerationState::Retiring)
    );
    assert_eq!(
        generation_state(&read, &gen_b.to_string()).expect("state b"),
        Some(GenerationState::Active)
    );
    assert_eq!(
        current_generation(&read, &wt.to_string()).expect("cur"),
        Some(gen_b.to_string())
    );
}

#[tokio::test]
async fn model_axis_only_switch_leaves_generation_active() {
    let (_home, layout, db) = open_state();
    let wt = worktree(&db, 40).await;
    init_projection(&db, &wt).await;
    let gen_a = allocate_ready(&db, &wt, 41).await;
    seed_occurrence(&db, &gen_a, 42, "a.rs").await;
    let ms_default = default_model_space();
    let shard_dir = layout.projection_shard(&wt.to_string());
    let vectors = FakeVectors::new();
    let uuids = SeqUuidV7::new();

    switch(
        &db,
        &FakeProjectionStore::new(),
        &shard_dir,
        params(),
        wt,
        gen_a,
        ms_default,
        &vectors,
        &uuids,
        1000,
    )
    .await
    .expect("first switch");

    let ms_b = uuid(43);
    insert_model_space(&db, &ms_b).await;

    switch(
        &db,
        &FakeProjectionStore::new(),
        &shard_dir,
        params(),
        wt,
        gen_a,
        ms_b,
        &vectors,
        &uuids,
        2000,
    )
    .await
    .expect("model-axis switch");

    let read = db.open_read().expect("read");
    let row = projection_state(&read, &wt.to_string())
        .expect("row")
        .expect("exists");
    assert_eq!(
        row.active_generation_id.as_deref(),
        Some(gen_a.to_string().as_str())
    );
    assert_eq!(
        row.active_model_space_id.as_deref(),
        Some(ms_b.to_string().as_str())
    );
    assert_eq!(
        generation_state(&read, &gen_a.to_string()).expect("state"),
        Some(GenerationState::Active),
        "generation stays active — only the model axis moved"
    );
}

#[tokio::test]
async fn unknown_worktree_is_a_typed_write_ahead_error() {
    let (_home, layout, db) = open_state();
    // No worktree/projection-state row created at all.
    let wt = uuid(60);
    let gen_a = uuid(61);
    let ms = default_model_space();
    let shard_dir = layout.projection_shard(&wt.to_string());
    let vectors = FakeVectors::new();
    let uuids = SeqUuidV7::new();

    let err = switch(
        &db,
        &FakeProjectionStore::new(),
        &shard_dir,
        params(),
        wt,
        gen_a,
        ms,
        &vectors,
        &uuids,
        1000,
    )
    .await
    .expect_err("unknown worktree");
    assert!(
        matches!(
            err,
            SwitchError::WriteAhead(local_rag_store::ProjectionStateError::UnknownWorktree)
        ),
        "got {err:?}"
    );
}

#[tokio::test]
async fn missing_vector_is_typed_and_leaves_status_updating() {
    let (_home, layout, db) = open_state();
    let wt = worktree(&db, 50).await;
    init_projection(&db, &wt).await;
    let gen_a = allocate_ready(&db, &wt, 51).await;
    let occ = seed_occurrence(&db, &gen_a, 52, "a.rs").await;
    let ms = default_model_space();
    let shard_dir = layout.projection_shard(&wt.to_string());

    let vectors = FakeVectors::new();
    vectors.block(&occ, RepresentationKind::CodeRaw);
    let uuids = SeqUuidV7::new();

    let err = switch(
        &db,
        &FakeProjectionStore::new(),
        &shard_dir,
        params(),
        wt,
        gen_a,
        ms,
        &vectors,
        &uuids,
        1000,
    )
    .await
    .expect_err("missing vector");
    assert!(
        matches!(
            &err,
            SwitchError::MissingVector {
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
        ProjectionStatus::Updating,
        "write-ahead landed; the commit never ran"
    );
}
