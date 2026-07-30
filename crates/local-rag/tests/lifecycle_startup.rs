//! T15-01 acceptance tests for `daemon::lifecycle::DaemonHandle::start`
//! (spec 02 §4.1's five ordered startup steps): full startup order, and
//! migration-only health mode.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use local_rag::daemon::{
    DaemonHandle, DaemonMode, DaemonStartupError, MigrationOnlyReason, StartOptions,
};
use local_rag_core::DataPolicy;
use local_rag_core::identity::{Uuid, UuidSource, uuidv7_from};
use local_rag_core::paths::StoreLayout;
use local_rag_search::UnavailableEmbedder;
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
        uuidv7_from(1000 + n, [0xCD; 10])
    }
}

fn open_layout() -> (TempHome, StoreLayout) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    (home, layout)
}

fn start_options(layout: StoreLayout) -> StartOptions {
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
        query_embedder: Arc::new(UnavailableEmbedder),
        memory_query_embedder: Arc::new(local_rag_memory::recall::UnavailableEmbedder),
        recall_token_budget: 1500,
        consolidation_batch_size: 20,
        consolidation_queue_threshold: 50,
        consolidation_poll_interval: std::time::Duration::from_millis(50),
    }
}

#[tokio::test]
async fn full_startup_binds_ready_and_serves_normal_mode() {
    let (_home, layout) = open_layout();
    let socket_path = layout.socket_path();

    let handle = DaemonHandle::start(start_options(layout.clone()))
        .await
        .expect("start");

    assert_eq!(*handle.mode.borrow(), DaemonMode::Normal);
    assert!(handle.lock_info.ready);
    assert!(socket_path.exists(), "the endpoint must be bound");
    assert_eq!(handle.socket_path, socket_path);
    assert!(handle.sessions.is_empty());

    handle.shutdown().await;
    assert!(
        !layout.store_lock().exists(),
        "shutdown must release the lock"
    );
}

/// A store whose `schema_migrations` names a version newer than this binary
/// supports (spec 13 §3's "a store newer than the binary... surfaces as
/// `INCOMPATIBLE_STORE`") does not abort startup outright: the daemon still
/// binds and reports `MigrationOnly` (spec 02 §6 "nothing degrades
/// silently").
#[tokio::test]
async fn an_incompatible_store_enters_migration_only_mode_but_still_binds() {
    let (_home, layout) = open_layout();

    // Fully migrate a real store first (mirrors `crates/store/tests/migrate.rs`'s
    // own raw-connection fixture idiom), then simulate "a newer binary already
    // touched this store" by hand-inserting a schema_migrations row beyond
    // this binary's own `ALL` set.
    let max_version = {
        let mut conn = rusqlite::Connection::open(layout.state_db()).expect("open state db");
        local_rag_store::migrate::run(
            &mut conn,
            local_rag_store::ALL,
            &layout.migration_lock(),
            500,
        )
        .expect("migrate to latest");
        let max: u32 = conn
            .query_row("SELECT MAX(version) FROM schema_migrations", [], |r| {
                r.get(0)
            })
            .expect("max version");
        conn.execute(
            "INSERT INTO schema_migrations (version, name, checksum, applied_at) \
             VALUES (?1, 'from-the-future', 'fake-checksum', ?2)",
            rusqlite::params![max + 1, 600],
        )
        .expect("seed a from-the-future migration row");
        max
    };

    let handle = DaemonHandle::start(start_options(layout.clone()))
        .await
        .expect("start must still succeed (degraded mode, not a hard failure)");

    match &*handle.mode.borrow() {
        DaemonMode::MigrationOnly {
            reason:
                MigrationOnlyReason::IncompatibleStore {
                    store_version,
                    binary_max_version,
                },
        } => {
            assert_eq!(*store_version, max_version + 1);
            assert_eq!(*binary_max_version, max_version);
        }
        other => panic!("expected MigrationOnly(IncompatibleStore), got {other:?}"),
    }
    assert!(
        handle.socket_path.exists(),
        "the endpoint must still bind in degraded mode"
    );
    assert!(handle.lock_info.ready);

    handle.shutdown().await;
}

/// The same idea, but for a checksum-drift record instead of a future
/// version.
#[tokio::test]
async fn a_checksum_drift_store_enters_migration_only_mode_too() {
    let (_home, layout) = open_layout();

    {
        let mut conn = rusqlite::Connection::open(layout.state_db()).expect("open state db");
        local_rag_store::migrate::run(
            &mut conn,
            local_rag_store::ALL,
            &layout.migration_lock(),
            500,
        )
        .expect("migrate to latest");
        conn.execute(
            "UPDATE schema_migrations SET checksum = 'corrupted' WHERE version = 1",
            [],
        )
        .expect("corrupt version 1's checksum");
    }

    let handle = DaemonHandle::start(start_options(layout.clone()))
        .await
        .expect("start must still succeed");

    match &*handle.mode.borrow() {
        DaemonMode::MigrationOnly {
            reason: MigrationOnlyReason::ChecksumDrift { version, .. },
        } => assert_eq!(*version, 1),
        other => panic!("expected MigrationOnly(ChecksumDrift), got {other:?}"),
    }

    handle.shutdown().await;
}

/// A store lock already held by a live process refuses startup outright —
/// no partial `DaemonHandle`, a genuine [`DaemonStartupError`].
#[tokio::test]
async fn a_locked_store_refuses_startup() {
    let (_home, layout) = open_layout();
    let first = DaemonHandle::start(start_options(layout.clone()))
        .await
        .expect("first start");

    let second = DaemonHandle::start(start_options(layout.clone())).await;
    match second {
        Err(DaemonStartupError::Lock(_)) => {}
        Ok(_) => panic!("a second daemon must not start against a locked store"),
        Err(other) => panic!("expected Lock error, got {other:?}"),
    }

    first.shutdown().await;
}
