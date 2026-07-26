//! T11-04 acceptance tests: resumable coverage backfill (spec 10 §3/§4 step 2,
//! 04 §3, 03 §4.2/§4.4).
//!
//! Every test drives the real worker against real `state.sqlite` +
//! `cache.sqlite` in an isolated [`TempHome`]: the expected subject set comes
//! from real pin roots, texts from real `source_blob` rows, vectors from the real
//! provider pool. Deterministic — fixed `now_ms`, ids from `uuidv7_from` with
//! pinned entropy, a counting embedder instead of a clock, no network, no sleeps.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use local_rag_core::config::DataPolicy;
use local_rag_embed::{
    BackfillError, BackfillParams, EmbedError, EmbedRequest, Embedder, HashingEmbedder, InFlight,
    ProviderEntry, ProviderPool, Vector, promote_if_covered, run_backfill,
};
use local_rag_store::registry::RepresentationKind;
use local_rag_store::{
    CacheDb, Coverage, EmbeddingKey, ModelSpaceState, RepresentationKey, RetentionParams,
    SubjectKind, all_embedding_meta, delete_embedding, encode_vector_le, get_embedding,
    insert_embedding, model_space_state, rusqlite,
};

use support::store::{self, Fixture, NOW, REPRESENTATION_ID};

/// Bodies whose first two entries are identical, so two occurrences share one
/// content blob — five occurrences, four distinct subjects.
const BODIES: [&str; 5] = [
    "fn parse(input: &str) -> Result<Ast, Error> { todo!() }",
    "fn parse(input: &str) -> Result<Ast, Error> { todo!() }",
    "struct Repository { rows: HashMap<Id, Row> }",
    "impl Display for Error { fn fmt(&self) -> Result { Ok(()) } }",
    "pub fn handler(req: Request) -> Response { Response::ok() }",
];

/// A `HashingEmbedder` that counts calls and can be told to fail.
struct CountingEmbedder {
    inner: HashingEmbedder,
    calls: AtomicUsize,
    texts: AtomicUsize,
    fail_after: Option<usize>,
}

impl CountingEmbedder {
    fn new() -> Arc<Self> {
        Arc::new(CountingEmbedder {
            inner: HashingEmbedder::new(RepresentationKind::CodeRaw),
            calls: AtomicUsize::new(0),
            texts: AtomicUsize::new(0),
            fail_after: None,
        })
    }

