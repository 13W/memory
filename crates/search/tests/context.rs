//! T12-04 acceptance tests: `source_blob` snippets, `get_file_context` and
//! `project_overview` (spec 09 §7, 11 §2, 12 §2).
//!
//! The card's central guarantee is negative and cannot be shown by unit tests:
//! **the live file is never read**. So the suite indexes a file, then *mutates
//! and deletes it on disk*, and asserts the answers are unchanged — which is
//! only possible because every excerpt comes from the stored `source_blob`
//! (spec 09 §7 `[FIXED]`).
//!
//! Cap arithmetic and UTF-8 boundary handling are golden-tested one layer down
//! (`crates/search/src/snippet.rs`); tree folding, the entry-point heuristic and
//! the cache live in `crates/search/src/overview.rs`. This binary covers what
//! only a real store shows: that the pieces are wired to the active generation.
//!
//! Fixture helpers follow `crates/search/tests/dense.rs`'s own, duplicated
//! rather than imported because integration test binaries cannot share code
//! without a `mod` file.
//!
//! Deterministic: isolated [`TempHome`]s, fixed `now_ms` literals, a fake
//! [`QueryEmbedder`], no network, no wall-clock sleeps.

use std::collections::HashMap;
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
use local_rag_protocol::{ErrorCode, SearchMode};
use local_rag_search::{
    QueryEmbedError, QueryEmbedder, SNIPPET_CAP_BYTES, SearchEngine, SearchRequest, TREE_DEPTH,
};
use local_rag_store::{
    CacheDb, DEFAULT_MODEL_SPACE_ID, DistanceMetric, GenerationState, NewFileRevision,
    NewOccurrence, NewParsedUnit, NewUnresolvedReference, NewlineStyle, RepresentationKey,
    RequestRoot, SkipReason, SourceCompression, StateDb, UnitKind, WorktreeKind,
    WorktreeLockRegistry, WorktreeRootFacts, allocate_generation, create_or_reuse_content_blob,
    create_repository, create_worktree, derive_content_blob, insert_file_revision,
    insert_generation_file, insert_occurrence, insert_parsed_unit, insert_projection_state,
    insert_skipped_file, insert_unresolved_reference, materialize_fts, observe_repository_path,
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

/// Seed one file with caller-chosen content, occupying the whole file as one
/// unit. The unit's span is `[0, content.len())`, so a snippet of it is the
/// file verbatim — which is what makes "the answer follows the stored bytes"
/// directly observable.
async fn seed_file(
    state: &StateDb,
    generation_id: &Uuid,
    seed: u8,
    path: &str,
    content: &str,
    local_name: &str,
) -> String {
    let gen_str = generation_id.to_string();
    let revision = uuid(seed).to_string();
    let unit = uuid(seed.wrapping_add(40)).to_string();
    let occ = occurrence_id(&gen_str, path, &unit);
    let content = content.to_string();
    let derived = derive_content_blob("rust", &content);
    let bytes = content.as_bytes().to_vec();
    let len = bytes.len() as i64;
    let derived_blob = derived.clone();
    let (rev, b, u, g, p, occ2, name) = (
        revision,
        derived.blob_id.clone(),
        unit,
        gen_str,
        path.to_string(),
        occ.clone(),
        local_name.to_string(),
    );
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
            // Two paths with identical content share one content-addressed
            // blob — structural sharing (spec 06 §2), so the fixture must reuse
            // rather than insert blindly.
            create_or_reuse_content_blob(tx, &derived_blob, "rust", NOW)?;
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

// ---- the central guarantee: stored bytes, never the live file ----------------

/// **The card's headline case.** A file is indexed, then rewritten *and* another
/// deleted on disk; every excerpt still shows what the generation captured.
///
/// This is what the strict `source_blob` invariant buys (spec 09 §7 `[FIXED]`):
/// a snippet quoting the live file would either show text that never existed at
/// the reported offsets, or fail outright once the file is gone.
#[tokio::test]
async fn snippets_survive_mutation_and_deletion_of_the_live_file() {
    let (home, layout, state, cache) = open_all();
    let (wt, path) = worktree(&state, 10).await;
    init_projection(&state, &wt, DIMS as u32, DistanceMetric::Dot).await;
    let generation = allocate_ready(&state, &wt, 11).await;

    // Real files on disk, with the exact bytes the generation records.
    let root = home.join("repo");
    std::fs::create_dir_all(root.join("src")).expect("mkdir");
    let original = "fn searchable() { let marker = 1; }\n";
    let doomed = "fn doomed() { searchable }\n";
    std::fs::write(root.join("src/keep.rs"), original).expect("write");
    std::fs::write(root.join("src/gone.rs"), doomed).expect("write");

    let kept = seed_file(
        &state,
        &generation,
        12,
        "src/keep.rs",
        original,
        "searchable",
    )
    .await;
    let removed = seed_file(&state, &generation, 14, "src/gone.rs", doomed, "doomed").await;
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

    // Now diverge the working tree from the generation, hard.
    std::fs::write(root.join("src/keep.rs"), "TOTALLY DIFFERENT CONTENT\n").expect("rewrite");
    std::fs::remove_file(root.join("src/gone.rs")).expect("delete");

    let engine = engine_with(
        &state,
        &cache,
        layout,
        Arc::new(UnitQueryEmbedder),
        ShardParams::with_dimensions(DIMS),
    );
    let response = engine
        .search_code(request(&path, 10), NOW + 1)
        .await
        .expect("no infra error")
        .expect("healthy");

    let by_id: HashMap<&str, &_> = response
        .results
        .iter()
        .map(|r| (r.occurrence_id.as_str(), r))
        .collect();
    let kept_result = by_id
        .get(kept.as_str())
        .expect("rewritten file still served");
    let kept_snippet = kept_result.snippet.as_ref().expect("snippet");
    assert_eq!(
        kept_snippet.text, original,
        "the snippet must show the indexed bytes, not the rewritten file"
    );
    assert!(!kept_snippet.text.contains("TOTALLY DIFFERENT"));

    let gone_result = by_id
        .get(removed.as_str())
        .expect("deleted file is still searchable in this generation");
    assert_eq!(
        gone_result.snippet.as_ref().expect("snippet").text,
        doomed,
        "a file deleted from disk still yields its original snippet"
    );

    // `get_file_context` reads the same bytes through the same invariant.
    let context = engine
        .get_file_context(&request_root(&path), "src/gone.rs")
        .await
        .expect("no infra error")
        .expect("the path is in the generation even though the file is gone");
    assert_eq!(
        context.occurrences[0]
            .snippet
            .as_ref()
            .expect("snippet")
            .text,
        doomed
    );
}

// ---- snippets in the search response -----------------------------------------

/// Ordinary snippets carry no truncation metadata; an over-cap unit does, and
/// the metadata describes the **full** span.
#[tokio::test]
async fn an_oversized_unit_is_truncated_with_metadata() {
    let (_home, layout, state, cache) = open_all();
    let (wt, path) = worktree(&state, 20).await;
    init_projection(&state, &wt, DIMS as u32, DistanceMetric::Dot).await;
    let generation = allocate_ready(&state, &wt, 21).await;

    let small = "fn searchable() {}\n";
    // Comfortably over the 8 KiB cap.
    let huge = format!("fn searchable() {{ {} }}\n", "x".repeat(SNIPPET_CAP_BYTES));
    let small_occ = seed_file(&state, &generation, 22, "src/small.rs", small, "searchable").await;
    let huge_occ = seed_file(&state, &generation, 24, "src/huge.rs", &huge, "searchable").await;
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

    let engine = engine_with(
        &state,
        &cache,
        layout,
        Arc::new(UnitQueryEmbedder),
        ShardParams::with_dimensions(DIMS),
    );
    let response = engine
        .search_code(request(&path, 10), NOW + 1)
        .await
        .expect("no infra error")
        .expect("healthy");

    let by_id: HashMap<&str, &_> = response
        .results
        .iter()
        .map(|r| (r.occurrence_id.as_str(), r))
        .collect();

    let small_snippet = by_id[small_occ.as_str()].snippet.as_ref().expect("snippet");
    assert_eq!(small_snippet.text, small);
    assert_eq!(small_snippet.truncation, None, "under the cap: nothing cut");

    let huge_snippet = by_id[huge_occ.as_str()].snippet.as_ref().expect("snippet");
    assert_eq!(huge_snippet.text.len(), SNIPPET_CAP_BYTES);
    let truncation = huge_snippet.truncation.as_ref().expect("truncated");
    assert_eq!(
        truncation.original_size,
        huge.len() as i64,
        "original_size describes the whole unit, not what survived"
    );
    assert_eq!(truncation.hash.len(), 64);
}

/// A truncated snippet serializes as an object, an untruncated one as the plain
/// string spec 09 §7 documents — the compatibility the widened type preserves.
#[tokio::test]
async fn snippet_serialization_matches_the_spec_shape() {
    let (_home, layout, state, cache) = open_all();
    let (wt, path) = worktree(&state, 30).await;
    init_projection(&state, &wt, DIMS as u32, DistanceMetric::Dot).await;
    let generation = allocate_ready(&state, &wt, 31).await;
    let small = "fn searchable() {}\n";
    seed_file(&state, &generation, 32, "src/small.rs", small, "searchable").await;
    let huge = format!("fn searchable() {{ {} }}\n", "x".repeat(SNIPPET_CAP_BYTES));
    seed_file(&state, &generation, 34, "src/huge.rs", &huge, "searchable").await;
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

    let engine = engine_with(
        &state,
        &cache,
        layout,
        Arc::new(UnitQueryEmbedder),
        ShardParams::with_dimensions(DIMS),
    );
    let response = engine
        .search_code(request(&path, 10), NOW + 1)
        .await
        .expect("no infra error")
        .expect("healthy");
    let json: serde_json::Value = serde_json::to_value(&response).expect("serialize");

    let mut saw_string = false;
    let mut saw_object = false;
    for result in json["results"].as_array().expect("array") {
        match &result["snippet"] {
            serde_json::Value::String(_) => saw_string = true,
            serde_json::Value::Object(o) => {
                saw_object = true;
                assert!(o.contains_key("text") && o.contains_key("truncation"));
                assert!(o["truncation"]["hash"].is_string());
                assert!(o["truncation"]["original_size"].is_i64());
            }
            other => panic!("unexpected snippet shape: {other:?}"),
        }
    }
    assert!(saw_string, "an untruncated snippet is a bare string");
    assert!(saw_object, "a truncated snippet widens to an object");
}

// ---- get_file_context (spec 11 §2) -------------------------------------------

/// The file's whole occurrence list, ascending by span, each with its excerpt.
#[tokio::test]
async fn file_context_lists_every_occurrence_of_the_path() {
    let (_home, layout, state, cache) = open_all();
    let (wt, path) = worktree(&state, 40).await;
    init_projection(&state, &wt, DIMS as u32, DistanceMetric::Dot).await;
    let generation = allocate_ready(&state, &wt, 41).await;
    let content = "fn alpha() { searchable }\n";
    let occ = seed_file(&state, &generation, 42, "src/a.rs", content, "alpha").await;
    seed_file(&state, &generation, 44, "src/b.rs", content, "beta").await;
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

    let engine = engine_with(
        &state,
        &cache,
        layout,
        Arc::new(UnitQueryEmbedder),
        ShardParams::with_dimensions(DIMS),
    );
    let context = engine
        .get_file_context(&request_root(&path), "src/a.rs")
        .await
        .expect("no infra error")
        .expect("the path is indexed");

    assert_eq!(context.path, "src/a.rs");
    assert_eq!(context.generation.id, generation.to_string());
    assert_eq!(context.generation.number, 1);
    assert_eq!(
        context.occurrences.len(),
        1,
        "only this path's occurrences: {:?}",
        context.occurrences
    );
    let only = &context.occurrences[0];
    assert_eq!(only.occurrence_id, occ);
    assert_eq!(only.unit_kind, "symbol");
    assert_eq!(only.name, "alpha");
    assert_eq!(only.span, [0, content.len() as i64]);
    assert_eq!(only.snippet.as_ref().expect("snippet").text, content);
}

/// A never-seen path and a deliberately skipped one are both
/// `PATH_NOT_INDEXED`, but their `details` differ — "why can't I see my file?"
/// must be answerable.
#[tokio::test]
async fn an_unknown_path_and_a_skipped_path_are_told_apart() {
    let (_home, layout, state, cache) = open_all();
    let (wt, path) = worktree(&state, 50).await;
    init_projection(&state, &wt, DIMS as u32, DistanceMetric::Dot).await;
    let generation = allocate_ready(&state, &wt, 51).await;
    seed_file(
        &state,
        &generation,
        52,
        "src/a.rs",
        "fn searchable() {}\n",
        "searchable",
    )
    .await;

    // A file the classifier refused (spec 06 §2.2): recorded, but with no
    // occurrences and — for `secret` — no `source_blob` at all.
    let g = generation.to_string();
    state
        .writer()
        .transaction(move |tx| {
            insert_skipped_file(tx, &g, "src/creds.env", SkipReason::Secret, None)
        })
        .await
        .expect("seed skipped file");

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

    let engine = engine_with(
        &state,
        &cache,
        layout,
        Arc::new(UnitQueryEmbedder),
        ShardParams::with_dimensions(DIMS),
    );

    let unknown = engine
        .get_file_context(&request_root(&path), "src/nowhere.rs")
        .await
        .expect("no infra error")
        .expect_err("not in the generation");
    assert_eq!(unknown.code, ErrorCode::PathNotIndexed);
    assert!(!unknown.retryable);
    assert_eq!(
        unknown.details.as_deref(),
        Some("no such path in the active generation")
    );

    let skipped = engine
        .get_file_context(&request_root(&path), "src/creds.env")
        .await
        .expect("no infra error")
        .expect_err("skipped, not indexed");
    assert_eq!(skipped.code, ErrorCode::PathNotIndexed);
    assert_eq!(skipped.details.as_deref(), Some("skipped, reason=secret"));
}

// ---- project_overview (spec 11 §2) -------------------------------------------

/// Tree, entry points and top imports, all derived from the active generation.
#[tokio::test]
async fn project_overview_describes_the_active_generation() {
    let (_home, layout, state, cache) = open_all();
    let (wt, path) = worktree(&state, 60).await;
    init_projection(&state, &wt, DIMS as u32, DistanceMetric::Dot).await;
    let generation = allocate_ready(&state, &wt, 61).await;

    let body = "fn searchable() {}\n";
    seed_file(&state, &generation, 62, "src/main.rs", body, "main").await;
    seed_file(&state, &generation, 64, "src/parser/mod.rs", body, "parse").await;
    seed_file(
        &state,
        &generation,
        66,
        "src/parser/deep/inner.rs",
        body,
        "deep",
    )
    .await;

    // Imports live on the revision, so they are seeded against the same ids
    // `seed_file` minted (`uuid(seed)` / `uuid(seed + 40)`).
    let imports = [
        (62u8, "serde"),
        (64, "serde"),
        (66, "serde"),
        (62, "tokio"),
        (64, "std::fmt"),
    ];
    for (seed, specifier) in imports {
        let (rev, unit, spec) = (
            uuid(seed).to_string(),
            uuid(seed.wrapping_add(40)).to_string(),
            specifier.to_string(),
        );
        state
            .writer()
            .transaction(move |tx| {
                insert_unresolved_reference(
                    tx,
                    &NewUnresolvedReference {
                        file_revision_id: &rev,
                        source_unit_id: &unit,
                        reference_text: &spec,
                        reference_kind: "import",
                    },
                )
            })
            .await
            .expect("seed import");
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

    let engine = engine_with(
        &state,
        &cache,
        layout,
        Arc::new(UnitQueryEmbedder),
        ShardParams::with_dimensions(DIMS),
    );
    let overview = engine
        .project_overview(&request_root(&path))
        .await
        .expect("no infra error")
        .expect("indexed worktree");

    assert_eq!(overview.generation.id, generation.to_string());

    // Tree: root totals everything, `src` totals its subtree, nothing deeper
    // than three levels exists.
    let root = overview
        .tree
        .iter()
        .find(|n| n.path.is_empty())
        .expect("root node");
    assert_eq!(root.file_count, 3);
    assert_eq!(root.occurrence_count, 3);
    let src = overview
        .tree
        .iter()
        .find(|n| n.path == "src")
        .expect("src node");
    assert_eq!(src.file_count, 3);
    assert!(
        overview.tree.iter().all(|n| n.depth <= TREE_DEPTH),
        "{:?}",
        overview.tree
    );

    // Entry points: the heuristic fires on main.rs and mod.rs, not on inner.rs.
    assert_eq!(
        overview.entry_points,
        vec!["src/main.rs".to_string(), "src/parser/mod.rs".to_string()]
    );

    // Top imports: by descending count, ties by specifier.
    assert_eq!(overview.top_imports[0].specifier, "serde");
    assert_eq!(overview.top_imports[0].count, 3);
    let rest: Vec<&str> = overview.top_imports[1..]
        .iter()
        .map(|i| i.specifier.as_str())
        .collect();
    assert_eq!(rest, ["std::fmt", "tokio"], "count ties break by specifier");
}

/// The overview is cached per generation: a repeat call returns the very same
/// allocation, and a generation switch produces a different one — the
/// invalidation the card asks for, which needs no explicit invalidation step
/// because the generation is part of the key.
#[tokio::test]
async fn the_overview_is_cached_per_generation() {
    let (_home, layout, state, cache) = open_all();
    let (wt, path) = worktree(&state, 70).await;
    init_projection(&state, &wt, DIMS as u32, DistanceMetric::Dot).await;
    let body = "fn searchable() {}\n";

    let gen_a = allocate_ready(&state, &wt, 71).await;
    seed_file(&state, &gen_a, 72, "src/main.rs", body, "main").await;
    commit_switch(&state, &layout, wt, gen_a).await;
    materialize_fts(&state, &cache, &wt.to_string(), &gen_a.to_string(), NOW)
        .await
        .expect("materialize fts");

    let engine = engine_with(
        &state,
        &cache,
        layout.clone(),
        Arc::new(UnitQueryEmbedder),
        ShardParams::with_dimensions(DIMS),
    );
    let first = engine
        .project_overview(&request_root(&path))
        .await
        .expect("no infra error")
        .expect("indexed");
    let second = engine
        .project_overview(&request_root(&path))
        .await
        .expect("no infra error")
        .expect("indexed");
    assert!(
        Arc::ptr_eq(&first, &second),
        "the second call must be served from the cache, not recomputed"
    );

    // A second generation with a different shape.
    let gen_b = allocate_ready(&state, &wt, 74).await;
    seed_file(&state, &gen_b, 76, "src/main.rs", body, "main").await;
    seed_file(&state, &gen_b, 78, "src/extra.rs", body, "extra").await;
    commit_switch(&state, &layout, wt, gen_b).await;
    materialize_fts(&state, &cache, &wt.to_string(), &gen_b.to_string(), NOW + 1)
        .await
        .expect("materialize fts for B");

    let after_switch = engine
        .project_overview(&request_root(&path))
        .await
        .expect("no infra error")
        .expect("indexed");
    assert!(!Arc::ptr_eq(&first, &after_switch), "a switch re-addresses");
    assert_eq!(after_switch.generation.id, gen_b.to_string());
    assert_eq!(after_switch.generation.number, 2);
    assert_eq!(
        after_switch
            .tree
            .iter()
            .find(|n| n.path.is_empty())
            .expect("root")
            .file_count,
        2,
        "the new generation's shape, not the cached one"
    );
}

/// Both tools reject a request that names no indexed worktree, before touching
/// a generation.
#[tokio::test]
async fn an_unknown_worktree_is_refused_by_both_tools() {
    let (_home, layout, state, cache) = open_all();
    let engine = engine_with(
        &state,
        &cache,
        layout,
        Arc::new(UnitQueryEmbedder),
        ShardParams::with_dimensions(DIMS),
    );
    let root = request_root("/nowhere/at/all");

    let context = engine
        .get_file_context(&root, "src/a.rs")
        .await
        .expect("no infra error")
        .expect_err("unknown worktree");
    assert_eq!(context.code, ErrorCode::WorktreeNotIndexed);

    let overview = engine
        .project_overview(&root)
        .await
        .expect("no infra error")
        .expect_err("unknown worktree");
    assert_eq!(overview.code, ErrorCode::WorktreeNotIndexed);
}
