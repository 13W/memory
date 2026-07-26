//! The end-to-end benchmark run: index → embed → switch → 49 queries
//! (spec 14 §7) — T12-05.
//!
//! Deliberately **not** a test: it needs model weights (~315 MiB), a real corpus
//! checkout, and a `libonnxruntime` the repository does not ship. It is invoked
//! as `cargo xtask bench`, and its *output* — the recorded run under
//! `fixtures/search/baseline/` — is what the committed thresholds are derived
//! from.
//!
//! # Comparability is the whole point
//!
//! The v1 baseline was measured on "project source only" (`node_modules`,
//! `dist`, `.git` excluded — 96 files, 544 chunks). A run over a different file
//! set produces numbers that look like the baseline's but mean something else,
//! which is worse than no numbers at all. [`PRUNED_DIRECTORIES`] therefore
//! mirrors that exclusion, and the report records the file/occurrence counts so
//! a reader can check the corpora matched.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use local_rag_core::config::DataPolicy;
use local_rag_core::identity::domain::path_fingerprint;
use local_rag_core::identity::path::CaseSensitivity;
use local_rag_core::identity::{Uuid, UuidSource, uuidv7_from};
use local_rag_core::paths::StoreLayout;
use local_rag_core::redaction::Scanner;
use local_rag_embed::{BackfillParams, Embedder, InFlight, ProviderEntry, ProviderPool};
use local_rag_index::classify::ClassifierConfig;
use local_rag_index::reconcile::{WorktreeMeta, reconcile_once};
use local_rag_index::scan::{ScanMode, StatCache};
use local_rag_models::{CATALOG, HttpFetcher, ModelCatalogEntry, install_model};
use local_rag_projection::{
    BruteForceProjectionStore, CacheVectorSource, ShardManager, params_for_model_space, shard_dir,
    switch,
};
use local_rag_protocol::SearchMode;
use local_rag_search::{QueryEmbedError, QueryEmbedder, SearchEngine, SearchRequest};
use local_rag_store::{
    CacheDb, DEFAULT_MODEL_SPACE_ID, RepresentationKey, RepresentationKind, RequestRoot,
    RetentionParams, StateDb, WorktreeKind, WorktreeLockRegistry, WorktreeRootFacts,
    create_repository, create_worktree, insert_projection_state, materialize_fts,
    observe_repository_path, observe_worktree_path, register_representation,
    set_model_space_representation,
};

use crate::bench::corpus::Corpus;
use crate::bench::report::{BenchReport, Latency, Provenance, QueryResult};
use crate::bench::score::{Candidate, Metrics, aggregate, rank_of_match};

/// Directories excluded from the indexed corpus, mirroring the v1 baseline's own
/// exclusion (see the module docs).
pub const PRUNED_DIRECTORIES: &[&str] = &["node_modules", "dist", ".git"];

/// The result limit every query runs with — v1 searched with `limit 5`, and the
/// deepest metric is `hit@5`.
pub const QUERY_LIMIT: usize = 5;

/// How many untimed warm-up passes precede the measured ones.
const WARMUP_PASSES: usize = 1;

/// How many timed passes each query gets; the p50/p95 are taken over all of
/// them.
const TIMED_PASSES: usize = 3;

/// A monotone UUIDv7 source: benchmark runs need ids, not entropy.
struct SeqUuids {
    counter: std::sync::atomic::AtomicU64,
}

impl UuidSource for SeqUuids {
    fn next_uuid(&self) -> Uuid {
        let n = self
            .counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        uuidv7_from(9_000_000 + n, [0x5a; 10])
    }
}

/// Adapts an [`Embedder`] to the search engine's [`QueryEmbedder`] seam.
struct PoolQueryEmbedder {
    embedder: Arc<dyn Embedder>,
}

