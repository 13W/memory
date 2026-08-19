//! D-024 acceptance tests for the continuous consolidation-trigger worker
//! (spec 07 §6), driven through a real [`DaemonHandle`] — not just the pure
//! `consolidation_trigger_tick`/`run_consolidation_trigger` unit tests in
//! `crates/local-rag/src/daemon/consolidation_trigger.rs` itself. Mirrors
//! `idle_shutdown.rs`/`checkpoint_shutdown.rs`'s own style.
//!
//! `build_best_effort_pool` returns an *empty*, network-free pool on a test
//! machine with no local model installed (see `daemon::resume::consolidation`'s
//! own doc) — so a real, live-daemon trigger cannot be proven to reach
//! `RunOutcome::Applied` here; it deterministically fails fast
//! (`GenError::NoProvider`) instead, same as the startup consolidation-resume
//! pass already accepts as this test tier's own boundary. What these tests
//! *can* prove through a real daemon: the worker actually opens a
//! `consolidation_run` row for a Stop-seeded session (the trigger fired),
//! that being alive between ticks does not block idle-shutdown, and that
//! shutdown completes promptly with the worker alive.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use local_rag::daemon::{DaemonHandle, LazyEmbedderProvider, StartOptions};
use local_rag_core::DataPolicy;
use local_rag_core::identity::{Uuid, UuidSource, uuidv7_from};
use local_rag_core::paths::StoreLayout;
use local_rag_core::spool::{FramePayload, encode_frame, encode_segment_header};
use local_rag_index::classify::ClassifierConfig;
use local_rag_store::{
    FailureKind, LEASE_DURATION_MS, LEASE_RENEW_INTERVAL_MS, NewConsolidationRun, RetentionParams,
    RunState, StateDb, WorktreeLockRegistry, create_consolidation_run, record_run_failure,
    transition_run,
};
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
        uuidv7_from(1000 + n, [0x34; 10])
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
    let locks = Arc::new(WorktreeLockRegistry::new());
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
        locks,
        query_embedder: None,
        memory_query_embedder: None,
        recall_token_budget: 1500,
        consolidation_batch_size: 20,
        consolidation_queue_threshold: 50,
        consolidation_idle_checkpoint_hours: 24,
        // A short cadence — real tests, real (tiny) ticks, no virtual clock
        // (this crate has no tokio `test-util` feature; see
        // `consolidation_trigger.rs`'s own unit tests for why).
        consolidation_poll_interval: Duration::from_millis(10),
        normalization_poll_interval: Duration::from_millis(10),
        normalization: local_rag::daemon::normalization::NormalizationParams::default(),
        retention: RetentionParams {
            keep_last_k: 2,
            window_ms: 7 * 24 * 60 * 60 * 1000,
        },
        classifier: ClassifierConfig::new(1024 * 1024),
        indexing_backstop_poll_interval: Duration::from_millis(10),
    }
}

