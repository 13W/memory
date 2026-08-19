//! T21-05 acceptance tests: the two-database write order, and what a crash
//! between the halves leaves behind.
//!
//! A translation moves an entry's subject hash, and the two writes live in two
//! databases that spec 03 §1.4 `[FIXED]` forbids joining in one transaction.
//! The order is therefore the guarantee, and these tests are what make it a
//! guarantee rather than a comment: the vector must be in `cache.sqlite` before
//! anything in `state.sqlite` claims the entry is normalized.
//!
//! Deterministic: an isolated `TempHome`, fixed `now_ms`, a scripted generator
//! and a `HashingEmbedder` — no model, no network, no clock.

#![cfg(unix)]

use std::sync::Arc;
use std::sync::Mutex;

use local_rag_core::config::DataPolicy;
use local_rag_core::paths::StoreLayout;
use local_rag_embed::{
    Embedder, FinishReason, GenError, GenRequest, GenResponse, Generator, GeneratorEntry,
    GeneratorPool, HashingEmbedder, ProviderEntry, ProviderPool,
};
use local_rag_memory::normalize::detect::ScriptClass;
use local_rag_memory::recall::{BruteForceCosine, QueryEmbedError, QueryEmbedder, dense_leg};
use local_rag_store::registry::RepresentationKind;
use local_rag_store::{
    CacheDb, DEFAULT_MODEL_SPACE_ID, GLOBAL_SCOPE_OWNER_ID, MemoryKind, NewMemoryEntry,
    RepresentationKey, ScopeKind, StateDb, create_memory_entry, normalization_for,
    recall_candidates_for_scope, register_representation, representation_key, rusqlite,
    set_model_space_representation,
};
use local_rag_test_support::TempHome;

use local_rag::daemon::normalization::write::{
    NormalizationOutcome, NormalizationTarget, apply_normalization,
};

/// The failpoint registry (`local_rag_test_support::failpoint::global()`) is
/// process-wide, so an armed-but-not-yet-disarmed point in one test would
/// otherwise fire inside a concurrently running one in this same binary — the
/// exact reason `crates/store/tests/consolidation_runner.rs` serializes its own
/// failpoint tests. Every test here takes the lock, not only the armed one.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const NOW: i64 = 1_000;
const STORE_UUID: &str = "01a00000-0000-7000-8000-00000000ffff";
const MEMORY_REPRESENTATION_ID: &str = "019fec1c-0000-7000-8000-00000000000a";
const RU: &str = "Для фьюжна поиска остановились на RRF вместо линейной комбинации весов";
const EN: &str = "For search fusion we settled on RRF instead of a linear combination of weights";

struct Fixture {
    _home: TempHome,
    state: StateDb,
    cache: CacheDb,
}

/// A generator that answers with a fixed script and counts its calls.
#[derive(Debug, Clone)]
struct ScriptedGenerator {
    answers: Arc<Mutex<Vec<String>>>,
}

impl Generator for ScriptedGenerator {
    fn generate(&self, _req: GenRequest) -> Result<GenResponse, GenError> {
        let text = self
            .answers
            .lock()
            .expect("lock")
            .pop()
            .unwrap_or_else(|| serde_json::json!({ "en": EN }).to_string());
        Ok(GenResponse {
            text,
            finish_reason: FinishReason::Stop,
            tokens_generated: None,
        })
    }
}

fn translator() -> GeneratorPool {
    GeneratorPool::new(vec![GeneratorEntry::local(
        "scripted",
        Arc::new(ScriptedGenerator {
            answers: Arc::new(Mutex::new(Vec::new())),
        }),
    )])
}

fn embedders() -> ProviderPool {
    ProviderPool::new(vec![ProviderEntry::local(
        "hashing-memory",
        Arc::new(HashingEmbedder::new(RepresentationKind::Memory)),
    )])
}

/// An embedder whose vectors are a different width than the registered memory
/// representation declares — a misconfigured pool, in one struct.
struct WrongWidthEmbedder;

impl Embedder for WrongWidthEmbedder {
    fn embed(
        &self,
        req: local_rag_embed::EmbedRequest,
    ) -> Result<Vec<local_rag_embed::Vector>, local_rag_embed::EmbedError> {
        Ok(req
            .texts
            .iter()
            .map(|_| local_rag_embed::Vector::new(vec![0.5; 7]))
            .collect())
    }

    fn key(&self) -> RepresentationKey {
        let mut key = Embedder::key(&HashingEmbedder::new(RepresentationKind::Memory));
        key.dimensions = 7;
        key
    }
}

/// Embeds a query with the same `HashingEmbedder` the vectors were written
/// with, so the dense leg's own arithmetic is exercised end to end.
struct HashingQueryEmbedder(HashingEmbedder);

