//! Sweeping durable memory into the English canon — group 21, phase 2
//! (`T21-13`, `T21-17`, [ADR-0011]).
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
//! # What it is instead: two halves, and the first one is free
//!
//! **Detect.** A pure, model-free pass that answers one question the queue
//! cannot answer for itself: *is this entry's canon already English?*
//!
//! That marker is load-bearing rather than tidy. `entries_needing_normalization`
//! is SQL with a `LIMIT` and cannot run a Rust detector inside itself, so an
//! entry is excluded from the queue by a **stored status**, never by being
//! re-examined. Without a persisted "already English" row, every English entry
//! would be offered on every tick, fill the limit, and starve the entries that
//! genuinely need work. The whole batch settles in one `state.sqlite`
//! transaction and spends no inference at all.
//!
//! **Translate** (`T21-17`). What is left after detection is the legacy set:
//! entries written before the boundary existed, still in the language their
//! author used. Nobody else will move them — `T21-14` closed the path a *new*
//! entry takes, not the ones already on disk — so this worker translates them,
//! bounded by `translate_batch` per tick, and installs the result as the
//! entry's canon.
//!
//! A canon install is one transaction and its order is load-bearing; see
//! `install_canon`, which is also where the sweep's idempotence lives: the
//! entry's `entry_version` is carried from the queue read into `apply_edit`, so
//! an entry that moved under a translation in flight is skipped rather than
//! overwritten.
//!
//! # Where the *other* translation happens
//!
//! At the boundary, which is ADR-0011 §Decision 2's whole point: `T21-14` on
//! the write path (above the store — a model must not run under the write
//! lock), `T21-15`/`T21-19` on the query paths. This worker shares their
//! [`boundary::Translator`] rather than owning a second one, and holds no
//! embedder and no `cache.sqlite` handle at all.
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
use local_rag_memory::normalize::translate::TranslateFailureKind;
use local_rag_store::{
    Actor, CURRENT_NORMALIZER_VERSION, EditMemoryOp, MAX_NORMALIZATION_ATTEMPTS, MemoryOpError,
    NormalizationStatus, NormalizationWrite, PendingNormalization, StateDb, UpsertOutcome,
    apply_edit, entries_needing_normalization, transient_backoff_delay_ms, upsert_normalization,
};
use tokio::sync::oneshot;

use super::jobs::{JobKind, JobRegistry};

/// One tick's budget and switch.
#[derive(Debug, Clone, Copy)]
pub struct NormalizationParams {
    /// Whether the worker does anything at all — `MemoryConfig.
    /// normalize_to_english`, default `true` again since `T21-17`.
    pub enabled: bool,
    /// Entries examined per tick. Generous, because examining is free: the
    /// detector is a pure function.
    pub scan_limit: usize,
    /// Entries **translated** per tick — the inference bound (`T21-17`).
    /// Deliberately small: a translation is a few hundred milliseconds of local
    /// GPU, and a backlog is drained across ticks rather than in one long stall.
    pub translate_batch: usize,
}

impl Default for NormalizationParams {
    fn default() -> Self {
        NormalizationParams {
            enabled: true,
            scan_limit: 512,
            translate_batch: 4,
        }
    }
}

/// Whether this tick may still defer its inference half to consolidation.
///
/// The deference itself is right — one process-wide model (D-054), and
/// consolidation is the latency-sensitive owner — but it must be **bounded**,
/// and `T21-17`'s own live acceptance is what proved it. On the owner's store
/// consolidation runs continuously (measured: six ticks in a row, `settled`
/// climbing, `translated 0` every time, `awaiting translation` pinned at 19,
/// while consolidation applied ~3 runs/minute against a candidate backlog that
/// was *growing*). Unbounded politeness is starvation with a nicer name: the
/// legacy set never drains, and ADR-0011's premise — that the store converges
/// on one canon — is never reached.
///
/// So the worker counts consecutive yields and, past
/// [`MAX_CONSECUTIVE_YIELDS`], insists. Consolidation still wins by default; it
/// can no longer win forever. What it costs when the tick does insist is one
/// `translate_batch` of local GPU, which is the bound the batch exists to set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deference {
    /// Consolidation may take this tick's inference half.
    Polite,
    /// It has taken enough of them; this tick translates anyway.
    Insistent,
}

