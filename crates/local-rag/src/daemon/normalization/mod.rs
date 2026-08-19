//! Applying English normalization to durable memory (ADR-0010) — group 21.
//!
//! - [`write`] (T21-05) — one entry: translate, embed under the new subject
//!   hash, then commit the normalization row. The order is that module's whole
//!   reason to exist.
//! - this module (T21-06) — the background worker that decides *which* entries
//!   to hand it, how many per tick, and what to do when one fails.
//!
//! # The tick's shape, and why it is this shape
//!
//! [`normalization_tick`] is one pass with no sleeping of its own;
//! [`run_normalization_worker`] is the `tokio::time::interval` + `oneshot`-stop
//! loop around it — the same form D-024 established for the consolidation
//! trigger, down to holding a [`JobKind::Normalization`] guard only while a tick
//! is actually working, never across the wait between ticks (spec 02 §4.3: an
//! idle daemon must stay idle-shutdown eligible).
//!
//! Three properties the tick owes the rest of the system:
//!
//! 1. **Already-English entries cost nothing.** The detector (T21-03) runs over
//!    the whole selected set, and every passthrough row is committed in **one**
//!    transaction with no generator call at all (ADR-0010 Decision 8).
//! 2. **Inference is bounded.** At most `translate_batch` entries are
//!    translated per tick, each through [`write::apply_normalization`] with its
//!    own pair of transactions — so an interrupted tick never leaves an entry
//!    normalized without a vector.
//! 3. **`Unavailable` aborts the tick and blames nobody.** A missing generator,
//!    a policy-blocked pool, or an embedder that does not match the registry is
//!    not an entry's fault: the tick stops, no row is marked failed, and no
//!    `attempt_count` moves (ADR-0010 Decision 10 — the pre-emptive lesson of
//!    D-050, whose retry storm this project has already paid for once).
//!
//! # Consolidation owns the model first
//!
//! Both this worker and the consolidation router drive the *same* process-wide
//! local generative model (D-054). Consolidation is the latency-sensitive one —
//! a session has ended and its observations are waiting — while normalization is
//! opportunistic catch-up that loses nothing by arriving a tick later. So a tick
//! that starts while a consolidation job is registered still commits its
//! passthrough batch (pure detection, `state.sqlite` only, no inference at all)
//! and defers **every** translation to a later tick, rather than queueing behind
//! consolidation inside the pool.
//!
//! # Dead-letter keyed by normalizer version, not build id
//!
//! D-050 dead-letters a mechanically-failing consolidation run per
//! `local_rag_core::BUILD_ID`. `memory_text_normalization` has no fingerprint
//! column (T21-01), and it does not need one: the deliberate equivalent is
//! `CURRENT_NORMALIZER_VERSION`. A mechanical failure is recorded with
//! `attempt_count = MAX_NORMALIZATION_ATTEMPTS`, which
//! `entries_needing_normalization` will not offer again — until the normalizer
//! version changes, which re-queues every row whatever its status. A change of
//! normalizer is a product decision, unlike an incidental rebuild, so gating on
//! it is the stricter and more honest rule.
//!
//! # Logs never carry an entry's text
//!
//! Every `tracing` line here names identifiers and counts only. Memory text is
//! the user's own writing; a background worker has no business copying it into
//! a log file, and a test asserts it never does.

pub mod write;

use std::sync::Arc;
use std::time::Duration;

use local_rag_core::config::DataPolicy;
use local_rag_core::hash::sha256_hex;
use local_rag_embed::{Embedder, GeneratorPool, ProviderEntry, ProviderPool};
use local_rag_memory::normalize::detect::{ScriptClass, script_class};
use local_rag_memory::normalize::translate::{TranslateFailureKind, classify_translate_failure};
use local_rag_store::{
    CURRENT_NORMALIZER_VERSION, CacheDb, MAX_NORMALIZATION_ATTEMPTS, PendingNormalization, StateDb,
    entries_needing_normalization, transient_backoff_delay_ms,
};
use tokio::sync::oneshot;

