//! `T21-06`/`T21-17` acceptance tests: what one normalization tick does, and
//! what it refuses to do.
//!
//! The properties under test are the ones that cost this project real GPU when
//! their consolidation equivalents were missing (D-050/D-057): an
//! already-English store must cost **zero** generator calls, inference must be
//! bounded per tick, a deterministic failure must dead-letter instead of
//! looping, and an unavailable generator must blame no entry at all.
//!
//! `T21-17` adds the half that installs a canon, and with it the properties
//! that only a *backfill* has: the rewrite is an audited `edit` by
//! `Actor::System`, a re-run over an unchanged store writes no second edit and
//! no second audit row, and an entry that moved under a translation in flight
//! is skipped rather than overwritten.
//!
//! Deterministic: an isolated `TempHome`, fixed `now_ms` literals, a scripted
//! generator with a call counter, a `HashingEmbedder`. Nothing here sleeps.

#![cfg(unix)]

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use local_rag_core::config::DataPolicy;
use local_rag_core::paths::StoreLayout;
use local_rag_embed::{
    Embedder, FinishReason, GenError, GenRequest, GenResponse, Generator, GeneratorEntry,
    GeneratorPool, HashingEmbedder,
};
use local_rag_store::registry::RepresentationKind;
use local_rag_store::{
    Actor, CURRENT_NORMALIZER_VERSION, DEFAULT_MODEL_SPACE_ID, EditMemoryOp, GLOBAL_SCOPE_OWNER_ID,
    MemoryKind, MemoryState, NewMemoryEntry, NormalizationStatus, ScopeKind, StateDb, apply_edit,
    create_memory_entry, entries_needing_normalization, memory_entry_by_id, normalization_for,
    read_audit_events_for_entity, register_representation, set_model_space_representation,
    transition_memory_entry,
};
use local_rag_test_support::TempHome;

use local_rag::daemon::jobs::{JobKind, JobRegistry};
use local_rag::daemon::normalization::boundary::Translator;
use local_rag::daemon::normalization::{
    AbortReason, Deference, MAX_CONSECUTIVE_YIELDS, NormalizationParams, normalization_tick,
};

mod support;

const NOW: i64 = 1_000;
const MEMORY_REPRESENTATION_ID: &str = "019fec1c-0000-7000-8000-00000000000b";
const RU: &str = "Для фьюжна поиска остановились на RRF вместо линейной комбинации весов";
const EN: &str = "For search fusion we settled on RRF instead of a linear combination of weights";

struct Fixture {
    _home: TempHome,
    /// Behind an `Arc` so a test can hand the writer to a probe that runs from
    /// *inside* a tick — the only place a race against the tick can be staged
    /// deterministically.
    state: Arc<StateDb>,
    jobs: JobRegistry,
}

