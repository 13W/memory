//! Runs the memory-recall benchmark end to end (spec 08 §6) — X-010.
//!
//! Deliberately not a test: it needs the installed ONNX weights
//! (~315 MiB, the same catalog entry `crate::bench` already uses) and a
//! `libonnxruntime` the repository does not ship. It is invoked as
//! `cargo xtask memory-recall-bench`. The plumbing itself (seed → recall →
//! score) is covered by this module's own `#[cfg(test)]` tests, which use
//! `UnavailableEmbedder` and a tiny synthetic corpus instead — no model, no
//! network, no heavy asset, exercising exactly the parts a corrupted-fixture
//! or wrong-representation bug would break.
//!
//! # No worktree, no code index
//!
//! Unlike `crate::bench::run`, this run never calls `reconcile_once` or
//! registers a worktree at all: every corpus entry is seeded
//! `scope_kind = Global` (mirroring `crate::memory_bench::run`'s own
//! "every seed is global" choice, and for the identical reason — this
//! benchmark measures recall quality, not scope-owner resolution, which
//! `local_rag_memory::guard`'s own tests already cover), so every query
//! resolves via [`local_rag_store::RequestRoot`]'s `GlobalOnly` branch. No
//! worktree also means no `code_raw`/`code_context` representation is ever
//! registered for the throwaway model space — only `memory` is, so
//! `run_backfill` embeds exactly the corpus's memory entries and nothing
//! else (see `crates/embed/tests/backfill.rs`'s
//! `a_memory_only_run_embeds_every_memory_entry`, the same shape this
//! function drives for real).
//!
//! # Only the `*_original` text variant (X-010)
//!
//! This task measures the as-is pipeline only — every entry is seeded from
//! `Entry::text_original` and every query runs `Query::query_original`.
//! Comparing against the hand-translated `*_english` variants is X-011's
//! job, not this module's.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use local_rag_core::config::DataPolicy;
use local_rag_core::identity::{SystemUuidV7, UuidSource};
use local_rag_core::paths::StoreLayout;
use local_rag_embed::{
    BackfillParams, EmbedRequest, Embedder, InFlight, ProviderEntry, ProviderPool, run_backfill,
};
use local_rag_memory::recall;
use local_rag_models::{
    DEFAULT_MODEL_ID, HttpFetcher, ModelCatalogEntry, OnnxEmbedder, find, install_model,
};
use local_rag_store::{
    CacheDb, CreateMemoryEntryError, DEFAULT_MODEL_SPACE_ID, GLOBAL_SCOPE_OWNER_ID, MemoryKind,
    NewMemoryEntry, RepresentationKey, RequestRoot, RetentionParams, ScopeKind, StateDb,
    create_memory_entry, register_representation, set_model_space_representation,
};

use crate::git::git_short_head;
use crate::memory_recall_bench::corpus::{Corpus, Entry};
use crate::memory_recall_bench::report::{
    Latency, MemoryRecallBenchReport, Provenance, QueryResult,
};
use crate::memory_recall_bench::score::{aggregate, aggregate_by_lang_pair, rank_of_match};
use crate::stats::percentile;

/// The result depth every query is scored at — matches the deepest declared
/// metric (`hit@5`), and mirrors `crate::bench::run::QUERY_LIMIT`'s identical
/// reasoning: a miss beyond this depth contributes `0`, not partial credit.
pub const QUERY_LIMIT: usize = 5;

/// Generous enough that the token budget never truncates the ranked list
/// before [`QUERY_LIMIT`] — this corpus's 24 entries, at a few hundred
/// characters each, sit far under it even all at once. The point of this run
/// is measuring rank quality, not budget behavior (that is
/// `recall`'s own `zero_token_budget_yields_empty_additional_context`-style
/// test's job).
const RECALL_TOKEN_BUDGET: u32 = 100_000;

const WARMUP_PASSES: usize = 1;
const TIMED_PASSES: usize = 3;

/// What `cargo xtask memory-recall-bench` was asked to do.
pub struct Options {
    pub corpus_path: PathBuf,
    /// A catalog `model_id` to run instead of [`local_rag_models::DEFAULT_MODEL_ID`].
    pub model_id: Option<String>,
}

/// Adapts a batch [`Embedder`] to `recall`'s single-query [`recall::QueryEmbedder`]
/// seam — the memory-side twin of `crate::bench::run::PoolQueryEmbedder`,
/// targeting the crate-local `local_rag_memory::recall` trait instead of
/// `local_rag_search`'s (the two are structurally identical but nominally
/// distinct — see `crates/memory/src/recall/dense.rs`'s own module doc on why).
struct MemoryPoolQueryEmbedder {
    embedder: Arc<dyn Embedder>,
}

