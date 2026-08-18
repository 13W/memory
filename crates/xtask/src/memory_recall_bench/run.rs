//! Runs the memory-recall benchmark end to end (spec 08 §6) — X-010/X-011.
//!
//! Deliberately not a test: it needs the installed ONNX weights
//! (~315 MiB, the same catalog entry `crate::bench` already uses) and a
//! `libonnxruntime` the repository does not ship. It is invoked as
//! `cargo xtask memory-recall-bench`. The plumbing itself (seed → recall →
//! score, and — since X-011 — which text variant a [`Config`] selects) is
//! covered by this module's own `#[cfg(test)]` tests, which use
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
//! # Four configurations (X-011), one embedder session
//!
//! [`Config`] picks which fixture text field feeds the store and which feeds
//! the query — `baseline` is what a real user experiences today (nothing
//! translated); `store_en`/`query_en` isolate one side each; `both_en` is
//! the full v1-style shape (English-only storage, query translated before
//! search). Nothing here calls a translator at runtime: both variants are
//! already sitting in the fixture, hand-translated once at authoring time
//! (X-010's corpus doc). Each configuration gets its **own** fresh store and
//! its own `run_backfill` pass — `store_en` and `baseline` genuinely embed
//! different text, so their `embedding_cache` rows cannot be shared; only
//! the already-open ONNX session ([`Embedder`]) is reused across
//! configurations, since embedding itself does not depend on which
//! configuration is asking.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use local_rag_core::config::DataPolicy;
use local_rag_core::identity::{Uuid, UuidSource, uuidv7_from};
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
use crate::memory_recall_bench::corpus::{Corpus, Entry, Query};
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

/// A seeded, deterministic [`UuidSource`] (D-068).
///
/// The harness used to mint `memory_id`s with [`local_rag_core::identity::
/// SystemUuidV7`], whose entropy comes from the OS CSPRNG. Spec 08 §6 breaks a
/// score tie by `created_at desc` and then by `memory_id`, so with every entry
/// seeded at one instant the last resort decided real rankings — and decided
/// them differently on every run. Two runs of the *same* configuration were
/// therefore not byte-comparable, and two different configurations could
/// diverge on a query whose inputs were identical (which is exactly what
/// `store_en` and `both_en` did on `mrq-13`, the `en-en` pair where both the
/// stored text and the query are the same string in either configuration).
/// A benchmark whose output moves without its input is not evidence, so the
/// source is seeded here and `created_at` is made distinct at the call site.
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

/// Which fixture text variant feeds the store and which feeds the query
/// (X-011). See the module doc's "four configurations" section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Config {
    /// Store and query both use `*_original` — today's real, as-is pipeline.
    Baseline,
    /// Store uses `*_english`, query stays `*_original`.
    StoreEn,
    /// Store stays `*_original`, query uses `*_english`.
    QueryEn,
    /// Store and query both use `*_english` — the v1-style shape (English-only
    /// storage, translated query).
    BothEn,
}

impl Config {
    /// Every configuration, `Baseline` first — `report::compare` looks for
    /// `Baseline` by name, so any caller sweeping "all" gets a comparable
    /// baseline in the set by construction.
    pub const ALL: [Config; 4] = [
        Config::Baseline,
        Config::StoreEn,
        Config::QueryEn,
        Config::BothEn,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Config::Baseline => "baseline",
            Config::StoreEn => "store_en",
            Config::QueryEn => "query_en",
            Config::BothEn => "both_en",
        }
    }

    pub fn from_str(s: &str) -> Option<Config> {
        match s {
            "baseline" => Some(Config::Baseline),
            "store_en" => Some(Config::StoreEn),
            "query_en" => Some(Config::QueryEn),
            "both_en" => Some(Config::BothEn),
            _ => None,
        }
    }

    /// Which of `entry`'s two text fields this configuration seeds the store
    /// with.
    fn store_text(self, entry: &Entry) -> &str {
        match self {
            Config::Baseline | Config::QueryEn => &entry.text_original,
            Config::StoreEn | Config::BothEn => &entry.text_english,
        }
    }

    /// Which of `query`'s two text fields this configuration searches with.
    fn query_text(self, query: &Query) -> &str {
        match self {
            Config::Baseline | Config::StoreEn => &query.query_original,
            Config::QueryEn | Config::BothEn => &query.query_english,
        }
    }
}