impl QueryEmbedder for HashingQueryEmbedder {
    fn embed_query(
        &self,
        query: &str,
        _key: &RepresentationKey,
    ) -> Result<Vec<f32>, QueryEmbedError> {
        let vectors = self
            .0
            .embed(local_rag_embed::EmbedRequest::new(
                RepresentationKind::Memory,
                vec![query.to_string()],
            ))
            .map_err(|e| QueryEmbedError {
                reason: e.to_string(),
            })?;
        Ok(vectors
            .into_iter()
            .next()
            .map(local_rag_embed::Vector::into_inner)
            .unwrap_or_default())
    }
}

async fn fixture(text: &str) -> Fixture {
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

    let text = text.to_string();
    state
        .writer()
        .transaction(move |tx| {
            create_memory_entry(
                tx,
                &NewMemoryEntry {
                    memory_id: "m-1",
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

    Fixture {
        _home: home,
        state,
        cache,
    }
}

fn cached_hashes(fixture: &Fixture) -> Vec<String> {
    let read = fixture.cache.open_read().expect("cache read");
    let mut stmt = read
        .prepare(
            "SELECT subject_hash FROM embedding_cache WHERE subject_kind = 'memory_entry' \
             ORDER BY subject_hash",
        )
        .expect("prepare");
    stmt.query_map([], |r| r.get::<_, String>(0))
        .expect("query")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("collect")
}

async fn apply(fixture: &Fixture, text: &str) -> NormalizationOutcome {
    apply_normalization(
        &fixture.state,
        &fixture.cache,
        &translator(),
        &embedders(),
        DataPolicy::LocalOnly,
        NormalizationTarget {
            memory_id: "m-1",
            text,
        },
        NOW,
    )
    .await
    .expect("apply_normalization")
}

/// The happy path, and the shape everything else is measured against: one
/// vector under the new hash, one `ready` row.
#[tokio::test]
async fn a_translation_lands_as_a_vector_and_then_a_row() {
    let _serial = SERIAL.lock().await;
    let fixture = fixture(RU).await;
    let outcome = apply(&fixture, RU).await;

    let NormalizationOutcome::Normalized {
        subject_hash,
        vectors_written,
    } = outcome
    else {
        panic!("expected a normalization, got {outcome:?}");
    };
    assert_eq!(vectors_written, 1);
    assert_eq!(cached_hashes(&fixture), vec![subject_hash]);

    let read = fixture.state.open_read().expect("state read");
    let row = normalization_for(&read, "m-1")
        .expect("read row")
        .expect("the row is committed");
    assert_eq!(row.normalized_text.as_deref(), Some(EN));
}

/// Nothing to translate: a `skipped` row, and `cache.sqlite` untouched — the
/// entry keeps the hash its existing vector is under.
#[tokio::test]
async fn a_passthrough_entry_never_touches_the_cache() {
    let _serial = SERIAL.lock().await;
    let fixture = fixture(EN).await;
    let outcome = apply(&fixture, EN).await;

    assert_eq!(
        outcome,
        NormalizationOutcome::Skipped {
            class: ScriptClass::English
        }
    );
    assert!(
        cached_hashes(&fixture).is_empty(),
        "a passthrough must not write a single cache row",
    );

    let read = fixture.state.open_read().expect("state read");
    let row = normalization_for(&read, "m-1")
        .expect("read row")
        .expect("a skipped row is still recorded");
    assert_eq!(row.normalized_text, None);
}

/// An `edit` that landed while the translation was in flight: the store's own
/// conditional write refuses, and the stale translation is not committed.
#[tokio::test]
async fn a_translation_of_text_that_has_since_changed_is_refused() {
    let _serial = SERIAL.lock().await;
    let fixture = fixture("совершенно другой текст, записанный после перевода").await;

    // The worker read RU before the edit; the store now holds something else.
    let outcome = apply(&fixture, RU).await;
    assert_eq!(outcome, NormalizationOutcome::TextMoved);

    let read = fixture.state.open_read().expect("state read");
    assert_eq!(
        normalization_for(&read, "m-1").expect("read row"),
        None,
        "a refused write must leave no row at all",
    );
}

/// Applying the same entry twice is a no-op beyond bookkeeping: the same hash,
/// one cache row, one state row.
#[tokio::test]
async fn applying_the_same_entry_twice_changes_nothing() {
    let _serial = SERIAL.lock().await;
    let fixture = fixture(RU).await;
    let first = apply(&fixture, RU).await;
    let second = apply(&fixture, RU).await;
    assert_eq!(first, second);
    assert_eq!(cached_hashes(&fixture).len(), 1);
}

/// A registry that no configured embedder matches: refuse, rather than commit
/// a row that declares the entry normalized with no vector under its new hash.
/// That state has no reader that could report it — the dense leg would simply
/// return nothing.
#[tokio::test]
async fn a_vector_no_registered_representation_can_take_is_refused() {
    let _serial = SERIAL.lock().await;
    let fixture = fixture(RU).await;

    // The registry says `memory` is this representation's key (bootstrap
    // dimensions); the pool is handed an embedder of a different width.
    let wrong_width = ProviderPool::new(vec![ProviderEntry::local(
        "hashing-code-raw",
        Arc::new(WrongWidthEmbedder),
    )]);

    let outcome = apply_normalization(
        &fixture.state,
        &fixture.cache,
        &translator(),
        &wrong_width,
        DataPolicy::LocalOnly,
        NormalizationTarget {
            memory_id: "m-1",
            text: RU,
        },
        NOW,
    )
    .await;

    assert!(
        matches!(
            outcome,
            Err(local_rag::daemon::normalization::write::NormalizationError::NoUsableRepresentation { .. })
        ),
        "{outcome:?}",
    );
    assert!(cached_hashes(&fixture).is_empty());
    let read = fixture.state.open_read().expect("state read");
    assert_eq!(
        normalization_for(&read, "m-1").expect("read row"),
        None,
        "nothing may claim the entry is normalized",
    );
}

/// The card's own acceptance: after normalization the **real** dense leg finds
/// a vector for the entry — which is only true if the hash the writer used and
/// the hash the reader computes are the same one.
#[tokio::test]
async fn after_normalization_the_real_dense_leg_finds_the_vector() {
    let _serial = SERIAL.lock().await;
    let fixture = fixture(RU).await;
    apply(&fixture, RU).await;

    let state_read = fixture.state.open_read().expect("state read");
    let cache_read = fixture.cache.open_read().expect("cache read");
    let candidates =
        recall_candidates_for_scope(&state_read, ScopeKind::Global, GLOBAL_SCOPE_OWNER_ID)
            .expect("candidates");
    assert_eq!(candidates.len(), 1);

    let key = representation_key(&state_read, MEMORY_REPRESENTATION_ID)
        .expect("read key")
        .expect("the representation is registered");
    let hits = dense_leg(
        &cache_read,
        EN,
        &key,
        MEMORY_REPRESENTATION_ID,
        &HashingQueryEmbedder(HashingEmbedder::new(RepresentationKind::Memory)),
        &BruteForceCosine,
        &candidates,
        10,
    )
    .expect("the dense leg must be available");

    assert_eq!(
        hits.len(),
        1,
        "the entry must resolve a vector — a hash mismatch would silently \
         return nothing at all",
    );
    assert_eq!(hits[0].memory_id, "m-1");
}

/// The property the whole module exists for, proven from the production path:
/// kill the process between the two databases and the store is still coherent.
#[cfg(feature = "failpoints")]
mod crash_between_the_two_databases {
    use super::*;

    use local_rag::daemon::normalization::write::FAILPOINT_AFTER_VECTOR;
    use local_rag_test_support::Action;

    fn arm() {
        let fp = local_rag_test_support::failpoint::global();
        fp.register(FAILPOINT_AFTER_VECTOR);
        fp.arm(FAILPOINT_AFTER_VECTOR, Action::Error)
            .expect("arm the crash point");
    }

    fn disarm() {
        local_rag_test_support::failpoint::global()
            .disarm(FAILPOINT_AFTER_VECTOR)
            .expect("disarm the crash point");
    }

    #[tokio::test]
    async fn a_crash_after_the_vector_leaves_no_row_and_the_retry_converges() {
        let _serial = SERIAL.lock().await;
        let fixture = fixture(RU).await;

        arm();
        let interrupted = apply_normalization(
            &fixture.state,
            &fixture.cache,
            &translator(),
            &embedders(),
            DataPolicy::LocalOnly,
            NormalizationTarget {
                memory_id: "m-1",
                text: RU,
            },
            NOW,
        )
        .await;
        disarm();
        assert!(
            interrupted.is_err(),
            "the crash point must abort the call: {interrupted:?}",
        );

        // The vector is already there — written *before* anything claimed the
        // entry was normalized.
        let orphaned = cached_hashes(&fixture);
        assert_eq!(
            orphaned.len(),
            1,
            "the vector must land first; that is the whole order",
        );

        // …and `state.sqlite` says nothing at all, so no reader is looking for
        // that hash yet. The cache row is unreferenced, which the
        // "cache.sqlite is fully rebuildable" invariant makes harmless.
        let read = fixture.state.open_read().expect("state read");
        assert_eq!(
            normalization_for(&read, "m-1").expect("read row"),
            None,
            "nothing in state.sqlite may claim the entry is normalized yet",
        );
        drop(read);

        // The retry converges on exactly the same hash: no second vector, and
        // now the row.
        let outcome = apply(&fixture, RU).await;
        let NormalizationOutcome::Normalized { subject_hash, .. } = outcome else {
            panic!("the retry must finish the job, got {outcome:?}");
        };
        assert_eq!(
            cached_hashes(&fixture),
            vec![subject_hash],
            "the retry reuses the vector the interrupted run wrote",
        );
        let read = fixture.state.open_read().expect("state read");
        assert!(
            normalization_for(&read, "m-1").expect("read row").is_some(),
            "and the row is committed this time",
        );
    }
}