impl recall::QueryEmbedder for MemoryPoolQueryEmbedder {
    fn embed_query(
        &self,
        query: &str,
        key: &RepresentationKey,
    ) -> Result<Vec<f32>, recall::QueryEmbedError> {
        if self.embedder.key().model_id != key.model_id {
            return Err(recall::QueryEmbedError {
                reason: format!(
                    "provider embeds with {}, the active representation wants {}",
                    self.embedder.key().model_id,
                    key.model_id
                ),
            });
        }
        let vectors = self
            .embedder
            .embed(EmbedRequest::new(key.kind, vec![query.to_string()]))
            .map_err(|e| recall::QueryEmbedError {
                reason: e.to_string(),
            })?;
        vectors
            .into_iter()
            .next()
            .map(|v| v.into_inner())
            .ok_or_else(|| recall::QueryEmbedError {
                reason: "provider returned no vector".to_string(),
            })
    }
}

pub async fn run(options: &Options) -> Result<MemoryRecallBenchReport, String> {
    let corpus = Corpus::load(&options.corpus_path).map_err(|e| format!("corpus: {e}"))?;

    let home = tempdir()?;
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().map_err(|e| format!("store layout: {e}"))?;

    let model_home = model_home()?;
    let model_layout = StoreLayout::new(model_home.join("local-rag"));
    model_layout
        .ensure()
        .map_err(|e| format!("model layout: {e}"))?;

    let entry = default_entry(options.model_id.as_deref())?;
    eprintln!(
        "[memory-recall-bench] model {} in {}",
        entry.model_id,
        model_layout.model_dir(entry.model_id).display()
    );
    let install_start = Instant::now();
    install_model(
        &model_layout,
        entry,
        &HttpFetcher::default(),
        &mut std::io::stderr(),
    )
    .map_err(|e| format!("install {}: {e}", entry.model_id))?;
    let install_ms = install_start.elapsed().as_millis() as u64;

    let embedder: Arc<dyn Embedder> = Arc::new(
        OnnxEmbedder::open_for_memory(&model_layout, entry)
            .map_err(|e| format!("open {} (memory representation): {e}", entry.model_id))?,
    );

    let state = StateDb::open(layout.state_db()).map_err(|e| format!("state: {e}"))?;
    let cache = CacheDb::open(layout.cache_db(), "memory-recall-bench")
        .map_err(|e| format!("cache: {e}"))?;
    let uuids = SystemUuidV7;
    let now_ms = 1_000;

    register_memory_representation(&state, embedder.as_ref(), now_ms).await?;

    let mut memory_id_by_corpus_id: BTreeMap<String, String> = BTreeMap::new();
    for corpus_entry in &corpus.entries {
        let memory_id = uuids.next_uuid().to_string();
        seed_entry(&state, &memory_id, corpus_entry, now_ms).await?;
        memory_id_by_corpus_id.insert(corpus_entry.id.clone(), memory_id);
    }
    let corpus_id_by_memory_id: BTreeMap<String, String> = memory_id_by_corpus_id
        .iter()
        .map(|(corpus_id, memory_id)| (memory_id.clone(), corpus_id.clone()))
        .collect();

    let embedded_at = Instant::now();
    let pool = ProviderPool::new(vec![ProviderEntry::local("memory", embedder.clone())]);
    let backfill = run_backfill(
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
        "[memory-recall-bench] embedded {} subjects in {embed_ms} ms",
        backfill.embedded
    );

    let state_read = state.open_read().map_err(|e| format!("state read: {e}"))?;
    let cache_read = cache.open_read().map_err(|e| format!("cache read: {e}"))?;
    let query_embedder = MemoryPoolQueryEmbedder {
        embedder: embedder.clone(),
    };

    let mut per_query = Vec::with_capacity(corpus.queries.len());
    let mut ranks: Vec<Option<usize>> = Vec::with_capacity(corpus.queries.len());
    let mut ranks_by_pair: Vec<(String, Option<usize>)> = Vec::with_capacity(corpus.queries.len());
    let mut timings_ms: Vec<f64> = Vec::new();

    for query in &corpus.queries {
        let expected_memory_id = memory_id_by_corpus_id
            .get(&query.expected_entry_id)
            .ok_or_else(|| format!("{}: unknown expected entry id", query.id))?;
        let request = recall::RecallRequest {
            root: RequestRoot {
                worktree_root: None,
                repo_hint: None,
            },
            query: &query.query_original,
        };

        let mut ranked_ids: Vec<String> = Vec::new();
        let mut returned = 0usize;
        for pass in 0..(WARMUP_PASSES + TIMED_PASSES) {
            let started = Instant::now();
            let outcome = recall::recall(
                &state_read,
                &cache_read,
                &query_embedder,
                &recall::BruteForceCosine,
                &request,
                RECALL_TOKEN_BUDGET,
            )
            .map_err(|e| format!("recall {}: {e}", query.id))?;
            let elapsed = started.elapsed().as_secs_f64() * 1000.0;
            if pass >= WARMUP_PASSES {
                timings_ms.push(elapsed);
            }
            returned = outcome.entries.len();
            ranked_ids = outcome
                .entries
                .iter()
                .take(QUERY_LIMIT)
                .map(|e| e.memory_id.clone())
                .collect();
        }

        let rank = rank_of_match(expected_memory_id, &ranked_ids);
        let top_result_id = ranked_ids
            .first()
            .and_then(|id| corpus_id_by_memory_id.get(id))
            .cloned();

        per_query.push(QueryResult {
            id: query.id.clone(),
            lang_pair: query.lang_pair.clone(),
            expected_entry_id: query.expected_entry_id.clone(),
            rank,
            top_result_id,
            returned,
        });
        ranks.push(rank);
        ranks_by_pair.push((query.lang_pair.clone(), rank));
    }

    let metrics = aggregate(&ranks);
    let metrics_by_lang_pair = aggregate_by_lang_pair(&ranks_by_pair);
    let latency = Latency {
        install_ms,
        embed_ms,
        recall_p50_ms: percentile(&mut timings_ms.clone(), 0.50),
        recall_p95_ms: percentile(&mut timings_ms.clone(), 0.95),
    };

    let provenance = Provenance {
        v2_commit: git_short_head(std::path::Path::new("."))
            .unwrap_or_else(|| "unknown".to_string()),
        corpus_path: options.corpus_path.display().to_string(),
        corpus_version: corpus.version.clone(),
        model_id: entry.model_id.to_string(),
        config: "baseline".to_string(),
        entry_count: corpus.entries.len(),
        query_count: corpus.queries.len(),
        host: std::env::consts::ARCH.to_string() + "-" + std::env::consts::OS,
    };

    Ok(MemoryRecallBenchReport::new(
        provenance,
        metrics,
        metrics_by_lang_pair,
        per_query,
        latency,
    ))
}

