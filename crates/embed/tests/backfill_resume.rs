//! T11-04: the resumable half of the backfill card — "crash each batch resumes".
//!
//! Spec 10 §4 step 2 requires the worker to be "batch, resumable"; spec 10 §3
//! `[FIXED]` is what makes that possible without a journal ("coverage … always
//! recomputable from `state.sqlite` × `embedding_cache`"). These tests kill the
//! worker at the named crash point that fires **after** a non-empty cache-write
//! transaction commits — the same placement `local_rag_store::retention`'s sweep
//! uses — and prove three things:
//!
//! 1. committed batches survive the kill;
//! 2. the next run redoes **none** of them and finishes the remainder;
//! 3. a further run is a no-op, i.e. the sequence converges.
//!
//! Serialized on a `tokio::sync::Mutex`: the failpoint registry is
//! process-global, and every test here arms it (or is vulnerable to another
//! test's arming) across `.await` points — the discipline D-005 established.
#![cfg(feature = "failpoints")]

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use local_rag_core::config::DataPolicy;
use local_rag_embed::{
    BackfillError, BackfillParams, EmbedError, EmbedRequest, Embedder, HashingEmbedder, InFlight,
    ProviderEntry, ProviderPool, Vector, run_backfill,
};
use local_rag_store::registry::{DEFAULT_MODEL_SPACE_ID, RepresentationKind};
use local_rag_store::{RepresentationKey, RetentionParams, all_embedding_meta};
use local_rag_test_support::{Action, failpoint::global};
use tokio::sync::Mutex;

use support::store::{self, Fixture, NOW};

const BETWEEN_BATCHES: &str = "embed.backfill.between_batches";

/// Serializes every test below against the process-global failpoint registry.
static SERIAL: Mutex<()> = Mutex::const_new(());

const BODIES: [&str; 6] = [
    "fn one() -> u8 { 1 }",
    "fn two() -> u8 { 2 }",
    "fn three() -> u8 { 3 }",
    "fn four() -> u8 { 4 }",
    "fn five() -> u8 { 5 }",
    "fn six() -> u8 { 6 }",
];

/// A `HashingEmbedder` that counts how many texts it has embedded.
struct CountingEmbedder {
    inner: HashingEmbedder,
    texts: AtomicUsize,
}

impl CountingEmbedder {
    fn new() -> Arc<Self> {
        Arc::new(CountingEmbedder {
            inner: HashingEmbedder::new(RepresentationKind::CodeRaw),
            texts: AtomicUsize::new(0),
        })
    }

    fn texts(&self) -> usize {
        self.texts.load(Ordering::SeqCst)
    }
}

impl Embedder for CountingEmbedder {
    fn embed(&self, req: EmbedRequest) -> Result<Vec<Vector>, EmbedError> {
        self.texts.fetch_add(req.texts.len(), Ordering::SeqCst);
        self.inner.embed(req)
    }

    fn key(&self) -> RepresentationKey {
        self.inner.key()
    }
}

fn retention() -> RetentionParams {
    RetentionParams {
        keep_last_k: 2,
        window_ms: 7 * 24 * 60 * 60 * 1000,
    }
}

/// One backfill run with one-subject batches, so the crash point is reachable
/// after each individual row.
async fn run_one(
    f: &Fixture,
    embedder: Arc<CountingEmbedder>,
    now_ms: i64,
) -> Result<local_rag_embed::BackfillReport, BackfillError> {
    let pool = ProviderPool::new(vec![ProviderEntry::local("local", embedder)]);
    run_backfill(
        &f.state,
        &f.cache,
        &pool,
        DataPolicy::LocalOnly,
        DEFAULT_MODEL_SPACE_ID,
        &BackfillParams {
            embed_batch: 1,
            write_batch_rows: 1,
        },
        &retention(),
        &InFlight::new(),
        now_ms,
    )
    .await
}

fn cached(f: &Fixture) -> usize {
    let read = f.cache.open_read().expect("cache read");
    all_embedding_meta(&read).expect("meta").len()
}