/// What `cargo xtask memory-recall-bench` was asked to do.
pub struct Options {
    pub corpus_path: PathBuf,
    /// A catalog `model_id` to run instead of [`local_rag_models::DEFAULT_MODEL_ID`].
    pub model_id: Option<String>,
    /// Which configurations to run, in order. Empty means `[Config::Baseline]`
    /// — X-010's original, single-configuration behavior.
    pub configs: Vec<Config>,
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

/// Run every requested configuration and return one report per configuration,
/// in the order given (`[Config::Baseline]` if `options.configs` is empty).
pub async fn run(options: &Options) -> Result<Vec<MemoryRecallBenchReport>, String> {
    let corpus = Corpus::load(&options.corpus_path).map_err(|e| format!("corpus: {e}"))?;

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

    let configs = if options.configs.is_empty() {
        vec![Config::Baseline]
    } else {
        options.configs.clone()
    };

    let mut reports = Vec::with_capacity(configs.len());
    for config in configs {
        eprintln!("[memory-recall-bench] running config={}", config.as_str());
        reports.push(
            run_one_config(
                &corpus,
                entry,
                embedder.clone(),
                config,
                install_ms,
                &options.corpus_path,
            )
            .await?,
        );
    }
    Ok(reports)
}

/// One configuration's full run: fresh store, seed with `config`'s chosen
/// store text, embed, then every query against `config`'s chosen query text.
async fn run_one_config(
    corpus: &Corpus,
    entry: &'static ModelCatalogEntry,
    embedder: Arc<dyn Embedder>,
    config: Config,
    install_ms: u64,
    corpus_path: &std::path::Path,
) -> Result<MemoryRecallBenchReport, String> {
    let home = tempdir()?;
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().map_err(|e| format!("store layout: {e}"))?;

    let state = StateDb::open(layout.state_db()).map_err(|e| format!("state: {e}"))?;
    let cache = CacheDb::open(layout.cache_db(), "memory-recall-bench")
        .map_err(|e| format!("cache: {e}"))?;
    let uuids = SeqUuidV7::new();
    let now_ms = 1_000;

    register_memory_representation(&state, embedder.as_ref(), now_ms).await?;

    let mut memory_id_by_corpus_id: BTreeMap<String, String> = BTreeMap::new();
    for (i, corpus_entry) in corpus.entries.iter().enumerate() {
        let memory_id = uuids.next_uuid().to_string();
        // D-068: one distinct `created_at` per entry, in corpus order. Spec
        // 08 §6's final ordering is `(score desc, created_at desc, memory_id)`,
        // so seeding every entry at the same instant pushed every tie down to
        // `memory_id` — which, minted from the clock and the OS CSPRNG, made
        // the run's own output depend on entropy. Distinct timestamps let the
        // documented tie-break do the deciding, and the seeded source below
        // makes even the last resort reproducible.
        let created_at = now_ms + i as i64;
        seed_entry(
            &state,
            &memory_id,
            corpus_entry,
            config.store_text(corpus_entry),
            created_at,
        )
        .await?;
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
        "[memory-recall-bench]   embedded {} subjects in {embed_ms} ms",
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
            query: config.query_text(query),
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
        corpus_path: corpus_path.display().to_string(),
        corpus_version: corpus.version.clone(),
        model_id: entry.model_id.to_string(),
        config: config.as_str().to_string(),
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

/// Seed one corpus entry as a `scope_kind = Global` memory entry, using
/// `text` (the caller's [`Config`]-selected variant) — see the module doc's
/// "no worktree, no code index" note for why global is the only scope this
/// benchmark ever uses.
async fn seed_entry(
    state: &StateDb,
    memory_id: &str,
    entry: &Entry,
    text: &str,
    now_ms: i64,
) -> Result<(), String> {
    let kind = MemoryKind::from_db(&entry.kind)
        .ok_or_else(|| format!("{}: unknown kind {:?}", entry.id, entry.kind))?;
    let (id, text, entry_id) = (memory_id.to_string(), text.to_string(), entry.id.clone());
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

/// Recreated fresh on every call (`remove_dir_all` then `create_dir_all`),
/// so [`run`]'s sequential per-[`Config`] loop never leaks state between
/// configurations despite reusing one directory name across calls.
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
    use crate::memory_recall_bench::corpus::Relevance;
    use local_rag_store::CacheDb;
    use local_rag_test_support::TempHome;

    fn entry(id: &str, kind: &str, text_original: &str, text_english: &str) -> Entry {
        Entry {
            id: id.to_string(),
            kind: kind.to_string(),
            text_original: text_original.to_string(),
            text_english: text_english.to_string(),
        }
    }

    fn query(
        id: &str,
        expected_entry_id: &str,
        query_original: &str,
        query_english: &str,
    ) -> Query {
        Query {
            id: id.to_string(),
            lang_pair: "ru-en".to_string(),
            expected_entry_id: expected_entry_id.to_string(),
            query_original: query_original.to_string(),
            query_english: query_english.to_string(),
        }
    }

    /// Pins the selection table itself — the one thing a copy-paste error in
    /// [`Config::store_text`]/[`Config::query_text`] would silently invert.
    #[test]
    fn each_config_selects_the_documented_text_variant() {
        let e = entry("mr-x", "fact", "RU_STORE", "EN_STORE");
        let q = query("mrq-x", "mr-x", "RU_QUERY", "EN_QUERY");

        assert_eq!(Config::Baseline.store_text(&e), "RU_STORE");
        assert_eq!(Config::Baseline.query_text(&q), "RU_QUERY");

        assert_eq!(Config::StoreEn.store_text(&e), "EN_STORE");
        assert_eq!(Config::StoreEn.query_text(&q), "RU_QUERY");

        assert_eq!(Config::QueryEn.store_text(&e), "RU_STORE");
        assert_eq!(Config::QueryEn.query_text(&q), "EN_QUERY");

        assert_eq!(Config::BothEn.store_text(&e), "EN_STORE");
        assert_eq!(Config::BothEn.query_text(&q), "EN_QUERY");
    }

    #[test]
    fn config_as_str_and_from_str_round_trip() {
        for config in Config::ALL {
            assert_eq!(Config::from_str(config.as_str()), Some(config));
        }
        assert_eq!(Config::from_str("not-a-real-config"), None);
    }

    #[test]
    fn all_starts_with_baseline() {
        assert_eq!(Config::ALL[0], Config::Baseline);
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
        let uuids = SeqUuidV7::new();
        let now_ms = 1_000;

        let entries = vec![
            entry(
                "mr-a",
                "fact",
                "the checkout service retries failed payments three times before giving up",
                "the checkout service retries failed payments three times before giving up",
            ),
            entry(
                "mr-b",
                "decision",
                "the team moved the notification queue off rabbitmq onto nats jetstream",
                "the team moved the notification queue off rabbitmq onto nats jetstream",
            ),
            entry(
                "mr-c",
                "convention",
                "every schema migration needs a reversible down step before merge",
                "every schema migration needs a reversible down step before merge",
            ),
        ];
        let mut memory_id_by_corpus_id = BTreeMap::new();
        for e in &entries {
            let memory_id = uuids.next_uuid().to_string();
            seed_entry(&state, &memory_id, e, &e.text_original, now_ms)
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
        let bad = entry("mr-x", "not-a-real-kind", "text", "text");
        let err = seed_entry(&state, "some-id", &bad, "text", 1_000)
            .await
            .expect_err("an unknown kind must be refused, not silently dropped");
        assert!(err.contains("unknown kind"), "{err}");
    }

    /// X-011's actual point: seeding under [`Config::StoreEn`] must persist
    /// the *English* variant, not silently fall back to the original —
    /// proven end to end (not just via the pure table test above) by seeding
    /// a Russian-only original whose English translation shares no lexical
    /// tokens with it, then showing an English-only query only matches once
    /// the English text is what actually got stored.
    #[tokio::test]
    async fn store_en_seeds_the_english_variant_not_the_original() {
        let home = TempHome::new().expect("temp home");
        let layout = StoreLayout::new(home.join("local-rag"));
        layout.ensure().expect("ensure store tree");
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        let cache = CacheDb::open(layout.cache_db(), "test").expect("open cache.sqlite");
        let uuids = SeqUuidV7::new();
        let now_ms = 1_000;

        let bilingual = entry(
            "mr-b",
            "decision",
            "команда перевела очередь уведомлений с rabbitmq на nats jetstream",
            "the team moved the notification queue off rabbitmq onto nats jetstream",
        );
        let memory_id = uuids.next_uuid().to_string();
        seed_entry(
            &state,
            &memory_id,
            &bilingual,
            Config::StoreEn.store_text(&bilingual),
            now_ms,
        )
        .await
        .expect("seed under store_en");

        let state_read = state.open_read().expect("state read");
        let cache_read = cache.open_read().expect("cache read");
        let english_query_request = recall::RecallRequest {
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
            &english_query_request,
            RECALL_TOKEN_BUDGET,
        )
        .expect("recall");
        let ranked_ids: Vec<String> = outcome
            .entries
            .iter()
            .map(|e| e.memory_id.clone())
            .collect();
        assert_eq!(
            rank_of_match(&memory_id, &ranked_ids),
            Some(1),
            "the English query must match the stored text — it only can if store_en actually \
             persisted the English variant, since the Russian original shares no tokens with it"
        );
    }

    /// D-068. Two runs of one configuration, same corpus, same embedder, must
    /// produce the same report — the property the harness lacked, and without
    /// which no two configurations can be honestly compared.
    #[tokio::test]
    async fn two_runs_of_one_config_agree_on_every_query() {
        let corpus = Corpus {
            family: "memory-recall".to_string(),
            version: "test".to_string(),
            relevance: Relevance {
                kind: "single-relevant".to_string(),
                graded: false,
                judgments_per_query: 1,
            },
            metrics: vec![
                "hit@1".to_string(),
                "hit@3".to_string(),
                "hit@5".to_string(),
                "mrr".to_string(),
            ],
            // Deliberately near-identical texts: with one shared `created_at`
            // these tie on score, which is precisely when the old harness fell
            // through to a random `memory_id` and stopped being reproducible.
            entries: vec![
                entry(
                    "mr-1",
                    "fact",
                    "shared token payload",
                    "shared token payload",
                ),
                entry(
                    "mr-2",
                    "fact",
                    "shared token payload",
                    "shared token payload",
                ),
                entry(
                    "mr-3",
                    "fact",
                    "shared token payload",
                    "shared token payload",
                ),
            ],
            queries: vec![
                query(
                    "mrq-1",
                    "mr-1",
                    "shared token payload",
                    "shared token payload",
                ),
                query(
                    "mrq-2",
                    "mr-2",
                    "shared token payload",
                    "shared token payload",
                ),
            ],
        };
        let model = find(DEFAULT_MODEL_ID).expect("catalog entry");
        let corpus_path = std::path::Path::new("fixtures/memory-recall/corpus.json");

        let mut runs = Vec::new();
        for _ in 0..2 {
            let embedder: Arc<dyn Embedder> = Arc::new(local_rag_embed::HashingEmbedder::new(
                local_rag_store::registry::RepresentationKind::Memory,
            ));
            runs.push(
                run_one_config(&corpus, model, embedder, Config::Baseline, 0, corpus_path)
                    .await
                    .expect("run"),
            );
        }

        // `latency` is wall-clock and provenance carries the run's own paths;
        // everything that describes *what was retrieved* must match exactly,
        // `top_result_id` included — that field is a minted `memory_id`, and it
        // is the one the old `SystemUuidV7` made vary between runs.
        assert_eq!(runs[0].per_query, runs[1].per_query);
        assert_eq!(runs[0].metrics, runs[1].metrics);
        assert_eq!(runs[0].metrics_by_lang_pair, runs[1].metrics_by_lang_pair);
        assert!(
            runs[0].per_query.iter().all(|q| q.top_result_id.is_some()),
            "the fixture is built so every query retrieves something; an empty \
             result would make the comparison vacuous"
        );
    }
}
