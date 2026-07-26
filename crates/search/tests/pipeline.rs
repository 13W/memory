//! T09-03 acceptance tests for the snapshot/read-lock search skeleton (spec
//! 09 §1, 06 §3, 02 §5/§6), mapped 1:1 to the task card's required scenarios.
//!
//! Fixture helpers combine `crates/projection/tests/manager.rs`'s
//! `established()` idiom (repo/worktree/projection-state/generation/
//! occurrence + `switch()`) with `crates/store/tests/fts_corruption.rs`'s
//! bulk-occurrence seeding, plus `crates/store/tests/resolve.rs`'s
//! worktree-path registration (needed here for the first time alongside a
//! real `switch()`/`materialize_fts` fixture, since `SearchEngine::search_code`
//! always resolves a real [`RequestRoot`]). Integration test binaries can't
//! share code without a `mod` file, so this duplicates rather than imports —
//! matching the existing `manager.rs`/`rebuild.rs`/`switch.rs`/`resolve.rs`
//! convention.
//!
//! Deterministic: isolated [`TempHome`], fixed `now_ms` literals, no network,
//! no wall-clock sleeps (paused virtual time for the `BUSY_RETRY` test,
//! mirroring `crates/store/tests/lock.rs`'s `read_bounded` timeout test).

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use local_rag_core::identity::domain::path_fingerprint;
use local_rag_core::identity::{Uuid, UuidSource, uuidv7_from};
use local_rag_core::paths::StoreLayout;
use local_rag_projection::{
    FakeProjectionStore, RepresentationKind, ShardManager, ShardParams, VectorSource, switch,
};
use local_rag_protocol::{DegradedMode, ErrorCode};
use local_rag_search::{SearchEngine, SearchRequest, Stage, StageObserver};
use local_rag_store::{
    CacheDb, DEFAULT_MODEL_SPACE_ID, FTS_SYNC_REBUILD_OCCURRENCE_THRESHOLD, GenerationState,
    LockLevel, NewContentBlob, NewFileRevision, NewOccurrence, NewParsedUnit, NewlineStyle,
    RequestRoot, SourceCompression, StateDb, UnitKind, WorktreeKind, WorktreeLockRegistry,
    WorktreeRootFacts, allocate_generation, create_repository, create_worktree,
    derive_content_blob, held_level, insert_content_blob, insert_file_revision,
    insert_generation_file, insert_occurrence, insert_parsed_unit, insert_projection_state,
    materialize_fts, observe_repository_path, observe_worktree_path, occurrence_id,
    transition_generation,
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
        uuidv7_from(4_000_000 + n, [0x33; 10])
    }
}

/// A test-only [`VectorSource`] that always returns a fixed `DIMS`-wide
/// vector (mirrors `crates/projection/tests/manager.rs`'s own `FakeVectors`,
/// renamed to match this file's `BlockableVectors` counterpart below).
struct AlwaysVectors;

impl VectorSource for AlwaysVectors {
    fn vector(&self, _occurrence_id: &str, _kind: RepresentationKind) -> Option<Vec<f32>> {
        Some(vec![1.0, 0.0, 0.0])
    }
}

/// A [`VectorSource`] that can be told to withhold specific
/// `(occurrence_id, kind)` vectors (mirrors `crates/projection/tests/
/// rebuild.rs`'s `FakeVectors`), used to make a rebuild-on-acquire genuinely
/// fail rather than self-heal.
struct BlockableVectors {
    blocked: Mutex<HashSet<(String, RepresentationKind)>>,
}

impl BlockableVectors {
    fn new() -> Self {
        Self {
            blocked: Mutex::new(HashSet::new()),
        }
    }

    fn block(&self, occurrence_id: &str, kind: RepresentationKind) {
        self.blocked
            .lock()
            .expect("blockable vectors mutex poisoned")
            .insert((occurrence_id.to_string(), kind));
    }
}

impl VectorSource for BlockableVectors {
    fn vector(&self, occurrence_id: &str, kind: RepresentationKind) -> Option<Vec<f32>> {
        if self
            .blocked
            .lock()
            .expect("blockable vectors mutex poisoned")
            .contains(&(occurrence_id.to_string(), kind))
        {
            None
        } else {
            Some(vec![1.0, 0.0, 0.0])
        }
    }
}

/// Records the [`LockLevel`] observed at every [`Stage`] callback —
/// "instrumentation proves lock held in every leg", T09-03's own acceptance
/// criterion.
#[derive(Default)]
struct RecordingObserver {
    stages: Mutex<Vec<(Stage, Option<LockLevel>)>>,
}