/// How many ticks in a row may be yielded before the sweep insists.
///
/// At a 60-second cadence that is a few minutes of deference per turn taken —
/// long enough that an ordinary consolidation burst is never interrupted, short
/// enough that a permanently busy consolidation cannot stall the sweep forever.
pub const MAX_CONSECUTIVE_YIELDS: u32 = 3;

/// Why a tick stopped early. Never a per-entry verdict — this is a condition
/// under which continuing would be wrong for every remaining entry too.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AbortReason {
    /// Reading the queue itself failed.
    QueueUnavailable(String),
    /// No usable generator, or a policy-blocked pool. Nothing was marked
    /// failed: a missing model is the environment's fault, not an entry's
    /// (ADR-0010 Decision 10, the pre-emptive lesson of D-050).
    Unavailable(String),
}

/// What one tick did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TickReport {
    /// Entries the queue offered.
    pub examined: usize,
    /// Entries settled as `english` in the batch write (no inference).
    pub settled: usize,
    /// Entries whose canon is not English and that this tick did not get to —
    /// over the batch bound, or deferred because consolidation holds the model.
    pub awaiting_translation: usize,
    /// Entries whose canon this tick rewrote into English.
    pub translated: usize,
    /// Entries whose translation was refused and recorded.
    pub failed: usize,
    /// Whether this tick skipped its inference half because a consolidation job
    /// was holding the shared local model. The detection batch still ran.
    pub yielded_to_consolidation: bool,
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
    status: NormalizationStatus,
    canon_text_sha256: String,
    source_language: Option<String>,
    attempt_count: i64,
    last_error: Option<String>,
    next_attempt_at: Option<i64>,
}

impl RowDraft {
    /// The detector found the canon already English: record that, and nothing
    /// else. There is no provenance to keep — the author's words *are* the
    /// canon (ADR-0011 §Decision 1).
    pub fn english(memory_id: &str, text: &str, class: ScriptClass) -> Self {
        RowDraft {
            memory_id: memory_id.to_string(),
            status: NormalizationStatus::English,
            canon_text_sha256: sha256_hex(text.as_bytes()),
            source_language: Some(script_label(class).to_string()),
            attempt_count: 0,
            last_error: None,
            next_attempt_at: None,
        }
    }

    /// A recorded refusal, for an entry whose canon therefore did **not**
    /// move. `attempt_count`/`next_attempt_at` are the caller's retry
    /// bookkeeping — see `record_failure`.
    pub fn failure(
        memory_id: &str,
        text: &str,
        attempt_count: i64,
        last_error: &str,
        next_attempt_at: Option<i64>,
    ) -> Self {
        RowDraft {
            memory_id: memory_id.to_string(),
            status: NormalizationStatus::Failed,
            canon_text_sha256: sha256_hex(text.as_bytes()),
            source_language: None,
            attempt_count,
            last_error: Some(last_error.to_string()),
            next_attempt_at,
        }
    }

