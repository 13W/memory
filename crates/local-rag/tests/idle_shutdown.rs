//! T15-01 acceptance test for the card's "pending spool prevents idle exit"
//! scenario (spec 02 §4.3 `[FIXED]`), driven through the real
//! `DaemonHandle::start` startup resume pass — not just the pure
//! `idle_eligible` predicate (unit-tested in `daemon::idle` itself).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use local_rag::daemon::{DaemonHandle, LazyEmbedderProvider, SessionGuard, StartOptions};
use local_rag_core::DataPolicy;
use local_rag_core::identity::{Uuid, UuidSource, uuidv7_from};
use local_rag_core::paths::StoreLayout;
use local_rag_core::spool::{FramePayload, encode_frame, encode_segment_header};
use local_rag_store::{LEASE_DURATION_MS, LEASE_RENEW_INTERVAL_MS};
use local_rag_test_support::TempHome;

struct SeqUuidV7 {
    counter: AtomicU64,
}
impl SeqUuidV7 {
    fn new() -> Self {
        Self {
            counter: AtomicU64::new(0),
        }
    }
}
impl UuidSource for SeqUuidV7 {
    fn next_uuid(&self) -> Uuid {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        uuidv7_from(1000 + n, [0xEF; 10])
    }
}

fn open_layout() -> (TempHome, StoreLayout) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    (home, layout)
}

fn start_options(layout: StoreLayout) -> StartOptions {
    let embedder_provider = Arc::new(LazyEmbedderProvider::new(&layout));
    StartOptions {
        layout,
        daemon_version: "0.0.0".to_string(),
        now_ms: 1_000,
        uuids: Arc::new(SeqUuidV7::new()),
        write_queue_capacity: 8,
        payload_ttl_hours: 72,
        consolidation_lease_ms: LEASE_DURATION_MS,
        consolidation_renew_interval_ms: LEASE_RENEW_INTERVAL_MS,
        data_policy: DataPolicy::LocalOnly,
        supported_proto: local_rag_protocol::SUPPORTED_PROTO_RANGE,
        max_open_shards: 8,
        embedder_provider,
        query_embedder: None,
        memory_query_embedder: None,
        recall_token_budget: 1500,
        consolidation_batch_size: 20,
        consolidation_queue_threshold: 50,
        consolidation_poll_interval: Duration::from_millis(50),
    }
}

fn write_spool_segment(layout: &StoreLayout, session_id: &str) {
    let frame = FramePayload {
        format_version: 1,
        source_event_id: format!("st:{session_id}:1"),
        dedup_key: None,
        event_type: "Stop".to_string(),
        captured_at: 1_000,
        session_id: session_id.to_string(),
        agent_id: None,
        turn_id: None,
        batch_id: None,
        worktree_root: None,
        commit: None,
        evidence_kind: "model_claim".to_string(),
        trust: "low".to_string(),
        paths: vec![],
        redaction_version: None,
        payload: None,
        short_evidence_excerpt: None,
    };
    let session_dir = layout.spool_session(session_id);
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let mut bytes = encode_segment_header().to_vec();
    bytes.extend_from_slice(&encode_frame(&frame).expect("under the frame cap"));
    std::fs::write(session_dir.join("000001.seg"), bytes).expect("write segment");
}

/// Bounded wait for the startup resume pass to finish importing everything
/// it can see, observed through the same `idle_eligible` signal the daemon
/// itself uses — not `handle.jobs.is_empty()` alone, which is racy here: the
/// resume task is spawned non-blocking relative to `start()` returning, so
/// immediately after `start()` the job registry may still be empty simply
/// because the spawned task has not been polled for the first time yet, not
/// because it already finished. No assertion in this helper depends on wall-
/// clock *duration*, only on eventual convergence within the bound.
async fn wait_until_idle_eligible(handle: &DaemonHandle) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !handle.is_idle_eligible() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("the startup resume pass must finish within the test's bound");
}

#[tokio::test]
async fn an_unimported_spool_segment_prevents_idle_exit_until_resumed() {
    let (_home, layout) = open_layout();
    write_spool_segment(&layout, "sess-pending");

    let handle = DaemonHandle::start(start_options(layout.clone()))
        .await
        .expect("start");

    // Immediately after `start()` returns, the startup resume pass (spec 02
    // §4.1 step 5, spawned non-blocking relative to readiness) has very
    // likely not finished yet — the pending segment must still refuse idle
    // shutdown at this instant.
    assert!(
        !handle.is_idle_eligible(),
        "the seeded segment must not be imported yet: {:?}",
        handle.idle_inputs()
    );

    // Once the resume pass has fully finished, the previously-pending
    // segment must have been imported, and idle-shutdown becomes eligible.
    wait_until_idle_eligible(&handle).await;

    handle.shutdown().await;
}

/// A live session alone (no spool involvement at all) also refuses idle
/// shutdown — the "all three" gate, exercised end to end through a real
/// `DaemonHandle`, not just the pure predicate.
#[tokio::test]
async fn a_live_session_prevents_idle_exit_even_with_no_pending_spool() {
    let (_home, layout) = open_layout();
    let handle = DaemonHandle::start(start_options(layout.clone()))
        .await
        .expect("start");
    wait_until_idle_eligible(&handle).await;

    let session: SessionGuard = handle.sessions.register("session-a");
    assert!(
        !handle.is_idle_eligible(),
        "a live session must refuse idle shutdown"
    );

    drop(session);
    assert!(
        handle.is_idle_eligible(),
        "dropping the last session restores idle eligibility"
    );

    handle.shutdown().await;
}