    /// Succeeds for the first `n` embedded texts, then fails every batch.
    fn failing_after(n: usize) -> Arc<Self> {
        Arc::new(CountingEmbedder {
            inner: HashingEmbedder::new(RepresentationKind::CodeRaw),
            calls: AtomicUsize::new(0),
            texts: AtomicUsize::new(0),
            fail_after: Some(n),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn texts(&self) -> usize {
        self.texts.load(Ordering::SeqCst)
    }
}

impl Embedder for CountingEmbedder {
    fn embed(&self, req: EmbedRequest) -> Result<Vec<Vector>, EmbedError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let before = self.texts.fetch_add(req.texts.len(), Ordering::SeqCst);
        if let Some(limit) = self.fail_after
            && before >= limit
        {
            return Err(EmbedError::permanent("provider is down"));
        }
        self.inner.embed(req)
    }

    fn key(&self) -> RepresentationKey {
        self.inner.key()
    }
}

fn pool_of(embedder: Arc<dyn Embedder>) -> ProviderPool {
    ProviderPool::new(vec![ProviderEntry::local("local", embedder)])
}

fn params(embed_batch: usize, write_batch_rows: usize) -> BackfillParams {
    BackfillParams {
        embed_batch,
        write_batch_rows,
    }
}

fn retention() -> RetentionParams {
    RetentionParams {
        keep_last_k: 2,
        window_ms: 7 * 24 * 60 * 60 * 1000,
    }
}

/// Walk the seeded default model space back to `building`.
///
/// The seed leaves it `active` (T07-02's migration seeds an active default), but
/// the edge under test — `building → projection_ready` — is the one the coverage
/// gate guards (spec 04 §3); from `active` the transition is refused on state
/// grounds before coverage is ever read.
async fn building(f: &Fixture) {
    f.state
        .writer()
        .transaction(move |tx| {
            tx.execute(
                "UPDATE model_space SET state = 'building' WHERE model_space_id = ?1",
                rusqlite::params![local_rag_store::registry::DEFAULT_MODEL_SPACE_ID],
            )
            .map(|_| ())
        })
        .await
        .expect("reset to building");
}

fn cached_rows(cache: &CacheDb) -> usize {
    let read = cache.open_read().expect("cache read");
    all_embedding_meta(&read).expect("meta").len()
}

fn coverage_of(fixture: &Fixture) -> Coverage {
    let read = fixture.state.open_read().expect("state read");
    let json: Option<String> = read
        .query_row(
            "SELECT coverage FROM model_space WHERE model_space_id = ?1",
            rusqlite::params![local_rag_store::registry::DEFAULT_MODEL_SPACE_ID],
            |r| r.get(0),
        )
        .expect("read coverage");
    json.map(|text| Coverage::from_json(&text).expect("coverage json"))
        .unwrap_or_default()
}

/// A full run embeds every distinct subject exactly once, reports it, and stores
/// matching coverage.
#[tokio::test(flavor = "multi_thread")]
async fn a_full_run_covers_every_distinct_subject() {
    let f = store::seeded(&BODIES).await;
    let embedder = CountingEmbedder::new();
    let pool = pool_of(embedder.clone());
    let expected = f.distinct_blobs() as u64;

    let report = run_backfill(
        &f.state,
        &f.cache,
        &pool,
        DataPolicy::LocalOnly,
        local_rag_store::registry::DEFAULT_MODEL_SPACE_ID,
        &params(2, 2),
        &retention(),
        &InFlight::new(),
        NOW,
    )
    .await
    .expect("backfill");

    // Five occurrences, four distinct content blobs: `content_blob` embeddings
    // are shared across paths (spec 03 §4.2 `[FIXED]`).
    assert_eq!(expected, 4, "fixture must exercise blob sharing");
    assert_eq!(report.embedded, expected);
    assert_eq!(report.reused, 0);
    assert_eq!(report.failed, 0);
    assert_eq!(embedder.texts(), expected as usize);
    assert_eq!(cached_rows(&f.cache), expected as usize);

    let entry = report
        .coverage
        .get(RepresentationKind::CodeRaw)
        .expect("code_raw tracked");
    assert_eq!(entry.expected, expected);
    assert_eq!(entry.ready, expected);
    assert_eq!(entry.failed, 0);
    assert!(
        report
            .coverage
            .fully_covered(&[RepresentationKind::CodeRaw])
    );
    // The advisory column holds what the run reported (spec 10 §3).
    assert_eq!(coverage_of(&f).to_json(), report.coverage.to_json());
}

/// Subjects already in the cache are reused, never re-embedded.
#[tokio::test(flavor = "multi_thread")]
async fn already_cached_subjects_are_not_re_embedded() {
    let f = store::seeded(&BODIES).await;
    let first = CountingEmbedder::new();
    let expected = f.distinct_blobs() as u64;

    let report = run_backfill(
        &f.state,
        &f.cache,
        &pool_of(first.clone()),
        DataPolicy::LocalOnly,
        local_rag_store::registry::DEFAULT_MODEL_SPACE_ID,
        &params(8, 500),
        &retention(),
        &InFlight::new(),
        NOW,
    )
    .await
    .expect("first run");
    assert_eq!(report.embedded, expected);

    // Second run over the same store: nothing to do.
    let second = CountingEmbedder::new();
    let report = run_backfill(
        &f.state,
        &f.cache,
        &pool_of(second.clone()),
        DataPolicy::LocalOnly,
        local_rag_store::registry::DEFAULT_MODEL_SPACE_ID,
        &params(8, 500),
        &retention(),
        &InFlight::new(),
        NOW + 1,
    )
    .await
    .expect("second run");

    assert_eq!(report.embedded, 0, "nothing left to embed");
    assert_eq!(report.reused, expected);
    assert_eq!(report.batches, 0, "no write transaction is opened");
    assert_eq!(second.calls(), 0, "the provider is never called");
    assert!(
        report
            .coverage
            .fully_covered(&[RepresentationKind::CodeRaw])
    );
}

/// A provider failure is counted as `failed`, never as `ready`, and keeps the
/// model space out of `projection_ready`.
#[tokio::test(flavor = "multi_thread")]
async fn a_failure_does_not_inflate_ready() {
    let f = store::seeded(&BODIES).await;
    building(&f).await;
    // One batch of 2 succeeds, everything after fails.
    let embedder = CountingEmbedder::failing_after(2);
    let expected = f.distinct_blobs() as u64;

    let report = run_backfill(
        &f.state,
        &f.cache,
        &pool_of(embedder.clone()),
        DataPolicy::LocalOnly,
        local_rag_store::registry::DEFAULT_MODEL_SPACE_ID,
        &params(2, 500),
        &retention(),
        &InFlight::new(),
        NOW,
    )
    .await
    .expect("run completes despite provider failures");

    assert_eq!(report.embedded, 2);
    assert_eq!(report.failed, expected - 2);
    let entry = report
        .coverage
        .get(RepresentationKind::CodeRaw)
        .expect("tracked");
    assert_eq!(entry.expected, expected);
    assert_eq!(entry.ready, 2, "only real rows count as ready");
    assert!(entry.failed > 0);
    assert!(
        !report
            .coverage
            .fully_covered(&[RepresentationKind::CodeRaw]),
        "an incomplete run must not read as covered"
    );

    // ... and the gate refuses the promotion.
    let outcome = promote_if_covered(
        &f.state,
        local_rag_store::registry::DEFAULT_MODEL_SPACE_ID,
        NOW,
    )
    .await
    .expect("transition attempted");
    assert!(
        matches!(
            outcome,
            Err(local_rag_store::ModelSpaceTransitionError::IncompleteCoverage)
        ),
        "expected IncompleteCoverage, got {outcome:?}"
    );
}

/// Full required coverage is what promotes a model space to `projection_ready`.
#[tokio::test(flavor = "multi_thread")]
async fn full_required_coverage_gates_projection_ready() {
    let f = store::seeded(&BODIES).await;
    building(&f).await;

    // Before any backfill: coverage is absent ⇒ the gate refuses.
    let before = promote_if_covered(
        &f.state,
        local_rag_store::registry::DEFAULT_MODEL_SPACE_ID,
        NOW,
    )
    .await
    .expect("attempted");
    assert!(matches!(
        before,
        Err(local_rag_store::ModelSpaceTransitionError::IncompleteCoverage)
    ));

    run_backfill(
        &f.state,
        &f.cache,
        &pool_of(CountingEmbedder::new()),
        DataPolicy::LocalOnly,
        local_rag_store::registry::DEFAULT_MODEL_SPACE_ID,
        &params(8, 500),
        &retention(),
        &InFlight::new(),
        NOW,
    )
    .await
    .expect("backfill");

    let after = promote_if_covered(
        &f.state,
        local_rag_store::registry::DEFAULT_MODEL_SPACE_ID,
        NOW + 1,
    )
    .await
    .expect("attempted");
    assert!(after.is_ok(), "full coverage must promote: {after:?}");

    let read = f.state.open_read().expect("state read");
    assert_eq!(
        model_space_state(&read, local_rag_store::registry::DEFAULT_MODEL_SPACE_ID)
            .expect("state")
            .expect("exists"),
        ModelSpaceState::ProjectionReady
    );
}

/// Two concurrent runs never embed the same subject twice.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_runs_deduplicate_work() {
    let f = store::seeded(&BODIES).await;
    let embedder = CountingEmbedder::new();
    let pool_a = pool_of(embedder.clone());
    let pool_b = pool_of(embedder.clone());
    let in_flight = InFlight::new();
    let expected = f.distinct_blobs() as u64;
    let (p, r) = (params(1, 1), retention());

    let (a, b) = tokio::join!(
        run_backfill(
            &f.state,
            &f.cache,
            &pool_a,
            DataPolicy::LocalOnly,
            local_rag_store::registry::DEFAULT_MODEL_SPACE_ID,
            &p,
            &r,
            &in_flight,
            NOW,
        ),
        run_backfill(
            &f.state,
            &f.cache,
            &pool_b,
            DataPolicy::LocalOnly,
            local_rag_store::registry::DEFAULT_MODEL_SPACE_ID,
            &p,
            &r,
            &in_flight,
            NOW,
        )
    );
    let (a, b) = (a.expect("run a"), b.expect("run b"));

    assert_eq!(
        embedder.texts(),
        expected as usize,
        "each subject is embedded exactly once across both runs"
    );
    assert_eq!(a.embedded + b.embedded, expected);
    for (name, report) in [("a", &a), ("b", &b)] {
        assert_eq!(
            report.embedded + report.reused + report.deferred + report.failed,
            expected,
            "run {name} must account for every expected subject exactly once: {report:?}"
        );
    }
    assert_eq!(cached_rows(&f.cache), expected as usize);
    assert!(in_flight.is_empty(), "reservations are always released");
}

/// A corrupt cached row is deleted and re-embedded, not silently trusted
/// (spec 03 §4.4 step 4).
#[tokio::test(flavor = "multi_thread")]
async fn a_corrupt_row_is_repaired() {
    let f = store::seeded(&BODIES).await;
    let expected = f.distinct_blobs() as u64;

    run_backfill(
        &f.state,
        &f.cache,
        &pool_of(CountingEmbedder::new()),
        DataPolicy::LocalOnly,
        local_rag_store::registry::DEFAULT_MODEL_SPACE_ID,
        &params(8, 500),
        &retention(),
        &InFlight::new(),
        NOW,
    )
    .await
    .expect("first run");

    // Corrupt one row's checksum in place.
    let read = f.cache.open_read().expect("cache read");
    let victim = all_embedding_meta(&read).expect("meta")[0].key.clone();
    drop(read);
    let key = victim.clone();
    f.cache
        .writer()
        .transaction(move |tx| {
            tx.execute(
                "UPDATE embedding_cache SET checksum = 'deadbeef' \
                 WHERE subject_kind = ?1 AND subject_hash = ?2 AND representation_id = ?3",
                rusqlite::params![
                    key.subject_kind.as_str(),
                    key.subject_hash,
                    key.representation_id
                ],
            )
            .map(|_| ())
        })
        .await
        .expect("corrupt row");

    let embedder = CountingEmbedder::new();
    let report = run_backfill(
        &f.state,
        &f.cache,
        &pool_of(embedder.clone()),
        DataPolicy::LocalOnly,
        local_rag_store::registry::DEFAULT_MODEL_SPACE_ID,
        &params(8, 500),
        &retention(),
        &InFlight::new(),
        NOW + 1,
    )
    .await
    .expect("repair run");

    assert_eq!(report.repaired, 1);
    assert_eq!(report.embedded, 1, "only the corrupt subject is redone");
    assert_eq!(report.reused, expected - 1);
    assert_eq!(embedder.texts(), 1);

    let read = f.cache.open_read().expect("cache read");
    let row = get_embedding(&read, &victim)
        .expect("get")
        .expect("row present");
    local_rag_store::verify_cached_embedding(&row).expect("row is valid again");
}

/// A `required` kind with no subject function refuses the run rather than
/// reporting zero expected subjects (which would read as "fully covered").
#[tokio::test(flavor = "multi_thread")]
async fn an_unsupported_required_kind_refuses_the_run() {
    let f = store::seeded(&BODIES).await;
    store::register_kind(
        &f.state,
        "66666666-6666-7666-8666-666666666666",
        store::foreign_key(RepresentationKind::CodeContext),
    )
    .await;

    let err = run_backfill(
        &f.state,
        &f.cache,
        &pool_of(CountingEmbedder::new()),
        DataPolicy::LocalOnly,
        local_rag_store::registry::DEFAULT_MODEL_SPACE_ID,
        &params(8, 500),
        &retention(),
        &InFlight::new(),
        NOW,
    )
    .await
    .expect_err("code_context has no subject function yet");

    assert!(
        matches!(
            err,
            BackfillError::UnsupportedRequiredKind {
                kind: RepresentationKind::CodeContext
            }
        ),
        "expected UnsupportedRequiredKind, got {err}"
    );
}

/// Rows a backfill wrote under a `building` model space survive an eviction pass
/// with a zero budget: the pin rule covers spaces that are still being filled.
#[tokio::test(flavor = "multi_thread")]
async fn rows_of_a_building_model_space_are_pinned() {
    let f = store::seeded(&BODIES).await;
    building(&f).await;

    run_backfill(
        &f.state,
        &f.cache,
        &pool_of(CountingEmbedder::new()),
        DataPolicy::LocalOnly,
        local_rag_store::registry::DEFAULT_MODEL_SPACE_ID,
        &params(8, 500),
        &retention(),
        &InFlight::new(),
        NOW,
    )
    .await
    .expect("backfill");
    let before = cached_rows(&f.cache);
    assert!(before > 0);

    // A zero budget evicts everything that is not pinned.
    let read = f.state.open_read().expect("state read");
    let report = local_rag_store::run_embedding_cache_eviction(
        &f.cache,
        &read,
        &local_rag_store::EvictionParams {
            budget_bytes: 0,
            retention: retention(),
        },
        NOW,
        false,
    )
    .await
    .expect("eviction");

    assert!(
        report.evicted.is_empty(),
        "backfilled rows of a building space must be pinned, evicted {:?}",
        report.evicted
    );
    assert_eq!(cached_rows(&f.cache), before);
}

/// An unrelated row (no expected subject backs it) is still evictable — the pin
/// widening must not turn eviction into a no-op.
#[tokio::test(flavor = "multi_thread")]
async fn unreferenced_rows_remain_evictable() {
    let f = store::seeded(&BODIES).await;
    let orphan = EmbeddingKey {
        subject_kind: SubjectKind::ContentBlob,
        subject_hash: "0".repeat(64),
        representation_id: REPRESENTATION_ID.to_string(),
    };
    let key = orphan.clone();
    f.cache
        .writer()
        .transaction(move |tx| insert_embedding(tx, &key, 3, &[0.1, 0.2, 0.3], NOW))
        .await
        .expect("insert orphan");

    let read = f.state.open_read().expect("state read");
    let report = local_rag_store::run_embedding_cache_eviction(
        &f.cache,
        &read,
        &local_rag_store::EvictionParams {
            budget_bytes: 0,
            retention: retention(),
        },
        NOW,
        false,
    )
    .await
    .expect("eviction");

    assert_eq!(report.evicted, vec![orphan.clone()]);
    let read = f.cache.open_read().expect("cache read");
    assert!(get_embedding(&read, &orphan).expect("get").is_none());
}

/// Deleting a subject's row and running again restores exactly it — the
/// "recomputable, not journalled" property backfill's resumability rests on.
#[tokio::test(flavor = "multi_thread")]
async fn a_deleted_row_is_restored_by_the_next_run() {
    let f = store::seeded(&BODIES).await;
    let expected = f.distinct_blobs() as u64;
    run_backfill(
        &f.state,
        &f.cache,
        &pool_of(CountingEmbedder::new()),
        DataPolicy::LocalOnly,
        local_rag_store::registry::DEFAULT_MODEL_SPACE_ID,
        &params(8, 500),
        &retention(),
        &InFlight::new(),
        NOW,
    )
    .await
    .expect("first run");

    let read = f.cache.open_read().expect("cache read");
    let victim = all_embedding_meta(&read).expect("meta")[1].key.clone();
    drop(read);
    let key = victim.clone();
    f.cache
        .writer()
        .transaction(move |tx| delete_embedding(tx, &key).map(|_| ()))
        .await
        .expect("delete row");

    let embedder = CountingEmbedder::new();
    let report = run_backfill(
        &f.state,
        &f.cache,
        &pool_of(embedder.clone()),
        DataPolicy::LocalOnly,
        local_rag_store::registry::DEFAULT_MODEL_SPACE_ID,
        &params(8, 500),
        &retention(),
        &InFlight::new(),
        NOW + 1,
    )
    .await
    .expect("second run");

    assert_eq!(report.embedded, 1);
    assert_eq!(report.reused, expected - 1);
    assert_eq!(embedder.texts(), 1);
    let read = f.cache.open_read().expect("cache read");
    let row = get_embedding(&read, &victim)
        .expect("get")
        .expect("restored");
    assert_eq!(
        row.byte_size,
        encode_vector_le(&vec![0.0_f32; row.dimensions as usize]).len() as i64
    );
}
