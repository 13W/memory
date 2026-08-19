//! T21-06 acceptance tests: what one normalization tick does, and what it
//! refuses to do.
//!
//! The properties under test are the ones that cost this project real GPU when
//! their consolidation equivalents were missing (D-050/D-057): an
//! already-English store must cost **zero** generator calls, inference must be
//! bounded per tick, a deterministic failure must dead-letter instead of
//! looping, and an unavailable generator must blame no entry at all.
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
    CURRENT_NORMALIZER_VERSION, CacheDb, DEFAULT_MODEL_SPACE_ID, GLOBAL_SCOPE_OWNER_ID, MemoryKind,
    MemoryState, NewMemoryEntry, NormalizationStatus, ScopeKind, StateDb, create_memory_entry,
    entries_needing_normalization, normalization_for, register_representation,
    set_model_space_representation, transition_memory_entry,
};
use local_rag_test_support::TempHome;

use local_rag::daemon::jobs::{JobKind, JobRegistry};
use local_rag::daemon::normalization::{AbortReason, NormalizationParams, normalization_tick};

mod support;

const NOW: i64 = 1_000;
const STORE_UUID: &str = "01a00000-0000-7000-8000-00000000fffe";
const MEMORY_REPRESENTATION_ID: &str = "019fec1c-0000-7000-8000-00000000000b";
const RU: &str = "Для фьюжна поиска остановились на RRF вместо линейной комбинации весов";
const EN: &str = "For search fusion we settled on RRF instead of a linear combination of weights";

struct Fixture {
    _home: TempHome,
    state: StateDb,
    cache: CacheDb,
    jobs: JobRegistry,
}

/// A generator that counts its calls and answers from a script; an exhausted
/// script answers with a valid translation, so a test that only cares about the
/// count does not have to enumerate them.
#[derive(Debug, Clone)]
struct CountingGenerator {
    calls: Arc<AtomicUsize>,
    answers: Arc<Mutex<Vec<Result<String, GenError>>>>,
    persistent_error: Option<GenError>,
    /// Set to observe the job registry from *inside* a tick — the only moment
    /// at which the tick's own `JobGuard` is provably held.
    probe: Option<(JobRegistry, Arc<Mutex<Vec<usize>>>)>,
}

impl CountingGenerator {
    fn new() -> Self {
        Self {
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
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            answers: Arc::new(Mutex::new(Vec::new())),
            persistent_error: Some(error),
            probe: None,
        }
    }

    fn answering(texts: Vec<&str>) -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            answers: Arc::new(Mutex::new(
                texts.into_iter().rev().map(|t| Ok(t.to_string())).collect(),
            )),
            persistent_error: None,
            probe: None,
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }
}

