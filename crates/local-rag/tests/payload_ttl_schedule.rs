//! `T23-09`/`D-123` acceptance tests: the payload TTL sweep actually runs
//! without a human typing `local-rag gc`, driven through a real
//! [`DaemonHandle`] — mirrors `lifecycle_startup.rs`/`idle_shutdown.rs`'s own
//! in-process style, not `cli_gc.rs`'s subprocess one, because what these
//! tests prove is the daemon's own scheduling, not the CLI wiring
//! `cli_gc.rs` already covers.
//!
//! `run_payload_ttl_sweep` itself (T13-05) has its exhaustive, deterministic-
//! clock unit tests in `crates/store/src/observation/payload_ttl.rs`; this
//! file's job is narrower and does not re-prove the sweep's own SQL. No test
//! here calls `run_payload_ttl_sweep` directly — every one seeds a row and
//! asks the *daemon* whether it went away, because the thing that was missing
//! before this card was a caller, not the sweep.
//!
//! Seeding uses raw `INSERT` for `observation_path`/`observation_payload`
//! through `StateDb::open(...).writer().transaction(...)` — the same
//! fixture idiom `cli_consolidation.rs`'s own `seed_observations` uses for
//! envelopes — because `insert_path`/`insert_payload` are `pub(crate)` in
//! `local-rag-store` and unreachable from an integration test; only
//! `insert_envelope` is public (T15-05).
//!
//! Every fixture's `expires_at` is set against the **real** wall clock
//! (`real_now_ms`, the same helper `cli_gc.rs` defines for its own subprocess
//! tests), never against `StartOptions.now_ms` — the whole point of this
//! card is that the worker reads the live clock per tick, not the instant the
//! daemon happened to start.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use local_rag::daemon::{DaemonHandle, LazyEmbedderProvider, StartOptions};
use local_rag_core::DataPolicy;
use local_rag_core::identity::{Uuid, UuidSource, uuidv7_from};
use local_rag_core::paths::StoreLayout;
use local_rag_index::classify::ClassifierConfig;
use local_rag_store::{
    LEASE_DURATION_MS, LEASE_RENEW_INTERVAL_MS, NewObservationEnvelope, RetentionParams, StateDb,
    WorktreeLockRegistry, insert_envelope,
};
use local_rag_test_support::TempHome;
use rusqlite::params;

struct SeqUuidV7 {
    counter: std::sync::atomic::AtomicU64,
}
impl SeqUuidV7 {
    fn new() -> Self {
        Self {
            counter: std::sync::atomic::AtomicU64::new(0),
        }
    }
}
impl UuidSource for SeqUuidV7 {
    fn next_uuid(&self) -> Uuid {
        let n = self
            .counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        uuidv7_from(1000 + n, [0x99; 10])
    }
}

fn open_layout() -> (TempHome, StoreLayout) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    (home, layout)
}

/// `gc_poll_interval` is the only field this file's tests vary; every other
/// field is the same fixed, inert configuration `lifecycle_startup.rs` uses.
fn start_options(layout: StoreLayout, gc_poll_interval: Duration) -> StartOptions {
    let embedder_provider = Arc::new(LazyEmbedderProvider::new(&layout));
    let locks = Arc::new(WorktreeLockRegistry::new());
    StartOptions {
        layout,
        daemon_version: "0.0.0".to_string(),
        now_ms: 1_000,
        lock_handover_budget: std::time::Duration::ZERO,
        indexing_shutdown_budget: local_rag::daemon::indexing::SHUTDOWN_JOIN_BUDGET,
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
        router_conflict_token_budget: local_rag_core::config::MemoryConfig::default()
            .router_conflict_token_budget,
        consolidation_batch_size: 20,
        consolidation_queue_threshold: 50,
        consolidation_idle_checkpoint_hours: 24,
        // Long enough that no consolidation tick can matter to these tests —
        // there is nothing for it to consolidate, and it must not race the
        // gc sweep's own writer-queue transaction.
        consolidation_poll_interval: Duration::from_secs(3600),
        normalization_poll_interval: Duration::from_secs(3600),
        normalization: local_rag::daemon::normalization::NormalizationParams::default(),
        retention: RetentionParams {
            keep_last_k: 2,
            window_ms: 7 * 24 * 60 * 60 * 1000,
        },
        classifier: ClassifierConfig::new(1024 * 1024),
        indexing_backstop_poll_interval: Duration::from_secs(3600),
        gc_poll_interval,
    }
}