/// A generator that counts its calls and answers from a script; an exhausted
/// script answers with a valid translation, so a test that only cares about the
/// count does not have to enumerate them.
#[derive(Clone)]
struct CountingGenerator {
    calls: Arc<AtomicUsize>,
    answers: Arc<Mutex<Vec<Result<String, GenError>>>>,
    persistent_error: Option<GenError>,
    /// Set to run arbitrary work from *inside* a tick — the only moment at
    /// which the tick's own `JobGuard` is provably held, and the only moment at
    /// which an entry can be moved out from under a translation in flight.
    probe: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl CountingGenerator {
    fn new() -> Self {
        CountingGenerator {
            calls: Arc::new(AtomicUsize::new(0)),
            answers: Arc::new(Mutex::new(Vec::new())),
            persistent_error: None,
            probe: None,
        }
    }

    /// Fails **persistently** with `error`: the pool retries a retryable
    /// failure on its own, so a one-shot script would let the second attempt
    /// succeed and hide what the test is measuring.
    fn always_failing(error: GenError) -> Self {
        CountingGenerator {
            persistent_error: Some(error),
            ..CountingGenerator::new()
        }
    }

    fn answering(texts: Vec<&str>) -> Self {
        CountingGenerator {
            answers: Arc::new(Mutex::new(
                texts.into_iter().rev().map(|t| Ok(t.to_string())).collect(),
            )),
            ..CountingGenerator::new()
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }
}

impl Generator for CountingGenerator {
    fn generate(&self, _req: GenRequest) -> Result<GenResponse, GenError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if let Some(probe) = &self.probe {
            probe();
        }
        if let Some(error) = &self.persistent_error {
            return Err(error.clone());
        }
        match self.answers.lock().expect("lock").pop() {
            Some(Ok(text)) => Ok(GenResponse {
                text,
                finish_reason: FinishReason::Stop,
                tokens_generated: None,
            }),
            Some(Err(e)) => Err(e),
            None => Ok(GenResponse {
                text: serde_json::json!({ "en": EN }).to_string(),
                finish_reason: FinishReason::Stop,
                tokens_generated: None,
            }),
        }
    }
}

fn translator_of(generator: &CountingGenerator) -> Translator {
    Translator {
        generators: Some(Arc::new(GeneratorPool::new(vec![GeneratorEntry::local(
            "counting",
            Arc::new(generator.clone()),
        )]))),
        model_id: "counting-model".to_string(),
        policy: DataPolicy::LocalOnly,
    }
}

fn params(translate_batch: usize) -> NormalizationParams {
    NormalizationParams {
        enabled: true,
        scan_limit: 512,
        translate_batch,
    }
}

async fn fixture() -> Fixture {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let state = Arc::new(StateDb::open(layout.state_db()).expect("open state.sqlite"));

    let key = Embedder::key(&HashingEmbedder::new(RepresentationKind::Memory));
    state
        .writer()
        .transaction(move |tx| {
            let registered = register_representation(tx, MEMORY_REPRESENTATION_ID, &key, NOW)?;
            set_model_space_representation(
                tx,
                DEFAULT_MODEL_SPACE_ID,
                RepresentationKind::Memory,
                &registered,
                true,
                NOW,
            )?;
            Ok(())
        })
        .await
        .expect("register the memory representation");

    Fixture {
        _home: home,
        state,
        jobs: JobRegistry::new(),
    }
}

async fn seed(fixture: &Fixture, memory_id: &str, text: &str) {
    let (id, text) = (memory_id.to_string(), text.to_string());
    fixture
        .state
        .writer()
        .transaction(move |tx| {
            create_memory_entry(
                tx,
                &NewMemoryEntry {
                    memory_id: &id,
                    kind: MemoryKind::Fact,
                    text: &text,
                    canonical_key: None,
                    scope_kind: ScopeKind::Global,
                    scope_owner_id: GLOBAL_SCOPE_OWNER_ID,
                    confidence: 0.5,
                    importance: 0.5,
                    valid_from_tree: None,
                    last_verified_tree: None,
                    supersedes_id: None,
                },
                NOW,
            )
        })
        .await
        .expect("create tx")
        .expect("create ok");
}

async fn tick(
    fixture: &Fixture,
    generator: &CountingGenerator,
    params: &NormalizationParams,
    now_ms: i64,
) -> local_rag::daemon::normalization::TickReport {
    normalization_tick(
        &fixture.state,
        &fixture.jobs,
        &translator_of(generator),
        params,
        Deference::Polite,
        now_ms,
    )
    .await
}

/// The entry's current text, straight from `memory_entry` — the canon itself,
/// not the normalization row's account of it.
async fn canon_of(fixture: &Fixture, memory_id: &str) -> String {
    let read = fixture.state.open_read().expect("read");
    memory_entry_by_id(&read, memory_id)
        .expect("read entry")
        .expect("the entry exists")
        .text
}

/// The tick offers only entries that actually lag, and never a terminal one.
#[tokio::test]
async fn a_tick_selects_only_lagging_non_terminal_entries() {
    let fixture = fixture().await;
    seed(&fixture, "m-live", "живой текст ещё не переведён").await;
    seed(&fixture, "m-gone", "мёртвый текст ещё не переведён").await;
    fixture
        .state
        .writer()
        .transaction(|tx| transition_memory_entry(tx, "m-gone", MemoryState::Retracted))
        .await
        .expect("transition tx")
        .expect("retract ok");

    let report = tick(&fixture, &CountingGenerator::new(), &params(0), NOW).await;

    assert_eq!(report.examined, 1, "the retracted entry is never offered");
    assert_eq!(
        report.awaiting_translation, 1,
        "with a batch of zero the live Russian entry is offered and left alone",
    );
    let read = fixture.state.open_read().expect("read");
    assert!(normalization_for(&read, "m-gone").expect("row").is_none());
}

/// An all-English store converges in one tick, in one transaction, at zero
/// inference cost — ADR-0010 Decision 8 (still in force) and the reason the
/// detector exists at all. Converging matters structurally, not just for speed:
/// an entry that never settles keeps filling the queue's `LIMIT` and starves
/// the entries that need work.
#[tokio::test]
async fn two_hundred_english_entries_settle_in_one_tick_at_zero_inference_cost() {
    let fixture = fixture().await;
    for i in 0..200 {
        seed(
            &fixture,
            &format!("m-{i:03}"),
            &format!("An English memory entry number {i} about search fusion and retry storms"),
        )
        .await;
    }

    let generator = CountingGenerator::new();
    let report = tick(&fixture, &generator, &params(4), NOW).await;

    assert_eq!(report.examined, 200);
    assert_eq!(report.settled, 200, "{report:?}");
    assert_eq!(
        report.awaiting_translation, 0,
        "an already-English store has nothing to translate",
    );
    assert_eq!(
        generator.calls(),
        0,
        "ADR-0010 Decision 8: an English store costs no inference at all",
    );

    // …and the queue is empty afterwards: one tick really did converge.
    let read = fixture.state.open_read().expect("read");
    let still_pending =
        entries_needing_normalization(&read, CURRENT_NORMALIZER_VERSION, NOW, 512).expect("queue");
    assert!(still_pending.is_empty(), "{} left", still_pending.len());
}

/// A second tick over an unchanged store does nothing at all.
#[tokio::test]
async fn a_second_tick_over_an_unchanged_store_is_a_no_op() {
    let fixture = fixture().await;
    seed(&fixture, "m-1", RU).await;

    let generator = CountingGenerator::new();
    let first = tick(&fixture, &generator, &params(4), NOW).await;
    assert_eq!(first.examined, 1);
    assert_eq!(first.translated, 1);

    let second = tick(&fixture, &generator, &params(4), NOW).await;
    assert_eq!(second.examined, 0, "{second:?}");
    assert_eq!(generator.calls(), 1, "no second call for the same entry");
}

/// The headline of `T21-17`, and the acceptance moved here from `T21-14`: a
/// backfilled entry's canon **is** the English text, its provenance keeps the
/// author's words, and the rewrite is an audited `edit` by `Actor::System` at
/// the new version. Spec 08 §3 `[FIXED]` allows a text change only that way, so
/// this is the check that the sweep fits the contract instead of bending it.
#[tokio::test]
async fn a_backfilled_entry_gets_an_english_canon_and_a_system_audit_row() {
    let fixture = fixture().await;
    seed(&fixture, "m-1", RU).await;

    let generator = CountingGenerator::new();
    let report = tick(&fixture, &generator, &params(4), NOW).await;
    assert_eq!(report.translated, 1, "{report:?}");
    assert_eq!(generator.calls(), 1);

    assert_eq!(canon_of(&fixture, "m-1").await, EN, "the canon is English");

    let read = fixture.state.open_read().expect("read");
    let row = normalization_for(&read, "m-1")
        .expect("row")
        .expect("a provenance row exists");
    assert_eq!(row.status, NormalizationStatus::Translated);
    assert_eq!(
        row.source_text.as_deref(),
        Some(RU),
        "the author can still read their own words",
    );

    let audits = read_audit_events_for_entity(&read, "memory_entry", "m-1").expect("audits");
    let edit = audits
        .iter()
        .find(|a| a.op == "edit")
        .expect("the rewrite is audited");
    assert_eq!(edit.actor, Actor::System);
    assert_eq!(
        edit.entity_version, 2,
        "the audit row carries the version the edit produced",
    );
}

/// Re-running the sweep over a store it already backfilled must be free and
/// silent: no second edit, no second audit row, no second call. This is what
/// makes the worker safe to leave running, and it comes from the settled
/// provenance row rather than from any bookkeeping of its own.
#[tokio::test]
async fn a_re_run_over_a_backfilled_store_writes_no_second_edit() {
    let fixture = fixture().await;
    seed(&fixture, "m-1", RU).await;

    let generator = CountingGenerator::new();
    tick(&fixture, &generator, &params(4), NOW).await;

    for _ in 0..3 {
        let again = tick(&fixture, &generator, &params(4), NOW).await;
        assert_eq!(again.examined, 0, "{again:?}");
    }

    assert_eq!(generator.calls(), 1, "one translation, however many ticks");
    let read = fixture.state.open_read().expect("read");
    let edits = read_audit_events_for_entity(&read, "memory_entry", "m-1")
        .expect("audits")
        .into_iter()
        .filter(|a| a.op == "edit")
        .count();
    assert_eq!(edits, 1, "exactly one canon rewrite is recorded");
}

/// The race the queue's `entry_version` exists to lose safely: somebody edits
/// the entry while its translation is in flight. The sweep must skip it — an
/// author's newer text is worth more than this tick's translation of the older
/// one — and the next tick simply re-reads it.
#[tokio::test]
async fn an_entry_edited_under_a_translation_in_flight_is_skipped_not_overwritten() {
    let fixture = fixture().await;
    seed(&fixture, "m-1", RU).await;

    const MEANWHILE: &str = "текст, который автор переписал прямо во время перевода";

    // The edit runs from inside the generator call, which is the one moment the
    // translation is provably in flight. A blocking store call is safe there:
    // `Translator::decide` runs the generator on `spawn_blocking`, so the
    // runtime is free to drive the writer.
    let state = Arc::clone(&fixture.state);
    let handle = tokio::runtime::Handle::current();
    let mut generator = CountingGenerator::new();
    generator.probe = Some(Arc::new(move || {
        handle
            .block_on(state.writer().transaction(|tx| {
                apply_edit(
                    tx,
                    &EditMemoryOp {
                        memory_id: "m-1",
                        expected_version: 1,
                        text: Some(MEANWHILE),
                        importance: None,
                        actor: Actor::User,
                        idempotency_key: None,
                    },
                    NOW,
                )
            }))
            .expect("edit tx")
            .expect("the author's edit lands first");
    }));

    let report = tick(&fixture, &generator, &params(4), NOW).await;

    assert_eq!(report.translated, 0, "{report:?}");
    assert_eq!(report.deferred, 1, "the entry is left for the next tick");
    assert_eq!(
        canon_of(&fixture, "m-1").await,
        MEANWHILE,
        "the author's newer text survives the sweep untouched",
    );

    let read = fixture.state.open_read().expect("read");
    assert_eq!(
        normalization_for(&read, "m-1").expect("row"),
        None,
        "a skipped entry is not marked at all",
    );
    drop(read);

    // …and the next tick, with nothing racing it, finishes the job.
    let report = tick(&fixture, &CountingGenerator::new(), &params(4), NOW).await;
    assert_eq!(report.translated, 1, "{report:?}");
}

/// A refused translation must leave the entry exactly as its author wrote it.
/// The store keeps a note of why, and the entry keeps working — a memory that
/// disappeared because a model would not translate it would be a far worse
/// failure than one that stays in Russian.
#[tokio::test]
async fn a_refused_translation_leaves_the_authors_text_in_place() {
    let fixture = fixture().await;
    seed(&fixture, "m-1", RU).await;

    let generator = CountingGenerator::answering(vec!["not json at all"]);
    let report = tick(&fixture, &generator, &params(4), NOW).await;

    assert_eq!(report.failed, 1, "{report:?}");
    assert_eq!(report.translated, 0);
    assert_eq!(canon_of(&fixture, "m-1").await, RU, "untouched");

    let read = fixture.state.open_read().expect("read");
    let row = normalization_for(&read, "m-1")
        .expect("row")
        .expect("the refusal is recorded");
    assert_eq!(row.status, NormalizationStatus::Failed);
    assert!(row.last_error.is_some(), "with a reason a human can read");
    assert_eq!(
        read_audit_events_for_entity(&read, "memory_entry", "m-1")
            .expect("audits")
            .into_iter()
            .filter(|a| a.op == "edit")
            .count(),
        0,
        "a refusal edits nothing",
    );
}

/// Inference is bounded per tick, and the remainder is simply left for the next
/// one. `MemoryConfig.normalization_batch` is what sets this number.
#[tokio::test]
async fn the_translate_batch_bounds_inference_per_tick() {
    let fixture = fixture().await;
    for i in 0..5 {
        seed(
            &fixture,
            &format!("m-{i}"),
            &format!("русский текст номер {i}, достаточно длинный для валидатора длины"),
        )
        .await;
    }

    let generator = CountingGenerator::new();
    let report = tick(&fixture, &generator, &params(2), NOW).await;

    assert_eq!(report.examined, 5);
    assert_eq!(report.translated, 2, "{report:?}");
    assert_eq!(report.awaiting_translation, 3, "the rest waits its turn");
    assert_eq!(generator.calls(), 2, "exactly the batch, never more");
}

/// D-050's shape, in this worker's own vocabulary: a deterministic failure is
/// attempted **once** per normalizer version, however many ticks run, and a
/// version change grants exactly one more attempt.
#[tokio::test]
async fn a_mechanical_failure_is_attempted_once_per_normalizer_version() {
    let fixture = fixture().await;
    seed(&fixture, "m-1", RU).await;

    // A structurally invalid answer is a mechanical rejection, whatever the
    // model does next time.
    let generator = CountingGenerator::answering(vec!["not json at all", "not json at all"]);
    for _ in 0..3 {
        tick(&fixture, &generator, &params(4), NOW).await;
    }
    assert_eq!(
        generator.calls(),
        1,
        "three ticks, one attempt — the dead-letter holds",
    );

    let read = fixture.state.open_read().expect("read");
    let row = normalization_for(&read, "m-1")
        .expect("row")
        .expect("the failure was recorded");
    assert_eq!(row.status, NormalizationStatus::Failed);
    assert_eq!(row.normalizer_version, CURRENT_NORMALIZER_VERSION);

    // A newer normalizer re-queues it — the escape hatch, and the only one.
    let due = entries_needing_normalization(&read, CURRENT_NORMALIZER_VERSION + 1, NOW, 10)
        .expect("queue");
    assert_eq!(due.len(), 1, "a normalizer bump grants one more attempt");
}

/// A transient failure backs off on the store's own curve — asserted by
/// arithmetic on a fixed clock, never by sleeping.
#[tokio::test]
async fn a_transient_failure_backs_off_without_sleeping() {
    let fixture = fixture().await;
    seed(&fixture, "m-1", RU).await;

    let generator = CountingGenerator::always_failing(GenError::retryable("model busy"));
    let report = tick(&fixture, &generator, &params(4), NOW).await;
    assert_eq!(report.failed, 1, "{report:?}");

    let read = fixture.state.open_read().expect("read");
    let row = normalization_for(&read, "m-1")
        .expect("row")
        .expect("recorded");
    assert_eq!(row.status, NormalizationStatus::Failed);
    assert_eq!(row.attempt_count, 1);
    let next = row.next_attempt_at.expect("a transient failure backs off");
    assert!(
        next >= NOW,
        "next_attempt_at {next} must not be in the past"
    );

    // Before the deadline the queue withholds it; after, it is due again.
    assert!(
        entries_needing_normalization(&read, CURRENT_NORMALIZER_VERSION, next - 1, 10)
            .expect("queue")
            .is_empty(),
    );
    assert_eq!(
        entries_needing_normalization(&read, CURRENT_NORMALIZER_VERSION, next, 10)
            .expect("queue")
            .len(),
        1,
    );
}

/// A missing generator is not an entry's fault: the tick stops, and not one
/// row is written — no status, no attempt (ADR-0010 Decision 10).
#[tokio::test]
async fn an_unavailable_generator_aborts_the_tick_and_blames_no_entry() {
    let fixture = fixture().await;
    seed(&fixture, "m-1", RU).await;
    seed(
        &fixture,
        "m-2",
        "второй русский текст, тоже подлежащий переводу",
    )
    .await;

    let report = normalization_tick(
        &fixture.state,
        &fixture.jobs,
        &Translator {
            generators: Some(Arc::new(GeneratorPool::new(Vec::new()))),
            model_id: "none".to_string(),
            policy: DataPolicy::LocalOnly,
        },
        &params(4),
        Deference::Polite,
        NOW,
    )
    .await;

    assert!(
        matches!(report.aborted, Some(AbortReason::Unavailable(_))),
        "{report:?}",
    );
    assert_eq!(report.failed, 0);
    let read = fixture.state.open_read().expect("read");
    for id in ["m-1", "m-2"] {
        assert_eq!(
            normalization_for(&read, id).expect("row"),
            None,
            "{id} must not be marked at all",
        );
    }
}

/// The shared local model has one owner at a time and consolidation is the
/// latency-sensitive one (D-054: the llama backend is a process-wide
/// singleton). A tick that starts while a consolidation job is registered still
/// commits its detection batch — pure detection, `state.sqlite` only — and
/// defers every translation instead of queueing behind consolidation.
#[tokio::test]
async fn a_running_consolidation_job_takes_the_inference_half_of_the_tick() {
    let fixture = fixture().await;
    seed(&fixture, "m-en", EN).await;
    seed(&fixture, "m-ru", RU).await;

    let generator = CountingGenerator::new();
    let consolidating = fixture.jobs.begin(JobKind::ConsolidationTrigger);
    let report = tick(&fixture, &generator, &params(4), NOW).await;
    drop(consolidating);

    assert_eq!(report.examined, 2);
    assert_eq!(report.settled, 1, "the free half still runs");
    assert_eq!(report.translated, 0, "the model belongs to consolidation");
    assert!(report.yielded_to_consolidation);
    assert_eq!(generator.calls(), 0, "not one inference call was queued");

    let read = fixture.state.open_read().expect("read");
    assert_eq!(
        normalization_for(&read, "m-en")
            .expect("row")
            .expect("m-en is settled")
            .status,
        NormalizationStatus::English,
    );
    assert!(
        normalization_for(&read, "m-ru").expect("row").is_none(),
        "a deferred entry is left untouched, not marked",
    );
    drop(read);

    // And the very next tick, with nothing holding the model, finishes the job.
    let report = tick(&fixture, &CountingGenerator::new(), &params(4), NOW).await;
    assert_eq!(report.translated, 1);
    assert!(!report.yielded_to_consolidation);
}

/// Politeness has a floor. Consolidation on this project's own store is not
/// bursty — it runs continuously against a candidate backlog that grows — so an
/// unbounded yield is starvation with a nicer name: `T21-17`'s live acceptance
/// watched six ticks in a row settle new entries and translate **zero**, with
/// `awaiting translation` pinned at 19. Past the bound the tick insists, and
/// what it spends is one `translate_batch`, not the whole backlog.
#[tokio::test]
async fn an_insistent_tick_translates_even_while_consolidation_runs() {
    let fixture = fixture().await;
    seed(&fixture, "m-ru", RU).await;

    let generator = CountingGenerator::new();
    let consolidating = fixture.jobs.begin(JobKind::ConsolidationTrigger);
    let report = normalization_tick(
        &fixture.state,
        &fixture.jobs,
        &translator_of(&generator),
        &params(4),
        Deference::Insistent,
        NOW,
    )
    .await;
    drop(consolidating);

    assert!(!report.yielded_to_consolidation, "{report:?}");
    assert_eq!(report.translated, 1);
    assert_eq!(generator.calls(), 1);
    assert_eq!(canon_of(&fixture, "m-ru").await, EN);
}

/// The counter that turns `Polite` into `Insistent` lives in the worker loop,
/// so proving the bound *works* means driving the real loop — with a
/// consolidation job held for the whole test, exactly the condition the live
/// store was in. Waits on the outcome rather than on a duration: a fixed sleep
/// would either be flaky or be testing the clock.
#[tokio::test]
async fn the_worker_stops_yielding_after_the_bound_and_drains_the_backlog() {
    let fixture = fixture().await;
    seed(&fixture, "m-ru", RU).await;

    // Held for the whole test: consolidation never lets go, which is precisely
    // the case an unbounded yield could not survive.
    let _consolidating = fixture.jobs.begin(JobKind::ConsolidationTrigger);

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let worker = tokio::spawn(local_rag::daemon::normalization::run_normalization_worker(
        Arc::clone(&fixture.state),
        fixture.jobs.clone(),
        translator_of(&CountingGenerator::new()),
        params(4),
        std::time::Duration::from_millis(10),
        || NOW,
        stop_rx,
    ));

    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while canon_of(&fixture, "m-ru").await != EN {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("a permanently busy consolidation must not stall the sweep forever");

    stop_tx.send(()).expect("the worker is listening");
    worker.await.expect("the worker task must not panic");
}

/// The bound is a small number of ticks, not a token gesture and not a
/// pretence. Asserted because a bound nobody checks drifts: at a 60-second
/// cadence this is the difference between "a few minutes of deference" and
/// "the sweep runs once an hour".
#[test]
fn the_deference_bound_stays_a_handful_of_ticks() {
    assert!(
        (1..=5).contains(&MAX_CONSECUTIVE_YIELDS),
        "MAX_CONSECUTIVE_YIELDS = {MAX_CONSECUTIVE_YIELDS} is outside the range the \
         doc justifies — change the doc deliberately, not the constant quietly",
    );
}

/// The other half of `the_job_guard_covers_the_tick_and_nothing_more`: idle
/// eligibility is false *during* a tick. Observed from inside the generator
/// call, which is the one moment the tick is provably mid-work — the debt
/// `T21-13` recorded when it removed this test along with the inference call it
/// hooked, now that `T21-17` has put a real one back.
#[tokio::test]
async fn the_daemon_is_not_idle_eligible_while_a_tick_is_working() {
    let fixture = fixture().await;
    seed(&fixture, "m-1", RU).await;

    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut generator = CountingGenerator::new();
    let (jobs, sink) = (fixture.jobs.clone(), Arc::clone(&seen));
    generator.probe = Some(Arc::new(move || {
        sink.lock().expect("lock").push(jobs.len());
    }));

    assert!(fixture.jobs.is_empty(), "idle before");
    let report = tick(&fixture, &generator, &params(4), NOW).await;
    assert_eq!(report.translated, 1);
    assert!(fixture.jobs.is_empty(), "idle after");

    assert_eq!(
        seen.lock().expect("lock").as_slice(),
        &[1],
        "a tick mid-work must be visible to the idle gate as a running job",
    );
}

/// A disabled worker touches nothing — not the store, not the registry, not
/// the generator. Not the shipped state any more (`T21-17` turned the switch
/// back on), which is exactly why the test now sets it explicitly.
#[tokio::test]
async fn a_disabled_worker_does_nothing_at_all() {
    let fixture = fixture().await;
    seed(&fixture, "m-1", RU).await;

    let generator = CountingGenerator::new();
    let disabled = NormalizationParams {
        enabled: false,
        ..NormalizationParams::default()
    };
    let report = tick(&fixture, &generator, &disabled, NOW).await;

    assert_eq!(report, Default::default());
    assert_eq!(generator.calls(), 0);
    assert!(fixture.jobs.is_empty(), "no job was ever registered");
    let read = fixture.state.open_read().expect("read");
    assert_eq!(normalization_for(&read, "m-1").expect("row"), None);
}

/// The job guard is held for the tick and released with it — otherwise a
/// daemon would either shut down mid-translation or never look idle again.
#[tokio::test]
async fn the_job_guard_covers_the_tick_and_nothing_more() {
    let fixture = fixture().await;
    seed(&fixture, "m-1", RU).await;
    assert!(fixture.jobs.is_empty(), "idle before the tick");

    tick(&fixture, &CountingGenerator::new(), &params(4), NOW).await;

    assert!(fixture.jobs.is_empty(), "idle again after the tick");
    assert_eq!(fixture.jobs.len(), 0);
}

/// Memory text is the user's own writing. A background worker has no business
/// copying it into a log file.
#[tokio::test]
async fn no_log_line_ever_carries_an_entry_text() {
    use std::io::Write;

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("lock").extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl tracing_subscriber::fmt::MakeWriter<'_> for Capture {
        type Writer = Capture;
        fn make_writer(&self) -> Self::Writer {
            self.clone()
        }
    }

    let fixture = fixture().await;
    seed(&fixture, "m-1", RU).await;
    seed(
        &fixture,
        "m-2",
        "английский тут ни при чём, это тоже кириллица",
    )
    .await;

    let buf = Capture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(buf.clone())
        .with_max_level(tracing::Level::TRACE)
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);