impl Generator for CountingGenerator {
    fn generate(&self, _req: GenRequest) -> Result<GenResponse, GenError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if let Some((jobs, seen)) = &self.probe {
            seen.lock().expect("lock").push(jobs.len());
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

fn pool_of(generator: &CountingGenerator) -> GeneratorPool {
    GeneratorPool::new(vec![GeneratorEntry::local(
        "counting",
        Arc::new(generator.clone()),
    )])
}

fn embedder() -> Option<Arc<dyn Embedder>> {
    Some(Arc::new(HashingEmbedder::new(RepresentationKind::Memory)))
}

fn params(translate_batch: usize) -> NormalizationParams {
    NormalizationParams {
        enabled: true,
        translate_batch,
        scan_limit: 512,
    }
}

async fn fixture() -> Fixture {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
    let cache = CacheDb::open(layout.cache_db(), STORE_UUID).expect("open cache.sqlite");

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
        cache,
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
        &fixture.cache,
        &pool_of(generator),
        embedder(),
        &fixture.jobs,
        params,
        DataPolicy::LocalOnly,
        now_ms,
    )
    .await
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

    let generator = CountingGenerator::new();
    let report = tick(&fixture, &generator, &params(10), NOW).await;

    assert_eq!(report.examined, 1, "the retracted entry is never offered");
    assert_eq!(report.translated, 1);
    let read = fixture.state.open_read().expect("read");
    assert!(normalization_for(&read, "m-gone").expect("row").is_none());
}

/// An all-English store converges in one tick at **zero** inference cost —
/// ADR-0010 Decision 8, and the reason the detector exists at all.
#[tokio::test]
async fn two_hundred_english_entries_cost_zero_generator_calls() {
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
    assert_eq!(report.passthrough, 200, "{report:?}");
    assert_eq!(report.translated, 0);
    assert_eq!(
        generator.calls(),
        0,
        "an already-English store must never reach the generator",
    );

    // …and the queue is empty afterwards: one tick really did converge.
    let read = fixture.state.open_read().expect("read");
    let still_pending =
        entries_needing_normalization(&read, CURRENT_NORMALIZER_VERSION, NOW, 512).expect("queue");
    assert!(still_pending.is_empty(), "{} left", still_pending.len());
}

/// Inference is bounded per tick, and the remainder is simply deferred.
#[tokio::test]
async fn the_translate_batch_bounds_inference_per_tick() {
    let fixture = fixture().await;
    for i in 0..5 {
        seed(
            &fixture,
            &format!("m-{i}"),
            &format!("русский текст номер {i}"),
        )
        .await;
    }

    let generator = CountingGenerator::new();
    let report = tick(&fixture, &generator, &params(2), NOW).await;

    assert_eq!(report.examined, 5);
    assert_eq!(report.translated, 2);
    assert_eq!(report.deferred, 3);
    assert_eq!(generator.calls(), 2, "exactly the batch, never more");
}

/// A second tick over an unchanged store does nothing at all.
#[tokio::test]
async fn a_second_tick_over_an_unchanged_store_is_a_no_op() {
    let fixture = fixture().await;
    seed(&fixture, "m-1", RU).await;

    let generator = CountingGenerator::new();
    let first = tick(&fixture, &generator, &params(4), NOW).await;
    assert_eq!(first.translated, 1);

    let second = tick(&fixture, &generator, &params(4), NOW).await;
    assert_eq!(second.examined, 0, "{second:?}");
    assert_eq!(generator.calls(), 1, "no second call for the same entry");
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
/// row is written — no status, no attempt.
#[tokio::test]
async fn an_unavailable_generator_aborts_the_tick_and_blames_no_entry() {
    let fixture = fixture().await;
    seed(&fixture, "m-1", RU).await;
    seed(&fixture, "m-2", "второй русский текст").await;

    let empty_pool = GeneratorPool::new(Vec::new());
    let report = normalization_tick(
        &fixture.state,
        &fixture.cache,
        &empty_pool,
        embedder(),
        &fixture.jobs,
        &params(4),
        DataPolicy::LocalOnly,
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

/// A disabled worker touches nothing — not the store, not the registry, not
/// the generator. This is the state T21-06 ships in.
#[tokio::test]
async fn a_disabled_worker_does_nothing_at_all() {
    let fixture = fixture().await;
    seed(&fixture, "m-1", RU).await;

    let generator = CountingGenerator::new();
    let report = tick(&fixture, &generator, &NormalizationParams::default(), NOW).await;

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

    let generator = CountingGenerator::new();
    tick(&fixture, &generator, &params(4), NOW).await;

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

    let generator = CountingGenerator::new();
    tick(&fixture, &generator, &params(4), NOW).await;
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
    let cache = Arc::new(CacheDb::open(layout.cache_db(), STORE_UUID).expect("open cache.sqlite"));
    let jobs = JobRegistry::new();

    let generator = CountingGenerator::new();
    let embedder: Arc<dyn Embedder> = Arc::new(HashingEmbedder::new(RepresentationKind::Memory));
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();

    let worker = tokio::spawn(local_rag::daemon::normalization::run_normalization_worker(
        Arc::clone(&state),
        Arc::clone(&cache),
        Arc::new(pool_of(&generator)),
        move || Some(Arc::clone(&embedder)),
        jobs.clone(),
        params(4),
        DataPolicy::LocalOnly,
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

/// The shared local model has one owner at a time and consolidation is the
/// latency-sensitive one (D-054: the llama backend is a process-wide
/// singleton). A tick that starts while a consolidation job is registered still
/// commits its passthrough batch — pure detection, `state.sqlite` only — and
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
    assert_eq!(report.passthrough, 1, "the free half still runs");
    assert_eq!(report.translated, 0, "the model belongs to consolidation");
    assert_eq!(report.deferred, 1);
    assert!(report.yielded_to_consolidation);
    assert_eq!(generator.calls(), 0, "not one inference call was queued");

    let read = fixture.state.open_read().expect("read");
    assert_eq!(
        normalization_for(&read, "m-en")
            .expect("row")
            .expect("m-en is settled")
            .status,
        NormalizationStatus::Skipped,
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

/// The other half of `the_job_guard_covers_the_tick_and_nothing_more`: idle
/// eligibility is false *during* a tick. Observed from inside the generator
/// call, which is the one moment the tick is provably mid-work.
#[tokio::test]
async fn the_daemon_is_not_idle_eligible_while_a_tick_is_working() {
    let fixture = fixture().await;
    seed(&fixture, "m-1", RU).await;

    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut generator = CountingGenerator::new();
    generator.probe = Some((fixture.jobs.clone(), Arc::clone(&seen)));

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
