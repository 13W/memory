//! T09-04 "load/failpoint" acceptance test (the card's own Результат):
//! injects the existing `projection.switch.before_commit` failpoint
//! (`crates/projection/src/switch.rs`, T07-05, spec 05 §10 F4 — fires after
//! the shard write already landed, before the final `state.sqlite` commit,
//! the exact window that would look torn to a naive reader) into a switch
//! racing against concurrent searches, and proves the search side never sees
//! a torn/corrupt tuple, never hangs, and only ever reports the one tuple
//! that was ever actually committed.
//!
//! `state.sqlite`-level recovery guarantees for this exact failpoint
//! (`status='updating'`, active tuple untouched, target retained) are already
//! proven by group 07's fault matrix
//! (`crates/projection/tests/fault_matrix.rs`'s F4 test) — not re-asserted
//! here.
//!
//! Exactly one `#[tokio::test]` in this file — no `serial()`-style guard
//! needed against the process-global failpoint registry, matching
//! `crates/projection/tests/switch_faults.rs`'s own reasoning verbatim (a
//! single test is trivially serial within its own process).
//!
//! Fixture helpers duplicate `crates/search/tests/generation_mixing.rs`'s own
//! (per this repo's established per-test-binary convention).
#![cfg(feature = "failpoints")]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use local_rag_core::identity::domain::path_fingerprint;
use local_rag_core::identity::{Uuid, UuidSource, uuidv7_from};
use local_rag_core::paths::StoreLayout;
use local_rag_projection::{
    FakeProjectionStore, RepresentationKind, ShardManager, ShardParams, SwitchError, VectorSource,
    switch,
};
use local_rag_protocol::{ErrorCode, SearchMode};
use local_rag_search::{
    NoopObserver, PipelineSnapshot, QueryEmbedError, QueryEmbedder, SearchEngine, SearchRequest,
};
use local_rag_store::{
    CacheDb, DEFAULT_MODEL_SPACE_ID, GenerationState, NewContentBlob, NewFileRevision,
    NewOccurrence, NewParsedUnit, NewlineStyle, RepresentationKey, RequestRoot, SourceCompression,
    StateDb, UnitKind, WorktreeKind, WorktreeLockRegistry, WorktreeRootFacts, allocate_generation,
    create_repository, create_worktree, derive_content_blob, insert_content_blob,
    insert_file_revision, insert_generation_file, insert_occurrence, insert_parsed_unit,
    insert_projection_state, materialize_fts, observe_repository_path, observe_worktree_path,
    occurrence_id, transition_generation,
};
use local_rag_test_support::{Action, TempHome, failpoint::global};

const DIMS: usize = 3;
const N_SEARCH_TASKS: usize = 3;
const EARLY_ITERS: usize = 3;
const BEFORE_COMMIT_FP: &str = "projection.switch.before_commit";

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
        uuidv7_from(7_000_000 + n, [0x66; 10])
    }
}

struct AlwaysVectors;

impl VectorSource for AlwaysVectors {
    fn vector(&self, _occurrence_id: &str, _kind: RepresentationKind) -> Option<Vec<f32>> {
        Some(vec![1.0, 0.0, 0.0])
    }
}

/// A deterministic [`QueryEmbedder`] (T12-02): every query embeds to the same
/// unit vector along the first axis, in whatever dimensionality the
/// representation declares.
///
/// Real query embedding needs an inference runtime the daemon owns (group 15);
/// this seam is precisely what lets these tests exercise the dense leg end to
/// end while staying offline and deterministic. `AlwaysVectors` gives every
/// *point* the same vector, so a healthy dense leg here returns every point of
/// the active tuple — enough to prove plumbing, ordering and identity mapping.
struct FixedQueryEmbedder;

