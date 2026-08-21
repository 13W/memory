//! T15-01 acceptance tests for `daemon::lifecycle::DaemonHandle::start`
//! (spec 02 §4.1's five ordered startup steps): full startup order, and
//! migration-only health mode.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use local_rag::daemon::{
    DaemonHandle, DaemonMode, DaemonStartupError, LazyEmbedderProvider, MigrationOnlyReason,
    StartOptions,
};
use local_rag_core::DataPolicy;
use local_rag_core::identity::{Uuid, UuidSource, uuidv7_from};
use local_rag_core::paths::StoreLayout;
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
    assert!(
        handle.indexing_supervisor().is_none(),
        "T20-06's indexing supervisor must never start in MigrationOnly \
         (no usable state.sqlite to read managed_worktree from)"
    );

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

/// spec 12 §6 `[SPEC]` ("the daemon refuses to start on a store whose owner
/// uid differs"), T16-04's "owner-only endpoint" — platform gated: `chown`
/// to a different uid only ever succeeds when this test process is already
/// root (`EPERM` otherwise), so the attempt itself is the gate, no separate
/// uid probe needed. Only `layout.root()` (a `TempHome`-isolated directory,
/// never a real system path) is ever chowned, and only when the attempt
/// already succeeded — this can never touch anything outside the test's own
/// temp tree. `perms::ensure_dir`'s `AlreadyExists` branch does
/// `symlink_metadata`/`verify_owner_meta` (both read-only) strictly
/// *before* its one write (`fs::set_permissions`), so the refusal below is
/// provably read-only regardless of privilege.
#[cfg(unix)]
#[tokio::test]
async fn a_wrong_owner_store_refuses_startup() {
    let (_home, layout) = open_layout(); // creates the tree, owned by us

    if std::os::unix::fs::chown(layout.root(), Some(1), None).is_err() {
        eprintln!("SKIP: not running as root — cannot fabricate a wrong-owner store");
        return;
    }

    match DaemonHandle::start(start_options(layout.clone())).await {
        Err(DaemonStartupError::Path(_)) => {}
        Ok(_) => panic!("a daemon must not start against a store it does not own"),
        Err(other) => panic!("expected Path(WrongOwner), got {other:?}"),
    }
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

/// `D-077`: shutdown must **signal** everything before it **waits** for
/// anything, and it must stop accepting first rather than last.
///
/// The order is the whole defect. `shutdown` used to await each worker to
/// completion before telling the next one anything, so for the length of the
/// first join the rest of the daemon was still running — and the indexing
/// supervisor, the one worker that starts minutes-long work on a timer, was
/// told last. Measured on the owner's store: `daemon stopping` at 11:16:28,
/// two fresh `indexing cycle started` lines at 11:16:59 and 11:17:17, and
/// `daemon stopped` only at 11:18:18. An indexing cycle there takes 109–140
/// seconds, so letting one start during shutdown costs two more minutes.
///
/// Asserted on the source rather than by driving a real daemon: reproducing it
/// needs a worker whose tick is long enough for another to start work inside
/// it, which is minutes of real indexing, and the property itself — "no await
/// stands between `daemon stopping` and the last signal" — is exactly a
/// statement about the order of the statements. Same shape and same reason as
/// `memory_normalization_worker.rs::the_generator_pool_is_built_once_per_process`.
#[test]
fn shutdown_signals_every_worker_before_it_waits_for_any_of_them() {
    let source = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/daemon/lifecycle.rs"
    ))
    .expect("read lifecycle.rs");
    let body = source
        .split_once("pub async fn shutdown(mut self) {")
        .expect("shutdown exists")
        .1;
    let body = body
        .split_once("drain_and_shutdown(")
        .expect("shutdown drains at the end")
        .0;
    // Comments quote these same identifiers; the assertions are about the code.
    let body: String = body
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");

    let at = |needle: &str| {
        body.find(needle)
            .unwrap_or_else(|| panic!("shutdown must still contain {needle:?}"))
    };

    let stop_accepting = at("handshake_join.take()");
    let signal_consolidation = at("consolidation_trigger_stop.take()");
    let signal_normalization = at("normalization_stop.take()");
    let cancel_indexing = at("indexing_supervisor.take()");
    // D-085: this used to name the three joins individually, and one of those
    // names — `resume_handles.drain(` — stopped existing when D-081 routed all
    // three through `await_workers_bounded` and took the handles with
    // `std::mem::take`. `at` panicked on the missing needle, so the test was
    // red on `master` from that commit onward. The anchor is now the single
    // construct that actually does the waiting, which is also the one this
    // assertion is about: the joins themselves are arguments evaluated on the
    // way into it, not separate wait points.
    let first_wait = at("await_workers_bounded(");

    assert!(
        stop_accepting < first_wait,
        "step 1 of spec 02 §4.3 is \"stop accepting\" — it cannot come after a wait",
    );
    for (what, at) in [
        ("the consolidation trigger", signal_consolidation),
        ("the normalization worker", signal_normalization),
        ("the indexing supervisor", cancel_indexing),
    ] {
        assert!(
            at < first_wait,
            "{what} must be told to stop before shutdown waits on anything — otherwise it \
             keeps running, and keeps starting new work, for the length of that wait (D-077)",
        );
    }
}