/// The real wall clock, milliseconds since the epoch — the payload sweep
/// worker reads the live `system_now_ms()`, not `StartOptions.now_ms`
/// (frozen at `1_000` above), so every fixture here is seeded relative to
/// this clock, the same convention `cli_gc.rs::real_now_ms` establishes for
/// its own subprocess tests.
fn real_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_millis() as i64
}

/// Comfortably past any budget, so a few milliseconds of real wall-clock
/// drift between seeding and a tick actually running can never flip a fixture
/// from "due" to "not yet due".
const HEADROOM_MS: i64 = 60 * 60 * 1_000;

/// Insert one envelope + path + payload row directly against `layout`'s
/// `state.sqlite`, bypassing `import_batch` — this file only cares about the
/// three rows existing with a chosen `expires_at`, not how they got there.
/// A fresh `StateDb::open` rather than a handle already held elsewhere: the
/// same "write straight to the store, independent of any running daemon"
/// pattern `cli/consolidation.rs`'s own module doc states and relies on.
async fn seed_payload(layout: &StoreLayout, observation_id: &str, expires_at: i64) {
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    let observation_id = observation_id.to_string();
    db.writer()
        .transaction(move |tx| {
            insert_envelope(
                tx,
                &NewObservationEnvelope {
                    observation_id: &observation_id,
                    source_event_id: &observation_id,
                    dedup_key: None,
                    payload_hash: "deadbeef",
                    event_type: "Stop",
                    evidence_kind: "user_statement",
                    trust: "normal",
                    source_timestamp: Some(0),
                    repo_id: None,
                    worktree_id: None,
                    session_id: "sess-ttl",
                    agent_id: None,
                    turn_id: None,
                    batch_id: None,
                    commit_hash: None,
                    short_evidence_excerpt: None,
                    redaction_version: None,
                },
            )?;
            tx.execute(
                "INSERT INTO observation_path (observation_id, normalized_path) VALUES (?1, ?2)",
                params![observation_id, "src/a.rs"],
            )?;
            tx.execute(
                "INSERT INTO observation_payload \
                   (observation_id, redacted_payload, byte_size, expires_at) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![observation_id, b"{}".as_slice(), 2i64, expires_at],
            )?;
            Ok(())
        })
        .await
        .expect("seed payload row");
}

/// Row counts read straight from `state.sqlite`, independent of whatever
/// `StateDb` instance the live daemon holds — a second read-only connection
/// against the same WAL file, the same pattern
/// `consolidation_trigger.rs::attempt_count` already establishes for polling
/// a running daemon's own store from outside it.
struct Counts {
    envelopes: i64,
    paths: i64,
    payloads: i64,
}

