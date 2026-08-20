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
    BackfillParams, EmbedRequest, Embedder, GeneratorEntry, GeneratorPool, InFlight, ProviderEntry,
    ProviderPool, run_backfill,
};
use local_rag_memory::normalize::translate::{
    TRANSLATOR_VERSION, TranslateRequest, Translation, translate,
};
use local_rag_memory::recall;
use local_rag_models::{
    DEFAULT_MODEL_ID, HttpFetcher, ModelCatalogEntry, OnnxEmbedder, find, install_model,
};
use local_rag_store::{
    CURRENT_NORMALIZER_VERSION, CacheDb, CreateMemoryEntryError, DEFAULT_MODEL_SPACE_ID,
    GLOBAL_SCOPE_OWNER_ID, MemoryKind, NewMemoryEntry, NormalizationStatus, NormalizationWrite,
    RepresentationKey, RepresentationKind, RequestRoot, RetentionParams, ScopeKind, StateDb,
    UpsertOutcome, create_memory_entry, model_space_required_representation_ids,
    recall_candidates_for_scope, register_representation, representation_key,
    set_model_space_representation, upsert_normalization,
};

use crate::git::git_short_head;
use crate::memory_recall_bench::corpus::{Corpus, Entry, Query};
use crate::memory_recall_bench::report::{
    Latency, MemoryRecallBenchReport, NormalizerRun, Provenance, QueryResult,
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
    /// **The shipped component** (T21-09). The store is seeded with
    /// `*_original`, exactly as `Baseline` is, and then the real
    /// `local_rag_memory::normalize::translate` runs over each entry against a
    /// real generator; whatever it produces is written as a
    /// `memory_text_normalization` row. Nothing else changes — the backfill
    /// already embeds each entry's *effective* text (T21-02), and recall
    /// searches that same text.
    ///
    /// The query stays `*_original`: translating the query is `T21-10`, an
    /// owner-decision card gated on the numbers this configuration produces.
    ///
    /// Unlike the four above, this configuration measures a *pipeline* rather
    /// than a hand-authored ceiling: the fixture's `text_english` is never read
    /// at all here, and the detector decides on its own which entries are worth
    /// translating.
    PipelineEn,
}

impl Config {
    /// Every configuration, `Baseline` first — `report::compare` looks for
    /// `Baseline` by name, so any caller sweeping "all" gets a comparable
    /// baseline in the set by construction.
    pub const ALL: [Config; 5] = [
        Config::Baseline,
        Config::StoreEn,
        Config::QueryEn,
        Config::BothEn,
        Config::PipelineEn,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Config::Baseline => "baseline",
            Config::StoreEn => "store_en",
            Config::QueryEn => "query_en",
            Config::BothEn => "both_en",
            Config::PipelineEn => "pipeline_en",
        }
    }

    pub fn from_str(s: &str) -> Option<Config> {
        match s {
            "baseline" => Some(Config::Baseline),
            "store_en" => Some(Config::StoreEn),
            "query_en" => Some(Config::QueryEn),
            "both_en" => Some(Config::BothEn),
            "pipeline_en" => Some(Config::PipelineEn),
            _ => None,
        }
    }

    /// Which of `entry`'s two text fields this configuration seeds the store
    /// with.
    fn store_text(self, entry: &Entry) -> &str {
        match self {
            Config::Baseline | Config::QueryEn | Config::PipelineEn => &entry.text_original,
            Config::StoreEn | Config::BothEn => &entry.text_english,
        }
    }

    /// Which of `query`'s two text fields this configuration searches with.
    fn query_text(self, query: &Query) -> &str {
        match self {
            Config::Baseline | Config::StoreEn | Config::PipelineEn => &query.query_original,
            Config::QueryEn | Config::BothEn => &query.query_english,
        }
    }

    /// Whether this configuration runs the real translator over each seeded
    /// entry. The **only** difference between [`Config::PipelineEn`] and
    /// [`Config::Baseline`]: same stored text, same query text, one extra
    /// component in the middle.
    ///
    /// A run whose set contains no such configuration never opens a generator
    /// at all, so the four fixture-driven configurations stay model-free.
    pub fn normalizes(self) -> bool {
        matches!(self, Config::PipelineEn)
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

    // T21-09: the GGUF is opened once, and **only** when some configuration in
    // this run actually translates. A `--config baseline` run stays exactly as
    // model-free as it was before this task.
    let generator = if configs.iter().any(|c| c.normalizes()) {
        Some(open_generator(&model_layout)?)
    } else {
        None
    };

    let mut reports = Vec::with_capacity(configs.len());
    for config in configs {
        eprintln!("[memory-recall-bench] running config={}", config.as_str());
        reports.push(
            run_one_config(
                &corpus,
                entry,
                embedder.clone(),
                generator.as_ref(),
                config,
                install_ms,
                &options.corpus_path,
            )
            .await?,
        );
    }
    Ok(reports)
}

