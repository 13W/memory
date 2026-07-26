//! T09-04 acceptance tests: generation-mixing under concurrent switch load
//! (spec 14 §4 "Generation-mixing") and "no L3 held during backend query"
//! (card bullet 3), both against a real [`SearchEngine`].
//!
//! Fixture helpers duplicate `crates/search/tests/pipeline.rs`'s own
//! (`open_all`, `uuid`, `worktree`, `request_root`, `init_projection`,
//! `allocate_ready`, `seed_occurrence`, `SeqUuidV7`, `AlwaysVectors`) —
//! integration test binaries can't share code without a `mod` file, matching
//! this repo's established per-file convention.
//!
//! Both tests share a `run_load` helper parameterized over the
//! [`ProjectionStore`] `ShardManager` is built with, so the "no L3" test can
//! swap in an instrumented store without duplicating the load shape.
//!
//! Deliberately **not** using this repo's dominant single-thread +
//! `yield_now` concurrency idiom (`crates/store/tests/lock.rs`): the card
//! explicitly asks for *load*, so these run on
//! `#[tokio::test(flavor = "multi_thread")]` with real `tokio::spawn`ed tasks
//! racing on OS threads. `no deadlock` is an explicit, assertable property
//! (`tokio::time::timeout`), not left to an external CI-level timeout.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use local_rag_core::identity::domain::path_fingerprint;
use local_rag_core::identity::{Uuid, UuidSource, uuidv7_from};
use local_rag_core::paths::StoreLayout;
use local_rag_projection::{
    DenseQuery, FakeProjectionStore, PointId, ProjectionHead, ProjectionPoint, ProjectionStore,
    RepresentationKind, ScoredPoint, ShardHandle, ShardManager, ShardParams, VectorSource, switch,
};
use local_rag_protocol::ErrorCode;
use local_rag_search::{PipelineSnapshot, SearchEngine, SearchRequest};
use local_rag_store::{
    CacheDb, DEFAULT_MODEL_SPACE_ID, GenerationState, LockLevel, NewContentBlob, NewFileRevision,
    NewOccurrence, NewParsedUnit, NewlineStyle, RequestRoot, SourceCompression, StateDb, UnitKind,
    WorktreeKind, WorktreeLockRegistry, WorktreeRootFacts, allocate_generation, create_repository,
    create_worktree, derive_content_blob, held_level, insert_content_blob, insert_file_revision,
    insert_generation_file, insert_occurrence, insert_parsed_unit, insert_projection_state,
    materialize_fts, observe_repository_path, observe_worktree_path, occurrence_id,
    transition_generation,
};
use local_rag_test_support::TempHome;

const DIMS: usize = 3;
const N_SEARCH_TASKS: usize = 4;
const EARLY_ITERS: usize = 3;
const MIN_POST_SWITCH_ITERS: usize = 3;
const MAX_LOOP_ITERS: usize = 500;

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
        uuidv7_from(6_000_000 + n, [0x55; 10])
    }
}

struct AlwaysVectors;

impl VectorSource for AlwaysVectors {
    fn vector(&self, _occurrence_id: &str, _kind: RepresentationKind) -> Option<Vec<f32>> {
        Some(vec![1.0, 0.0, 0.0])
    }
}

/// A [`ProjectionStore`] wrapping [`FakeProjectionStore`] whose opened
/// handles record `local_rag_store::held_level()` on every `search()` call —
/// "no L3 held during backend query" (card bullet 3): by the time
/// `ShardManager::acquire` returns a handle, its own brief L3 scope has
/// already exited (`crates/projection/src/manager.rs`'s `checked_scope_sync`
/// wraps only the synchronous map lookup), so every recorded sample here is
/// expected to be `Some(LockLevel::L2Read)`, never `L3`. This is a regression
/// guard proving that structural fact under real concurrent load, not a test
/// expected to catch a live bug today.
struct RecordingProjectionStore {
    inner: FakeProjectionStore,
    sink: Arc<Mutex<Vec<Option<LockLevel>>>>,
}

