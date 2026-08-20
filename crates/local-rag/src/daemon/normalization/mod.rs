//! Marking which durable-memory entries already speak the canon's language —
//! group 21, phase 2 (`T21-13`, [ADR-0011]).
//!
//! # What this module stopped being
//!
//! Phase 1 (`T21-05`/`T21-06`) put a whole translation pipeline here: translate
//! an entry, write its vector into `cache.sqlite` under a *new* subject hash,
//! then commit a normalization row into `state.sqlite` — two databases, two
//! transactions, a named crash point between them, and an entire second
//! definition of "which text is embedded" to keep them agreeing.
//!
//! ADR-0011 deleted the reason for all of it. English is the canon now, so an
//! entry has exactly one text, its subject hash is `H(memory_id, text)` again,
//! and its vector is written by the ordinary backfill like every other
//! subject's. `write.rs` — `apply_normalization`, `write_vectors`, the
//! `memory.normalization.after_vector` failpoint — is gone with the problem it
//! solved.
//!
//! # What is left, and why it is not nothing
//!
//! The detector. It is pure, model-free and free, and it answers one question
//! the queue cannot answer for itself: *is this entry's canon already English?*
//!
//! That marker is load-bearing rather than tidy. `entries_needing_normalization`
//! is SQL with a `LIMIT` and cannot run a Rust detector inside itself, so an
//! entry is excluded from the queue by a **stored status**, never by being
//! re-examined. Without a persisted "already English" row, every English entry
//! would be offered on every tick, fill the limit, and starve the entries that
//! genuinely need work. So this worker sweeps, detects, and settles — in one
//! `state.sqlite` transaction per batch, spending no inference at all.
//!
//! # Where translation went
//!
//! To the boundary, which is the whole point of ADR-0011 §Decision 2:
//! `T21-14` translates on the write path (above the store — a model must not
//! run under the write lock), `T21-15`/`T21-19` on the query path, and `T21-17`
//! drains the legacy set once. None of them lives here, and this module holds
//! no generator, no embedder and no `cache.sqlite` handle any more.
//!
//! # Logs never carry an entry's text
//!
//! Every `tracing` line here names identifiers and counts only. Memory text is
//! the user's own writing; a background worker has no business copying it into
//! a log file, and a test asserts it never does.
//!
//! [ADR-0011]: ../../../../../docs/adr/0011-english-canon-for-durable-memory.md

pub mod boundary;

use std::sync::Arc;
use std::time::Duration;

use local_rag_core::hash::sha256_hex;
use local_rag_memory::normalize::detect::{ScriptClass, script_class};
use local_rag_store::{
    CURRENT_NORMALIZER_VERSION, NormalizationStatus, NormalizationWrite, PendingNormalization,
    StateDb, UpsertOutcome, entries_needing_normalization, upsert_normalization,
};
use tokio::sync::oneshot;

use super::jobs::{JobKind, JobRegistry};

/// One tick's budget and switch.
#[derive(Debug, Clone, Copy)]
pub struct NormalizationParams {
    /// Whether the worker does anything at all — `MemoryConfig.
    /// normalize_to_english`, default `false` since `T21-11`.
    pub enabled: bool,
    /// Entries examined per tick. Generous, because examining is free: the
    /// detector is a pure function and this worker spends no inference.
    pub scan_limit: usize,
}

impl Default for NormalizationParams {
    fn default() -> Self {
        NormalizationParams {
            enabled: false,
            scan_limit: 512,
        }
    }
}

/// Why a tick stopped early. Never a per-entry verdict — this is a condition
/// under which continuing would be wrong for every remaining entry too.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AbortReason {
    /// Reading the queue itself failed.
    QueueUnavailable(String),
}

/// What one tick did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TickReport {
    /// Entries the queue offered.
    pub examined: usize,
    /// Entries settled as `english` in the batch write (no inference).
    pub settled: usize,
    /// Entries whose canon is not English. This worker does not translate them
    /// — `T21-14`/`T21-17` do — so it leaves them for the boundary and reports
    /// how many are waiting.
    pub awaiting_translation: usize,
    /// Entries left for a later tick: refused by the conditional write because
    /// their text moved under it.
    pub deferred: usize,
    /// Set when the tick stopped early; see [`AbortReason`].
    pub aborted: Option<AbortReason>,
}

/// One row to settle, as the caller already computed it.
///
/// Kept as an explicit value rather than written inline so the batch write
/// below stays a plain fold over already-decided outcomes — the same reason
/// `local_rag_store::NormalizationWrite` supplies `attempt_count` instead of
/// incrementing it in SQL: a replayed batch must leave the row identical.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowDraft {
    memory_id: String,
    canon_text_sha256: String,
    source_language: Option<String>,
}

