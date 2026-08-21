//! T12-03 acceptance tests: RRF fusion and the canonical `search_code`
//! response (spec 09 §4/§5/§7).
//!
//! The arithmetic of fusion is golden-tested one layer down, in
//! `crates/search/src/fusion.rs`; the response *shape* is unit-tested in
//! `crates/protocol/src/search.rs`. This binary covers what only a real store
//! can show: that both legs' candidates arrive at fusion, that a document found
//! by both appears once with both ranks, that `results[]` is populated from
//! `state.sqlite`, that `mode` really selects legs, and that two identical
//! requests serialize to identical bytes.
//!
//! Fixture helpers follow `crates/search/tests/dense.rs`'s own (real
//! `switch()` onto the production brute-force backend + a materialized FTS
//! view), duplicated rather than imported because integration test binaries
//! cannot share code without a `mod` file.
//!
//! Deterministic: isolated [`TempHome`]s, fixed `now_ms` literals, a fake
//! [`QueryEmbedder`], no network, no wall-clock sleeps.

use std::collections::HashSet;
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
use local_rag_protocol::{DegradedMode, ErrorCode, SearchMode};
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
        query_degraded: None,
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

// ---- the canonical response (spec 09 §7) -------------------------------------

/// A healthy hybrid search returns spec 09 §7's shape end to end: fused
/// `results[]` with metadata read from `state.sqlite`, the generation
/// reference, no degradation, no diagnostics.
#[tokio::test]
async fn a_healthy_hybrid_search_returns_the_canonical_response() {
    let (_home, layout, state, cache, _wt, generation, path, occurrences) =
        established(10, 3, DistanceMetric::Dot).await;

    let engine = engine_with(
        &state,
        &cache,
        layout,
        Arc::new(UnitQueryEmbedder),
        ShardParams::with_dimensions(DIMS),
    );
    let response = engine
        .search_code(request(&path, 5), NOW + 1)
        .await
        .expect("no infrastructure error")
        .expect("healthy tuple must not be an error envelope");

    assert_eq!(response.degraded, None);
    assert!(response.diagnostics.is_empty());
    assert_eq!(response.generation.id, generation.to_string());
    assert_eq!(
        response.generation.number, 1,
        "the first generation of a worktree is number 1"
    );
    assert_eq!(response.results.len(), occurrences.len());

    // Every §7 field except `snippet` (T12-04) is populated from the store.
    let first = &response.results[0];
    assert!(occurrences.contains(&first.occurrence_id));
    assert!(
        first.path.starts_with("src/f") && first.path.ends_with(".rs"),
        "path came from generation_file: {}",
        first.path
    );
    assert_eq!(first.name, "unit");
    assert_eq!(first.unit_kind, "symbol");
    assert_eq!(first.language, "rust");
    assert!(first.span[1] > first.span[0], "span is a real byte range");
    assert_eq!(first.qualified_name, None, "no caller derives one yet");
    let snippet = first.snippet.as_ref().expect("T12-04 fills snippets");
    assert!(
        snippet.text.starts_with("fn unit_") && snippet.text.contains("searchable"),
        "the snippet is the unit's own bytes: {:?}",
        snippet.text
    );
    assert_eq!(snippet.truncation, None, "a small unit is not truncated");
    assert!(first.score > 0.0);

    // Both legs found everything, so every result carries both ranks.
    assert!(
        response
            .results
            .iter()
            .all(|r| r.legs.lexical.is_some() && r.legs.dense.is_some()),
        "{:?}",
        response.results
    );
    // Scores descend.
    assert!(
        response
            .results
            .windows(2)
            .all(|w| w[0].score >= w[1].score),
        "{:?}",
        response.results.iter().map(|r| r.score).collect::<Vec<_>>()
    );
}

/// A document both legs found is **one** result carrying both ranks — the
/// occurrence merge spec 09 §4 requires, seen through the real pipeline.
#[tokio::test]
async fn a_document_found_by_both_legs_appears_once() {
    let (_home, layout, state, cache, _wt, _gen, path, occurrences) =
        established(20, 4, DistanceMetric::Dot).await;

    let engine = engine_with(
        &state,
        &cache,
        layout,
        Arc::new(UnitQueryEmbedder),
        ShardParams::with_dimensions(DIMS),
    );
    let snapshot = engine
        .search_code_instrumented(request(&path, 10), NOW + 1, &NoopObserver)
        .await
        .expect("no infra error")
        .expect("healthy");

    // Both legs returned every occurrence...
    assert_eq!(snapshot.lexical.len(), occurrences.len());
    assert_eq!(snapshot.dense.len(), occurrences.len());
    // ...and fusion still emitted each exactly once.
    let ids: Vec<&str> = snapshot
        .response
        .results
        .iter()
        .map(|r| r.occurrence_id.as_str())
        .collect();
    assert_eq!(ids.len(), occurrences.len());
    assert_eq!(
        ids.iter().collect::<HashSet<_>>().len(),
        occurrences.len(),
        "no duplicates"
    );
}