fn write_spool_segment(layout: &StoreLayout, session_id: &str, event_type: &str) {
    let session_dir = layout.spool_session(session_id);
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let frame = FramePayload {
        format_version: 1,
        source_event_id: format!("evt:{session_id}:1"),
        dedup_key: None,
        event_type: event_type.to_string(),
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
    let mut bytes = encode_segment_header().to_vec();
    bytes.extend_from_slice(&encode_frame(&frame).expect("under the frame cap"));
    std::fs::write(session_dir.join("000001.seg"), bytes).expect("write segment");
}

fn consolidation_run_count(layout: &StoreLayout, session_id: &str) -> i64 {
    let conn = rusqlite::Connection::open_with_flags(
        layout.state_db(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("open state.sqlite read-only");
    conn.query_row(
        "SELECT count(*) FROM consolidation_run WHERE session_id = ?1",
        [session_id],
        |r| r.get(0),
    )
    .expect("count consolidation_run rows")
}

#[tokio::test]
async fn a_stop_event_drives_a_real_consolidation_run_through_the_live_daemon() {
    let (_home, layout) = open_layout();
    write_spool_segment(&layout, "sess-a", "Stop");

    // `consolidation_queue_threshold: 1` rather than the default 50: T15-01's
    // own startup spool-import pass races the trigger worker's very first
    // tick for this same session, and whichever import call actually
    // consumes the fresh Stop bytes is the one that observes `saw_stop`
    // (D-024's own doc note on this exact race, `lifecycle.rs`'s
    // `spawn_consolidation_trigger`) — a low threshold makes this
    // acceptance test assert on the durable, race-independent effect (the
    // envelope's backlog eventually crosses the threshold) rather than on
    // which particular pass happens to win.
    let mut opts = start_options(layout.clone());
    opts.consolidation_queue_threshold = 1;
    let handle = DaemonHandle::start(opts).await.expect("start");

    tokio::time::timeout(Duration::from_secs(10), async {
        while consolidation_run_count(&layout, "sess-a") == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the trigger worker must open a run for the Stop-seeded session within the bound");

    handle.shutdown().await;
}

#[tokio::test]
async fn the_trigger_worker_alive_between_ticks_does_not_block_idle_shutdown() {
    let (_home, layout) = open_layout();
    let handle = DaemonHandle::start(start_options(layout.clone()))
        .await
        .expect("start");

    tokio::time::timeout(Duration::from_secs(10), async {
        while !handle.is_idle_eligible() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("startup resume must finish within the test's bound");

    // Let several trigger-worker ticks elapse while genuinely idle (no spool
    // sessions, no live session) — its `JobGuard` is held only for the
    // duration of one tick's active work (D-024), never across the wait
    // between ticks, so idle eligibility must survive.
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert!(
        handle.is_idle_eligible(),
        "the trigger worker being alive between ticks must not block idle shutdown"
    );

    handle.shutdown().await;
}

#[tokio::test]
async fn shutdown_completes_within_a_bounded_timeout_with_the_worker_alive() {
    let (_home, layout) = open_layout();
    let handle = DaemonHandle::start(start_options(layout.clone()))
        .await
        .expect("start");

    tokio::time::timeout(Duration::from_secs(5), handle.shutdown())
        .await
        .expect("shutdown must complete within the bound even with the D-024 worker alive");
}

// ---------------------------------------------------------------------------
// D-073: one owner of the stale-run sweep, so one attempt per start
// ---------------------------------------------------------------------------

/// Read one run's `attempt_count` straight from the store.
fn attempt_count(layout: &StoreLayout, run_id: &str) -> i64 {
    let conn = rusqlite::Connection::open_with_flags(
        layout.state_db(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("open state.sqlite read-only");
    conn.query_row(
        "SELECT attempt_count FROM consolidation_run WHERE run_id = ?1",
        [run_id],
        |r| r.get(0),
    )
    .expect("read attempt_count")
}

/// D-073: a parked run must get **one** attempt per daemon start, not two.
///
/// The fixture is a `failed` run whose last failure was `Mechanical` under a
/// *foreign* build fingerprint — D-050's "a rebuild earns it exactly one more
/// attempt" shape, which `stale_runs` therefore hands out exactly once. With
/// a poll interval far longer than the test's own bound, the only sweep that
/// can run is the startup one, so the attempt count after startup measures
/// precisely how many drivers performed it: before this fix the one-shot
/// `lifecycle::spawn_consolidation_resume` and the trigger's own immediate
/// first tick both did, and the count moved by two.
#[tokio::test]
async fn a_parked_run_gets_exactly_one_attempt_per_daemon_start() {
    let (_home, layout) = open_layout();
    {
        let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
        db.writer()
            .transaction(|tx| {
                create_consolidation_run(
                    tx,
                    &NewConsolidationRun {
                        run_id: "run-parked",
                        session_id: "sess-parked",
                        from_received_seq: 1,
                        to_received_seq: 3,
                        router_version: "v1",
                    },
                    1_000,
                )?;
                transition_run(tx, "run-parked", RunState::Running, 1_000)?
                    .expect("pending -> running");
                record_run_failure(
                    tx,
                    "run-parked",
                    FailureKind::Mechanical,
                    "seeded: dead-lettered under an older build",
                    false,
                    Some("some-older-build"),
                    1_000,
                )?
                .expect("running -> failed");
                Ok(())
            })
            .await
            .expect("seed parked run");
    }
    assert_eq!(attempt_count(&layout, "run-parked"), 1, "fixture");

    // Long enough that the trigger's *second* tick cannot land inside this
    // test: whatever the count reaches here was produced by startup alone.
    let mut opts = start_options(layout.clone());
    opts.consolidation_poll_interval = Duration::from_secs(30);
    let handle = DaemonHandle::start(opts).await.expect("start");

    tokio::time::timeout(Duration::from_secs(10), async {
        while attempt_count(&layout, "run-parked") == 1 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the startup sweep must retry the parked run within the bound");

    // Give a second sweep every chance to appear before asserting it did not.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        attempt_count(&layout, "run-parked"),
        2,
        "exactly one retry per start — two means two drivers swept the same rows"
    );

    handle.shutdown().await;
}