impl QueryEmbedder for FixedQueryEmbedder {
    fn embed_query(
        &self,
        _query: &str,
        key: &RepresentationKey,
    ) -> Result<Vec<f32>, QueryEmbedError> {
        let mut vector = vec![0.0; key.dimensions as usize];
        if let Some(first) = vector.first_mut() {
            *first = 1.0;
        }
        Ok(vector)
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
    uuidv7_from(2600, rand)
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
        mode: SearchMode::Hybrid,
        name_pattern: None,
        query_degraded: None,
    };
    for attempt in 0..20 {
        match engine
            .search_code_instrumented(request.clone(), now_ms, &NoopObserver)
            .await
        {
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

/// **"load/failpoint tests alternate generation/model switch against
/// searches"** (the card's own Результат): a switch that fails via an
/// injected failpoint right before its commit must leave every concurrently
/// running search seeing only the one tuple that was ever actually
/// committed (genA) — never genB (whose commit never ran), never a
/// third/torn value — and no search ever hangs or errors with anything but
/// `BUSY_RETRY`.
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
async fn switch_failure_before_commit_never_corrupts_concurrent_search() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let (_home, layout, state, cache) = open_all();
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

        let locks = Arc::new(WorktreeLockRegistry::new());
        let shards = Arc::new(ShardManager::new(
            state.clone(),
            Arc::new(FakeProjectionStore::new()),
            layout.clone(),
            params(),
            Arc::new(AlwaysVectors),
            Arc::new(SeqUuidV7::new()),
            8,
        ));
        let engine = Arc::new(SearchEngine::with_embedder(
            state.clone(),
            cache,
            locks.clone(),
            shards,
            Arc::new(FixedQueryEmbedder),
            Duration::from_secs(2),
        ));

        let barrier = Arc::new(tokio::sync::Barrier::new(N_SEARCH_TASKS + 1));
        let snapshots: Arc<Mutex<Vec<PipelineSnapshot>>> = Arc::new(Mutex::new(Vec::new()));

        let mut handles = Vec::new();
        for _ in 0..N_SEARCH_TASKS {
            let engine = engine.clone();
            let path = path.clone();
            let barrier = barrier.clone();
            let snapshots = snapshots.clone();
            handles.push(tokio::spawn(async move {
                let mut now_ms = 10_000i64;
                for _ in 0..EARLY_ITERS {
                    do_one_search(&engine, &path, now_ms, &snapshots).await;
                    now_ms += 1;
                }
                barrier.wait().await;
                // A few more rounds after the switch attempt so the failed
                // switch's window is genuinely raced against, not just
                // preceded.
                for _ in 0..EARLY_ITERS {
                    do_one_search(&engine, &path, now_ms, &snapshots).await;
                    now_ms += 1;
                }
            }));
        }

        global().register(BEFORE_COMMIT_FP);
        let switch_state = state.clone();
        let switch_locks = locks.clone();
        let switch_barrier = barrier.clone();
        let switch_handle = tokio::spawn(async move {
            switch_barrier.wait().await;
            let wt_str = wt.to_string();
            let uuids = SeqUuidV7::new();
            global()
                .arm(BEFORE_COMMIT_FP, Action::Error)
                .expect("arm before_commit failpoint");
            let result = switch_locks
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
                .await;
            global().disarm(BEFORE_COMMIT_FP).expect("disarm");
            match result {
                Err(SwitchError::Failpoint(name)) => assert_eq!(name, BEFORE_COMMIT_FP),
                other => panic!("expected injected failpoint failure, got {other:?}"),
            }
        });

        for h in handles {
            h.await.expect("join search task");
        }
        switch_handle.await.expect("join switch task");

        let collected = snapshots.lock().expect("snapshots mutex poisoned").clone();
        assert!(!collected.is_empty(), "must have collected some snapshots");
        let valid_a = (gen_a.to_string(), ms.to_string());
        for snap in &collected {
            let tuple = (
                snap.response.generation.id.clone(),
                snap.model_space_id.clone(),
            );
            assert_eq!(
                tuple, valid_a,
                "genB's commit never ran — every search must still see genA"
            );
        }
    })
    .await
    .expect("must not deadlock");
}