/// `limit` bounds the response even though each leg searched to
/// `candidate_depth(limit)` — 50 candidates in, 3 results out.
#[tokio::test]
async fn results_are_capped_by_limit_not_by_candidate_depth() {
    let (_home, layout, state, cache, _wt, _gen, path, occurrences) =
        established(30, 20, DistanceMetric::Dot).await;
    assert_eq!(occurrences.len(), 20);

    let engine = engine_with(
        &state,
        &cache,
        layout,
        Arc::new(UnitQueryEmbedder),
        ShardParams::with_dimensions(DIMS),
    );
    let snapshot = engine
        .search_code_instrumented(request(&path, 3), NOW + 1, &NoopObserver)
        .await
        .expect("no infra error")
        .expect("healthy");

    assert_eq!(
        snapshot.lexical.len(),
        20,
        "the leg searched to candidate depth (max(3·4, 50) = 50), finding all 20"
    );
    assert_eq!(
        snapshot.response.results.len(),
        3,
        "the response carries `limit`"
    );
}

// ---- modes (spec 09 §5) ------------------------------------------------------

/// `mode=lexical` runs the FTS leg only: the dense leg is not run at all (the
/// embedder would panic), and a single served leg is **not** a degradation.
#[tokio::test]
async fn lexical_mode_runs_only_the_fts_leg() {
    let (_home, layout, state, cache, _wt, _gen, path, occurrences) =
        established(40, 3, DistanceMetric::Dot).await;

    struct PanickingEmbedder;
    impl QueryEmbedder for PanickingEmbedder {
        fn embed_query(
            &self,
            _query: &str,
            _key: &RepresentationKey,
        ) -> Result<Vec<f32>, QueryEmbedError> {
            panic!("mode=lexical must never reach an embedding provider");
        }
    }

    let engine = engine_with(
        &state,
        &cache,
        layout,
        Arc::new(PanickingEmbedder),
        ShardParams::with_dimensions(DIMS),
    );
    let mut req = request(&path, 10);
    req.mode = SearchMode::Lexical;
    let snapshot = engine
        .search_code_instrumented(req, NOW + 1, &NoopObserver)
        .await
        .expect("no infra error")
        .expect("healthy");

    assert!(snapshot.dense.is_empty(), "the dense leg did not run");
    assert_eq!(snapshot.lexical.len(), occurrences.len());
    assert_eq!(
        snapshot.response.degraded, None,
        "a mode that asked for one leg and got it is not degraded"
    );
    assert!(snapshot.response.diagnostics.is_empty());
    assert!(
        snapshot
            .response
            .results
            .iter()
            .all(|r| r.legs.lexical.is_some() && r.legs.dense.is_none()),
        "only the lexical leg contributes ranks"
    );
}

/// `mode=code` runs the dense leg only.
#[tokio::test]
async fn code_mode_runs_only_the_dense_leg() {
    let (_home, layout, state, cache, _wt, _gen, path, occurrences) =
        established(50, 3, DistanceMetric::Dot).await;

    let engine = engine_with(
        &state,
        &cache,
        layout,
        Arc::new(UnitQueryEmbedder),
        ShardParams::with_dimensions(DIMS),
    );
    let mut req = request(&path, 10);
    req.mode = SearchMode::Code;
    let snapshot = engine
        .search_code_instrumented(req, NOW + 1, &NoopObserver)
        .await
        .expect("no infra error")
        .expect("healthy");

    assert!(snapshot.lexical.is_empty(), "the lexical leg did not run");
    assert_eq!(snapshot.dense.len(), occurrences.len());
    assert_eq!(snapshot.response.degraded, None);
    assert!(
        snapshot
            .response
            .results
            .iter()
            .all(|r| r.legs.dense.is_some() && r.legs.lexical.is_none())
    );
}