async fn register_memory_representation(
    state: &StateDb,
    embedder: &dyn Embedder,
    now_ms: i64,
) -> Result<(), String> {
    let key = embedder.key();
    state
        .writer()
        .transaction(move |tx| {
            let id = register_representation(tx, "memory-recall-bench", &key, now_ms)?;
            set_model_space_representation(tx, DEFAULT_MODEL_SPACE_ID, key.kind, &id, true, now_ms)
        })
        .await
        .map_err(|e| format!("register representation: {e}"))
}

/// Seed one corpus entry as a `scope_kind = Global` memory entry — see the
/// module doc's "no worktree, no code index" note for why global is the only
/// scope this benchmark ever uses.
async fn seed_entry(
    state: &StateDb,
    memory_id: &str,
    entry: &Entry,
    now_ms: i64,
) -> Result<(), String> {
    let kind = MemoryKind::from_db(&entry.kind)
        .ok_or_else(|| format!("{}: unknown kind {:?}", entry.id, entry.kind))?;
    let (id, text, entry_id) = (
        memory_id.to_string(),
        entry.text_original.clone(),
        entry.id.clone(),
    );
    state
        .writer()
        .transaction(move |tx| {
            create_memory_entry(
                tx,
                &NewMemoryEntry {
                    memory_id: &id,
                    kind,
                    text: &text,
                    canonical_key: None,
                    scope_kind: ScopeKind::Global,
                    scope_owner_id: GLOBAL_SCOPE_OWNER_ID,
                    confidence: 0.7,
                    importance: 0.5,
                    valid_from_tree: None,
                    last_verified_tree: None,
                    supersedes_id: None,
                },
                now_ms,
            )
        })
        .await
        .map_err(|e| format!("{entry_id}: seeding tx: {e}"))?
        .map_err(|e: CreateMemoryEntryError| format!("{entry_id}: seeding domain: {e}"))?;
    Ok(())
}

fn default_entry(model_id: Option<&str>) -> Result<&'static ModelCatalogEntry, String> {
    let model_id = model_id.unwrap_or(DEFAULT_MODEL_ID);
    find(model_id).ok_or_else(|| format!("{model_id:?} is not in the embedding catalog"))
}

