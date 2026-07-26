//! T12-02 acceptance tests: the dense leg as the search pipeline actually runs
//! it (spec 09 §3), over the **production** brute-force backend (ADR-0003) and
//! a real `switch()`-established active tuple.
//!
//! What this binary covers, one card bullet each:
//!
//! * *active representation selection* — the query is embedded with the active
//!   model space's own `code_raw` `RepresentationKey`, and a space registered
//!   with different dimensions is served with those dimensions;
//! * *distance metrics* — the same points and query rank differently under
//!   `dot` and `cosine`, taken from `representation.distance_metric`;
//! * *missing/corrupt shard ⇒ explicit `lexical_only`* — plus every other dense
//!   failure path (no provider, wrong-dimensioned embedding), each with a
//!   diagnostic and never an error envelope;
//! * *no tenant/generation filter dependence within a per-worktree shard* — the
//!   query carries no filter at all (`DenseQuery` has only a vector and `k`),
//!   and a second generation's occurrences are unreachable because they are not
//!   in the shard, not because something filtered them out.
//!
//! Backend-level behavior (persistence, idempotence, corruption) lives in
//! `crates/projection/tests/backend_contract.rs` and `brute_force.rs`'s own unit
//! tests; this binary is only about the pipeline wiring above them.
//!
//! Deterministic: isolated [`TempHome`]s, fixed `now_ms` literals, a fake
//! [`QueryEmbedder`] (no inference runtime), no network, no wall-clock sleeps.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use local_rag_core::identity::domain::path_fingerprint;
use local_rag_core::identity::{Uuid, UuidSource, uuidv7_from};
use local_rag_core::paths::StoreLayout;
use local_rag_projection::{
    BruteForceProjectionStore, RepresentationKind, ShardManager, ShardParams, VectorSource,
    params_for_model_space, shard_dir, switch,
};
use local_rag_protocol::{DegradedMode, SearchMode};
use local_rag_search::{
    NoopObserver, QueryEmbedError, QueryEmbedder, SearchEngine, SearchRequest, UnavailableEmbedder,
};
use local_rag_store::{
    CacheDb, DEFAULT_MODEL_SPACE_ID, DistanceMetric, GenerationState, NewContentBlob,
    NewFileRevision, NewOccurrence, NewParsedUnit, NewlineStyle, RepresentationKey, RequestRoot,
    SourceCompression, StateDb, UnitKind, WorktreeKind, WorktreeLockRegistry, WorktreeRootFacts,
    allocate_generation, create_repository, create_worktree, derive_content_blob,
    insert_content_blob, insert_file_revision, insert_generation_file, insert_occurrence,
    insert_parsed_unit, insert_projection_state, materialize_fts, observe_repository_path,
    observe_worktree_path, occurrence_id, register_representation, set_model_space_representation,
    transition_generation,
};
use local_rag_test_support::TempHome;

const DIMS: usize = 3;
const NOW: i64 = 1_000;

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
        uuidv7_from(8_000_000 + n, [0x77; 10])
    }
}

/// A [`VectorSource`] that derives each point's vector from its occurrence id,
/// so different occurrences are genuinely different neighbours (unlike the
/// constant-vector fakes the lock/concurrency suites use, where ranking is all
/// tie-break).
///
/// `code_raw` and `code_context` get *different* vectors for the same
/// occurrence — the `code_context` one deliberately much closer to the query
/// used below, so a leg that forgot to filter by kind would rank them first and
/// fail loudly instead of quietly returning the right answer for the wrong
/// reason.
struct PerOccurrenceVectors;

impl VectorSource for PerOccurrenceVectors {
    fn vector(&self, occurrence_id: &str, kind: RepresentationKind) -> Option<Vec<f32>> {
        // First hex digit of the occurrence id, as a stable per-occurrence
        // magnitude in [1, 16].
        let seed = occurrence_id
            .chars()
            .next()
            .and_then(|c| c.to_digit(16))
            .unwrap_or(0) as f32
            + 1.0;
        match kind {
            RepresentationKind::CodeRaw => Some(vec![seed, 0.0, 0.0]),
            // Closer to the `[1,0,0]` query than any code_raw point can be under
            // cosine, and larger than any of them under dot.
            _ => Some(vec![100.0, 0.0, 0.0]),
        }
    }
}