impl QueryEmbedder for PoolQueryEmbedder {
    fn embed_query(
        &self,
        query: &str,
        key: &RepresentationKey,
    ) -> Result<Vec<f32>, QueryEmbedError> {
        if self.embedder.key().model_id != key.model_id {
            return Err(QueryEmbedError::new(format!(
                "provider embeds with {}, the active representation wants {}",
                self.embedder.key().model_id,
                key.model_id
            )));
        }
        let vectors = self
            .embedder
            .embed(local_rag_embed::EmbedRequest {
                kind: RepresentationKind::CodeRaw,
                texts: vec![query.to_string()],
            })
            .map_err(|e| QueryEmbedError::new(e.to_string()))?;
        vectors
            .into_iter()
            .next()
            .map(|v| v.into_inner())
            .ok_or_else(|| QueryEmbedError::new("provider returned no vector"))
    }
}

/// What `cargo xtask bench` was asked to do.
pub struct Options {
    /// The corpus checkout to index.
    pub corpus_dir: PathBuf,
    /// Optional subdirectory of the checkout to index *instead of* the whole
    /// thing — the knob that makes a run comparable with a baseline that
    /// indexed only part of a repository.
    ///
    /// The v1 baseline walked `<root>/src/` and nothing else
    /// (`scripts/benchmark.ts::collectSrcFiles`), so a whole-repo run silently
    /// measures a different corpus — including, in v1's own checkout,
    /// `scripts/benchmark.ts`, the file that holds all 49 query strings as
    /// literals and is therefore a near-perfect lexical match for the wrong
    /// answer.
    pub subdir: Option<String>,
    /// Which legs to run.
    pub mode: SearchMode,
}