impl RecordingProjectionStore {
    fn new(sink: Arc<Mutex<Vec<Option<LockLevel>>>>) -> Self {
        Self {
            inner: FakeProjectionStore::new(),
            sink,
        }
    }
}

impl ProjectionStore for RecordingProjectionStore {
    fn open(
        &self,
        dir: &Path,
        params: ShardParams,
    ) -> local_rag_projection::Result<Box<dyn ShardHandle>> {
        let inner = self.inner.open(dir, params)?;
        Ok(Box::new(RecordingHandle {
            inner,
            sink: self.sink.clone(),
        }))
    }
}

struct RecordingHandle {
    inner: Box<dyn ShardHandle>,
    sink: Arc<Mutex<Vec<Option<LockLevel>>>>,
}

impl ShardHandle for RecordingHandle {
    fn read_head(&self) -> local_rag_projection::Result<Option<ProjectionHead>> {
        self.inner.read_head()
    }

    fn point_ids(&self) -> local_rag_projection::Result<Box<dyn Iterator<Item = PointId> + '_>> {
        self.inner.point_ids()
    }

    fn point_count(&self) -> local_rag_projection::Result<u64> {
        self.inner.point_count()
    }

    fn upsert(&self, points: &[ProjectionPoint]) -> local_rag_projection::Result<()> {
        self.inner.upsert(points)
    }

    fn delete(&self, ids: &[PointId]) -> local_rag_projection::Result<()> {
        self.inner.delete(ids)
    }

    fn write_head(&self, head: &ProjectionHead) -> local_rag_projection::Result<()> {
        self.inner.write_head(head)
    }

    fn search(&self, q: &DenseQuery) -> local_rag_projection::Result<Vec<ScoredPoint>> {
        self.sink
            .lock()
            .expect("recording sink mutex poisoned")
            .push(held_level());
        self.inner.search(q)
    }

    fn optimize(&self) -> local_rag_projection::Result<()> {
        self.inner.optimize()
    }

    fn destroy(self: Box<Self>) -> local_rag_projection::Result<()> {
        self.inner.destroy()
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
    uuidv7_from(2500, rand)
}

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