/// The real local generative model `pipeline_en` translates with — installed
/// and opened exactly the way `crate::memory_bench::run` already does it, into
/// the same benchmark model home (`StoreLayout::model_dir` namespaces by
/// `model_id`, so the GGUF and the ONNX weights coexist without either
/// re-downloading).
fn open_generator(model_layout: &StoreLayout) -> Result<GeneratorSession, String> {
    let entry =
        local_rag_generate::find(local_rag_generate::DEFAULT_MODEL_ID).ok_or_else(|| {
            format!(
                "{:?} is not in the generator catalog",
                local_rag_generate::DEFAULT_MODEL_ID
            )
        })?;
    eprintln!(
        "[memory-recall-bench] translator {} in {}",
        entry.model_id,
        model_layout.model_dir(entry.model_id).display()
    );
    local_rag_generate::install_model(
        model_layout,
        entry,
        &local_rag_generate::HttpFetcher::default(),
        &mut std::io::stderr(),
    )
    .map_err(|e| format!("install {}: {e}", entry.model_id))?;
    let generator = local_rag_generate::LlamaGenerator::open(model_layout, entry)
        .map_err(|e| format!("open {}: {e}", entry.model_id))?;
    Ok(GeneratorSession {
        model_id: entry.model_id.to_string(),
        pool: GeneratorPool::new(vec![GeneratorEntry::local("llama", Arc::new(generator))]),
    })
}

/// A generator plus the id it answers under — the id belongs in the report's
/// provenance, and `GeneratorPool` does not surface it.
pub struct GeneratorSession {
    pub model_id: String,
    pub pool: GeneratorPool,
}

/// One configuration's full run: fresh store, seed with `config`'s chosen
/// store text, embed, then every query against `config`'s chosen query text.
#[allow(clippy::too_many_arguments)]
async fn run_one_config(
    corpus: &Corpus,
    entry: &'static ModelCatalogEntry,
    embedder: Arc<dyn Embedder>,
    generator: Option<&GeneratorSession>,
    config: Config,
    install_ms: u64,
    corpus_path: &std::path::Path,
) -> Result<MemoryRecallBenchReport, String> {
    let home = tempdir(config)?;
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().map_err(|e| format!("store layout: {e}"))?;

    let state = StateDb::open(layout.state_db()).map_err(|e| format!("state: {e}"))?;
    let cache = CacheDb::open(layout.cache_db(), "memory-recall-bench")
        .map_err(|e| format!("cache: {e}"))?;
    let uuids = SeqUuidV7::new();
    let now_ms = 1_000;

    register_memory_representation(&state, embedder.as_ref(), now_ms).await?;

    // T21-09: `pipeline_en` seeds the original text and then runs the real
    // component over it. Every other configuration leaves this `None`, and its
    // provenance says so.
    let mut normalizer = config.normalizes().then(|| {
        let session = generator.expect("a normalizing config always gets a generator");
        NormalizerRun {
            model_id: session.model_id.clone(),
            prompt_version: TRANSLATOR_VERSION,
            normalizer_version: CURRENT_NORMALIZER_VERSION,
            translated: 0,
            passthrough: 0,
            failed: 0,
            failures: Vec::new(),
        }
    });

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
        if let Some(run) = normalizer.as_mut() {
            let pool = &generator
                .expect("a normalizing config always gets a generator")
                .pool;
            normalize_entry(
                &state,
                pool,
                &memory_id,
                corpus_entry,
                config.store_text(corpus_entry),
                run,
                created_at,
            )
            .await?;
        }
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

    // T21-09: the dense leg, called directly with exactly what the pipeline
    // hands it, so each query can be scored on that leg alone as well as after
    // fusion. Without this the benchmark cannot tell "the embedder never saw
    // the English text" from "the embedder saw it and RRF still ranked the
    // lexical leg's answer higher" — and those two conclusions point at
    // opposite fixes.
    let dense_context = memory_representation(&state_read)?;
    let candidates =
        recall_candidates_for_scope(&state_read, ScopeKind::Global, GLOBAL_SCOPE_OWNER_ID)
            .map_err(|e| format!("candidates: {e}"))?;

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
        let mut dense_degraded: Option<String> = None;
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
            dense_degraded = outcome.dense_degraded.as_ref().map(|d| format!("{d:?}"));
            ranked_ids = outcome
                .entries
                .iter()
                .take(QUERY_LIMIT)
                .map(|e| e.memory_id.clone())
                .collect();
        }

        let rank = rank_of_match(expected_memory_id, &ranked_ids);
        let dense_hits_list = dense_context
            .as_ref()
            .and_then(|(key, representation_id)| {
                recall::dense_leg(
                    &cache_read,
                    config.query_text(query),
                    key,
                    representation_id,
                    &query_embedder,
                    &recall::BruteForceCosine,
                    &candidates,
                    QUERY_LIMIT,
                )
                .ok()
            })
            .unwrap_or_default();
        let dense_hits = dense_hits_list.len();
        let dense_rank = dense_hits_list
            .iter()
            .find(|h| h.memory_id == *expected_memory_id)
            .map(|h| h.rank);
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
            dense_degraded,
            dense_rank,
            dense_hits,
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
        normalizer,
    };

    // Let this configuration's writer threads finish before the next one
    // starts: `CacheDb::close` joins its own, and dropping the read handles
    // first keeps SQLite from holding the files open behind them.
    drop(state_read);
    drop(cache_read);
    cache.close();

    Ok(MemoryRecallBenchReport::new(
        provenance,
        metrics,
        metrics_by_lang_pair,
        per_query,
        latency,
    ))
}