/// Run the benchmark end to end.
pub async fn run(options: &Options) -> Result<BenchReport, String> {
    let corpus =
        Corpus::load(&crate::bench::corpus_fixture_path()).map_err(|e| format!("corpus: {e}"))?;

    let home = tempdir()?;
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().map_err(|e| format!("store layout: {e}"))?;

    let model_home = model_home()?;
    let model_layout = StoreLayout::new(model_home.join("local-rag"));
    model_layout
        .ensure()
        .map_err(|e| format!("model layout: {e}"))?;

    // 1. Model assets. Pinned URL + size + sha256 (spec 10 §5), so "fetch a
    //    model" cannot become "fetch arbitrary bytes"; an already-installed
    //    model is a no-op that re-prints nothing.
    let entry = default_entry()?;
    eprintln!(
        "[bench] model {} in {}",
        entry.model_id,
        model_layout.model_dir(entry.model_id).display()
    );
    install_model(
        &model_layout,
        entry,
        &HttpFetcher::default(),
        &mut std::io::stderr(),
    )
    .map_err(|e| format!("install {}: {e}", entry.model_id))?;
    let embedder: Arc<dyn Embedder> = Arc::new(
        local_rag_models::OnnxEmbedder::open(&model_layout, entry)
            .map_err(|e| format!("open {}: {e}", entry.model_id))?,
    );

    let state = Arc::new(StateDb::open(layout.state_db()).map_err(|e| format!("state: {e}"))?);
    let cache =
        Arc::new(CacheDb::open(layout.cache_db(), "bench").map_err(|e| format!("cache: {e}"))?);
    let uuids = Arc::new(SeqUuids {
        counter: std::sync::atomic::AtomicU64::new(0),
    });
    let now_ms = 1_000;

    // 2. Register the worktree and the model space's `code_raw` representation
    //    under the provider's own key, so shard params and query embedding agree
    //    by construction (spec 09 §3).
    let worktree_id = uuids.next_uuid();
    let repo_id = uuids.next_uuid();
    // The worktree root *is* the indexed scope, so restricting the corpus is
    // just rooting the worktree deeper. Result paths become relative to it,
    // which the substring matcher does not care about.
    let checkout = options
        .corpus_dir
        .canonicalize()
        .map_err(|e| format!("corpus dir: {e}"))?;
    let root = match &options.subdir {
        Some(sub) => checkout
            .join(sub)
            .canonicalize()
            .map_err(|e| format!("corpus subdir {sub:?}: {e}"))?,
        None => checkout.clone(),
    };
    register_worktree(&state, &repo_id, &worktree_id, &root, now_ms).await?;
    register_representation_for(&state, embedder.as_ref(), now_ms).await?;

    // 3. Index.
    let indexed_at = Instant::now();
    let meta = WorktreeMeta {
        worktree_id: worktree_id.to_string(),
        root: root.clone(),
        kind: WorktreeKind::Main,
        case: CaseSensitivity::Sensitive,
        prune_roots: PRUNED_DIRECTORIES.iter().map(|d| d.to_string()).collect(),
    };
    let mut stat_cache = StatCache::new();
    let report = reconcile_once(
        &state,
        &meta,
        ScanMode::Strict,
        &mut stat_cache,
        &ClassifierConfig::new(1024 * 1024),
        &Scanner::new(),
        uuids.as_ref(),
        now_ms,
    )
    .await
    .map_err(|e| format!("reconcile: {e:?}"))?;
    let index_ms = indexed_at.elapsed().as_millis() as u64;
    let generation_id: Uuid = report
        .build
        .generation_id
        .parse()
        .map_err(|_| "generation id is not a UUID".to_string())?;
    eprintln!(
        "[bench] indexed {} files, {} occurrences in {index_ms} ms",
        report.build.files_indexed, report.build.occurrences
    );

    // `build_generation` already leaves the generation `projection_ready`
    // (spec 04 §1); transitioning it again would be an illegal self-edge.

    // 4. Embed every occurrence into `embedding_cache`.
    let embedded_at = Instant::now();
    let pool = ProviderPool::new(vec![ProviderEntry::local("onnx", embedder.clone())]);
    let backfill = local_rag_embed::run_backfill(
        &state,
        &cache,
        &pool,
        DataPolicy::LocalOnly,
        DEFAULT_MODEL_SPACE_ID,
        &BackfillParams::default(),
        &RetentionParams {
            keep_last_k: 2,
            window_ms: 7 * 24 * 60 * 60 * 1000,
        },
        &InFlight::new(),
        now_ms,
    )
    .await
    .map_err(|e| format!("backfill: {e}"))?;
    let embed_ms = embedded_at.elapsed().as_millis() as u64;
    eprintln!(
        "[bench] embedded {} subjects in {embed_ms} ms",
        backfill.embedded
    );

    // 5. Project and materialize both views.
    let model_space: Uuid = DEFAULT_MODEL_SPACE_ID
        .parse()
        .map_err(|_| "default model space id".to_string())?;
    let params = {
        let read = state.open_read().map_err(|e| format!("state read: {e}"))?;
        params_for_model_space(&read, &model_space).map_err(|e| format!("shard params: {e}"))?
    };
    {
        let read_state = state.open_read().map_err(|e| format!("state read: {e}"))?;
        let vectors =
            CacheVectorSource::new(&state, &cache, &read_state, &generation_id, &model_space)
                .map_err(|e| format!("vector source: {e}"))?;
        switch(
            &state,
            &BruteForceProjectionStore::new(),
            &shard_dir(&layout, &worktree_id, &model_space),
            params,
            worktree_id,
            generation_id,
            model_space,
            &vectors,
            uuids.as_ref(),
            now_ms,
        )
        .await
        .map_err(|e| format!("switch: {e}"))?;
    }
    materialize_fts(
        &state,
        &cache,
        &worktree_id.to_string(),
        &report.build.generation_id,
        now_ms,
    )
    .await
    .map_err(|e| format!("materialize fts: {e}"))?;

    // 6. Run the 49 queries.
    let engine = SearchEngine::with_embedder(
        state.clone(),
        cache.clone(),
        Arc::new(WorktreeLockRegistry::new()),
        Arc::new(ShardManager::new(
            state.clone(),
            Arc::new(BruteForceProjectionStore::new()),
            layout.clone(),
            params,
            Arc::new(NoVectors),
            uuids.clone(),
            8,
        )),
        Arc::new(PoolQueryEmbedder {
            embedder: embedder.clone(),
        }),
        std::time::Duration::from_secs(30),
    );
    let request_root = request_root(&root);

    let mut per_query = Vec::with_capacity(corpus.queries.len());
    let mut ranks = Vec::with_capacity(corpus.queries.len());
    let mut timings_ms: Vec<f64> = Vec::new();

    for query in &corpus.queries {
        let mut candidates = Vec::new();
        let mut returned = 0usize;
        for pass in 0..(WARMUP_PASSES + TIMED_PASSES) {
            let started = Instant::now();
            let response = engine
                .search_code(
                    SearchRequest {
                        root: request_root.clone(),
                        query: query.query.clone(),
                        mode: options.mode,
                        limit: QUERY_LIMIT,
                        name_pattern: None,
                    },
                    now_ms,
                )
                .await
                .map_err(|e| format!("search {}: {e}", query.id))?
                .map_err(|e| format!("search {}: {}", query.id, e.code))?;
            let elapsed = started.elapsed().as_secs_f64() * 1000.0;
            if pass >= WARMUP_PASSES {
                timings_ms.push(elapsed);
            }
            returned = response.results.len();
            candidates = response
                .results
                .iter()
                .map(|r| Candidate {
                    path: r.path.clone(),
                    name: r.name.clone(),
                })
                .collect();
        }

        let rank = rank_of_match(query, &candidates);
        let matched = rank.and_then(|r| candidates.get(r - 1));
        per_query.push(QueryResult {
            id: query.id.clone(),
            group: query.group.clone(),
            rank,
            matched_path: matched.map(|c| c.path.clone()),
            matched_name: matched.map(|c| c.name.clone()),
            returned,
            v1_rank: None,
        });
        ranks.push(rank);
    }

    let metrics: Metrics = aggregate(&ranks);
    let latency = Latency {
        index_ms,
        embed_ms,
        search_p50_ms: percentile(&mut timings_ms.clone(), 0.50),
        search_p95_ms: percentile(&mut timings_ms.clone(), 0.95),
    };

    Ok(BenchReport::new(
        Provenance {
            v2_commit: git_short_head(Path::new(".")).unwrap_or_else(|| "unknown".to_string()),
            corpus_path: root.display().to_string(),
            // The *checkout*'s commit, not the subdirectory's — `git -C <subdir>`
            // still resolves to the repository, but naming the checkout is what a
            // reader needs to reproduce the run.
            corpus_commit: git_short_head(&checkout).unwrap_or_else(|| "unknown".to_string()),
            corpus_subdir: options.subdir.clone(),
            corpus_version: corpus.version.clone(),
            model_id: entry.model_id.to_string(),
            mode: options.mode.as_str().to_string(),
            files_indexed: report.build.files_indexed,
            occurrences: report.build.occurrences,
            host: std::env::consts::ARCH.to_string() + "-" + std::env::consts::OS,
        },
        metrics,
        per_query,
        latency,
    ))
}

