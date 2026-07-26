//! T12-01 acceptance tests: the lexical leg as the search pipeline actually
//! runs it (spec 09 §1) — a real [`SearchEngine`] over a real `switch()`-
//! established active tuple and a real [`materialize_fts`] view, so the
//! request's `query`/`limit`/`name_pattern` are proven to reach the BM25 query,
//! and a stale `fts_projection_head` is proven never to be served as valid.
//!
//! The column-level ranking goldens, filter edge cases and depth arithmetic
//! live one layer down, in `crates/store/tests/fts_query.rs` — this binary only
//! covers what the *pipeline* adds: request plumbing and the
//! validated-view-only precondition.
//!
//! Fixture helpers follow `crates/search/tests/pipeline.rs`'s own idiom
//! (worktree with a resolvable path → projection state → `projection_ready`
//! generation → `switch()`), duplicated rather than imported because integration
//! test binaries cannot share code without a `mod` file — the convention this
//! crate's other test binaries already follow.
//!
//! Deterministic: an isolated [`TempHome`], fixed `now_ms` literals, no network,
//! no wall-clock sleeps.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use local_rag_core::identity::domain::path_fingerprint;
use local_rag_core::identity::{Uuid, UuidSource, uuidv7_from};
use local_rag_core::paths::StoreLayout;
use local_rag_projection::{
    FakeProjectionStore, RepresentationKind, ShardManager, ShardParams, VectorSource, switch,
};
use local_rag_protocol::{DegradedMode, SearchMode};
use local_rag_search::{NoopObserver, QueryEmbedError, QueryEmbedder, SearchEngine, SearchRequest};
use local_rag_store::{
    CacheDb, DEFAULT_MODEL_SPACE_ID, FTS_SYNC_REBUILD_OCCURRENCE_THRESHOLD, GenerationState,
    NewContentBlob, NewFileRevision, NewOccurrence, NewParsedUnit, NewlineStyle, RepresentationKey,
    RequestRoot, SourceCompression, StateDb, UnitKind, WorktreeKind, WorktreeLockRegistry,
    WorktreeRootFacts, allocate_generation, create_repository, create_worktree,
    derive_content_blob, insert_content_blob, insert_file_revision, insert_generation_file,
    insert_occurrence, insert_parsed_unit, insert_projection_state, materialize_fts,
    observe_repository_path, observe_worktree_path, occurrence_id, transition_generation,
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
        uuidv7_from(5_000_000 + n, [0x44; 10])
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

// ---- fixtures ----------------------------------------------------------------

fn open_all() -> (TempHome, StoreLayout, Arc<StateDb>, Arc<CacheDb>) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let state = Arc::new(StateDb::open(layout.state_db()).expect("open state.sqlite"));
    let cache =
        Arc::new(CacheDb::open(layout.cache_db(), "lexical-tests").expect("open cache.sqlite"));
    (home, layout, state, cache)
}

fn uuid(seed: u8) -> Uuid {
    let mut rand = [0u8; 10];
    rand[9] = seed;
    uuidv7_from(3000, rand)
}

/// Register a worktree with a resolvable current path (spec 02 §3.3).
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

/// The default model space must declare its code representations for the
/// expected point set to resolve (T11-05).
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
        .expect("register default-space code representations");
}

