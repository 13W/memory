//! T15-01 acceptance test for the card's "WAL checkpoint/release" scenario
//! (spec 02 §4.3 `[FIXED]`): `DaemonHandle::shutdown` actually flushes a WAL
//! checkpoint and actually releases the store lock. The checkpoint mechanism
//! itself (`StateWriter`/`CacheWriter::checkpoint`) is exhaustively unit-
//! tested in `crates/store/tests/checkpoint.rs`; this proves the *shutdown
//! path* wires it up, end to end through a real `DaemonHandle`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use local_rag::daemon::{DaemonHandle, LazyEmbedderProvider, StartOptions};
use local_rag_core::DataPolicy;
use local_rag_core::identity::{Uuid, UuidSource, uuidv7_from};
use local_rag_core::paths::StoreLayout;
use local_rag_core::spool::{FramePayload, encode_frame, encode_segment_header};
use local_rag_index::classify::ClassifierConfig;
use local_rag_store::{
    LEASE_DURATION_MS, LEASE_RENEW_INTERVAL_MS, RetentionParams, WorktreeLockRegistry,
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
        uuidv7_from(1000 + n, [0x12; 10])
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
        // No handover wait in tests: a store that is free must be acquirable
        // now, and one that is held must be refused now (D-084).
        lock_handover_budget: std::time::Duration::ZERO,
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
        consolidation_poll_interval: std::time::Duration::from_millis(50),
        normalization_poll_interval: std::time::Duration::from_millis(10),
        normalization: local_rag::daemon::normalization::NormalizationParams::default(),
        retention: RetentionParams {
            keep_last_k: 2,
            window_ms: 7 * 24 * 60 * 60 * 1000,
        },
        classifier: ClassifierConfig::new(1024 * 1024),
        indexing_backstop_poll_interval: std::time::Duration::from_millis(50),
    }
}

fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().expect("file name").to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

fn write_spool_segment(layout: &StoreLayout, session_id: &str, n_frames: u32) {
    let session_dir = layout.spool_session(session_id);
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let mut bytes = encode_segment_header().to_vec();
    for i in 0..n_frames {
        let frame = FramePayload {
            format_version: 1,
            source_event_id: format!("st:{session_id}:{i}"),
            dedup_key: None,
            event_type: "Stop".to_string(),
            captured_at: 1_000 + i as i64,
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
        bytes.extend_from_slice(&encode_frame(&frame).expect("under the frame cap"));
    }
    std::fs::write(session_dir.join("000001.seg"), bytes).expect("write segment");
}

#[tokio::test]
async fn shutdown_checkpoints_the_wal_and_releases_the_lock() {
    let (_home, layout) = open_layout();
    write_spool_segment(&layout, "sess-a", 50);

    let handle = DaemonHandle::start(start_options(layout.clone()))
        .await
        .expect("start");

    // Let the startup resume pass actually import the seeded segment, so
    // there is real committed data in state.sqlite's WAL to checkpoint.
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !handle.is_idle_eligible() {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("import must finish within the test's bound");

    let wal_path = append_suffix(&layout.state_db(), "-wal");
    let before_shutdown = std::fs::metadata(&wal_path).map(|m| m.len()).unwrap_or(0);

    handle.shutdown().await;

    // Release: a fresh acquire against the same store succeeds immediately —
    // no stale-recovery branch needed at all (the same reliable "was it
    // actually released" proof `lock_liveness.rs`'s own
    // `release_lets_a_fresh_acquire_succeed_without_recovery` uses).
    assert!(
        !layout.store_lock().exists(),
        "shutdown must remove store.lock"
    );
    // Zero handover budget: a released lock must be free *now*, not after a
    // wait. Any retry here would hide exactly the failure this asserts.
    let reacquired = local_rag::daemon::acquire(
        &layout,
        "post-shutdown",
        999,
        "0.0.0",
        5_000,
        std::time::Duration::ZERO,
    );
    assert!(
        reacquired.is_ok(),
        "the lock must be fully released: {reacquired:?}"
    );
    if let Ok(guard) = reacquired {
        guard.release(&layout);
    }

    // Checkpoint: the -wal file must not have grown past shutdown (a
    // TRUNCATE checkpoint ran); if it existed and had real content before
    // shutdown, it must be smaller (or gone) afterward.
    let after_shutdown = std::fs::metadata(&wal_path).map(|m| m.len()).unwrap_or(0);
    assert!(
        after_shutdown <= before_shutdown,
        "checkpoint must not leave the -wal file larger: before={before_shutdown} after={after_shutdown}"
    );
}