/// A [`local_rag_projection::VectorSource`] that supplies nothing.
///
/// The `ShardManager`'s rebuild-on-acquire path needs one, but a benchmark run
/// has just built the shard from `embedding_cache`; if it were ever corrupt, a
/// silent rebuild from thin air would be worse than a visible failure.
struct NoVectors;

impl local_rag_projection::VectorSource for NoVectors {
    fn vector(
        &self,
        _occurrence_id: &str,
        _kind: local_rag_projection::RepresentationKind,
    ) -> Option<Vec<f32>> {
        None
    }
}

fn default_entry() -> Result<&'static ModelCatalogEntry, String> {
    CATALOG
        .iter()
        .find(|e| e.model_id == local_rag_models::DEFAULT_MODEL_ID)
        .ok_or_else(|| "default model missing from the catalog".to_string())
}

async fn register_worktree(
    state: &StateDb,
    repo_id: &Uuid,
    worktree_id: &Uuid,
    root: &Path,
    now_ms: i64,
) -> Result<(), String> {
    let (r, w, p) = (
        repo_id.to_string(),
        worktree_id.to_string(),
        root.display().to_string(),
    );
    let fp = path_fingerprint(&p);
    state
        .writer()
        .transaction(move |tx| {
            create_repository(tx, &r, None, now_ms)?;
            create_worktree(tx, &w, &r, WorktreeKind::Main, now_ms)?;
            observe_worktree_path(tx, &w, &p, &p, &fp, now_ms)?;
            observe_repository_path(tx, &r, &p, now_ms)?;
            insert_projection_state(tx, &w, now_ms)
        })
        .await
        .map_err(|e| format!("register worktree: {e}"))
}