impl RecordingObserver {
    fn new() -> Self {
        Self::default()
    }

    fn stages(&self) -> Vec<(Stage, Option<LockLevel>)> {
        self.stages
            .lock()
            .expect("recording observer mutex poisoned")
            .clone()
    }
}

impl StageObserver for RecordingObserver {
    fn on_stage(&self, stage: Stage) {
        self.stages
            .lock()
            .expect("recording observer mutex poisoned")
            .push((stage, held_level()));
    }
}

fn open_all() -> (TempHome, StoreLayout, Arc<StateDb>, Arc<CacheDb>) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let state = Arc::new(StateDb::open(layout.state_db()).expect("open state.sqlite"));
    let cache =
        Arc::new(CacheDb::open(layout.cache_db(), "search-tests").expect("open cache.sqlite"));
    (home, layout, state, cache)
}

fn uuid(seed: u8) -> Uuid {
    let mut rand = [0u8; 10];
    rand[9] = seed;
    uuidv7_from(2000, rand)
}

/// Register a worktree with a resolvable current path (spec 02 §3.3):
/// `SearchEngine::search_code` always resolves a real [`RequestRoot`], unlike
/// `manager.rs`'s tests which operate directly on worktree ids.
async fn worktree(state: &StateDb, seed: u8) -> (Uuid, String) {
    let repo = uuid(seed).to_string();
    let wt = uuid(seed.wrapping_add(100));
    let wt_str = wt.to_string();
    let path = format!("/repo/wt-{seed}");
    let fp = path_fingerprint(&path);
    let (r, w, p, f) = (repo, wt_str, path.clone(), fp);
    state
        .writer()
        .transaction(move |tx| {
            create_repository(tx, &r, None, 1000)?;
            create_worktree(tx, &w, &r, WorktreeKind::Main, 1000)?;
            observe_worktree_path(tx, &w, &p, &p, &f, 1000)?;
            observe_repository_path(tx, &r, &p, 1000)
        })
        .await
        .expect("create repo + worktree + observe path");
    (wt, path)
}

/// The [`RequestRoot`] that resolves to the worktree [`worktree`] registered
/// at `path`.
fn request_root(path: &str) -> RequestRoot {
    RequestRoot {
        worktree_root: Some(WorktreeRootFacts {
            observed_canonical_path: path.to_string(),
            display_path: path.to_string(),
            path_fingerprint: path_fingerprint(path),
            kind: WorktreeKind::Main,
            common_dir_fingerprint: None,
            remote_fingerprint: None,
        }),
        repo_hint: None,
    }
}

async fn init_projection(state: &StateDb, worktree_id: &Uuid) {
    let w = worktree_id.to_string();
    state
        .writer()
        .transaction(move |tx| insert_projection_state(tx, &w, 1000))
        .await
        .expect("init projection state");
    // T11-05: the expected point set now joins `model_space_representation`, so
    // the default space has to declare its required code kinds.
    register_code_representations(state, &default_model_space()).await;
}

async fn allocate_ready(state: &StateDb, worktree_id: &Uuid, gen_seed: u8) -> Uuid {
    let genr = uuid(gen_seed);
    let (w, g) = (worktree_id.to_string(), genr.to_string());
    state
        .writer()
        .transaction(move |tx| allocate_generation(tx, &w, &g, 1000).map(|_| ()))
        .await
        .expect("allocate generation");
    let g2 = genr.to_string();
    state
        .writer()
        .transaction(move |tx| transition_generation(tx, &g2, GenerationState::ProjectionReady))
        .await
        .expect("transition tx (infra)")
        .expect("building -> projection_ready is legal");
    genr
}