    fn as_write(&self) -> NormalizationWrite<'_> {
        NormalizationWrite {
            memory_id: &self.memory_id,
            status: self.status,
            // Neither of this type's constructors moves the canon, so the
            // guard and the stored hash are the same value — see
            // `NormalizationWrite`'s own doc for why they are separate fields
            // at all. The writer that *does* move it is `install_canon`, which
            // builds its row through `boundary::OwnedNormalizationRow`.
            expected_text_sha256: &self.canon_text_sha256,
            canon_text_sha256: &self.canon_text_sha256,
            source_text: None,
            source_language: self.source_language.as_deref(),
            normalizer_model_id: None,
            prompt_version: None,
            normalizer_version: CURRENT_NORMALIZER_VERSION,
            attempt_count: self.attempt_count,
            last_error: self.last_error.as_deref(),
            next_attempt_at: self.next_attempt_at,
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

/// Run one pass. No sleeping and no retries of its own; a bounded amount of
/// inference, and only in the second half.
pub async fn normalization_tick(
    state_db: &StateDb,
    jobs: &JobRegistry,
    translator: &boundary::Translator,
    params: &NormalizationParams,
    deference: Deference,
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
    let other_len = other.len();
    report.awaiting_translation = other_len;

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

    let mut to_translate = other;
    if to_translate.is_empty() {
        return report;
    }

    // Consolidation holds the same process-wide model (D-054); let it have it —
    // but only so many times in a row. The detection batch above is already
    // committed (it needed no inference), so a yield costs a tick of latency and
    // nothing else; what it must not cost is the sweep never running at all.
    // See [`Deference`] for the measurement that put the bound there.
    //
    // Advisory read, not a lock: the point is politeness, not exclusion.
    if deference == Deference::Polite
        && jobs.any_running(&[JobKind::ConsolidationResume, JobKind::ConsolidationTrigger])
    {
        report.yielded_to_consolidation = true;
        return report;
    }

    if to_translate.len() > params.translate_batch {
        to_translate.truncate(params.translate_batch);
    }
    report.awaiting_translation = other_len.saturating_sub(to_translate.len());

    for (entry, _) in to_translate {
        let decided = translator.decide(&entry.memory_id, &entry.text).await;
        match decided {
            translated @ boundary::Normalized::Translated { .. } => {
                match install_canon(state_db, &entry, translated, now_ms).await {
                    InstallOutcome::Installed => {
                        report.translated += 1;
                        // Identifiers and counts only: the entry's text is the
                        // user's own writing and has no business in a log file.
                        tracing::debug!(
                            "local-rag: memory entry {} rewritten into the English canon",
                            entry.memory_id.as_str()
                        );
                    }
                    InstallOutcome::Moved => report.deferred += 1,
                    InstallOutcome::Failed(reason) => {
                        tracing::warn!(
                            "local-rag: could not install a canon for {}: {reason}",
                            entry.memory_id.as_str()
                        );
                        report.deferred += 1;
                    }
                }
            }
            // The detector said non-Latin and the translator disagreed. Record
            // what the translator saw; it is the one that just looked.
            boundary::Normalized::AlreadyEnglish { class } => {
                let draft = RowDraft::english(&entry.memory_id, &entry.text, class);
                match write_rows(state_db, vec![draft], now_ms).await {
                    Ok(_) => report.settled += 1,
                    Err(_) => report.deferred += 1,
                }
            }
            boundary::Normalized::Refused { reason, kind } => {
                if kind == TranslateFailureKind::Unavailable {
                    // Every remaining entry would fail the same way, and none of
                    // them is at fault. Stop, mark nothing.
                    report.aborted = Some(AbortReason::Unavailable(reason));
                    return report;
                }
                if record_failure(state_db, &entry, kind, &reason, now_ms).await {
                    report.failed += 1;
                } else {
                    report.deferred += 1;
                }
            }
        }
    }

    report
}

/// What [`install_canon`] did.
enum InstallOutcome {
    Installed,
    /// The entry's version moved while the translation was in flight, so the
    /// edit was refused. Not a failure of anything — the next tick re-reads it.
    Moved,
    Failed(String),
}

/// Rewrite one entry's canon into English and record where it came from — in
/// **one** `state.sqlite` transaction (`T21-17`).
///
/// Order is load-bearing, and it is the same order the write boundary uses
/// (`T21-14`). `apply_edit` deletes the entry's normalization row when the text
/// changes (`T21-07`), so the provenance row is written *after* it; the reverse
/// would write a row `apply_edit` then dropped, leaving an English canon with
/// no record of the author's words. Both take `&Transaction`, so the pair is
/// atomic without needing a store API of its own.
///
/// The rewrite is an ordinary audited `edit` by [`Actor::System`] — spec 08 §3
/// `[FIXED]` allows a text change only that way, and ADR-0011 §Decision 5 named
/// the system actor precisely so this sweep would fit the contract rather than
/// bend it. The audit row is what makes a canon rewrite visible afterwards.
async fn install_canon(
    state_db: &StateDb,
    entry: &PendingNormalization,
    decided: boundary::Normalized,
    now_ms: i64,
) -> InstallOutcome {
    let memory_id = entry.memory_id.clone();
    let english = decided.canon(&entry.text).to_string();
    let row = boundary::OwnedNormalizationRow::for_canon(&memory_id, &english, decided);
    let expected_version = entry.entry_version;

    let outcome = state_db
        .writer()
        .transaction(move |tx| {
            let applied = apply_edit(
                tx,
                &EditMemoryOp {
                    memory_id: &memory_id,
                    expected_version,
                    text: Some(&english),
                    importance: None,
                    actor: Actor::System,
                    // No key: this sweep's idempotence comes from
                    // `expected_version`, which a second run cannot match
                    // twice, and a stored key would only add a way for a
                    // *later* legitimate edit to be replayed as this one.
                    idempotency_key: None,
                },
                now_ms,
            )?;
            if let Err(e) = applied {
                return Ok(Err(e));
            }
            upsert_normalization(tx, &row.as_write(), now_ms)?;
            Ok(Ok(()))
        })
        .await;

    match outcome {
        Ok(Ok(())) => InstallOutcome::Installed,
        // An optimistic-concurrency refusal is the expected shape of "somebody
        // edited this entry while we were translating it".
        Ok(Err(MemoryOpError::OptimisticConflict { .. })) => InstallOutcome::Moved,
        Ok(Err(other)) => InstallOutcome::Failed(other.to_string()),
        Err(e) => InstallOutcome::Failed(e.to_string()),
    }
}

/// Record one entry's refusal. Returns whether a row was actually written — a
/// refused conditional write means the entry's text moved, which is not a
/// failure of anything.
async fn record_failure(
    state_db: &StateDb,
    entry: &PendingNormalization,
    kind: TranslateFailureKind,
    reason: &str,
    now_ms: i64,
) -> bool {
    let attempt_count = entry.attempt_count + 1;
    let (attempt_count, next_attempt_at) = match kind {
        // Mechanical: the same text under the same normalizer fails the same
        // way, so park it at the cap rather than spending the remaining
        // attempts proving it (D-050's dead-letter, keyed by normalizer
        // version — see this module's own doc).
        TranslateFailureKind::Mechanical => (MAX_NORMALIZATION_ATTEMPTS, None),
        TranslateFailureKind::Transient => (
            attempt_count,
            Some(now_ms + transient_backoff_delay_ms(attempt_count)),
        ),
        TranslateFailureKind::Unavailable => unreachable!("aborts the tick instead"),
    };

    let draft = RowDraft::failure(
        &entry.memory_id,
        &entry.text,
        attempt_count,
        &boundary::refusal_reason(reason, boundary::LAST_ERROR_MAX_CHARS),
        next_attempt_at,
    );
    match write_rows(state_db, vec![draft], now_ms).await {
        Ok(outcomes) => matches!(outcomes.first(), Some(UpsertOutcome::Written)),
        Err(e) => {
            tracing::warn!(
                "local-rag: could not record a normalization failure for {}: {e}",
                entry.memory_id.as_str()
            );
            false
        }
    }
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
    translator: boundary::Translator,
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
    // The bounded-deference counter lives here rather than in the tick, so the
    // tick stays a pure function of its inputs and a test can put it in either
    // state directly instead of arranging three ticks to get there.
    let mut consecutive_yields: u32 = 0;
    loop {
        tokio::select! {
            _ = &mut stop => return,
            _ = ticker.tick() => {
                let deference = if consecutive_yields < MAX_CONSECUTIVE_YIELDS {
                    Deference::Polite
                } else {
                    Deference::Insistent
                };
                let report = normalization_tick(
                    &state_db, &jobs, &translator, &params, deference, now_ms(),
                )
                .await;
                consecutive_yields = if report.yielded_to_consolidation {
                    consecutive_yields + 1
                } else {
                    0
                };
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
            Some(AbortReason::Unavailable(reason)) => {
                tracing::warn!(
                    "local-rag: memory normalization has no usable translator, \
                     so entries keep the language they were written in: {reason}"
                )
            }
            None => tracing::info!("local-rag: memory normalization resumed"),
        }
        *last_abort = report.aborted.clone();
    }
    if report.settled > 0 || report.translated > 0 || report.failed > 0 {
        tracing::info!(
            "local-rag: memory normalization tick — examined {}, settled {}, \
             translated {}, failed {}, awaiting translation {}, deferred {}",
            report.examined,
            report.settled,
            report.translated,
            report.failed,
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