async fn register_representation_for(
    state: &StateDb,
    embedder: &dyn Embedder,
    now_ms: i64,
) -> Result<(), String> {
    let key = embedder.key();
    state
        .writer()
        .transaction(move |tx| {
            let id = register_representation(tx, "bench-code-raw", &key, now_ms)?;
            set_model_space_representation(
                tx,
                DEFAULT_MODEL_SPACE_ID,
                RepresentationKind::CodeRaw,
                &id,
                true,
                now_ms,
            )
        })
        .await
        .map_err(|e| format!("register representation: {e}"))
}

fn request_root(root: &Path) -> RequestRoot {
    let path = root.display().to_string();
    RequestRoot {
        worktree_root: Some(WorktreeRootFacts {
            observed_canonical_path: path.clone(),
            display_path: path.clone(),
            path_fingerprint: path_fingerprint(&path),
            kind: WorktreeKind::Main,
            common_dir_fingerprint: None,
            remote_fingerprint: None,
        }),
        repo_hint: None,
    }
}

/// Nearest-rank percentile over already-collected samples.
fn percentile(samples: &mut [f64], q: f64) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = ((samples.len() as f64) * q).ceil() as usize;
    samples[idx.saturating_sub(1).min(samples.len() - 1)]
}

fn git_short_head(dir: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .args([
            "-C",
            &dir.display().to_string(),
            "rev-parse",
            "--short",
            "HEAD",
        ])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Where model weights are kept **between** runs.
///
/// The store is disposable — a fresh index every run is the point — but the
/// weights are not: re-downloading ~315 MiB per run would make iterating on the
/// benchmark absurd.
fn model_home() -> Result<PathBuf, String> {
    if let Some(explicit) = std::env::var_os("LOCAL_RAG_BENCH_MODEL_HOME") {
        return Ok(PathBuf::from(explicit));
    }
    let home = std::env::var_os("HOME").ok_or_else(|| "HOME is unset".to_string())?;
    Ok(PathBuf::from(home).join(".local/share/local-rag-bench"))
}

fn tempdir() -> Result<PathBuf, String> {
    let base = std::env::temp_dir().join(format!("local-rag-bench-{}", std::process::id()));
    // A previous run under the same pid must never be mistaken for this one's.
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).map_err(|e| format!("temp dir: {e}"))?;
    Ok(base)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exclusion list is what keeps a run comparable with the recorded v1
    /// baseline ("project source only").
    #[test]
    fn the_baselines_exclusions_are_mirrored() {
        assert!(PRUNED_DIRECTORIES.contains(&"node_modules"));
        assert!(PRUNED_DIRECTORIES.contains(&"dist"));
        assert!(PRUNED_DIRECTORIES.contains(&".git"));
    }

    #[test]
    fn queries_run_at_the_baselines_limit() {
        assert_eq!(QUERY_LIMIT, 5, "v1 searched with limit 5; hit@5 is deepest");
    }

    #[test]
    fn percentile_is_nearest_rank_and_total() {
        let mut one = [7.0];
        assert_eq!(percentile(&mut one, 0.5), 7.0);
        assert_eq!(percentile(&mut one, 0.95), 7.0);

        let mut ten: Vec<f64> = (1..=10).map(|n| n as f64).collect();
        assert_eq!(percentile(&mut ten, 0.50), 5.0);
        assert_eq!(percentile(&mut ten, 0.95), 10.0);

        assert_eq!(percentile(&mut [], 0.5), 0.0, "empty is 0, never NaN");
    }
}