/// Seed one occurrence with real file content; returns its `occurrence_id`.
///
/// Unlike `crates/projection/tests/manager.rs`'s own `seed_occurrence` (whose
/// tests never call `materialize_fts`), the `content_blob.blob_id` here MUST
/// be the real [`derive_content_blob`] hash of `source_blob` — `materialize_fts`
/// recomputes it from the stored bytes and rejects a mismatch.
async fn seed_occurrence(state: &StateDb, generation_id: &Uuid, seed: u8, path: &str) -> String {
    let gen_str = generation_id.to_string();
    let revision = uuid(seed).to_string();
    let unit = uuid(seed.wrapping_add(40)).to_string();
    let occ = occurrence_id(&gen_str, path, &unit);
    let derived = derive_content_blob("rust", "hello\n");
    let (rev, b, u, g, p, occ2) = (
        revision,
        derived.blob_id.clone(),
        unit,
        gen_str,
        path.to_string(),
        occ.clone(),
    );
    let (algo, norm) = (derived.algo_version, derived.normalization_version);
    state
        .writer()
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
                    algo_version: algo,
                    normalization_version: norm,
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

/// Seed `count` distinct occurrences for `generation_id` in one transaction,
/// sharing a single `file_revision`/`content_blob` to keep a large fixture
/// cheap to build (mirrors `crates/store/tests/fts_corruption.rs`'s own
/// helper). Returns the first occurrence's id.
async fn seed_bulk_occurrences(state: &StateDb, generation_id: &Uuid, count: u64) -> String {
    let gen_str = generation_id.to_string();
    let file_revision_id = uuid(250).to_string();
    let derived = derive_content_blob("rust", "a");
    let first_occ = occurrence_id(&gen_str, "bulk0.rs", "bulk-unit-0");
    let (fr, blob, g) = (file_revision_id, derived.blob_id.clone(), gen_str);
    let (algo, norm) = (derived.algo_version, derived.normalization_version);
    state
        .writer()
        .transaction(move |tx| {
            insert_file_revision(
                tx,
                &NewFileRevision {
                    file_revision_id: &fr,
                    content_hash: &fr,
                    parser_fingerprint: "bulk-fp",
                    source_blob: b"a",
                    compression: SourceCompression::None,
                    source_encoding: "utf-8",
                    newline_style: NewlineStyle::Lf,
                    source_size: 1,
                },
                1000,
            )?;
            insert_content_blob(
                tx,
                &NewContentBlob {
                    blob_id: &blob,
                    language: "rust",
                    algo_version: algo,
                    normalization_version: norm,
                },
                1000,
            )?;
            for i in 0..count {
                let unit_id = format!("bulk-unit-{i}");
                let path = format!("bulk{i}.rs");
                insert_parsed_unit(
                    tx,
                    &NewParsedUnit {
                        unit_id: &unit_id,
                        file_revision_id: &fr,
                        unit_kind: UnitKind::Symbol,
                        syntax_locator: &format!("loc:{i}"),
                        blob_id: &blob,
                        span_start: 0,
                        span_end: 1,
                        local_name: None,
                        kind: None,
                        parent_unit_id: None,
                    },
                )?;
                insert_generation_file(tx, &g, &path, &path, &fr)?;
                let occ = occurrence_id(&g, &path, &unit_id);
                insert_occurrence(
                    tx,
                    &NewOccurrence {
                        occurrence_id: &occ,
                        generation_id: &g,
                        normalized_path: &path,
                        unit_id: &unit_id,
                        qualified_name: None,
                        context_hash: None,
                    },
                )?;
            }
            Ok(())
        })
        .await
        .expect("seed bulk occurrences");
    first_occ
}

/// Establish a real, converged `active` tuple + shard via one `switch()`
/// over a single occurrence: worktree (with a resolvable path), one
/// occurrence, model space default. Returns
/// `(worktree_id, generation_id, path, shard_dir, occurrence_id)`.
async fn establish_single(
    state: &StateDb,
    layout: &StoreLayout,
    seed: u8,
) -> (Uuid, Uuid, String, PathBuf, String) {
    let (wt, path) = worktree(state, seed).await;
    init_projection(state, &wt).await;
    let gen_a = allocate_ready(state, &wt, seed.wrapping_add(1)).await;
    let occ = seed_occurrence(state, &gen_a, seed.wrapping_add(2), "a.rs").await;
    let ms = default_model_space();
    let shard_dir = layout.projection_shard(&wt.to_string());

    switch(
        state,
        &FakeProjectionStore::new(),
        &shard_dir,
        params(),
        wt,
        gen_a,
        ms,
        &AlwaysVectors,
        &SeqUuidV7::new(),
        1000,
    )
    .await
    .expect("establish active tuple via switch");

    (wt, gen_a, path, shard_dir, occ)
}

/// [`establish_single`]'s bulk counterpart: `count` occurrences (used to push
/// the FTS view's fresh occurrence count above
/// [`FTS_SYNC_REBUILD_OCCURRENCE_THRESHOLD`], forcing a genuine
/// `DeferredBackground` rather than a synchronous self-heal). Returns
/// `(worktree_id, generation_id, path, shard_dir, first_occurrence_id)`.
async fn establish_bulk(
    state: &StateDb,
    layout: &StoreLayout,
    seed: u8,
    count: u64,
) -> (Uuid, Uuid, String, PathBuf, String) {
    let (wt, path) = worktree(state, seed).await;
    init_projection(state, &wt).await;
    let gen_a = allocate_ready(state, &wt, seed.wrapping_add(1)).await;
    let first_occ = seed_bulk_occurrences(state, &gen_a, count).await;
    let ms = default_model_space();
    let shard_dir = layout.projection_shard(&wt.to_string());

    switch(
        state,
        &FakeProjectionStore::new(),
        &shard_dir,
        params(),
        wt,
        gen_a,
        ms,
        &AlwaysVectors,
        &SeqUuidV7::new(),
        1000,
    )
    .await
    .expect("establish active tuple via switch (bulk)");

    (wt, gen_a, path, shard_dir, first_occ)
}

/// **"instrumentation proves lock held in every leg"**: a fully healthy
/// hybrid search runs every stage under `L2.read`, and the lock is released
/// both before and after the call.
#[tokio::test]
async fn lock_is_held_in_every_leg_of_a_successful_hybrid_search() {
    let (_home, layout, state, cache) = open_all();
    let (wt, gen_a, path, _shard_dir, _occ) = establish_single(&state, &layout, 10).await;
    materialize_fts(&state, &cache, &wt.to_string(), &gen_a.to_string(), 2000)
        .await
        .expect("materialize fts");

    let locks = Arc::new(WorktreeLockRegistry::new());
    let shards = Arc::new(ShardManager::new(
        state.clone(),
        Arc::new(FakeProjectionStore::new()),
        layout,
        params(),
        Arc::new(AlwaysVectors),
        Arc::new(SeqUuidV7::new()),
        8,
    ));
    let engine = SearchEngine::new(
        state.clone(),
        cache.clone(),
        locks,
        shards,
        Duration::from_millis(500),
    );

    assert_eq!(held_level(), None, "no lock held before the call");

    let observer = RecordingObserver::new();
    let request = SearchRequest {
        root: request_root(&path),
        query: "search".to_string(),
        limit: 5,
        name_pattern: None,
        query_vector: vec![1.0, 0.0, 0.0],
        k: 5,
    };
    let outcome = engine
        .search_code_instrumented(request, 3000, &observer)
        .await
        .expect("no infrastructure error");

    assert_eq!(held_level(), None, "lock released after the call");

    let snapshot = outcome.expect("must not be an error envelope");
    assert_eq!(snapshot.worktree_id, wt.to_string());
    assert_eq!(snapshot.generation_id, gen_a.to_string());
    assert_eq!(snapshot.degraded, None, "both legs are healthy");
    assert!(snapshot.diagnostics.is_empty());

    let stages = observer.stages();
    assert_eq!(
        stages.iter().map(|(stage, _)| *stage).collect::<Vec<_>>(),
        vec![
            Stage::ActiveTuple,
            Stage::FtsLeg,
            Stage::DenseLeg,
            Stage::LexicalLeg,
            Stage::Enrichment,
        ],
    );
    for (stage, level) in &stages {
        assert_eq!(
            *level,
            Some(LockLevel::L2Read),
            "{stage:?} must run with L2.read held"
        );
    }
}

/// **"unknown root"**: a request with no resolvable worktree yields
/// `WORKTREE_NOT_INDEXED` before any lock is ever taken.
#[tokio::test]
async fn unknown_root_yields_worktree_not_indexed() {
    let (_home, layout, state, cache) = open_all();
    let locks = Arc::new(WorktreeLockRegistry::new());
    let shards = Arc::new(ShardManager::new(
        state.clone(),
        Arc::new(FakeProjectionStore::new()),
        layout,
        params(),
        Arc::new(AlwaysVectors),
        Arc::new(SeqUuidV7::new()),
        8,
    ));
    let engine = SearchEngine::new(state, cache, locks, shards, Duration::from_millis(500));

    let request = SearchRequest {
        root: RequestRoot::default(),
        query: "search".to_string(),
        limit: 5,
        name_pattern: None,
        query_vector: vec![1.0, 0.0, 0.0],
        k: 5,
    };
    let outcome = engine
        .search_code(request, 1000)
        .await
        .expect("no infrastructure error");
    let err = outcome.expect_err("must be an error envelope");
    assert_eq!(err.code, ErrorCode::WorktreeNotIndexed);
    assert!(!err.retryable);
}

/// **"dense-only"**: an FTS view that never had a head, above the
/// synchronous-rebuild threshold, defers to background — dense stays
/// healthy, so the response is `degraded: dense_only`, never an error.
#[tokio::test]
async fn fts_diverged_above_threshold_degrades_dense_only() {
    let (_home, layout, state, cache) = open_all();
    let total = FTS_SYNC_REBUILD_OCCURRENCE_THRESHOLD + 1;
    let (wt, _gen_a, path, _shard_dir, _first_occ) =
        establish_bulk(&state, &layout, 20, total).await;
    // Deliberately no `materialize_fts` call: the head stays missing, and the
    // fresh occurrence count is above threshold, so `open_and_validate_fts`
    // must defer to background rather than self-heal synchronously.

    let locks = Arc::new(WorktreeLockRegistry::new());
    let shards = Arc::new(ShardManager::new(
        state.clone(),
        Arc::new(FakeProjectionStore::new()),
        layout,
        params(),
        Arc::new(AlwaysVectors),
        Arc::new(SeqUuidV7::new()),
        8,
    ));
    let engine = SearchEngine::new(
        state.clone(),
        cache.clone(),
        locks,
        shards,
        Duration::from_millis(500),
    );

    let request = SearchRequest {
        root: request_root(&path),
        query: "search".to_string(),
        limit: 5,
        name_pattern: None,
        query_vector: vec![1.0, 0.0, 0.0],
        k: 5,
    };
    let outcome = engine
        .search_code(request, 3000)
        .await
        .expect("no infrastructure error");
    let snapshot = outcome.expect("dense is healthy; must not be an error envelope");
    assert_eq!(snapshot.worktree_id, wt.to_string());
    assert_eq!(snapshot.degraded, Some(DegradedMode::DenseOnly));
    assert!(!snapshot.diagnostics.is_empty());
}

/// **"lexical-only"**: a corrupted shard whose rebuild-on-acquire cannot
/// succeed (a withheld vector) leaves dense unavailable while FTS is fine —
/// `degraded: lexical_only`, never an error.
#[tokio::test]
async fn dense_unavailable_degrades_lexical_only() {
    let (_home, layout, state, cache) = open_all();
    let (wt, gen_a, path, shard_dir, occ) = establish_single(&state, &layout, 30).await;
    materialize_fts(&state, &cache, &wt.to_string(), &gen_a.to_string(), 2000)
        .await
        .expect("materialize fts");

    // Corrupt the persisted points file directly (mirrors
    // `crates/projection/tests/manager.rs::corrupt_cold_shard_self_heals_on_acquire`).
    std::fs::write(shard_dir.join("points"), "0a\tnothex\n").expect("corrupt points file");

    // Unlike that test, this manager's vectors withhold the seeded
    // occurrence's `code_raw` representation, so the self-heal rebuild
    // triggered by `acquire` fails instead of succeeding.
    let blocking = Arc::new(BlockableVectors::new());
    blocking.block(&occ, RepresentationKind::CodeRaw);

    let locks = Arc::new(WorktreeLockRegistry::new());
    let shards = Arc::new(ShardManager::new(
        state.clone(),
        Arc::new(FakeProjectionStore::new()),
        layout,
        params(),
        blocking,
        Arc::new(SeqUuidV7::new()),
        8,
    ));
    let engine = SearchEngine::new(
        state.clone(),
        cache.clone(),
        locks,
        shards,
        Duration::from_millis(500),
    );

    let request = SearchRequest {
        root: request_root(&path),
        query: "search".to_string(),
        limit: 5,
        name_pattern: None,
        query_vector: vec![1.0, 0.0, 0.0],
        k: 5,
    };
    let outcome = engine
        .search_code(request, 5000)
        .await
        .expect("no infrastructure error");
    let snapshot = outcome.expect("fts is healthy; must not be an error envelope");
    assert_eq!(snapshot.degraded, Some(DegradedMode::LexicalOnly));
    assert!(!snapshot.diagnostics.is_empty());
}

/// **"neither"**: both legs unavailable at once (FTS deferred above
/// threshold, dense's rebuild-on-acquire blocked) yields `INDEX_UNAVAILABLE`,
/// never a silent partial response.
#[tokio::test]
async fn both_legs_unavailable_yields_index_unavailable() {
    let (_home, layout, state, cache) = open_all();
    let total = FTS_SYNC_REBUILD_OCCURRENCE_THRESHOLD + 1;
    let (_wt, _gen_a, path, shard_dir, first_occ) =
        establish_bulk(&state, &layout, 40, total).await;
    // No materialize_fts: FTS defers to background (as in the dense-only test).

    std::fs::write(shard_dir.join("points"), "0a\tnothex\n").expect("corrupt points file");
    let blocking = Arc::new(BlockableVectors::new());
    blocking.block(&first_occ, RepresentationKind::CodeRaw);

    let locks = Arc::new(WorktreeLockRegistry::new());
    let shards = Arc::new(ShardManager::new(
        state.clone(),
        Arc::new(FakeProjectionStore::new()),
        layout,
        params(),
        blocking,
        Arc::new(SeqUuidV7::new()),
        8,
    ));
    let engine = SearchEngine::new(
        state.clone(),
        cache.clone(),
        locks,
        shards,
        Duration::from_millis(500),
    );

    let request = SearchRequest {
        root: request_root(&path),
        query: "search".to_string(),
        limit: 5,
        name_pattern: None,
        query_vector: vec![1.0, 0.0, 0.0],
        k: 5,
    };
    let outcome = engine
        .search_code(request, 6000)
        .await
        .expect("no infrastructure error");
    let err = outcome.expect_err("both legs are unavailable; must be an error envelope");
    assert_eq!(err.code, ErrorCode::IndexUnavailable);
    assert!(!err.retryable);
    assert!(err.details.is_some());
}

/// **"bounded writer wait/BUSY_RETRY"**: a writer holding `L2.write` past the
/// engine's read-wait budget makes the search time out with `BUSY_RETRY`.
/// Paused virtual time (mirrors `crates/store/tests/lock.rs`'s
/// `read_bounded` timeout test) — no real sleep.
/// Register the two code representations (`code_raw`, `code_context`) as
/// `required` for `model_space_id`.
///
/// T11-05 replaced `expected::REQUIRED_REPRESENTATION_KINDS`'s hardcoded pair
/// with a real `model_space_representation` join, so a fixture's model space now
/// has to declare what it requires. Registering exactly that pair keeps every
/// pre-existing expectation in this file (2 points per occurrence) unchanged.
async fn register_code_representations(state: &StateDb, model_space_id: &Uuid) {
    let space = model_space_id.to_string();
    state
        .writer()
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

#[tokio::test(start_paused = true)]
async fn writer_holding_l2_write_delays_search_past_bound_yields_busy_retry() {
    let (_home, layout, state, cache) = open_all();
    let (wt, gen_a, path, _shard_dir, _occ) = establish_single(&state, &layout, 50).await;
    materialize_fts(&state, &cache, &wt.to_string(), &gen_a.to_string(), 2000)
        .await
        .expect("materialize fts");

    let locks = Arc::new(WorktreeLockRegistry::new());
    let shards = Arc::new(ShardManager::new(
        state.clone(),
        Arc::new(FakeProjectionStore::new()),
        layout,
        params(),
        Arc::new(AlwaysVectors),
        Arc::new(SeqUuidV7::new()),
        8,
    ));
    let engine = SearchEngine::new(
        state,
        cache,
        locks.clone(),
        shards,
        Duration::from_millis(50),
    );

    let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
    let (proceed_tx, proceed_rx) = std::sync::mpsc::channel::<()>();
    let wt_str = wt.to_string();
    let writer_task = tokio::spawn(async move {
        locks
            .write(&wt_str, async move {
                entered_tx.send(()).ok();
                tokio::task::spawn_blocking(move || proceed_rx.recv().ok())
                    .await
                    .ok();
            })
            .await;
    });
    tokio::task::spawn_blocking(move || entered_rx.recv().expect("writer entered"))
        .await
        .expect("join entered-wait");

    let request = SearchRequest {
        root: request_root(&path),
        query: "search".to_string(),
        limit: 5,
        name_pattern: None,
        query_vector: vec![1.0, 0.0, 0.0],
        k: 5,
    };
    let search_task = tokio::spawn(async move { engine.search_code(request, 7000).await });

    // Give the search task real scheduling opportunities to run its
    // synchronous prefix through to registering its bounded wait (no sleep).
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_millis(51)).await;

    let outcome = search_task
        .await
        .expect("join search task")
        .expect("no infrastructure error");
    let err = outcome.expect_err("must be a BUSY_RETRY error envelope");
    assert_eq!(err.code, ErrorCode::BusyRetry);
    assert!(err.retryable);

    proceed_tx.send(()).ok();
    writer_task.await.expect("join writer task");
}
