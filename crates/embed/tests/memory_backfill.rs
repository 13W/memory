//! T21-05 acceptance tests for the memory-only backfill pass.
//!
//! `run_memory_backfill` exists because durable-memory vectors were only ever
//! produced inside a **code**-indexing cycle. Two properties matter and both are
//! pinned here: it really does embed memory subjects on its own, and it leaves
//! `model_space.coverage` alone — a memory-only pass never looked at
//! `code_raw`, so recomputing coverage from it would decide a gate (spec 04 §3)
//! on a number this run did not produce.
//!
//! Deterministic: an isolated `TempHome`, fixed `now_ms`, a `HashingEmbedder`,
//! no network.

mod support;

use std::sync::Arc;

use local_rag_core::config::DataPolicy;
use local_rag_embed::{
    BackfillParams, Embedder, HashingEmbedder, InFlight, ProviderEntry, ProviderPool,
    run_memory_backfill,
};
use local_rag_store::registry::RepresentationKind;
use local_rag_store::{
    DEFAULT_MODEL_SPACE_ID, GLOBAL_SCOPE_OWNER_ID, MemoryKind, NewMemoryEntry, ScopeKind, StateDb,
    create_memory_entry, rusqlite,
};

use support::store::{self, Fixture, NOW};

const MEMORY_REPRESENTATION_ID: &str = "019fec1c-0000-7000-8000-00000000000a";

async fn seed_memory(state: &StateDb, memory_id: &str, text: &str) {
    let (id, text) = (memory_id.to_string(), text.to_string());
    state
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

async fn register_memory(state: &StateDb) -> String {
    let key = Embedder::key(&HashingEmbedder::new(RepresentationKind::Memory));
    store::register_kind(state, MEMORY_REPRESENTATION_ID, key).await
}

fn memory_pool() -> ProviderPool {
    ProviderPool::new(vec![ProviderEntry::local(
        "hashing-memory",
        Arc::new(HashingEmbedder::new(RepresentationKind::Memory)),
    )])
}

fn stored_coverage(state: &StateDb) -> Option<String> {
    let read = state.open_read().expect("read conn");
    read.query_row(
        "SELECT coverage FROM model_space WHERE model_space_id = ?1",
        rusqlite::params![DEFAULT_MODEL_SPACE_ID],
        |r| r.get::<_, Option<String>>(0),
    )
    .expect("read coverage")
}

fn cached_memory_rows(fixture: &Fixture) -> i64 {
    let read = fixture.cache.open_read().expect("cache read");
    read.query_row(
        "SELECT COUNT(*) FROM embedding_cache WHERE subject_kind = 'memory_entry'",
        [],
        |r| r.get(0),
    )
    .expect("count cached memory rows")
}

/// The pass embeds every memory entry and reuses them on a second run — the
/// ordinary idempotence every backfill in this codebase has.
#[tokio::test]
async fn a_memory_only_pass_embeds_every_entry_and_then_reuses_them() {
    let fixture = store::seeded(&["fn a() {}"]).await;
    register_memory(&fixture.state).await;
    seed_memory(&fixture.state, "m-1", "первая запись").await;
    seed_memory(&fixture.state, "m-2", "second entry").await;

    let report = run_memory_backfill(
        &fixture.state,
        &fixture.cache,
        &memory_pool(),
        DataPolicy::LocalOnly,
        DEFAULT_MODEL_SPACE_ID,
        &BackfillParams::default(),
        &InFlight::new(),
        NOW,
    )
    .await
    .expect("memory backfill");
    assert_eq!(report.embedded, 2, "both entries embedded: {report:?}");
    assert_eq!(cached_memory_rows(&fixture), 2);

    let second = run_memory_backfill(
        &fixture.state,
        &fixture.cache,
        &memory_pool(),
        DataPolicy::LocalOnly,
        DEFAULT_MODEL_SPACE_ID,
        &BackfillParams::default(),
        &InFlight::new(),
        NOW,
    )
    .await
    .expect("second memory backfill");
    assert_eq!(second.embedded, 0);
    assert_eq!(second.reused, 2, "a second pass re-embeds nothing");
}

/// The load-bearing property: a memory-only pass must not write coverage. It
/// never looked at `code_raw`, and `Coverage::fully_covered` gates
/// `projection_ready` on exactly that number.
#[tokio::test]
async fn a_memory_only_pass_leaves_coverage_untouched() {
    let fixture = store::seeded(&["fn a() {}"]).await;
    store::register_code_raw(&fixture.state).await;
    register_memory(&fixture.state).await;
    seed_memory(&fixture.state, "m-1", "первая запись").await;

    let before = stored_coverage(&fixture.state);

    let report = run_memory_backfill(
        &fixture.state,
        &fixture.cache,
        &memory_pool(),
        DataPolicy::LocalOnly,
        DEFAULT_MODEL_SPACE_ID,
        &BackfillParams::default(),
        &InFlight::new(),
        NOW,
    )
    .await
    .expect("memory backfill");
    assert_eq!(report.embedded, 1, "the memory entry was embedded");

    assert_eq!(
        stored_coverage(&fixture.state),
        before,
        "a memory-only pass must not decide the model space's coverage — it \
         never looked at code_raw",
    );
}

/// A store that never registered the memory kind is not an error: there is no
/// key to write under, and no reader is looking for one.
#[tokio::test]
async fn a_store_without_a_memory_representation_degrades_rather_than_failing() {
    let fixture = store::seeded(&["fn a() {}"]).await;
    store::register_code_raw(&fixture.state).await;
    seed_memory(&fixture.state, "m-1", "первая запись").await;

    let report = run_memory_backfill(
        &fixture.state,
        &fixture.cache,
        &memory_pool(),
        DataPolicy::LocalOnly,
        DEFAULT_MODEL_SPACE_ID,
        &BackfillParams::default(),
        &InFlight::new(),
        NOW,
    )
    .await
    .expect("an unregistered kind is 'nothing to do', not a failure");

    assert_eq!(report.embedded, 0);
    assert_eq!(report.failed, 0);
    assert_eq!(cached_memory_rows(&fixture), 0);
}