async fn init_projection(state: &StateDb, worktree_id: &Uuid) {
    let w = worktree_id.to_string();
    state
        .writer()
        .transaction(move |tx| insert_projection_state(tx, &w, 1000))
        .await
        .expect("init projection state");
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

/// Seed one occurrence with real, caller-chosen content/name/path — the
/// searchable payload this binary's assertions are about. `blob_id` must be the
/// real [`derive_content_blob`] hash, since `materialize_fts` recomputes it.
async fn seed_named_occurrence(
    state: &StateDb,
    generation_id: &Uuid,
    seed: u8,
    path: &str,
    local_name: &str,
    content: &str,
) -> String {
    let gen_str = generation_id.to_string();
    let revision = uuid(seed).to_string();
    let unit = uuid(seed.wrapping_add(40)).to_string();
    let occ = occurrence_id(&gen_str, path, &unit);
    let derived = derive_content_blob("rust", content);
    let bytes = content.as_bytes().to_vec();
    let len = bytes.len() as i64;
    let (rev, b, u, g, p, occ2, name) = (
        revision,
        derived.blob_id.clone(),
        unit,
        gen_str,
        path.to_string(),
        occ.clone(),
        local_name.to_string(),
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
                    syntax_locator: &format!("fn:{name}"),
                    blob_id: &b,
                    span_start: 0,
                    span_end: len,
                    local_name: Some(&name),
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

/// Seed `count` cheap occurrences sharing one revision/blob, tagged so several
/// generations can coexist in the same store.
async fn seed_bulk(state: &StateDb, generation_id: &Uuid, tag: &str, count: u64) {
    let gen_str = generation_id.to_string();
    let file_revision_id = format!("rev-{tag}");
    let derived = derive_content_blob("rust", "a");
    let (fr, blob, g, t) = (
        file_revision_id,
        derived.blob_id.clone(),
        gen_str,
        tag.to_string(),
    );
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
                let unit_id = format!("{t}-unit-{i}");
                let path = format!("{t}{i}.rs");
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
}

async fn commit_switch_to(
    state: &StateDb,
    shard_dir: &Path,
    worktree_id: Uuid,
    generation_id: Uuid,
) {
    switch(
        state,
        &FakeProjectionStore::new(),
        shard_dir,
        params(),
        worktree_id,
        generation_id,
        default_model_space(),
        &AlwaysVectors,
        &SeqUuidV7::new(),
        1000,
    )
    .await
    .expect("switch to generation");
}

fn engine_over(state: &Arc<StateDb>, cache: &Arc<CacheDb>, layout: StoreLayout) -> SearchEngine {
    let shards = Arc::new(ShardManager::new(
        state.clone(),
        Arc::new(FakeProjectionStore::new()),
        layout,
        params(),
        Arc::new(AlwaysVectors),
        Arc::new(SeqUuidV7::new()),
        8,
    ));
    SearchEngine::with_embedder(
        state.clone(),
        cache.clone(),
        Arc::new(WorktreeLockRegistry::new()),
        shards,
        Arc::new(FixedQueryEmbedder),
        Duration::from_millis(500),
    )
}

fn request(path: &str, query: &str, name_pattern: Option<&str>, limit: usize) -> SearchRequest {
    SearchRequest {
        root: request_root(path),
        query: query.to_string(),
        limit,
        mode: SearchMode::Hybrid,
        name_pattern: name_pattern.map(str::to_string),
    }
}

/// How many `fts_doc` rows the cache physically holds for `worktree_id`.
fn fts_doc_rows(cache: &CacheDb, worktree_id: &str) -> i64 {
    let read = cache.open_read().expect("cache read conn");
    read.query_row(
        "SELECT COUNT(*) FROM fts_doc WHERE worktree_id = ?1",
        [worktree_id],
        |r| r.get(0),
    )
    .expect("count fts_doc rows")
}

// ---- tests --------------------------------------------------------------------

/// The request's `query` reaches the BM25 query and comes back as ranked
/// candidates of the **active** generation, with a healthy (non-degraded)
/// response.
#[tokio::test]
async fn a_healthy_search_returns_ranked_lexical_candidates() {
    let (_home, layout, state, cache) = open_all();
    let (wt, path) = worktree(&state, 10).await;
    init_projection(&state, &wt).await;
    let generation = allocate_ready(&state, &wt, 11).await;
    let wanted = seed_named_occurrence(
        &state,
        &generation,
        12,
        "src/parser/imports.rs",
        "extractImports",
        "fn extract_imports() {}\n",
    )
    .await;
    seed_named_occurrence(
        &state,
        &generation,
        14,
        "src/build/compile.rs",
        "compileModule",
        "fn compile_module() {}\n",
    )
    .await;
    let shard_dir = layout.projection_shard(&wt.to_string());
    commit_switch_to(&state, &shard_dir, wt, generation).await;
    materialize_fts(
        &state,
        &cache,
        &wt.to_string(),
        &generation.to_string(),
        2000,
    )
    .await
    .expect("materialize fts");

    let engine = engine_over(&state, &cache, layout);
    let snapshot = engine
        .search_code_instrumented(
            request(&path, "extractImports", None, 5),
            3000,
            &NoopObserver,
        )
        .await
        .expect("no infrastructure error")
        .expect("healthy tuple must not be an error envelope");

    assert_eq!(snapshot.response.degraded, None);
    assert_eq!(snapshot.response.generation.id, generation.to_string());
    assert_eq!(
        snapshot
            .lexical
            .iter()
            .map(|h| h.occurrence_id.clone())
            .collect::<Vec<_>>(),
        vec![wanted],
        "the camelCase symbol is found; the unrelated one is not"
    );
    assert_eq!(snapshot.lexical[0].rank, 1);

    // A query matching nothing is an empty leg, not an error and not degraded.
    let empty = engine
        .search_code_instrumented(
            request(&path, "nonexistentterm", None, 5),
            3000,
            &NoopObserver,
        )
        .await
        .expect("no infrastructure error")
        .expect("still healthy");
    assert_eq!(empty.response.degraded, None);
    assert!(empty.lexical.is_empty());
}

/// `name_pattern` travels from the request into the column-scoped prefix
/// filter.
#[tokio::test]
async fn name_pattern_from_the_request_filters_the_leg() {
    let (_home, layout, state, cache) = open_all();
    let (wt, path) = worktree(&state, 20).await;
    init_projection(&state, &wt).await;
    let generation = allocate_ready(&state, &wt, 21).await;
    let extractor = seed_named_occurrence(
        &state,
        &generation,
        22,
        "src/a.rs",
        "extractImports",
        "fn a() { shared }\n",
    )
    .await;
    seed_named_occurrence(
        &state,
        &generation,
        24,
        "src/b.rs",
        "compileModule",
        "fn b() { shared }\n",
    )
    .await;
    let shard_dir = layout.projection_shard(&wt.to_string());
    commit_switch_to(&state, &shard_dir, wt, generation).await;
    materialize_fts(
        &state,
        &cache,
        &wt.to_string(),
        &generation.to_string(),
        2000,
    )
    .await
    .expect("materialize fts");

    let engine = engine_over(&state, &cache, layout);

    // Unfiltered: both bodies carry `shared`.
    let all = engine
        .search_code_instrumented(request(&path, "shared", None, 5), 3000, &NoopObserver)
        .await
        .expect("no infra error")
        .expect("healthy")
        .lexical;
    assert_eq!(all.len(), 2);

    // Filtered by a name prefix: only the matching symbol survives.
    let filtered = engine
        .search_code_instrumented(
            request(&path, "shared", Some("extractImp"), 5),
            3000,
            &NoopObserver,
        )
        .await
        .expect("no infra error")
        .expect("healthy")
        .lexical;
    assert_eq!(
        filtered
            .iter()
            .map(|h| h.occurrence_id.clone())
            .collect::<Vec<_>>(),
        vec![extractor]
    );

    // A pattern nothing matches is an empty leg, not an error.
    let none = engine
        .search_code_instrumented(
            request(&path, "shared", Some("zzz"), 5),
            3000,
            &NoopObserver,
        )
        .await
        .expect("no infra error")
        .expect("healthy")
        .lexical;
    assert!(none.is_empty());
}

/// The request's `limit` drives spec 09 §4's candidate depth: `limit = 1`
/// floors at 50, so a 60-row generation yields exactly 50 candidates.
#[tokio::test]
async fn request_limit_drives_the_candidate_depth() {
    let (_home, layout, state, cache) = open_all();
    let (wt, path) = worktree(&state, 30).await;
    init_projection(&state, &wt).await;
    let generation = allocate_ready(&state, &wt, 31).await;
    seed_bulk(&state, &generation, "wide", 60).await;
    let shard_dir = layout.projection_shard(&wt.to_string());
    commit_switch_to(&state, &shard_dir, wt, generation).await;
    materialize_fts(
        &state,
        &cache,
        &wt.to_string(),
        &generation.to_string(),
        2000,
    )
    .await
    .expect("materialize fts");

    let engine = engine_over(&state, &cache, layout);

    // Every bulk row's path is `wide<N>.rs`, so the `wide` path token matches
    // all 60 of them.
    let floored = engine
        .search_code_instrumented(request(&path, "wide", None, 1), 3000, &NoopObserver)
        .await
        .expect("no infra error")
        .expect("healthy")
        .lexical;
    assert_eq!(floored.len(), 50, "max(1·4, 50) = 50");

    let raised = engine
        .search_code_instrumented(request(&path, "wide", None, 14), 3000, &NoopObserver)
        .await
        .expect("no infra error")
        .expect("healthy")
        .lexical;
    assert_eq!(raised.len(), 56, "max(14·4, 50) = 56");
}

/// **A stale head is never queried as valid.** Generation A is materialized
/// (valid head), then the worktree switches to generation B, which is above the
/// synchronous-rebuild threshold. `fts_projection_head` now names A while the
/// active generation is B: the view is invalid, the leg must not run at all,
/// and the response is explicitly `dense_only` with a diagnostic — never A's
/// rows, and never a silently empty lexical result `[FIXED]`.
#[tokio::test]
async fn a_stale_head_degrades_to_dense_only_without_running_the_leg() {
    let (_home, layout, state, cache) = open_all();
    let (wt, path) = worktree(&state, 40).await;
    init_projection(&state, &wt).await;
    let shard_dir = layout.projection_shard(&wt.to_string());

    // Generation A: small, materialized, genuinely valid.
    let gen_a = allocate_ready(&state, &wt, 41).await;
    let stale_occ = seed_named_occurrence(
        &state,
        &gen_a,
        42,
        "src/a.rs",
        "extractImports",
        "fn a() { landmark }\n",
    )
    .await;
    commit_switch_to(&state, &shard_dir, wt, gen_a).await;
    materialize_fts(&state, &cache, &wt.to_string(), &gen_a.to_string(), 2000)
        .await
        .expect("materialize fts for A");

    // Sanity: while A is active, the landmark term is genuinely findable.
    let engine = engine_over(&state, &cache, layout.clone());
    let on_a = engine
        .search_code_instrumented(request(&path, "landmark", None, 5), 2500, &NoopObserver)
        .await
        .expect("no infra error")
        .expect("healthy on A");
    assert_eq!(on_a.response.degraded, None);
    assert_eq!(
        on_a.lexical
            .iter()
            .map(|h| h.occurrence_id.clone())
            .collect::<Vec<_>>(),
        vec![stale_occ]
    );

    // Generation B: above the synchronous-rebuild threshold and never
    // materialized, so validation defers to background instead of self-healing.
    let gen_b = allocate_ready(&state, &wt, 44).await;
    seed_bulk(
        &state,
        &gen_b,
        "b",
        FTS_SYNC_REBUILD_OCCURRENCE_THRESHOLD + 1,
    )
    .await;
    commit_switch_to(&state, &shard_dir, wt, gen_b).await;

    // A's rows are still physically present in the cache — this is what makes
    // the assertion below meaningful rather than vacuous.
    assert_eq!(fts_doc_rows(&cache, &wt.to_string()), 1);

    let engine = engine_over(&state, &cache, layout);
    let snapshot = engine
        .search_code_instrumented(request(&path, "landmark", None, 5), 3000, &NoopObserver)
        .await
        .expect("no infrastructure error")
        .expect("dense is healthy; must not be an error envelope");

    assert_eq!(snapshot.response.generation.id, gen_b.to_string());
    assert_eq!(snapshot.response.degraded, Some(DegradedMode::DenseOnly));
    assert!(
        !snapshot.response.diagnostics.is_empty(),
        "a degraded response carries its validation reason (spec 02 §6)"
    );
    assert!(
        snapshot.lexical.is_empty(),
        "the stale head's generation-A rows must never be served: {:?}",
        snapshot.lexical
    );
    // And the cache was not mutated by the read path.
    assert_eq!(fts_doc_rows(&cache, &wt.to_string()), 1);
}