/// A [`QueryEmbedder`] returning a fixed unit vector along the first axis, in
/// whatever dimensionality the representation declares.
struct UnitQueryEmbedder;

impl QueryEmbedder for UnitQueryEmbedder {
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

/// A [`QueryEmbedder`] that ignores the representation's dimensionality — the
/// mistake the leg must catch before it reaches the shard.
struct WrongDimensionEmbedder;

impl QueryEmbedder for WrongDimensionEmbedder {
    fn embed_query(
        &self,
        _query: &str,
        _key: &RepresentationKey,
    ) -> Result<Vec<f32>, QueryEmbedError> {
        Ok(vec![1.0; DIMS + 4])
    }
}

/// Records the [`RepresentationKey`] it was asked to embed with, so a test can
/// assert *which* representation the leg selected.
struct RecordingEmbedder {
    seen: Arc<std::sync::Mutex<Vec<RepresentationKey>>>,
}

impl QueryEmbedder for RecordingEmbedder {
    fn embed_query(
        &self,
        _query: &str,
        key: &RepresentationKey,
    ) -> Result<Vec<f32>, QueryEmbedError> {
        self.seen
            .lock()
            .expect("recording embedder mutex poisoned")
            .push(key.clone());
        Ok(vec![0.0; key.dimensions as usize])
    }
}

// ---- fixtures ----------------------------------------------------------------

fn open_all() -> (TempHome, StoreLayout, Arc<StateDb>, Arc<CacheDb>) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let state = Arc::new(StateDb::open(layout.state_db()).expect("open state.sqlite"));
    let cache = Arc::new(CacheDb::open(layout.cache_db(), "dense-tests").expect("open cache"));
    (home, layout, state, cache)
}

fn uuid(seed: u8) -> Uuid {
    let mut rand = [0u8; 10];
    rand[9] = seed;
    uuidv7_from(4000, rand)
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
            create_repository(tx, &r, None, NOW)?;
            create_worktree(tx, &w, &r, WorktreeKind::Main, NOW)?;
            observe_worktree_path(tx, &w, &p, &p, &f, NOW)?;
            observe_repository_path(tx, &r, &p, NOW)
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

/// Register `code_raw` + `code_context` for the default model space with an
/// explicit metric and dimensionality — the two axes the dense leg reads back.
async fn register_representations(
    state: &StateDb,
    model_space_id: &Uuid,
    dimensions: u32,
    metric: DistanceMetric,
) {
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
                let id = register_representation(
                    tx,
                    &representation_id,
                    &RepresentationKey {
                        kind,
                        representation_version: 1,
                        normalization_version: 1,
                        model_id: format!("dense-test-model-{dimensions}"),
                        dimensions,
                        distance_metric: metric,
                    },
                    NOW,
                )?;
                set_model_space_representation(tx, &space, kind, &id, true, NOW)?;
            }
            Ok(())
        })
        .await
        .expect("register representations");
}

async fn init_projection(
    state: &StateDb,
    worktree_id: &Uuid,
    dimensions: u32,
    metric: DistanceMetric,
) {
    let w = worktree_id.to_string();
    state
        .writer()
        .transaction(move |tx| insert_projection_state(tx, &w, NOW))
        .await
        .expect("init projection state");
    register_representations(state, &default_model_space(), dimensions, metric).await;
}