async fn seed_occurrence(state: &StateDb, generation_id: &Uuid, seed: u8, path: &str) -> String {
    let gen_str = generation_id.to_string();
    let revision = uuid(seed).to_string();
    let unit = uuid(seed.wrapping_add(40)).to_string();
    let occ = occurrence_id(&gen_str, path, &unit);
    // Content varies by path so distinct occurrences never collide on the
    // content-derived `content_blob.blob_id` (this file, unlike
    // `pipeline.rs`, seeds more than one occurrence per fixture).
    let content = format!("hello from {path}\n");
    let derived = derive_content_blob("rust", &content);
    let content_bytes = content.into_bytes();
    let source_size = content_bytes.len() as i64;
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
                    source_blob: &content_bytes,
                    compression: SourceCompression::None,
                    source_encoding: "utf-8",
                    newline_style: NewlineStyle::Lf,
                    source_size,
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
                    // Must cover the whole content: `materialize_fts` recomputes
                    // `blob_id` from `source_blob[span_start..span_end]`, and
                    // `derive_content_blob` above was computed over the whole
                    // string — a stale/short span here is exactly what produces
                    // `FtsMaterializeError::BlobMismatch`.
                    span_end: source_size,
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

/// Establish genA active (with materialized FTS) plus a second, dormant
/// genB — allocated and seeded, but deliberately **not** materialized: FTS's
/// `fts_projection_head` is one row per worktree, so materializing a
/// not-yet-active generation would overwrite it and produce a spurious
/// `GenerationMismatch` before the real switch even happens. After the real
/// switch, the small occurrence count self-heals FTS synchronously on the
/// next search (well under `FTS_SYNC_REBUILD_OCCURRENCE_THRESHOLD`).
async fn setup() -> (
    TempHome,
    StoreLayout,
    Arc<StateDb>,
    Arc<CacheDb>,
    Uuid,
    String,
    Uuid,
    Uuid,
) {
    let (home, layout, state, cache) = open_all();
    let (wt, path) = worktree(&state, 10).await;
    init_projection(&state, &wt).await;
    let gen_a = allocate_ready(&state, &wt, 20).await;
    seed_occurrence(&state, &gen_a, 21, "a.rs").await;
    let gen_b = allocate_ready(&state, &wt, 30).await;
    seed_occurrence(&state, &gen_b, 31, "b.rs").await;

    let ms = default_model_space();
    let shard_dir = layout.projection_shard(&wt.to_string());
    switch(
        &state,
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
    .expect("establish genA active via switch");
    materialize_fts(&state, &cache, &wt.to_string(), &gen_a.to_string(), 2000)
        .await
        .expect("materialize fts for genA");

    (home, layout, state, cache, wt, path, gen_a, gen_b)
}

/// Run `N_SEARCH_TASKS` concurrent search loops against `engine` (built over
/// `store`), racing a single switch-to-`gen_b` task. Returns every
/// successfully collected [`PipelineSnapshot`].
#[allow(clippy::too_many_arguments)]
async fn run_load(
    state: Arc<StateDb>,
    cache: Arc<CacheDb>,
    store: Arc<dyn ProjectionStore>,
    layout: StoreLayout,
    wt: Uuid,
    path: String,
    gen_b: Uuid,
) -> Vec<PipelineSnapshot> {
    let ms = default_model_space();
    let locks = Arc::new(WorktreeLockRegistry::new());
    let shards = Arc::new(ShardManager::new(
        state.clone(),
        store,
        layout.clone(),
        params(),
        Arc::new(AlwaysVectors),
        Arc::new(SeqUuidV7::new()),
        8,
    ));
    let engine = Arc::new(SearchEngine::new(
        state.clone(),
        cache,
        locks.clone(),
        shards,
        Duration::from_secs(2),
    ));

    let barrier = Arc::new(tokio::sync::Barrier::new(N_SEARCH_TASKS + 1));
    let switch_committed = Arc::new(AtomicBool::new(false));
    let snapshots: Arc<Mutex<Vec<PipelineSnapshot>>> = Arc::new(Mutex::new(Vec::new()));

    let mut handles = Vec::new();
    for _ in 0..N_SEARCH_TASKS {
        let engine = engine.clone();
        let path = path.clone();
        let barrier = barrier.clone();
        let switch_committed = switch_committed.clone();
        let snapshots = snapshots.clone();
        handles.push(tokio::spawn(async move {
            let mut now_ms = 10_000i64;
            for _ in 0..EARLY_ITERS {
                do_one_search(&engine, &path, now_ms, &snapshots).await;
                now_ms += 1;
            }
            barrier.wait().await;
            let mut post = 0usize;
            for _ in 0..MAX_LOOP_ITERS {
                if post >= MIN_POST_SWITCH_ITERS {
                    break;
                }
                do_one_search(&engine, &path, now_ms, &snapshots).await;
                now_ms += 1;
                if switch_committed.load(Ordering::SeqCst) {
                    post += 1;
                }
            }
            assert!(
                post >= MIN_POST_SWITCH_ITERS,
                "switch never committed within {MAX_LOOP_ITERS} iterations"
            );
        }));
    }

    let switch_state = state.clone();
    let switch_locks = locks.clone();
    let switch_barrier = barrier.clone();
    let switch_flag = switch_committed.clone();
    let shard_dir = layout.projection_shard(&wt.to_string());
    let switch_handle = tokio::spawn(async move {
        switch_barrier.wait().await;
        let wt_str = wt.to_string();
        let uuids = SeqUuidV7::new();
        switch_locks
            .write(&wt_str, async {
                switch(
                    &switch_state,
                    &FakeProjectionStore::new(),
                    &shard_dir,
                    params(),
                    wt,
                    gen_b,
                    ms,
                    &AlwaysVectors,
                    &uuids,
                    50_000,
                )
                .await
            })
            .await
            .expect("switch to genB must succeed");
        switch_flag.store(true, Ordering::SeqCst);
    });

    for h in handles {
        h.await.expect("join search task");
    }
    switch_handle.await.expect("join switch task");

    snapshots.lock().expect("snapshots mutex poisoned").clone()
}

/// One search call, tolerant of `BUSY_RETRY` (retried up to a few times —
/// with `AlwaysVectors`/`FakeProjectionStore` an `L2.write` hold is
/// microseconds, so this should be rare, not structural). Any other error
/// envelope or infrastructure error fails the test immediately.
async fn do_one_search(
    engine: &SearchEngine,
    path: &str,
    now_ms: i64,
    snapshots: &Mutex<Vec<PipelineSnapshot>>,
) {
    let request = SearchRequest {
        root: request_root(path),
        query: "search".to_string(),
        limit: 5,
        name_pattern: None,
        query_vector: vec![1.0, 0.0, 0.0],
        k: 5,
    };
    for attempt in 0..20 {
        match engine.search_code(request.clone(), now_ms).await {
            Ok(Ok(snapshot)) => {
                snapshots
                    .lock()
                    .expect("snapshots mutex poisoned")
                    .push(snapshot);
                return;
            }
            Ok(Err(env)) if env.code == ErrorCode::BusyRetry => {
                assert!(attempt < 19, "BUSY_RETRY did not clear after 20 attempts");
            }
            Ok(Err(env)) => panic!("unexpected error envelope: {env:?}"),
            Err(e) => panic!("unexpected infrastructure error: {e:?}"),
        }
    }
}

/// **"every response contains occurrences from exactly one active
/// generation/model tuple"** (spec 14 §4 "Generation-mixing", card bullet 1):
/// under real concurrent load, every collected snapshot's tuple is exactly
/// one of the two genuinely-committed tuples, never a third/hybrid value —
/// and both are actually observed, proving real interleaving occurred.
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn generation_mixing_under_concurrent_switch_load() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let (_home, layout, state, cache, wt, path, gen_a, gen_b) = setup().await;
        let ms = default_model_space();

        let snapshots = run_load(
            state,
            cache,
            Arc::new(FakeProjectionStore::new()),
            layout,
            wt,
            path,
            gen_b,
        )
        .await;

        assert!(!snapshots.is_empty(), "must have collected some snapshots");
        let valid_a = (gen_a.to_string(), ms.to_string());
        let valid_b = (gen_b.to_string(), ms.to_string());
        let mut saw_a = false;
        let mut saw_b = false;
        for snap in &snapshots {
            let tuple = (snap.generation_id.clone(), snap.model_space_id.clone());
            assert!(
                tuple == valid_a || tuple == valid_b,
                "snapshot tuple {tuple:?} is neither the pre- nor post-switch tuple — mixed/torn read"
            );
            saw_a |= tuple == valid_a;
            saw_b |= tuple == valid_b;
        }
        assert!(saw_a, "genA's tuple was never observed — switch ran too early");
        assert!(saw_b, "genB's tuple was never observed — switch ran too late/never");
    })
    .await
    .expect("must not deadlock");
}

/// **"no L3 held during backend query"** (card bullet 3): every sample of
/// `held_level()` captured from inside `ShardHandle::search()` under the same
/// concurrent load is `Some(LockLevel::L2Read)`, never `L3` — proving
/// `ShardManager::acquire`'s brief L3 scope has always already exited by the
/// time the actual dense query runs, even under real concurrent pressure.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_l3_held_during_backend_query_under_concurrent_load() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let (_home, layout, state, cache, wt, path, _gen_a, gen_b) = setup().await;

        let sink: Arc<Mutex<Vec<Option<LockLevel>>>> = Arc::new(Mutex::new(Vec::new()));
        let store = Arc::new(RecordingProjectionStore::new(sink.clone()));

        let snapshots = run_load(state, cache, store, layout, wt, path, gen_b).await;
        assert!(!snapshots.is_empty(), "must have collected some snapshots");

        let samples = sink.lock().expect("sink mutex poisoned");
        assert!(
            !samples.is_empty(),
            "no search() calls were recorded — wiring broke"
        );
        for level in samples.iter() {
            assert_eq!(
                *level,
                Some(LockLevel::L2Read),
                "backend query must run under L2.read, never with L3 held"
            );
        }
    })
    .await
    .expect("must not deadlock");
}