fn counts(layout: &StoreLayout, observation_id: &str) -> Counts {
    let conn = rusqlite::Connection::open_with_flags(
        layout.state_db(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("open state.sqlite read-only");
    let envelopes: i64 = conn
        .query_row(
            "SELECT count(*) FROM observation_envelope WHERE observation_id = ?1",
            [observation_id],
            |r| r.get(0),
        )
        .expect("read observation_envelope");
    let paths: i64 = conn
        .query_row(
            "SELECT count(*) FROM observation_path WHERE observation_id = ?1",
            [observation_id],
            |r| r.get(0),
        )
        .expect("read observation_path");
    let payloads: i64 = conn
        .query_row(
            "SELECT count(*) FROM observation_payload WHERE observation_id = ?1",
            [observation_id],
            |r| r.get(0),
        )
        .expect("read observation_payload");
    Counts {
        envelopes,
        paths,
        payloads,
    }
}

/// Bounded wait for `observation_id`'s payload row to disappear — the same
/// timeout-plus-poll shape `idle_shutdown.rs::wait_until_idle_eligible` and
/// `consolidation_trigger.rs`'s own stale-run poll use, so a slow CI machine
/// gets more time rather than a flaky race.
async fn wait_for_payload_removed(layout: &StoreLayout, observation_id: &str) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while counts(layout, observation_id).payloads > 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the payload TTL sweep must remove the overdue row within the bound");
}

/// The card's own acceptance shape: an overdue payload is removed by the
/// daemon with nobody typing `local-rag gc`. `gc_poll_interval` is an hour —
/// far longer than this test's own bound — so only the worker's *immediate*
/// first tick can be what removes it, proving that tick alone is a working
/// substitute for `spawn_startup_gc`'s one-shot pattern.
///
/// Also the card's structural test: `observation_envelope`/`observation_path`
/// survive — spec 12 §3's "envelopes are durable... survive payload expiry",
/// unchanged by this card and reasserted here against the real worker rather
/// than only against `run_payload_ttl_sweep` directly.
#[tokio::test]
async fn an_overdue_payload_is_removed_by_the_daemon_without_a_command() {
    let (_home, layout) = open_layout();
    let overdue_at = real_now_ms() - HEADROOM_MS;
    seed_payload(&layout, "obs-overdue", overdue_at).await;

    let handle = DaemonHandle::start(start_options(layout.clone(), Duration::from_secs(3600)))
        .await
        .expect("start");

    wait_for_payload_removed(&layout, "obs-overdue").await;

    let after = counts(&layout, "obs-overdue");
    assert_eq!(after.payloads, 0, "the overdue payload must be gone");
    assert_eq!(
        after.envelopes, 1,
        "the envelope must survive payload expiry (spec 12 §3)"
    );
    assert_eq!(
        after.paths, 1,
        "observation_path must survive payload expiry too"
    );

    handle.shutdown().await;
}

/// The card's other half: the sweep fires again **on the tick**, not only at
/// start — proven so a single-shot worker cannot pass this test by accident.
/// The payload is **not yet due** when the daemon starts (`expires_at` a
/// short, real-clock moment in the future): the immediate first tick sees
/// `expires_at > now` and must remove nothing, so only a *later* tick, once
/// real wall-clock time actually passes the deadline, can be what removes it.
/// `expires_at` is set against the real clock (far larger than
/// `StartOptions.now_ms == 1_000`), so this test also fails if the worker
/// were reading the frozen startup clock instead of the live one: against
/// `1_000` this row would never become due at all.
#[tokio::test]
async fn the_sweep_fires_again_on_the_tick_not_only_at_start() {
    let (_home, layout) = open_layout();
    let becomes_due_at = real_now_ms() + 300;
    seed_payload(&layout, "obs-later", becomes_due_at).await;

    let handle = DaemonHandle::start(start_options(layout.clone(), Duration::from_millis(20)))
        .await
        .expect("start");

    wait_for_payload_removed(&layout, "obs-later").await;

    let after = counts(&layout, "obs-later");
    assert_eq!(after.payloads, 0, "a later tick must remove it too");
    assert_eq!(after.envelopes, 1, "the envelope still survives");

    handle.shutdown().await;
}

/// A payload not yet due must survive alongside one that is — the sweep
/// must not delete the table, only rows past their own deadline.
#[tokio::test]
async fn a_payload_that_is_not_due_survives() {
    let (_home, layout) = open_layout();
    let overdue_at = real_now_ms() - HEADROOM_MS;
    let not_due_at = real_now_ms() + HEADROOM_MS;
    seed_payload(&layout, "obs-overdue", overdue_at).await;
    seed_payload(&layout, "obs-live", not_due_at).await;

    let handle = DaemonHandle::start(start_options(layout.clone(), Duration::from_millis(20)))
        .await
        .expect("start");

    wait_for_payload_removed(&layout, "obs-overdue").await;

    // Give the worker every chance to have run more than once before
    // asserting the live row is untouched.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let live = counts(&layout, "obs-live");
    assert_eq!(live.payloads, 1, "a payload not yet due must survive");

    handle.shutdown().await;
}