impl RowDraft {
    /// The detector found the canon already English: record that, and nothing
    /// else. There is no provenance to keep — the author's words *are* the
    /// canon (ADR-0011 §Decision 1).
    pub fn english(memory_id: &str, text: &str, class: ScriptClass) -> Self {
        RowDraft {
            memory_id: memory_id.to_string(),
            canon_text_sha256: sha256_hex(text.as_bytes()),
            source_language: Some(script_label(class).to_string()),
        }
    }

    fn as_write(&self) -> NormalizationWrite<'_> {
        NormalizationWrite {
            memory_id: &self.memory_id,
            status: NormalizationStatus::English,
            // This writer does not move the canon, so the guard and the stored
            // hash are the same value — see `NormalizationWrite`'s own doc for
            // why they are separate fields at all.
            expected_text_sha256: &self.canon_text_sha256,
            canon_text_sha256: &self.canon_text_sha256,
            source_text: None,
            source_language: self.source_language.as_deref(),
            normalizer_model_id: None,
            prompt_version: None,
            normalizer_version: CURRENT_NORMALIZER_VERSION,
            attempt_count: 0,
            last_error: None,
            next_attempt_at: None,
        }
    }
}

/// The detector's answer, as an advisory label for the row.
fn script_label(class: ScriptClass) -> &'static str {
    match class {
        ScriptClass::English => "en",
        ScriptClass::NonLatin => "non-latin",
        ScriptClass::Undetermined => "undetermined",
    }
}

/// Commit every draft in one `state.sqlite` transaction.
pub async fn write_rows(
    state_db: &StateDb,
    drafts: Vec<RowDraft>,
    now_ms: i64,
) -> Result<Vec<UpsertOutcome>, local_rag_store::WriteError> {
    if drafts.is_empty() {
        return Ok(Vec::new());
    }
    state_db
        .writer()
        .transaction(move |tx| {
            let mut outcomes = Vec::with_capacity(drafts.len());
            for draft in &drafts {
                outcomes.push(upsert_normalization(tx, &draft.as_write(), now_ms)?);
            }
            Ok(outcomes)
        })
        .await
}

/// Run one pass. No sleeping, no retries of its own, no inference.
pub async fn normalization_tick(
    state_db: &StateDb,
    jobs: &JobRegistry,
    params: &NormalizationParams,
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

    let (english, other): (Vec<_>, Vec<_>) = pending
        .into_iter()
        .map(|entry| {
            let class = script_class(&entry.text);
            (entry, class)
        })
        .partition(|(_, class)| *class != ScriptClass::NonLatin);
    report.awaiting_translation = other.len();

    if !english.is_empty() {
        let drafts: Vec<RowDraft> = english
            .iter()
            .map(|(entry, class)| RowDraft::english(&entry.memory_id, &entry.text, *class))
            .collect();
        match write_rows(state_db, drafts, now_ms).await {
            Ok(outcomes) => {
                for outcome in outcomes {
                    if matches!(outcome, UpsertOutcome::Written) {
                        report.settled += 1;
                    } else {
                        report.deferred += 1;
                    }
                }
            }
            Err(e) => {
                // A failed batch write is infrastructure, not an entry's fault:
                // nothing is marked, the next tick re-offers the same set.
                tracing::warn!("local-rag: normalization batch write failed: {e}");
                report.deferred += english.len();
            }
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
pub async fn run_normalization_worker(
    state_db: Arc<StateDb>,
    jobs: JobRegistry,
    params: NormalizationParams,
    poll_interval: Duration,
    now_ms: impl Fn() -> i64 + Send,
    mut stop: oneshot::Receiver<()>,
) {
    let mut ticker = tokio::time::interval(poll_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Logged only when the answer changes: an unreadable queue is a standing
    // condition, not news every tick.
    let mut last_abort: Option<AbortReason> = None;
    loop {
        tokio::select! {
            _ = &mut stop => return,
            _ = ticker.tick() => {
                let report = normalization_tick(&state_db, &jobs, &params, now_ms()).await;
                log_tick(&report, &mut last_abort);
            }
        }
    }
}

fn log_tick(report: &TickReport, last_abort: &mut Option<AbortReason>) {
    if report.aborted != *last_abort {
        match &report.aborted {
            Some(AbortReason::QueueUnavailable(reason)) => {
                tracing::error!(
                    "local-rag: memory normalization could not read its queue: {reason}"
                )
            }
            None => tracing::info!("local-rag: memory normalization resumed"),
        }
        *last_abort = report.aborted.clone();
    }
    if report.settled > 0 {
        tracing::info!(
            "local-rag: memory normalization tick — examined {}, settled {}, \
             awaiting translation {}, deferred {}",
            report.examined,
            report.settled,
            report.awaiting_translation,
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