use super::jobs::{JobKind, JobRegistry};
use write::{
    NormalizationError, NormalizationOutcome, NormalizationTarget, RowDraft, apply_normalization,
    write_rows,
};

/// One tick's budget and switch.
#[derive(Debug, Clone, Copy)]
pub struct NormalizationParams {
    /// Whether the worker does anything at all. T21-08 wires this to
    /// `MemoryConfig.normalize_to_english` (default `true`, ADR-0010
    /// Decision 11); until then the daemon ships it **off**, so no store starts
    /// spending inference before the switch that turns it off exists.
    pub enabled: bool,
    /// Entries translated per tick — the inference bound. Deliberately small:
    /// a translation is ~a second of local GPU, and the queue is drained across
    /// ticks rather than in one long stall.
    pub translate_batch: usize,
    /// Entries examined per tick. Far larger than `translate_batch` because
    /// examining is free — the detector is pure — and an all-English store
    /// should converge in one tick at no cost.
    pub scan_limit: usize,
}

impl Default for NormalizationParams {
    fn default() -> Self {
        NormalizationParams {
            enabled: false,
            translate_batch: 4,
            scan_limit: 512,
        }
    }
}

/// Why a tick stopped early. Never a per-entry verdict — these are conditions
/// under which continuing would be wrong for every remaining entry too.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AbortReason {
    /// No usable generator, a policy-blocked pool, or an embedder that does not
    /// match the registry. Nothing was marked failed.
    Unavailable(String),
    /// Reading the queue itself failed.
    QueueUnavailable(String),
}

/// What one tick did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TickReport {
    /// Entries the queue offered.
    pub examined: usize,
    /// Entries recorded as `skipped` in the batch write (no inference).
    pub passthrough: usize,
    /// Entries translated and committed.
    pub translated: usize,
    /// Entries whose failure was recorded (mechanical or transient).
    pub failed: usize,
    /// Entries left for a later tick — over the batch bound, or refused by the
    /// conditional write because their text moved.
    pub deferred: usize,
    /// Set when the tick stopped early; see [`AbortReason`].
    pub aborted: Option<AbortReason>,
    /// Whether this tick skipped its inference half because a consolidation job
    /// was holding the shared local model. The passthrough batch still ran.
    pub yielded_to_consolidation: bool,
}

