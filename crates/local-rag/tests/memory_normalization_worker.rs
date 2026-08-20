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

use local_rag_core::paths::StoreLayout;
use local_rag_embed::{Embedder, HashingEmbedder};
use local_rag_store::registry::RepresentationKind;
use local_rag_store::{
    CURRENT_NORMALIZER_VERSION, DEFAULT_MODEL_SPACE_ID, GLOBAL_SCOPE_OWNER_ID, MemoryKind,
    MemoryState, NewMemoryEntry, ScopeKind, StateDb, create_memory_entry,
    entries_needing_normalization, normalization_for, register_representation,
    set_model_space_representation, transition_memory_entry,
};
use local_rag_test_support::TempHome;

use local_rag::daemon::jobs::JobRegistry;
use local_rag::daemon::normalization::{NormalizationParams, normalization_tick};

mod support;

const NOW: i64 = 1_000;
const MEMORY_REPRESENTATION_ID: &str = "019fec1c-0000-7000-8000-00000000000b";
const RU: &str = "Для фьюжна поиска остановились на RRF вместо линейной комбинации весов";
const EN: &str = "For search fusion we settled on RRF instead of a linear combination of weights";

struct Fixture {
    _home: TempHome,
    state: StateDb,
    jobs: JobRegistry,
}

fn params() -> NormalizationParams {
    NormalizationParams {
        enabled: true,
        scan_limit: 512,
    }
}

async fn fixture() -> Fixture {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let state = StateDb::open(layout.state_db()).expect("open state.sqlite");

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
    params: &NormalizationParams,
    now_ms: i64,
) -> local_rag::daemon::normalization::TickReport {
    normalization_tick(&fixture.state, &fixture.jobs, params, now_ms).await
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

    let report = tick(&fixture, &params(), NOW).await;

    assert_eq!(report.examined, 1, "the retracted entry is never offered");
    assert_eq!(
        report.awaiting_translation, 1,
        "the live entry is Russian: this worker leaves it for the boundary",
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
async fn two_hundred_english_entries_settle_in_one_tick() {
    let fixture = fixture().await;
    for i in 0..200 {
        seed(
            &fixture,
            &format!("m-{i:03}"),
            &format!("An English memory entry number {i} about search fusion and retry storms"),
        )
        .await;
    }

    let report = tick(&fixture, &params(), NOW).await;

    assert_eq!(report.examined, 200);
    assert_eq!(report.settled, 200, "{report:?}");
    assert_eq!(
        report.awaiting_translation, 0,
        "an already-English store has nothing for the boundary to translate",
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

    // A non-English entry is examined and left for the boundary (T21-14): this
    // worker settles only what is already English, so the queue keeps offering
    // it — which is correct, and is exactly why `awaiting_translation` is a
    // reported number rather than a silence.
    let first = tick(&fixture, &params(), NOW).await;
    assert_eq!(first.examined, 1);
    assert_eq!(first.awaiting_translation, 1);
    assert_eq!(first.settled, 0);

    let second = tick(&fixture, &params(), NOW).await;
    assert_eq!(
        second, first,
        "an unchanged store gives an identical verdict"
    );
}

/// A disabled worker touches nothing — not the store, not the registry, not
/// the generator. This is the state T21-06 ships in.
#[tokio::test]
async fn a_disabled_worker_does_nothing_at_all() {
    let fixture = fixture().await;
    seed(&fixture, "m-1", RU).await;

    let report = tick(&fixture, &NormalizationParams::default(), NOW).await;

    assert_eq!(report, Default::default());
    assert!(fixture.jobs.is_empty(), "no job was ever registered");
    let read = fixture.state.open_read().expect("read");
    assert_eq!(normalization_for(&read, "m-1").expect("row"), None);
}

/// The job guard is held for the tick and released with it — otherwise a
/// daemon would either shut down mid-translation or never look idle again.
#[tokio::test]
async fn the_job_guard_covers_the_tick_and_nothing_more() {
    // T21-13 note: the companion test that observed the registry *mid*-tick
    // hooked the generator call to do it. This worker no longer makes one, and
    // the honest replacement — racing another task against the tick — would be
    // timing-dependent. The mid-work observation returns with T21-14, which
    // puts a real inference call back on a path worth watching; until then the
    // before/after invariant below is what is actually checked.
    let fixture = fixture().await;
    seed(&fixture, "m-1", RU).await;
    assert!(fixture.jobs.is_empty(), "idle before the tick");

    tick(&fixture, &params(), NOW).await;

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

    tick(&fixture, &params(), NOW).await;
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
        params(),
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