/// `mode=semantic` is refused with `UNSUPPORTED_MODE` (spec 09 §5, post-v0) —
/// and refused *before* anything else happens: an unresolvable root would
/// otherwise produce `WORKTREE_NOT_INDEXED`, so the code proves the ordering.
#[tokio::test]
async fn semantic_mode_is_refused_before_any_work() {
    let (_home, layout, state, cache, _wt, _gen, _path, _occ) =
        established(60, 1, DistanceMetric::Dot).await;

    let engine = engine_with(
        &state,
        &cache,
        layout,
        Arc::new(UnitQueryEmbedder),
        ShardParams::with_dimensions(DIMS),
    );
    let mut req = request("/nowhere/at/all", 10);
    req.mode = SearchMode::Semantic;
    let err = engine
        .search_code(req, NOW + 1)
        .await
        .expect("no infra error")
        .expect_err("must be an error envelope");

    assert_eq!(err.code, ErrorCode::UnsupportedMode);
    assert!(!err.retryable);
    assert!(err.message.contains("semantic"), "{}", err.message);
}

/// A single-leg mode whose one leg cannot serve has nothing to degrade onto:
/// `INDEX_UNAVAILABLE`, not an empty "successful" response.
#[tokio::test]
async fn a_single_leg_mode_whose_leg_fails_is_index_unavailable() {
    let (_home, layout, state, cache, _wt, _gen, path, _occ) =
        established(70, 2, DistanceMetric::Dot).await;

    let engine = engine_with(
        &state,
        &cache,
        layout,
        Arc::new(UnavailableEmbedder),
        ShardParams::with_dimensions(DIMS),
    );
    let mut req = request(&path, 10);
    req.mode = SearchMode::Code;
    let err = engine
        .search_code(req, NOW + 1)
        .await
        .expect("no infra error")
        .expect_err("the only requested leg is unavailable");

    assert_eq!(err.code, ErrorCode::IndexUnavailable);
    assert!(
        err.details
            .as_deref()
            .is_some_and(|d| d.contains("no embedding provider")),
        "the reason travels with the error: {:?}",
        err.details
    );
}

/// The same failure in `hybrid` is a *degradation*, because the other leg was
/// also requested and did serve.
#[tokio::test]
async fn hybrid_degrades_when_one_requested_leg_fails() {
    let (_home, layout, state, cache, _wt, _gen, path, occurrences) =
        established(80, 2, DistanceMetric::Dot).await;

    let engine = engine_with(
        &state,
        &cache,
        layout,
        Arc::new(UnavailableEmbedder),
        ShardParams::with_dimensions(DIMS),
    );
    let response = engine
        .search_code(request(&path, 10), NOW + 1)
        .await
        .expect("no infra error")
        .expect("the lexical leg still serves");

    assert_eq!(response.degraded, Some(DegradedMode::LexicalOnly));
    assert!(!response.diagnostics.is_empty());
    assert_eq!(response.results.len(), occurrences.len());
    assert!(
        response
            .results
            .iter()
            .all(|r| r.legs.dense.is_none() && r.legs.lexical.is_some())
    );
}

// ---- determinism -------------------------------------------------------------

/// Two identical requests serialize to identical bytes (spec 09 §7 + T12-03's
/// acceptance criterion). This is the property `HashMap`-based fusion could
/// silently break: iteration order is randomized per process, so only the
/// `(score desc, occurrence_id asc)` sort makes the output stable.
#[tokio::test]
async fn repeated_identical_requests_serialize_to_identical_bytes() {
    let (_home, layout, state, cache, _wt, _gen, path, _occ) =
        established(90, 12, DistanceMetric::Dot).await;

    let engine = engine_with(
        &state,
        &cache,
        layout,
        Arc::new(UnitQueryEmbedder),
        ShardParams::with_dimensions(DIMS),
    );

    let mut rendered: Vec<Vec<u8>> = Vec::new();
    for _ in 0..5 {
        let response = engine
            .search_code(request(&path, 10), NOW + 1)
            .await
            .expect("no infra error")
            .expect("healthy");
        rendered.push(serde_json::to_vec(&response).expect("serialize"));
    }
    assert!(
        rendered.windows(2).all(|w| w[0] == w[1]),
        "repeated searches must render byte-identically"
    );
    // And the bytes really are the §7 document, not an empty one.
    let json: serde_json::Value = serde_json::from_slice(&rendered[0]).expect("parse");
    assert_eq!(json["results"].as_array().expect("array").len(), 10);
    assert!(json["degraded"].is_null());
    assert!(json["generation"]["id"].is_string());
}