/// Where model weights are kept **between** runs — reuses `crate::bench::run`'s
/// own `LOCAL_RAG_BENCH_MODEL_HOME` env var and cache root (same reasoning
/// `crate::memory_bench::run::model_home` already gives for its own GGUF
/// weights): `StoreLayout::model_dir` namespaces by `model_id`, so this run's
/// ONNX session shares the root `cargo xtask bench` already populated,
/// without a second ~315 MiB download.
fn model_home() -> Result<PathBuf, String> {
    if let Some(explicit) = std::env::var_os("LOCAL_RAG_BENCH_MODEL_HOME") {
        return Ok(PathBuf::from(explicit));
    }
    let home = std::env::var_os("HOME").ok_or_else(|| "HOME is unset".to_string())?;
    Ok(PathBuf::from(home).join(".local/share/local-rag-bench"))
}

fn tempdir() -> Result<PathBuf, String> {
    let base = std::env::temp_dir().join(format!(
        "local-rag-memory-recall-bench-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).map_err(|e| format!("temp dir: {e}"))?;
    Ok(base)
}

#[cfg(test)]
mod tests {
    use super::*;
    use local_rag_store::CacheDb;
    use local_rag_test_support::TempHome;

    fn entry(id: &str, kind: &str, text: &str) -> Entry {
        Entry {
            id: id.to_string(),
            kind: kind.to_string(),
            text_original: text.to_string(),
            text_english: text.to_string(),
        }
    }

    /// Proves this module's own wiring (seed → recall → rank_of_match) end to
    /// end, without a real ONNX session: `UnavailableEmbedder` degrades the
    /// dense leg cleanly (as `recall`'s own tests already establish), leaving
    /// the lexical leg to do the matching — exactly the seam a corrupted
    /// fixture or a wrong `scope_kind`/representation-registration bug in
    /// this module would break.
    #[tokio::test]
    async fn seeded_entries_are_recallable_and_rank_correctly() {
        let home = TempHome::new().expect("temp home");
        let layout = StoreLayout::new(home.join("local-rag"));
        layout.ensure().expect("ensure store tree");
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        let cache = CacheDb::open(layout.cache_db(), "test").expect("open cache.sqlite");
        let uuids = SystemUuidV7;
        let now_ms = 1_000;

        let entries = vec![
            entry(
                "mr-a",
                "fact",
                "the checkout service retries failed payments three times before giving up",
            ),
            entry(
                "mr-b",
                "decision",
                "the team moved the notification queue off rabbitmq onto nats jetstream",
            ),
            entry(
                "mr-c",
                "convention",
                "every schema migration needs a reversible down step before merge",
            ),
        ];
        let mut memory_id_by_corpus_id = BTreeMap::new();
        for e in &entries {
            let memory_id = uuids.next_uuid().to_string();
            seed_entry(&state, &memory_id, e, now_ms)
                .await
                .expect("seed");
            memory_id_by_corpus_id.insert(e.id.clone(), memory_id);
        }

        let state_read = state.open_read().expect("state read");
        let cache_read = cache.open_read().expect("cache read");
        let request = recall::RecallRequest {
            root: RequestRoot {
                worktree_root: None,
                repo_hint: None,
            },
            query: "notification queue rabbitmq",
        };
        let outcome = recall::recall(
            &state_read,
            &cache_read,
            &recall::UnavailableEmbedder,
            &recall::BruteForceCosine,
            &request,
            RECALL_TOKEN_BUDGET,
        )
        .expect("recall");

        let ranked_ids: Vec<String> = outcome
            .entries
            .iter()
            .take(QUERY_LIMIT)
            .map(|e| e.memory_id.clone())
            .collect();
        let expected = &memory_id_by_corpus_id["mr-b"];
        assert_eq!(
            rank_of_match(expected, &ranked_ids),
            Some(1),
            "the lexical leg alone should surface the term-overlapping entry first"
        );
    }

    #[tokio::test]
    async fn seeding_rejects_an_unknown_memory_kind() {
        let home = TempHome::new().expect("temp home");
        let layout = StoreLayout::new(home.join("local-rag"));
        layout.ensure().expect("ensure store tree");
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        let bad = entry("mr-x", "not-a-real-kind", "text");
        let err = seed_entry(&state, "some-id", &bad, 1_000)
            .await
            .expect_err("an unknown kind must be refused, not silently dropped");
        assert!(err.contains("unknown kind"), "{err}");
    }
}