async fn allocate_ready(state: &StateDb, worktree_id: &Uuid, gen_seed: u8) -> Uuid {
    let genr = uuid(gen_seed);
    let (w, g) = (worktree_id.to_string(), genr.to_string());
    state
        .writer()
        .transaction(move |tx| allocate_generation(tx, &w, &g, NOW).map(|_| ()))
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

/// Seed one occurrence with real content; returns its `occurrence_id`.
async fn seed_occurrence(state: &StateDb, generation_id: &Uuid, seed: u8, path: &str) -> String {
    let gen_str = generation_id.to_string();
    let revision = uuid(seed).to_string();
    let unit = uuid(seed.wrapping_add(40)).to_string();
    let occ = occurrence_id(&gen_str, path, &unit);
    let content = format!("fn unit_{seed}() {{ searchable }}\n");
    let derived = derive_content_blob("rust", &content);
    let bytes = content.as_bytes().to_vec();
    let len = bytes.len() as i64;
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
                    source_blob: &bytes,
                    compression: SourceCompression::None,
                    source_encoding: "utf-8",
                    newline_style: NewlineStyle::Lf,
                    source_size: len,
                },
                NOW,
            )?;
            insert_content_blob(
                tx,
                &NewContentBlob {
                    blob_id: &b,
                    language: "rust",
                    algo_version: algo,
                    normalization_version: norm,
                },
                NOW,
            )?;
            insert_parsed_unit(
                tx,
                &NewParsedUnit {
                    unit_id: &u,
                    file_revision_id: &rev,
                    unit_kind: UnitKind::Symbol,
                    syntax_locator: "fn:unit",
                    blob_id: &b,
                    span_start: 0,
                    span_end: len,
                    local_name: Some("unit"),
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

async fn commit_switch(
    state: &StateDb,
    layout: &StoreLayout,
    worktree_id: Uuid,
    generation_id: Uuid,
) {
    let read = state.open_read().expect("read");
    let params = params_for_model_space(&read, &default_model_space()).expect("params");
    drop(read);
    switch(
        state,
        &BruteForceProjectionStore::new(),
        &shard_dir(layout, &worktree_id, &default_model_space()),
        params,
        worktree_id,
        generation_id,
        default_model_space(),
        &PerOccurrenceVectors,
        &SeqUuidV7::new(),
        NOW,
    )
    .await
    .expect("switch to generation");
}

fn engine_with(
    state: &Arc<StateDb>,
    cache: &Arc<CacheDb>,
    layout: StoreLayout,
    embedder: Arc<dyn QueryEmbedder>,
    params: ShardParams,
) -> SearchEngine {
    let shards = Arc::new(ShardManager::new(
        state.clone(),
        Arc::new(BruteForceProjectionStore::new()),
        layout,
        params,
        Arc::new(PerOccurrenceVectors),
        Arc::new(SeqUuidV7::new()),
        8,
    ));
    SearchEngine::with_embedder(
        state.clone(),
        cache.clone(),
        Arc::new(WorktreeLockRegistry::new()),
        shards,
        embedder,
        Duration::from_millis(500),
    )
}

fn request(path: &str, limit: usize) -> SearchRequest {
    SearchRequest {
        root: request_root(path),
        query: "searchable".to_string(),
        limit,
        mode: SearchMode::Hybrid,
        name_pattern: None,
    }
}

/// A worktree with `count` occurrences, switched onto the brute-force backend
/// and with a materialized FTS view, under `metric`.
///
/// Returns `(home, layout, state, cache, worktree, generation, path, occurrences)`.
#[allow(clippy::type_complexity)]
async fn established(
    seed: u8,
    count: u8,
    metric: DistanceMetric,
) -> (
    TempHome,
    StoreLayout,
    Arc<StateDb>,
    Arc<CacheDb>,
    Uuid,
    Uuid,
    String,
    Vec<String>,
) {
    let (home, layout, state, cache) = open_all();
    let (wt, path) = worktree(&state, seed).await;
    init_projection(&state, &wt, DIMS as u32, metric).await;
    let generation = allocate_ready(&state, &wt, seed.wrapping_add(1)).await;
    let mut occurrences = Vec::new();
    for i in 0..count {
        occurrences.push(
            seed_occurrence(
                &state,
                &generation,
                seed.wrapping_add(10).wrapping_add(i),
                &format!("src/f{i}.rs"),
            )
            .await,
        );
    }
    commit_switch(&state, &layout, wt, generation).await;
    materialize_fts(
        &state,
        &cache,
        &wt.to_string(),
        &generation.to_string(),
        NOW,
    )
    .await
    .expect("materialize fts");
    (
        home,
        layout,
        state,
        cache,
        wt,
        generation,
        path,
        occurrences,
    )
}

// ---- happy path --------------------------------------------------------------

/// A healthy hybrid search returns dense candidates identified by
/// `occurrence_id`, ranked best-first, with no degradation — and **only**
/// `code_raw` points, even though the shard also holds `code_context` ones that
/// score higher.
#[tokio::test]
async fn a_healthy_search_returns_dense_candidates_by_occurrence_id() {
    let (_home, layout, state, cache, _wt, _gen, path, occurrences) =
        established(10, 4, DistanceMetric::Dot).await;

    let engine = engine_with(
        &state,
        &cache,
        layout,
        Arc::new(UnitQueryEmbedder),
        ShardParams::with_dimensions(DIMS),
    );
    let snapshot = engine
        .search_code_instrumented(request(&path, 5), NOW + 1, &NoopObserver)
        .await
        .expect("no infrastructure error")
        .expect("healthy tuple must not be an error envelope");

    assert_eq!(snapshot.response.degraded, None);
    assert!(snapshot.dense_served());
    assert_eq!(
        snapshot.dense.len(),
        occurrences.len(),
        "one hit per occurrence — `code_context` points are filtered out, \
         not merely out-ranked: {:?}",
        snapshot.dense
    );
    assert_eq!(
        snapshot
            .dense
            .iter()
            .map(|h| h.occurrence_id.clone())
            .collect::<HashSet<_>>(),
        occurrences.iter().cloned().collect::<HashSet<_>>(),
        "every hit is an occurrence of the active generation"
    );
    assert_eq!(
        snapshot.dense.iter().map(|h| h.rank).collect::<Vec<_>>(),
        (1..=occurrences.len()).collect::<Vec<_>>(),
        "ranks are 1-based and dense"
    );
    assert!(
        snapshot.dense.windows(2).all(|w| w[0].score >= w[1].score),
        "scores descend — higher is closer: {:?}",
        snapshot.dense.iter().map(|h| h.score).collect::<Vec<_>>()
    );
}

/// `with_dense_kind` moves the *whole* dense leg — the point filter and the
/// query's representation together (D-016).
///
/// The default is asserted in the same test, because the risk being guarded is
/// precisely that one of the two halves moves and the other does not: a leg that
/// searched `code_context` points with a `code_raw` query key would still return
/// results, just meaningless ones.
#[tokio::test]
async fn the_dense_kind_selects_both_the_points_and_the_query_representation() {
    let (_home, layout, state, cache, _wt, _gen, path, occurrences) =
        established(70, 4, DistanceMetric::Dot).await;

    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let engine = engine_with(
        &state,
        &cache,
        layout,
        Arc::new(RecordingEmbedder { seen: seen.clone() }),
        ShardParams::with_dimensions(DIMS),
    )
    .with_dense_kind(RepresentationKind::CodeContext);

    let snapshot = engine
        .search_code_instrumented(request(&path, 5), NOW + 1, &NoopObserver)
        .await
        .expect("no infrastructure error")
        .expect("healthy tuple must not be an error envelope");

    assert_eq!(
        seen.lock().expect("mutex poisoned")[0].kind,
        local_rag_store::RepresentationKind::CodeContext,
        "the query follows the searched kind"
    );
    assert_eq!(
        snapshot.dense.len(),
        occurrences.len(),
        "one hit per occurrence, now from the context points"
    );
    assert_eq!(
        snapshot
            .dense
            .iter()
            .map(|h| h.occurrence_id.clone())
            .collect::<HashSet<_>>(),
        occurrences.iter().cloned().collect::<HashSet<_>>()
    );
}

/// The query is embedded with the **active** model space's `code_raw`
/// representation — its `model_id`, `dimensions` and `distance_metric`, not a
/// store-wide default. `code_raw` because that is the untouched default
/// (spec 09 §3: "v0 ships `code_raw`") — no `with_dense_kind` here.
#[tokio::test]
async fn the_query_is_embedded_with_the_active_representation() {
    let (_home, layout, state, cache, _wt, _gen, path, _occ) =
        established(20, 2, DistanceMetric::Cosine).await;

    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let engine = engine_with(
        &state,
        &cache,
        layout,
        Arc::new(RecordingEmbedder { seen: seen.clone() }),
        ShardParams::with_dimensions(DIMS),
    );
    engine
        .search_code_instrumented(request(&path, 5), NOW + 1, &NoopObserver)
        .await
        .expect("no infra error")
        .expect("healthy");

    let seen = seen.lock().expect("mutex poisoned");
    assert_eq!(seen.len(), 1, "exactly one embedding per search");
    let key = &seen[0];
    assert_eq!(key.kind, local_rag_store::RepresentationKind::CodeRaw);
    assert_eq!(key.dimensions, DIMS as u32);
    assert_eq!(
        key.distance_metric,
        DistanceMetric::Cosine,
        "the registered metric travels with the key"
    );
    assert_eq!(key.model_id, format!("dense-test-model-{DIMS}"));
}

/// `representation.distance_metric` really decides the ranking: the same
/// points and the same query come back in opposite orders under `dot` and
/// `cosine`, because `PerOccurrenceVectors` varies magnitude along one axis.
#[tokio::test]
async fn the_registered_distance_metric_orders_the_dense_leg() {
    // Under `dot`, the longest vector wins; under `cosine` all these points are
    // perfectly aligned with the query, so they tie and fall back to the
    // deterministic point-id tie-break — a different order than `dot`'s.
    let (_home_a, layout_a, state_a, cache_a, _wt, _gen, path_a, _occ) =
        established(30, 4, DistanceMetric::Dot).await;
    let dot_order: Vec<f32> = engine_with(
        &state_a,
        &cache_a,
        layout_a,
        Arc::new(UnitQueryEmbedder),
        ShardParams::with_dimensions(DIMS),
    )
    .search_code_instrumented(request(&path_a, 5), NOW + 1, &NoopObserver)
    .await
    .expect("no infra error")
    .expect("healthy")
    .dense
    .iter()
    .map(|h| h.score)
    .collect();

    let (_home_b, layout_b, state_b, cache_b, _wt, _gen, path_b, _occ) =
        established(40, 4, DistanceMetric::Cosine).await;
    let cosine_order: Vec<f32> = engine_with(
        &state_b,
        &cache_b,
        layout_b,
        Arc::new(UnitQueryEmbedder),
        ShardParams::with_dimensions(DIMS),
    )
    .search_code_instrumented(request(&path_b, 5), NOW + 1, &NoopObserver)
    .await
    .expect("no infra error")
    .expect("healthy")
    .dense
    .iter()
    .map(|h| h.score)
    .collect();

    assert!(
        dot_order.iter().any(|s| *s > 1.5),
        "dot scores are raw magnitudes: {dot_order:?}"
    );
    assert!(
        cosine_order.iter().all(|s| (*s - 1.0).abs() < 1e-5),
        "cosine normalizes them all to 1.0: {cosine_order:?}"
    );
    assert_ne!(dot_order, cosine_order, "the metric changed the scores");
}

/// Spec 09 §4's candidate depth applies to this leg too: `limit = 1` floors at
/// 50, so a 60-occurrence generation yields exactly 50 dense candidates.
///
/// This fixture deliberately forces the leg's **second** backend call:
/// `PerOccurrenceVectors` scores every `code_context` point above every
/// `code_raw` one, so the first window (`50 × 2 kinds = 100` of the shard's 120
/// points) contains all 60 `code_context` points and only 40 `code_raw` ones —
/// ten short. Without the whole-shard retry this test would see 40, which is
/// exactly the starvation the retry exists to rule out.
#[tokio::test]
async fn request_limit_drives_the_dense_candidate_depth() {
    let (_home, layout, state, cache, _wt, _gen, path, occurrences) =
        established(50, 60, DistanceMetric::Dot).await;
    assert_eq!(occurrences.len(), 60);

    let engine = engine_with(
        &state,
        &cache,
        layout,
        Arc::new(UnitQueryEmbedder),
        ShardParams::with_dimensions(DIMS),
    );
    let floored = engine
        .search_code_instrumented(request(&path, 1), NOW + 1, &NoopObserver)
        .await
        .expect("no infra error")
        .expect("healthy")
        .dense;
    assert_eq!(floored.len(), 50, "max(1·4, 50) = 50");

    let raised = engine
        .search_code_instrumented(request(&path, 14), NOW + 1, &NoopObserver)
        .await
        .expect("no infra error")
        .expect("healthy")
        .dense;
    assert_eq!(raised.len(), 56, "max(14·4, 50) = 56");
}

/// Within a per-worktree shard there is no tenant or generation filter (spec 05
/// §1/§2): the query carries a vector and `k`, nothing else. A previous
/// generation's occurrences are unreachable because the switch removed their
/// points, not because the query excluded them.
#[tokio::test]
async fn the_dense_leg_serves_exactly_the_active_generations_occurrences() {
    let (_home, layout, state, cache, wt, gen_a, path, gen_a_occurrences) =
        established(60, 3, DistanceMetric::Dot).await;

    // A second generation over different paths, then a switch onto it.
    let gen_b = allocate_ready(&state, &wt, 70).await;
    let mut gen_b_occurrences = Vec::new();
    for i in 0..3u8 {
        gen_b_occurrences
            .push(seed_occurrence(&state, &gen_b, 80 + i, &format!("src/b{i}.rs")).await);
    }
    commit_switch(&state, &layout, wt, gen_b).await;
    materialize_fts(&state, &cache, &wt.to_string(), &gen_b.to_string(), NOW + 1)
        .await
        .expect("materialize fts for B");

    let engine = engine_with(
        &state,
        &cache,
        layout,
        Arc::new(UnitQueryEmbedder),
        ShardParams::with_dimensions(DIMS),
    );
    let snapshot = engine
        .search_code_instrumented(request(&path, 5), NOW + 2, &NoopObserver)
        .await
        .expect("no infra error")
        .expect("healthy");

    assert_eq!(snapshot.response.generation.id, gen_b.to_string());
    assert_eq!(snapshot.response.degraded, None);
    let served: HashSet<String> = snapshot
        .dense
        .iter()
        .map(|h| h.occurrence_id.clone())
        .collect();
    assert_eq!(
        served,
        gen_b_occurrences.iter().cloned().collect::<HashSet<_>>()
    );
    assert!(
        gen_a_occurrences.iter().all(|o| !served.contains(o)),
        "generation A's occurrences must be unreachable"
    );
    assert_ne!(gen_a.to_string(), gen_b.to_string());
}

// ---- degradation paths -------------------------------------------------------

/// No embedding provider ⇒ explicit `lexical_only` with a diagnostic, never a
/// silently empty dense leg pretending to be a healthy hybrid search.
#[tokio::test]
async fn without_an_embedding_provider_the_search_degrades_to_lexical_only() {
    let (_home, layout, state, cache, _wt, _gen, path, _occ) =
        established(90, 2, DistanceMetric::Dot).await;

    let engine = engine_with(
        &state,
        &cache,
        layout,
        Arc::new(UnavailableEmbedder),
        ShardParams::with_dimensions(DIMS),
    );
    let snapshot = engine
        .search_code_instrumented(request(&path, 5), NOW + 1, &NoopObserver)
        .await
        .expect("no infrastructure error")
        .expect("fts is healthy; must not be an error envelope");

    assert_eq!(snapshot.response.degraded, Some(DegradedMode::LexicalOnly));
    assert!(!snapshot.dense_served());
    assert!(snapshot.dense.is_empty());
    assert!(
        snapshot
            .response
            .diagnostics
            .iter()
            .any(|d| d.contains("no embedding provider")),
        "the reason must be reported: {:?}",
        snapshot.response.diagnostics
    );
    assert!(
        !snapshot.lexical.is_empty(),
        "the lexical leg still serves the query"
    );
}

/// An embedding whose dimensionality disagrees with the representation is
/// caught by the leg, with a diagnostic naming both numbers — before the shard
/// is even asked.
#[tokio::test]
async fn a_wrong_dimensioned_embedding_degrades_to_lexical_only() {
    let (_home, layout, state, cache, _wt, _gen, path, _occ) =
        established(100, 2, DistanceMetric::Dot).await;

    let engine = engine_with(
        &state,
        &cache,
        layout,
        Arc::new(WrongDimensionEmbedder),
        ShardParams::with_dimensions(DIMS),
    );
    let snapshot = engine
        .search_code_instrumented(request(&path, 5), NOW + 1, &NoopObserver)
        .await
        .expect("no infra error")
        .expect("healthy fts");

    assert_eq!(snapshot.response.degraded, Some(DegradedMode::LexicalOnly));
    assert!(snapshot.dense.is_empty());
    assert!(
        snapshot
            .response
            .diagnostics
            .iter()
            .any(|d| d.contains("dimensions") && d.contains(&format!("{}", DIMS + 4))),
        "diagnostic must name the mismatch: {:?}",
        snapshot.response.diagnostics
    );
}

/// A shard whose on-disk points are corrupt beyond self-healing degrades to
/// `lexical_only` with the backend's own reason — the F12 path, now over the
/// production backend.
#[tokio::test]
async fn a_corrupt_shard_degrades_to_lexical_only() {
    let (_home, layout, state, cache, wt, _gen, path, _occ) =
        established(110, 2, DistanceMetric::Dot).await;

    // Corrupt `points.bin` so the shard cannot be opened, and withhold the
    // vectors a rebuild-on-acquire would need, so the self-heal cannot succeed
    // either (mirrors `pipeline.rs::dense_unavailable_degrades_lexical_only`,
    // adapted to the brute-force file layout).
    let dir = shard_dir(&layout, &wt, &default_model_space());
    std::fs::write(dir.join("points.bin"), b"not a points file").expect("corrupt points.bin");

    struct NoVectors;
    impl VectorSource for NoVectors {
        fn vector(&self, _occurrence_id: &str, _kind: RepresentationKind) -> Option<Vec<f32>> {
            None
        }
    }

    let shards = Arc::new(ShardManager::new(
        state.clone(),
        Arc::new(BruteForceProjectionStore::new()),
        layout,
        ShardParams::with_dimensions(DIMS),
        Arc::new(NoVectors),
        Arc::new(SeqUuidV7::new()),
        8,
    ));
    let engine = SearchEngine::with_embedder(
        state.clone(),
        cache.clone(),
        Arc::new(WorktreeLockRegistry::new()),
        shards,
        Arc::new(UnitQueryEmbedder),
        Duration::from_millis(500),
    );

    let snapshot = engine
        .search_code_instrumented(request(&path, 5), NOW + 1, &NoopObserver)
        .await
        .expect("no infrastructure error")
        .expect("fts is healthy; must not be an error envelope");

    assert_eq!(snapshot.response.degraded, Some(DegradedMode::LexicalOnly));
    assert!(snapshot.dense.is_empty());
    assert!(
        !snapshot.response.diagnostics.is_empty(),
        "a degraded response carries its reason (spec 02 §6)"
    );
}

/// A query with no text embeds nothing — an empty leg, not a degraded search.
#[tokio::test]
async fn a_textless_query_leaves_the_dense_leg_empty_but_healthy() {
    let (_home, layout, state, cache, _wt, _gen, path, _occ) =
        established(120, 2, DistanceMetric::Dot).await;

    struct PanickingEmbedder;
    impl QueryEmbedder for PanickingEmbedder {
        fn embed_query(
            &self,
            _query: &str,
            _key: &RepresentationKey,
        ) -> Result<Vec<f32>, QueryEmbedError> {
            panic!("a textless query must never reach a provider");
        }
    }

    let engine = engine_with(
        &state,
        &cache,
        layout,
        Arc::new(PanickingEmbedder),
        ShardParams::with_dimensions(DIMS),
    );
    let mut req = request(&path, 5);
    req.query = "   ".to_string();
    let snapshot = engine
        .search_code_instrumented(req, NOW + 1, &NoopObserver)
        .await
        .expect("no infra error")
        .expect("healthy");

    assert_eq!(
        snapshot.response.degraded, None,
        "an empty query is not a degradation"
    );
    assert!(snapshot.dense.is_empty());
    assert!(snapshot.response.diagnostics.is_empty());
}

/// The shard directory is per (worktree, model space) — `shard_dir` — and the
/// params it is opened with come from the registry, so a store-wide fallback
/// can never silently size or score a shard. (Guards the wiring the tests above
/// depend on.)
#[tokio::test]
async fn shard_params_come_from_the_active_model_space() {
    let (_home, _layout, state, _cache, _wt, _gen, _path, _occ) =
        established(130, 1, DistanceMetric::Cosine).await;

    let read = state.open_read().expect("read");
    let params = params_for_model_space(&read, &default_model_space()).expect("params");
    assert_eq!(
        params,
        ShardParams {
            dimensions: DIMS,
            distance_metric: DistanceMetric::Cosine,
        }
    );
}

/// `shard_dir` is a pure function of `(layout, worktree, model space)` — no
/// generation axis, which is what makes "no generation filter inside a shard"
/// structural rather than enforced.
#[test]
fn a_shard_directory_has_no_generation_axis() {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    let wt = uuid(1);
    let ms = default_model_space();
    let a: &Path = &shard_dir(&layout, &wt, &ms);
    let b = shard_dir(&layout, &wt, &ms);
    assert_eq!(a, b.as_path());
    assert_ne!(shard_dir(&layout, &uuid(2), &ms), b);
}