/// The active `memory` representation for the benchmark's own model space —
/// the same pair `recall`'s own `resolve_memory_representation` resolves for a
/// `GlobalOnly` request, read here so the dense leg can be scored on its own
/// (T21-09).
fn memory_representation(
    state_read: &local_rag_store::rusqlite::Connection,
) -> Result<Option<(RepresentationKey, String)>, String> {
    let representations =
        model_space_required_representation_ids(state_read, DEFAULT_MODEL_SPACE_ID)
            .map_err(|e| format!("representations: {e}"))?;
    let Some((_, representation_id)) = representations
        .into_iter()
        .find(|(kind, _)| *kind == RepresentationKind::Memory)
    else {
        return Ok(None);
    };
    let Some(key) =
        representation_key(state_read, &representation_id).map_err(|e| format!("key: {e}"))?
    else {
        return Ok(None);
    };
    Ok(Some((key, representation_id)))
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

/// Run the **real** translator over one seeded entry and install what it
/// produced as that entry's canon (T21-13, ADR-0011).
///
/// Three outcomes, all of them normal:
///
/// - a validated English variant → the entry's `text` **becomes** it and a
///   `translated` row keeps the author's original as provenance. From here on
///   every leg reads that one text, which is the whole point of the English
///   canon;
/// - the detector found nothing non-Latin → an `english` row, written without a
///   single generator call (ADR-0010 Decision 8, still in force);
/// - the validator refused the answer → a `failed` row. The canon stays the
///   original and the run continues: a benchmark that aborted here would go
///   blind precisely when the shipped component did something interesting, and
///   the metric is supposed to include that outcome, not hide it.
///
/// Order inside the transaction is load-bearing. `upsert_normalization` guards
/// by re-reading `memory_entry.text` and comparing it to `expected_text_sha256`,
/// so the provenance row is written **before** the canon moves — the reverse
/// order would have the guard compare the new English text against the original
/// hash and refuse its own write.
///
/// Installing the canon with a bare `UPDATE` is a harness liberty, stated
/// rather than hidden: the production path (T21-14/T21-17) goes through
/// `apply_edit` so the rewrite earns an `audit_event`. This store is synthetic,
/// built and thrown away per configuration, and has no audit trail to keep
/// honest.
async fn normalize_entry(
    state: &StateDb,
    pool: &GeneratorPool,
    memory_id: &str,
    corpus_entry: &Entry,
    text: &str,
    run: &mut NormalizerRun,
    now_ms: i64,
) -> Result<(), String> {
    let outcome = translate(
        pool,
        DataPolicy::LocalOnly,
        TranslateRequest { memory_id, text },
    );

    let sha = local_rag_core::hash::sha256_hex(text.as_bytes());
    let (status, english, language, last_error) = match outcome {
        Ok(Translation::Translated { english }) => {
            run.translated += 1;
            (NormalizationStatus::Translated, Some(english), None, None)
        }
        Ok(Translation::Passthrough { class }) => {
            run.passthrough += 1;
            (
                NormalizationStatus::English,
                None,
                Some(format!("{class:?}")),
                None,
            )
        }
        Err(e) => {
            run.failed += 1;
            run.failures.push((corpus_entry.id.clone(), e.to_string()));
            eprintln!(
                "[memory-recall-bench]   {} not translated: {e}",
                corpus_entry.id
            );
            (NormalizationStatus::Failed, None, None, Some(e.to_string()))
        }
    };

    let (id, model_id) = (memory_id.to_string(), run.model_id.clone());
    let normalizer_version = run.normalizer_version;
    let original = text.to_string();
    let outcome = state
        .writer()
        .transaction(move |tx| {
            // The canon after this write: the translation when there is one,
            // otherwise the text that is already there.
            let canon = english.as_deref().unwrap_or(original.as_str());
            let canon_sha = local_rag_core::hash::sha256_hex(canon.as_bytes());
            let outcome = upsert_normalization(
                tx,
                &NormalizationWrite {
                    memory_id: &id,
                    status,
                    expected_text_sha256: &sha,
                    canon_text_sha256: &canon_sha,
                    source_text: english.as_ref().map(|_| original.as_str()),
                    source_language: language.as_deref(),
                    normalizer_model_id: Some(&model_id),
                    prompt_version: Some(TRANSLATOR_VERSION),
                    normalizer_version,
                    attempt_count: 1,
                    last_error: last_error.as_deref(),
                    next_attempt_at: None,
                },
                now_ms,
            )?;
            if matches!(outcome, UpsertOutcome::Written) && english.is_some() {
                tx.execute(
                    "UPDATE memory_entry SET text = ?2 WHERE memory_id = ?1",
                    local_rag_store::rusqlite::params![&id, canon],
                )?;
            }
            Ok(outcome)
        })
        .await
        .map_err(|e| format!("{}: normalization tx: {e}", corpus_entry.id))?;
    match outcome {
        UpsertOutcome::Written => Ok(()),
        other => Err(format!(
            "{}: normalization refused: {other:?}",
            corpus_entry.id
        )),
    }
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

/// A store directory of this configuration's own — `…-<pid>/<config>`.
///
/// It used to be one directory reused across configurations, deleted and
/// recreated per call. That raced: `StateDb`'s writer thread is detached and
/// `CacheDb`'s is joined only on an explicit `close`, so the next
/// configuration could delete files the previous one's threads still held, and
/// the following `run_backfill` failed with a bare SQLite `disk I/O error`
/// (observed at T21-09, on the second configuration of a five-configuration
/// sweep, killing the whole run). Per-configuration directories remove the
/// shared resource instead of trying to time the cleanup; the parent is
/// removed once, up front, so a rerun of the same pid still starts clean.
fn tempdir(config: Config) -> Result<PathBuf, String> {
    let base = std::env::temp_dir().join(format!(
        "local-rag-memory-recall-bench-{}",
        std::process::id()
    ));
    let dir = base.join(config.as_str());
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("temp dir: {e}"))?;
    Ok(dir)
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

        // T21-09: the shipped pipeline reads neither English field of the
        // fixture — it produces its own, at run time.
        assert_eq!(Config::PipelineEn.store_text(&e), "RU_STORE");
        assert_eq!(Config::PipelineEn.query_text(&q), "RU_QUERY");
        assert!(Config::PipelineEn.normalizes());
        for config in [
            Config::Baseline,
            Config::StoreEn,
            Config::QueryEn,
            Config::BothEn,
        ] {
            assert!(
                !config.normalizes(),
                "{} is fixture-driven and must never open a generator",
                config.as_str(),
            );
        }
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

    // -----------------------------------------------------------------
    // T21-09: the shipped pipeline's own plumbing, with no model at all
    // -----------------------------------------------------------------

    /// A generator that answers with a scripted translation and counts its
    /// calls — the point of several tests below is that it is **not** called.
    #[derive(Debug, Clone)]
    struct ScriptedTranslator {
        answer: Result<String, local_rag_embed::GenError>,
        calls: Arc<AtomicU64>,
    }

    impl ScriptedTranslator {
        fn translating(english: &str) -> Self {
            Self {
                answer: Ok(serde_json::json!({ "en": english }).to_string()),
                calls: Arc::new(AtomicU64::new(0)),
            }
        }

        fn refusing() -> Self {
            Self {
                answer: Ok("not json at all".to_string()),
                calls: Arc::new(AtomicU64::new(0)),
            }
        }

        fn pool(&self) -> GeneratorPool {
            GeneratorPool::new(vec![GeneratorEntry::local(
                "scripted",
                Arc::new(self.clone()),
            )])
        }

        fn calls(&self) -> u64 {
            self.calls.load(Ordering::Relaxed)
        }
    }

    impl local_rag_embed::Generator for ScriptedTranslator {
        fn generate(
            &self,
            _req: local_rag_embed::GenRequest,
        ) -> Result<local_rag_embed::GenResponse, local_rag_embed::GenError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.answer
                .clone()
                .map(|text| local_rag_embed::GenResponse {
                    text,
                    finish_reason: local_rag_embed::FinishReason::Stop,
                    tokens_generated: None,
                })
        }
    }

    fn empty_run(model_id: &str) -> NormalizerRun {
        NormalizerRun {
            model_id: model_id.to_string(),
            prompt_version: TRANSLATOR_VERSION,
            normalizer_version: CURRENT_NORMALIZER_VERSION,
            translated: 0,
            passthrough: 0,
            failed: 0,
            failures: Vec::new(),
        }
    }

    /// The seeding contract of `pipeline_en`: the **original** text stays in
    /// `memory_entry`, the English variant goes into
    /// `memory_text_normalization`, and the effective text — the one thing that
    /// decides what gets embedded — is the English one.
    #[tokio::test]
    async fn pipeline_en_seeds_the_original_and_normalizes_beside_it() {
        let home = TempHome::new().expect("temp home");
        let layout = StoreLayout::new(home.join("local-rag"));
        layout.ensure().expect("ensure store tree");
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        let uuids = SeqUuidV7::new();

        let russian = "команда перевела очередь уведомлений с rabbitmq на nats jetstream";
        let english = "the team moved the notification queue off rabbitmq onto nats jetstream";
        let bilingual = entry("mr-b", "decision", russian, "IGNORED FIXTURE ENGLISH");
        let memory_id = uuids.next_uuid().to_string();
        let text = Config::PipelineEn.store_text(&bilingual);
        assert_eq!(text, russian, "the store gets the original, like baseline");
        seed_entry(&state, &memory_id, &bilingual, text, 1_000)
            .await
            .expect("seed");

        let translator = ScriptedTranslator::translating(english);
        let mut run = empty_run("scripted-model");
        normalize_entry(
            &state,
            &translator.pool(),
            &memory_id,
            &bilingual,
            text,
            &mut run,
            2_000,
        )
        .await
        .expect("normalize");

        assert_eq!(run.translated, 1);
        assert_eq!(run.passthrough, 0);
        assert_eq!(run.failed, 0);
        assert_eq!(translator.calls(), 1);

        let read = state.open_read().expect("state read");
        let stored = local_rag_store::memory_entry_by_id(&read, &memory_id)
            .expect("read entry")
            .expect("entry exists");
        assert_eq!(
            stored.text, english,
            "the canon is English now (ADR-0011 §Decision 1) — that is what every leg reads",
        );
        let row = local_rag_store::normalization_for(&read, &memory_id)
            .expect("read normalization")
            .expect("row exists");
        assert_eq!(row.status, NormalizationStatus::Translated);
        assert_eq!(
            row.source_text.as_deref(),
            Some(russian),
            "the author's own words survive as provenance",
        );
        assert_eq!(row.normalizer_model_id.as_deref(), Some("scripted-model"));

        let texts = local_rag_store::all_memory_entries_with_text(&read).expect("entry texts");
        let (_, embedded) = texts
            .into_iter()
            .find(|(id, _)| *id == memory_id)
            .expect("entry present");
        assert_eq!(
            embedded, english,
            "one text, and the backfill embeds it — which is what makes this configuration \
             measure the pipeline rather than the fixture",
        );
    }

    /// ADR-0010 Decision 8, measured: an already-English entry costs the
    /// benchmark exactly zero inference, same as it costs the daemon.
    #[tokio::test]
    async fn pipeline_en_never_calls_the_generator_for_english_text() {
        let home = TempHome::new().expect("temp home");
        let layout = StoreLayout::new(home.join("local-rag"));
        layout.ensure().expect("ensure store tree");
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        let uuids = SeqUuidV7::new();

        let text = "the team moved the notification queue off rabbitmq onto nats jetstream";
        let english_entry = entry("mr-e", "decision", text, text);
        let memory_id = uuids.next_uuid().to_string();
        seed_entry(&state, &memory_id, &english_entry, text, 1_000)
            .await
            .expect("seed");

        let translator = ScriptedTranslator::translating("SHOULD NOT BE USED");
        let mut run = empty_run("scripted-model");
        normalize_entry(
            &state,
            &translator.pool(),
            &memory_id,
            &english_entry,
            text,
            &mut run,
            2_000,
        )
        .await
        .expect("normalize");

        assert_eq!(translator.calls(), 0, "the detector answered on its own");
        assert_eq!(run.passthrough, 1);
        assert_eq!(run.translated, 0);

        let read = state.open_read().expect("state read");
        let row = local_rag_store::normalization_for(&read, &memory_id)
            .expect("read normalization")
            .expect("row exists");
        assert_eq!(row.status, NormalizationStatus::English);
        assert_eq!(row.source_text, None);
        let texts = local_rag_store::all_memory_entries_with_text(&read).expect("entry texts");
        let (_, canon) = texts
            .into_iter()
            .find(|(id, _)| *id == memory_id)
            .expect("entry present");
        assert_eq!(canon, text, "already English: the canon is untouched");
    }

    /// A refused translation is a normal outcome, not an abort: the entry keeps
    /// its original text and the run goes on, so the metric includes the
    /// failure instead of the failure erasing the metric.
    #[tokio::test]
    async fn a_refused_translation_is_recorded_and_the_run_continues() {
        let home = TempHome::new().expect("temp home");
        let layout = StoreLayout::new(home.join("local-rag"));
        layout.ensure().expect("ensure store tree");
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        let uuids = SeqUuidV7::new();

        let russian = "команда перевела очередь уведомлений с rabbitmq на nats jetstream";
        let bilingual = entry("mr-b", "decision", russian, "IGNORED FIXTURE ENGLISH");
        let memory_id = uuids.next_uuid().to_string();
        seed_entry(&state, &memory_id, &bilingual, russian, 1_000)
            .await
            .expect("seed");

        let translator = ScriptedTranslator::refusing();
        let mut run = empty_run("scripted-model");
        normalize_entry(
            &state,
            &translator.pool(),
            &memory_id,
            &bilingual,
            russian,
            &mut run,
            2_000,
        )
        .await
        .expect("a refusal is not an error for the caller");

        assert_eq!(run.failed, 1);
        assert_eq!(run.translated, 0);
        assert_eq!(run.failures.len(), 1);
        assert_eq!(run.failures[0].0, "mr-b");
        assert!(
            run.failures[0].1.contains("rejected"),
            "the reason travels into the report: {:?}",
            run.failures[0].1,
        );

        let read = state.open_read().expect("state read");
        let row = local_rag_store::normalization_for(&read, &memory_id)
            .expect("read normalization")
            .expect("row exists");
        assert_eq!(row.status, NormalizationStatus::Failed);
        let texts = local_rag_store::all_memory_entries_with_text(&read).expect("entry texts");
        let (_, canon) = texts
            .into_iter()
            .find(|(id, _)| *id == memory_id)
            .expect("entry present");
        assert_eq!(
            canon, russian,
            "a failed translation degrades to today's behaviour — the author's own text stays \
             the canon, which is ADR-0011 §Decision 3's \"eventually English\"",
        );
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
                run_one_config(
                    &corpus,
                    model,
                    embedder,
                    None,
                    Config::Baseline,
                    0,
                    corpus_path,
                )
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