    tick(&fixture, &CountingGenerator::new(), &params(4), NOW).await;
    drop(guard);

    let logged = String::from_utf8(buf.0.lock().expect("lock").clone()).expect("utf8");
    for fragment in [RU, "английский тут ни при чём", EN] {
        assert!(
            !logged.contains(fragment),
            "a log line leaked entry text: {logged}",
        );
    }
}

/// `oneshot` stop is immediate: the worker must not sit out its poll interval
/// before noticing. The interval here is an hour, so a worker that waits for
/// the next tick fails this test by timing out rather than by luck.
#[tokio::test]
async fn a_stop_signal_ends_the_worker_without_waiting_for_the_next_tick() {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let state = Arc::new(StateDb::open(layout.state_db()).expect("open state.sqlite"));
    let jobs = JobRegistry::new();

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();

    let worker = tokio::spawn(local_rag::daemon::normalization::run_normalization_worker(
        Arc::clone(&state),
        jobs.clone(),
        translator_of(&CountingGenerator::new()),
        params(4),
        std::time::Duration::from_secs(3600),
        || NOW,
        stop_rx,
    ));

    stop_tx.send(()).expect("worker is listening");
    tokio::time::timeout(std::time::Duration::from_secs(5), worker)
        .await
        .expect("a stop signal must end the worker without waiting for the next tick")
        .expect("the worker task must not panic");

    assert!(jobs.is_empty(), "no job outlives the worker");
}