/// Run one normalization pass. No sleeping, no retries of its own.
#[allow(clippy::too_many_arguments)]
pub async fn normalization_tick(
    state_db: &StateDb,
    cache: &CacheDb,
    generators: &GeneratorPool,
    embedder: Option<Arc<dyn Embedder>>,
    jobs: &JobRegistry,
    params: &NormalizationParams,
    policy: DataPolicy,
    now_ms: i64,
) -> TickReport {
    let mut report = TickReport::default();
    if !params.enabled {
        return report;
    }

    // The guard covers the whole active pass — including the queue read, so a
    // shutdown racing a tick sees "busy" from the first store access.
    let _job = jobs.begin(JobKind::Normalization);

    let pending = match read_queue(state_db, params.scan_limit, now_ms) {
        Ok(pending) => pending,
        Err(reason) => {
            report.aborted = Some(AbortReason::QueueUnavailable(reason));
            return report;
        }
    };
    report.examined = pending.len();
    if pending.is_empty() {
        return report;
    }

    // The detector is pure and free: run it over everything the queue offered,
    // and settle every already-English entry in one transaction.
    let (passthrough, mut to_translate): (Vec<_>, Vec<_>) = pending
        .into_iter()
        .map(|entry| {
            let class = script_class(&entry.text);
            (entry, class)
        })
        .partition(|(_, class)| *class != ScriptClass::NonLatin);

    if !passthrough.is_empty() {
        let drafts: Vec<RowDraft> = passthrough
            .iter()
            .map(|(entry, class)| RowDraft::passthrough(&entry.memory_id, &entry.text, *class))
            .collect();
        match write_rows(state_db, drafts, now_ms).await {
            Ok(outcomes) => {
                for outcome in outcomes {
                    if matches!(outcome, local_rag_store::UpsertOutcome::Written) {
                        report.passthrough += 1;
                    } else {
                        report.deferred += 1;
                    }
                }
            }
            Err(e) => {
                // A failed batch write is infrastructure, not an entry's fault:
                // nothing is marked, the next tick re-offers the same set.
                tracing::warn!("local-rag: normalization passthrough batch failed: {e}");
                report.deferred += passthrough.len();
            }
        }
    }

    // Consolidation holds the same process-wide model; let it have it. The
    // passthrough batch above has already been committed — it needs no
    // inference — so yielding here costs nothing but a tick of latency.
    if !to_translate.is_empty()
        && jobs.any_running(&[JobKind::ConsolidationResume, JobKind::ConsolidationTrigger])
    {
        report.deferred += to_translate.len();
        report.yielded_to_consolidation = true;
        return report;
    }

    if to_translate.len() > params.translate_batch {
        report.deferred += to_translate.len() - params.translate_batch;
        to_translate.truncate(params.translate_batch);
    }

    for (entry, _) in to_translate {
        let outcome = apply_normalization(
            state_db,
            cache,
            generators,
            &pool_for(embedder.clone()),
            policy,
            NormalizationTarget {
                memory_id: &entry.memory_id,
                text: &entry.text,
            },
            now_ms,
        )
        .await;

        match outcome {
            Ok(NormalizationOutcome::Normalized { .. }) => {
                report.translated += 1;
                tracing::debug!(
                    "local-rag: normalized memory entry {}",
                    entry.memory_id.as_str()
                );
            }
            Ok(NormalizationOutcome::Skipped { .. }) => report.passthrough += 1,
            Ok(NormalizationOutcome::TextMoved) => report.deferred += 1,
            Err(e) => match classify(&e) {
                TranslateFailureKind::Unavailable => {
                    // Every remaining entry would fail the same way, and none of
                    // them is at fault. Stop, mark nothing.
                    report.aborted = Some(AbortReason::Unavailable(e.to_string()));
                    return report;
                }
                kind => {
                    if record_failure(state_db, &entry, kind, &e, now_ms).await {
                        report.failed += 1;
                    } else {
                        report.deferred += 1;
                    }
                }
            },
        }
    }

    report
}