/// Killed after its first committed batch, the run resumes exactly where it
/// stopped: nothing is re-embedded, nothing is lost, and the sequence converges.
#[tokio::test(flavor = "multi_thread")]
async fn a_kill_between_batches_resumes_without_redoing_work() {
    let _serial = SERIAL.lock().await;
    let f = store::seeded(&BODIES).await;
    let expected = f.distinct_blobs();
    assert_eq!(expected, BODIES.len(), "each body is a distinct subject");

    // 1. Kill after the first committed batch.
    global().reset();
    let embedder = CountingEmbedder::new();
    global().register(BETWEEN_BATCHES);
    global().arm(BETWEEN_BATCHES, Action::Error).expect("armed");
    let err = run_one(&f, embedder.clone(), NOW)
        .await
        .expect_err("the crash point fires");
    assert!(
        matches!(err, BackfillError::Interrupted),
        "expected Interrupted, got {err}"
    );
    global().disarm(BETWEEN_BATCHES).expect("declared");

    let after_kill = cached(&f);
    assert_eq!(after_kill, 1, "the committed batch survived the kill");
    assert_eq!(
        embedder.texts(),
        1,
        "no work beyond the committed batch was done"
    );

    // 2. Resume: the remaining subjects, and only those.
    let resumed = CountingEmbedder::new();
    let report = run_one(&f, resumed.clone(), NOW + 1)
        .await
        .expect("resume completes");
    assert_eq!(
        resumed.texts(),
        expected - after_kill,
        "the resumed run embeds only what is still missing"
    );
    assert_eq!(
        report.reused, after_kill as u64,
        "committed rows are reused"
    );
    assert_eq!(report.embedded, (expected - after_kill) as u64);
    assert_eq!(cached(&f), expected);
    assert!(
        report
            .coverage
            .fully_covered(&[RepresentationKind::CodeRaw])
    );

    // 3. Converged: a third run does nothing at all.
    let idle = CountingEmbedder::new();
    let report = run_one(&f, idle.clone(), NOW + 2).await.expect("third run");
    assert_eq!(idle.texts(), 0);
    assert_eq!(report.embedded, 0);
    assert_eq!(report.reused, expected as u64);
    assert_eq!(cached(&f), expected);
}

/// Killing at *every* batch boundary still converges: each run commits one more
/// subject, and the totals never double-count.
#[tokio::test(flavor = "multi_thread")]
async fn killing_at_each_batch_still_converges() {
    let _serial = SERIAL.lock().await;
    let f = store::seeded(&BODIES).await;
    let expected = f.distinct_blobs();

    global().reset();
    let mut embedded_total = 0usize;
    for round in 0..expected {
        global().register(BETWEEN_BATCHES);
        global().arm(BETWEEN_BATCHES, Action::Error).expect("armed");
        let embedder = CountingEmbedder::new();
        let err = run_one(&f, embedder.clone(), NOW + round as i64)
            .await
            .expect_err("killed every round");
        assert!(matches!(err, BackfillError::Interrupted), "{err}");
        global().disarm(BETWEEN_BATCHES).expect("declared");

        // Exactly one new subject per round — the run embeds one batch, commits
        // it, and dies at the crash point.
        assert_eq!(
            embedder.texts(),
            1,
            "round {round} must embed exactly one subject"
        );
        embedded_total += 1;
        assert_eq!(cached(&f), embedded_total, "round {round}");
    }

    // Everything is in place; the next run is a pure no-op that reports full
    // coverage.
    let idle = CountingEmbedder::new();
    let report = run_one(&f, idle.clone(), NOW + 100)
        .await
        .expect("final run");
    assert_eq!(idle.texts(), 0);
    assert_eq!(report.reused, expected as u64);
    assert!(
        report
            .coverage
            .fully_covered(&[RepresentationKind::CodeRaw])
    );
}

/// The crash point is declared even when it never fires, so `arm` on a healthy
/// build cannot silently target a typo (the registry is strict about unknown
/// names).
#[tokio::test(flavor = "multi_thread")]
async fn the_crash_point_is_registered_by_a_normal_run() {
    let _serial = SERIAL.lock().await;
    let f = store::seeded(&BODIES).await;

    global().reset();
    run_one(&f, CountingEmbedder::new(), NOW)
        .await
        .expect("healthy run");

    assert!(
        global().is_declared(BETWEEN_BATCHES),
        "a completed run must have registered {BETWEEN_BATCHES}"
    );
}