/// Spec 02 §4.3 `[FIXED]`: a background worker that is merely *alive* must not
/// keep the daemon from exiting when idle. Driven through the real
/// `DaemonHandle::start` — with the worker enabled and ticking, unlike the
/// shipped default — so it is the wiring under test, not the predicate.
#[tokio::test]
async fn the_normalization_worker_alive_between_ticks_does_not_block_idle_shutdown() {
    let (_home, layout) = support::open_layout();
    let mut opts = support::start_options(layout);
    opts.normalization.enabled = true;
    opts.normalization_poll_interval = std::time::Duration::from_millis(10);

    let handle = local_rag::daemon::DaemonHandle::start(opts)
        .await
        .expect("start");

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !handle.is_idle_eligible() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("a worker that only ticks must never hold the daemon awake");

    tokio::time::timeout(std::time::Duration::from_secs(5), handle.shutdown())
        .await
        .expect("shutdown must complete within the bound with the T21-06 worker alive");
}

/// D-054, structurally: llama.cpp's backend handle is a process-wide singleton,
/// so the *only* safe number of `build_best_effort_pool` calls in a daemon
/// process is one — every consumer takes an `Arc` clone of that one pool. The
/// runtime symptom of a second call (an empty pool for the rest of the uptime,
/// recoverable only by restart, and only on a coin flip) is exactly the kind
/// that no unit test observes, so the invariant is asserted on the source.
#[test]
fn the_generator_pool_is_built_once_per_process() {
    let source = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/daemon/lifecycle.rs"
    ))
    .expect("read lifecycle.rs");

    let calls = source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .filter(|line| line.contains("build_best_effort_pool(&"))
        .count();

    assert_eq!(
        calls, 1,
        "lifecycle must build the generator pool exactly once and share the `Arc` (D-054)",
    );
}