/// Drive [`normalization_tick`] on a fixed cadence until `stop` fires.
///
/// Same loop shape as `daemon::consolidation_trigger::run_consolidation_trigger`
/// (D-024): an `interval` whose first tick is immediate, a `select!` against a
/// `oneshot` so shutdown never waits out a full period, and per-tick logging
/// that stays silent on routine outcomes — this runs forever, so a line per
/// quiet tick would be pure noise.
#[allow(clippy::too_many_arguments)]
pub async fn run_normalization_worker(
    state_db: Arc<StateDb>,
    cache: Arc<CacheDb>,
    generators: Arc<GeneratorPool>,
    embedder: impl Fn() -> Option<Arc<dyn Embedder>> + Send,
    jobs: JobRegistry,
    params: NormalizationParams,
    policy: DataPolicy,
    poll_interval: Duration,
    now_ms: impl Fn() -> i64 + Send,
    mut stop: oneshot::Receiver<()>,
) {
    let mut ticker = tokio::time::interval(poll_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Logged only when the answer changes: an unavailable generator is a
    // standing condition, not news every tick.
    let mut last_abort: Option<AbortReason> = None;
    loop {
        tokio::select! {
            _ = &mut stop => return,
            _ = ticker.tick() => {
                let report = normalization_tick(
                    &state_db,
                    &cache,
                    &generators,
                    embedder(),
                    &jobs,
                    &params,
                    policy,
                    now_ms(),
                )
                .await;
                log_tick(&report, &mut last_abort);
            }
        }
    }
}

fn log_tick(report: &TickReport, last_abort: &mut Option<AbortReason>) {
    if report.aborted != *last_abort {
        match &report.aborted {
            Some(AbortReason::Unavailable(reason)) => tracing::warn!(
                "local-rag: memory normalization paused — {reason}; no entry was marked failed"
            ),
            Some(AbortReason::QueueUnavailable(reason)) => {
                tracing::error!(
                    "local-rag: memory normalization could not read its queue: {reason}"
                )
            }
            None => tracing::info!("local-rag: memory normalization resumed"),
        }
        *last_abort = report.aborted.clone();
    }
    if report.translated > 0 || report.failed > 0 {
        tracing::info!(
            "local-rag: memory normalization tick — examined {}, passthrough {}, translated {}, \
             failed {}, deferred {}",
            report.examined,
            report.passthrough,
            report.translated,
            report.failed,
            report.deferred,
        );
    }
}

fn read_queue(
    state_db: &StateDb,
    limit: usize,
    now_ms: i64,
) -> Result<Vec<PendingNormalization>, String> {
    let read = state_db.open_read().map_err(|e| e.to_string())?;
    entries_needing_normalization(&read, CURRENT_NORMALIZER_VERSION, now_ms, limit)
        .map_err(|e| e.to_string())
}

/// A one-entry pool over the lazily-probed memory embedder. `None` becomes an
/// empty pool, whose own `NoProvider` classifies as `Unavailable` — the same
/// answer, produced by the same code path, as a generator that is missing.
fn pool_for(embedder: Option<Arc<dyn Embedder>>) -> ProviderPool {
    match embedder {
        Some(embedder) => ProviderPool::new(vec![ProviderEntry::local("memory", embedder)]),
        None => ProviderPool::new(Vec::new()),
    }
}

/// Map a write-path failure onto the retry vocabulary.
fn classify(error: &NormalizationError) -> TranslateFailureKind {
    match error {
        NormalizationError::Translate(e) => classify_translate_failure(e),
        // The embedder and the registry disagree about width. That is a
        // configuration fault of this process, identical for every entry, so it
        // must not cost any single entry an attempt.
        NormalizationError::NoUsableRepresentation { .. } => TranslateFailureKind::Unavailable,
        NormalizationError::Embed(_)
        | NormalizationError::CacheWrite(_)
        | NormalizationError::CacheOpen(_)
        | NormalizationError::StateWrite(_)
        | NormalizationError::StateRead(_)
        | NormalizationError::Sqlite(_) => TranslateFailureKind::Transient,
        #[cfg(feature = "failpoints")]
        NormalizationError::FailpointInjected => TranslateFailureKind::Transient,
    }
}

/// Record one entry's failure. Returns whether the row was actually written —
/// a refused conditional write means the entry's text moved, which is not a
/// failure of anything.
async fn record_failure(
    state_db: &StateDb,
    entry: &PendingNormalization,
    kind: TranslateFailureKind,
    error: &NormalizationError,
    now_ms: i64,
) -> bool {
    let attempt_count = entry.attempt_count + 1;
    let (attempt_count, next_attempt_at) = match kind {
        // Mechanical: the same text under the same normalizer fails the same
        // way, so park it at the cap rather than spending the remaining
        // attempts proving it (D-050's dead-letter, keyed by normalizer version
        // — see this module's own doc).
        TranslateFailureKind::Mechanical => (MAX_NORMALIZATION_ATTEMPTS, None),
        TranslateFailureKind::Transient => (
            attempt_count,
            Some(now_ms + transient_backoff_delay_ms(attempt_count)),
        ),
        TranslateFailureKind::Unavailable => unreachable!("handled by the caller"),
    };

    let draft = RowDraft::failure(
        &entry.memory_id,
        &sha256_hex(entry.text.as_bytes()),
        attempt_count,
        &error.to_string(),
        next_attempt_at,
    );
    match write_rows(state_db, vec![draft], now_ms).await {
        Ok(outcomes) => matches!(
            outcomes.first(),
            Some(local_rag_store::UpsertOutcome::Written)
        ),
        Err(e) => {
            tracing::warn!(
                "local-rag: could not record a normalization failure for {}: {e}",
                entry.memory_id.as_str()
            );
            false
        }
    }
}
