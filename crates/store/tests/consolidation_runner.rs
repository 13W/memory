//! T14-06 acceptance tests for the consolidation lease/cursor runner (spec 08
//! §4): bounded snapshot never past `to_seq`, lease expiry/renewal while the
//! generator runs outside any transaction, atomic op apply (a mid-batch
//! rejection rolls back the *whole* attempt — no partial mutation, no
//! advanced cursor), retry-after-rejection producing no duplicate rows, and
//! crash recovery at each of the runner's named failpoints.
//!
//! All tests are deterministic: an isolated [`TempHome`], fixed `now_ms`
//! literals, ids minted from [`uuidv7_from`] with fixed entropy, and — for
//! the lease-renewal test — paused virtual time (`start_paused = true`) so
//! nothing here sleeps in real wall-clock time.
//!
//! Failpoint tests share [`SERIAL`] with every other test in this file, for
//! the same reason `crates/store/tests/memory_op.rs` does: the failpoint
//! registry (`local_rag_test_support::failpoint::global()`) is process-wide,
//! so an armed-but-not-yet-disarmed failpoint in one test could otherwise
//! fire in a concurrently running test in this same binary.

use std::time::Duration;

use local_rag_core::identity::uuidv7_from;
use local_rag_core::paths::StoreLayout;
#[cfg(feature = "failpoints")]
use local_rag_store::memory::RunnerError;
use local_rag_store::memory::{
    ApplyReport, ClassifiedFailure, ConsolidationWindow, GeneratedOp, NewMemoryEntry,
    ProposedOperation, RunOutcome, RunOutcomeError, RunState, SnapshotOutcome,
    candidate_evidence_for, commit_apply_run, consolidation_run_state, create_memory_entry,
    memory_evidence_for, open_next_run, processing_cursor, run_once, stale_runs,
};
use local_rag_store::rusqlite::{Connection, params};
use local_rag_store::{
    LEASE_DURATION_MS, LEASE_RENEW_INTERVAL_MS, MemoryKind, ScopeKind, StateDb,
    UNBOUNDED_WINDOW_CHARS,
};
use local_rag_test_support::TempHome;
use tokio::sync::Mutex;

#[cfg(feature = "failpoints")]
use local_rag_test_support::Action;

static SERIAL: Mutex<()> = Mutex::const_new(());

/// A temporary store with an ensured tree and an opened [`StateDb`] (runs the
/// full production migration set).
fn open_state() -> (TempHome, StateDb) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    (home, db)
}

/// A distinct, deterministic UUIDv7 string keyed by `seed`, never touching the
/// clock or entropy source.
fn uuid(seed: u8) -> String {
    let mut rand = [0u8; 10];
    rand[9] = seed;
    uuidv7_from(1000, rand).to_string()
}

/// Insert `count` standalone `observation_envelope` rows for `session_id`
/// (received_seq 1..=count, assuming an otherwise-empty session), returning
/// their `observation_id`s in order.
async fn seed_envelopes(db: &StateDb, session_id: &str, seed: u8, count: u8) -> Vec<String> {
    let ids: Vec<String> = (0..count).map(|i| uuid(seed.wrapping_add(i))).collect();
    let (session, insert_ids) = (session_id.to_string(), ids.clone());
    db.writer()
        .transaction(move |tx| {
            for (i, id) in insert_ids.iter().enumerate() {
                tx.execute(
                    "INSERT INTO observation_envelope \
                       (observation_id, source_event_id, payload_hash, event_type, \
                        evidence_kind, trust, session_id) \
                     VALUES (?1, ?2, 'deadbeef', 'Stop', 'user_statement', 'normal', ?3)",
                    params![id, format!("evt-{i}"), session],
                )?;
            }
            Ok(())
        })
        .await
        .expect("seed envelopes");
    ids
}

/// Open the next run for `session_id`, unwrapping the infrastructure result
/// (tests assert on the domain [`SnapshotOutcome`] themselves).
async fn open_run(
    db: &StateDb,
    run_id: &str,
    session_id: &str,
    batch: i64,
    now_ms: i64,
) -> SnapshotOutcome {
    let (rid, sid) = (run_id.to_string(), session_id.to_string());
    db.writer()
        .transaction(move |tx| {
            open_next_run(
                tx,
                &rid,
                &sid,
                batch,
                UNBOUNDED_WINDOW_CHARS,
                "v1",
                LEASE_DURATION_MS,
                now_ms,
                "build-test",
            )
        })
        .await
        .expect("open tx")
}

/// A worktree-scoped `fact` entry with no canonical key, at `entry_version =
/// 1`.
async fn create_memory(db: &StateDb, memory_id: &str, scope_owner: &str) {
    let (id, owner) = (memory_id.to_string(), scope_owner.to_string());
    db.writer()
        .transaction(move |tx| {
            create_memory_entry(
                tx,
                &NewMemoryEntry {
                    memory_id: &id,
                    kind: MemoryKind::Fact,
                    text: "some durable text",
                    canonical_key: None,
                    scope_kind: ScopeKind::Worktree,
                    scope_owner_id: &owner,
                    confidence: 0.5,
                    importance: 0.5,
                    valid_from_tree: None,
                    last_verified_tree: None,
                    supersedes_id: None,
                },
                1_000,
            )
        })
        .await
        .expect("create tx")
        .expect("create ok");
}

fn entry_version(conn: &Connection, memory_id: &str) -> i64 {
    conn.query_row(
        "SELECT entry_version FROM memory_entry WHERE memory_id = ?1",
        params![memory_id],
        |r| r.get(0),
    )
    .expect("read entry_version")
}

fn audit_count_for(conn: &Connection, entity_id: &str) -> i64 {
    conn.query_row(
        "SELECT count(*) FROM audit_event WHERE entity_id = ?1",
        params![entity_id],
        |r| r.get(0),
    )
    .expect("count audit rows")
}

fn read_lease_until(conn: &Connection, run_id: &str) -> Option<i64> {
    conn.query_row(
        "SELECT lease_until FROM consolidation_run WHERE run_id = ?1",
        params![run_id],
        |r| r.get(0),
    )
    .expect("read lease_until")
}

// ---------------------------------------------------------------------------
// Never past to_seq
// ---------------------------------------------------------------------------

#[tokio::test]
async fn open_next_run_never_bounds_the_window_past_max_received_seq() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    seed_envelopes(&db, "sess-1", 1, 5).await;

    match open_run(&db, &uuid(9), "sess-1", 3, 1_000).await {
        SnapshotOutcome::Opened(window) => {
            assert_eq!(window.from_received_seq, 1);
            assert_eq!(
                window.to_received_seq, 3,
                "to = min(from+batch-1, max_seq) = min(3, 5) = 3, never past to_seq"
            );
        }
        other => panic!("expected Opened, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Generator runs outside any transaction
// ---------------------------------------------------------------------------

/// Operational proof that `run_once` never holds the writer-queue slot open
/// across the generator call: while a deliberately-blocked mock generator is
/// pending, an unrelated write must still complete promptly. Structurally,
/// the generator closure's parameter type is [`ConsolidationWindow`] — plain
/// owned data with no database handle of any kind — so it could never reach
/// the runner's own transaction even if it tried; this test proves the
/// *operational* consequence of that structural guarantee.
#[tokio::test]
async fn generator_runs_outside_any_tx_writer_queue_stays_free_while_it_is_pending() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    seed_envelopes(&db, "sess-1", 60, 2).await;
    let run_id = uuid(65);
    let SnapshotOutcome::Opened(window) = open_run(&db, &run_id, "sess-1", 10, 1_000).await else {
        panic!("expected Opened");
    };
    let lease_until = 1_000 + LEASE_DURATION_MS;

    let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
    let (unblock_tx, unblock_rx) = tokio::sync::oneshot::channel::<()>();

    let generate = move |_window: ConsolidationWindow| async move {
        let _ = started_tx.send(());
        let _ = unblock_rx.await;
        Ok::<Vec<GeneratedOp>, ClassifiedFailure>(vec![GeneratedOp::Noop])
    };

    let run_fut = run_once(
        &db,
        window,
        lease_until,
        LEASE_DURATION_MS,
        LEASE_RENEW_INTERVAL_MS,
        1_000,
        "build-test",
        generate,
    );

    let probe_fut = async {
        started_rx.await.expect("generator started");
        let probe = tokio::time::timeout(
            Duration::from_secs(5),
            db.writer().transaction(|tx| {
                tx.execute("CREATE TABLE IF NOT EXISTS probe (x INTEGER)", [])?;
                tx.execute("INSERT INTO probe (x) VALUES (1)", [])
            }),
        )
        .await;
        assert!(
            probe.is_ok(),
            "an unrelated write must not hang while the generator is pending"
        );
        let _ = unblock_tx.send(());
    };

    let (run_result, ()) = tokio::join!(run_fut, probe_fut);
    assert!(
        matches!(run_result, Ok(RunOutcome::Applied(_))),
        "{run_result:?}"
    );
}

// ---------------------------------------------------------------------------
// Lease expiry/renewal
// ---------------------------------------------------------------------------

/// A generator that outlives one lease duration only survives if the runner
/// actually renews the lease every [`LEASE_RENEW_INTERVAL_MS`] while it
/// runs. Paused virtual time (`start_paused = true`) auto-advances whenever
/// every pending future is blocked purely on a timer (the documented tokio
/// `test-util` behavior `crates/store/tests/lock.rs` already relies on for
/// `read_bounded`'s timeout test) — no real sleep anywhere in this test.
#[tokio::test(start_paused = true)]
async fn lease_renews_on_cadence_while_the_generator_runs() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    seed_envelopes(&db, "sess-1", 80, 2).await;
    let run_id = uuid(85);
    let SnapshotOutcome::Opened(window) = open_run(&db, &run_id, "sess-1", 10, 0).await else {
        panic!("expected Opened");
    };
    let original_lease_until = LEASE_DURATION_MS; // acquired at now_ms = 0

    let generate = move |_w: ConsolidationWindow| async move {
        // Longer than one lease duration (120s): only completes without the
        // run's lease going stale if renewal actually happens every ~30s.
        tokio::time::sleep(Duration::from_millis(150_000)).await;
        Ok::<Vec<GeneratedOp>, ClassifiedFailure>(vec![GeneratedOp::Noop])
    };

    let run_fut = run_once(
        &db,
        window,
        original_lease_until,
        LEASE_DURATION_MS,
        LEASE_RENEW_INTERVAL_MS,
        0,
        "build-test",
        generate,
    );

    let probe_fut = async {
        // D-034: a single `sleep(45_000)` then one-shot read raced the
        // renewal's own write under CI load. The renewal transaction crosses
        // to `StateWriter`'s dedicated real OS thread (never inline on the
        // async task, `daemon::shutdown`'s own doc), so it is not a pure
        // timer wait — `tokio::time::pause`'s auto-advance-on-idle only
        // tracks registered timers, not that channel round-trip. On a fast,
        // unloaded machine the writer thread replies before the executor
        // ever considers itself idle, so this never raced locally; a
        // contended CI runner can let auto-advance skip the probe's own
        // sleep past 30s (the renewal's virtual deadline) before that real
        // reply has actually landed, observed once as `got Some(120000)`
        // (the pre-renewal value) on a real `windows/ubuntu-latest` run.
        // Poll instead of a single point-in-time check: each retry both
        // advances virtual time (past the first renewal's 30s deadline, and
        // past `original_lease_until` all the same) and lets more real time
        // elapse for the pending write to land, without weakening the
        // assertion — it still fails loudly, just after genuinely giving the
        // renewal a chance to be visible, up to and past the run's own 150s
        // generator sleep so a truly-never-renewed lease still fails, not
        // just a slow one.
        let deadline = tokio::time::Instant::now() + Duration::from_millis(160_000);
        loop {
            tokio::time::sleep(Duration::from_millis(5_000)).await;
            let read = db.open_read().expect("read conn");
            if read_lease_until(&read, &run_id).unwrap_or(0) > original_lease_until {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("lease_until never moved forward from the original {original_lease_until}");
            }
        }
    };

    let (run_result, ()) = tokio::join!(run_fut, probe_fut);
    assert!(
        matches!(run_result, Ok(RunOutcome::Applied(_))),
        "{run_result:?}"
    );
}

// ---------------------------------------------------------------------------
// Atomicity: cursor cannot advance on partial apply
// ---------------------------------------------------------------------------

/// A later op's rejection must roll back the *whole* batch — including an
/// earlier op that would, on its own, have succeeded — and must never
/// advance the cursor. This is the regression guard for T14-06's central
/// design fix: `StateWriter::transaction` commits on an outer `Ok(_)`
/// regardless of an inner `Err`, so [`commit_apply_run`] must convert a
/// rejection into a genuine `rusqlite::Error` rather than returning
/// `Ok(Err(_))` from the write closure.
#[tokio::test]
async fn cursor_cannot_advance_when_a_later_op_is_rejected() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    seed_envelopes(&db, "sess-1", 40, 2).await;
    let run_id = uuid(45);
    let SnapshotOutcome::Opened(window) = open_run(&db, &run_id, "sess-1", 10, 1_000).await else {
        panic!("expected Opened");
    };
    let lease_until = 1_000 + LEASE_DURATION_MS;

    let owner = uuid(46);
    let memory_id = uuid(47);
    create_memory(&db, &memory_id, &owner).await;

    let ops = vec![
        GeneratedOp::Noop, // op 0: would succeed trivially
        GeneratedOp::Materialize {
            operation: ProposedOperation::Reinforce {
                memory_id: memory_id.clone(),
                expected_version: 7, // wrong -- rejects op 1
                confidence: Some(0.5),
            },
            evidence_observation_ids: vec![],
        },
    ];

    let err = commit_apply_run(&db, window.clone(), vec![], lease_until, ops, 2_000)
        .await
        .expect_err("op 1's optimistic conflict must reject the whole batch");
    assert!(matches!(err, RunOutcomeError::Rejected(_)), "{err:?}");

    let read = db.open_read().expect("read conn");
    assert_eq!(
        processing_cursor(&read, "sess-1").expect("cursor"),
        None,
        "cursor must not advance -- the whole batch (including op 0's noop) rolled back"
    );
    assert_eq!(
        consolidation_run_state(&read, &run_id).expect("state"),
        Some(RunState::Running),
        "run stays running (retry-eligible), never applied"
    );
    assert_eq!(
        entry_version(&read, &memory_id),
        1,
        "no mutation landed at all"
    );
}

// ---------------------------------------------------------------------------
// Op retry no duplicates
// ---------------------------------------------------------------------------

/// Given atomic apply (the fix above) plus lease fencing, a *successful*
/// `commit_apply_run` always finalizes its run to `applied` in the same
/// transaction — so a legitimate retry can only ever follow a *rejected* (and
/// therefore fully rolled-back) attempt, never a successful one. This test
/// proves the actually-reachable "retry no duplicates" property: the
/// rejected first attempt leaves zero residue, and the corrected retry
/// produces *exactly one* committed effect, not two.
#[tokio::test]
async fn retry_after_a_rejected_batch_produces_exactly_one_set_of_rows() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let ids = seed_envelopes(&db, "sess-1", 30, 2).await;
    let run_id = uuid(35);
    let SnapshotOutcome::Opened(window) = open_run(&db, &run_id, "sess-1", 10, 1_000).await else {
        panic!("expected Opened");
    };
    let lease_until = 1_000 + LEASE_DURATION_MS;

    let owner = uuid(36);
    let memory_id = uuid(37);
    create_memory(&db, &memory_id, &owner).await;

    let first_attempt = vec![
        GeneratedOp::Materialize {
            operation: ProposedOperation::Reinforce {
                memory_id: memory_id.clone(),
                expected_version: 1,
                confidence: Some(0.9),
            },
            evidence_observation_ids: vec![ids[0].clone()],
        },
        GeneratedOp::Materialize {
            operation: ProposedOperation::Reinforce {
                memory_id: memory_id.clone(),
                expected_version: 99, // wrong -- rejects the whole batch
                confidence: Some(0.95),
            },
            evidence_observation_ids: vec![ids[1].clone()],
        },
    ];
    let err = commit_apply_run(
        &db,
        window.clone(),
        vec![],
        lease_until,
        first_attempt,
        2_000,
    )
    .await
    .expect_err("op 1's optimistic conflict must reject the whole batch");
    assert!(matches!(err, RunOutcomeError::Rejected(_)), "{err:?}");

    let read = db.open_read().expect("read conn");
    assert_eq!(
        entry_version(&read, &memory_id),
        1,
        "op 0 must not have committed either"
    );
    assert_eq!(
        audit_count_for(&read, &memory_id),
        0,
        "no audit row from the rejected attempt"
    );
    drop(read);

    // A realistic retry after the router observes current state: just the
    // corrected op.
    let retry = vec![GeneratedOp::Materialize {
        operation: ProposedOperation::Reinforce {
            memory_id: memory_id.clone(),
            expected_version: 1,
            confidence: Some(0.9),
        },
        evidence_observation_ids: vec![ids[0].clone()],
    }];
    let report = commit_apply_run(&db, window, vec![], lease_until, retry, 3_000)
        .await
        .expect("retry succeeds");
    assert_eq!(
        report,
        ApplyReport {
            applied: 1,
            replayed: 0,
            noop: 0,
            proposed: 0,
        }
    );

    let read = db.open_read().expect("read conn");
    assert_eq!(
        entry_version(&read, &memory_id),
        2,
        "exactly one reinforce landed, not two"
    );
    assert_eq!(
        audit_count_for(&read, &memory_id),
        1,
        "exactly one audit_event row"
    );
}

// ---------------------------------------------------------------------------
// Crash at each failpoint
// ---------------------------------------------------------------------------

#[cfg(feature = "failpoints")]
fn arm(name: &str) {
    let fp = local_rag_test_support::failpoint::global();
    fp.register(name);
    fp.arm(name, Action::Error).expect("arm failpoint");
}

#[cfg(feature = "failpoints")]
fn disarm(name: &str) {
    local_rag_test_support::failpoint::global()
        .disarm(name)
        .expect("disarm failpoint");
}

#[cfg(feature = "failpoints")]
async fn noop_generator(_w: ConsolidationWindow) -> Result<Vec<GeneratedOp>, ClassifiedFailure> {
    Ok(vec![GeneratedOp::Noop])
}

#[cfg(feature = "failpoints")]
#[tokio::test]
async fn crash_after_snapshot_leaves_the_run_untouched_and_retry_converges() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    seed_envelopes(&db, "sess-1", 50, 2).await;
    let run_id = uuid(52);
    let SnapshotOutcome::Opened(window) = open_run(&db, &run_id, "sess-1", 10, 1_000).await else {
        panic!("expected Opened");
    };
    let lease_until = 1_000 + LEASE_DURATION_MS;

    arm("memory.consolidation.after_snapshot");
    let result = run_once(
        &db,
        window.clone(),
        lease_until,
        LEASE_DURATION_MS,
        LEASE_RENEW_INTERVAL_MS,
        1_000,
        "build-test",
        noop_generator,
    )
    .await;
    assert!(
        matches!(result, Err(RunnerError::FailpointInjected)),
        "{result:?}"
    );
    disarm("memory.consolidation.after_snapshot");

    let read = db.open_read().expect("read conn");
    assert_eq!(
        consolidation_run_state(&read, &run_id).expect("state"),
        Some(RunState::Running),
        "the failpoint fires before any further write -- nothing changed"
    );
    assert_eq!(processing_cursor(&read, "sess-1").expect("cursor"), None);
    drop(read);

    let outcome = run_once(
        &db,
        window,
        lease_until,
        LEASE_DURATION_MS,
        LEASE_RENEW_INTERVAL_MS,
        1_000,
        "build-test",
        noop_generator,
    )
    .await
    .expect("retry run_once");
    assert!(matches!(outcome, RunOutcome::Applied(_)), "{outcome:?}");
}

#[cfg(feature = "failpoints")]
#[tokio::test]
async fn crash_after_generate_leaves_the_run_untouched_and_retry_converges() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    seed_envelopes(&db, "sess-1", 55, 2).await;
    let run_id = uuid(57);
    let SnapshotOutcome::Opened(window) = open_run(&db, &run_id, "sess-1", 10, 1_000).await else {
        panic!("expected Opened");
    };
    let lease_until = 1_000 + LEASE_DURATION_MS;

    arm("memory.consolidation.after_generate");
    let result = run_once(
        &db,
        window.clone(),
        lease_until,
        LEASE_DURATION_MS,
        LEASE_RENEW_INTERVAL_MS,
        1_000,
        "build-test",
        noop_generator,
    )
    .await;
    assert!(
        matches!(result, Err(RunnerError::FailpointInjected)),
        "{result:?}"
    );
    disarm("memory.consolidation.after_generate");

    let read = db.open_read().expect("read conn");
    assert_eq!(
        consolidation_run_state(&read, &run_id).expect("state"),
        Some(RunState::Running),
        "the failpoint fires after the (successful) generator call but before apply -- \
         nothing was ever applied"
    );
    assert_eq!(processing_cursor(&read, "sess-1").expect("cursor"), None);
    drop(read);

    let outcome = run_once(
        &db,
        window,
        lease_until,
        LEASE_DURATION_MS,
        LEASE_RENEW_INTERVAL_MS,
        1_000,
        "build-test",
        noop_generator,
    )
    .await
    .expect("retry run_once");
    assert!(matches!(outcome, RunOutcome::Applied(_)), "{outcome:?}");
}

#[cfg(feature = "failpoints")]
#[tokio::test]
async fn crash_before_cursor_advance_rolls_back_the_whole_apply() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    seed_envelopes(&db, "sess-1", 58, 2).await;
    let run_id = uuid(59);
    let SnapshotOutcome::Opened(window) = open_run(&db, &run_id, "sess-1", 10, 1_000).await else {
        panic!("expected Opened");
    };
    let lease_until = 1_000 + LEASE_DURATION_MS;
    let to_seq = window.to_received_seq;

    arm("memory.consolidation.apply.before_cursor_advance");
    let result = commit_apply_run(
        &db,
        window.clone(),
        vec![],
        lease_until,
        vec![GeneratedOp::Noop],
        2_000,
    )
    .await;
    assert!(
        matches!(result, Err(RunOutcomeError::Write(_))),
        "{result:?}"
    );
    disarm("memory.consolidation.apply.before_cursor_advance");

    let read = db.open_read().expect("read conn");
    assert_eq!(
        processing_cursor(&read, "sess-1").expect("cursor"),
        None,
        "the noop's own zero-write nature doesn't matter -- the cursor advance never committed"
    );
    assert_eq!(
        consolidation_run_state(&read, &run_id).expect("state"),
        Some(RunState::Running)
    );
    drop(read);

    let report = commit_apply_run(
        &db,
        window,
        vec![],
        lease_until,
        vec![GeneratedOp::Noop],
        3_000,
    )
    .await
    .expect("retry succeeds");
    assert_eq!(
        report,
        ApplyReport {
            applied: 0,
            replayed: 0,
            noop: 1,
            proposed: 0,
        }
    );
    let read = db.open_read().expect("read conn");
    assert_eq!(
        processing_cursor(&read, "sess-1").expect("cursor"),
        Some(to_seq)
    );
    assert_eq!(
        consolidation_run_state(&read, &run_id).expect("state"),
        Some(RunState::Applied)
    );
}

// ---------------------------------------------------------------------------
// D-069: a repeated citation inside one op, and the dead-letter that bounds
// every other deterministic apply failure
// ---------------------------------------------------------------------------

/// `(last_failure_kind, last_failure_fingerprint, next_retry_at, attempt_count)`
/// — D-050's circuit-breaker bookkeeping, read back raw.
fn failure_row(
    conn: &Connection,
    run_id: &str,
) -> (Option<String>, Option<String>, Option<i64>, i64) {
    conn.query_row(
        "SELECT last_failure_kind, last_failure_fingerprint, next_retry_at, attempt_count \
         FROM consolidation_run WHERE run_id = ?1",
        params![run_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )
    .expect("read failure bookkeeping")
}

/// The live D-069 defect, on the `Materialize` branch: router output is
/// untrusted data and cited the *same* `observation_id` twice inside one op.
/// `memory_evidence` is keyed `(memory_id, observation_id)`, so before the fix
/// the repeat aborted the whole window transaction — and, classified
/// `Transient`, was retried forever at one full local-model generation per
/// ~15s. The duplicate must simply collapse.
#[tokio::test]
async fn a_repeated_citation_inside_one_materialize_op_still_applies() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let ids = seed_envelopes(&db, "sess-1", 60, 2).await;
    let run_id = uuid(62);
    let SnapshotOutcome::Opened(window) = open_run(&db, &run_id, "sess-1", 10, 1_000).await else {
        panic!("expected Opened");
    };
    let lease_until = 1_000 + LEASE_DURATION_MS;

    let owner = uuid(63);
    let memory_id = uuid(64);
    create_memory(&db, &memory_id, &owner).await;

    let ops = vec![GeneratedOp::Materialize {
        operation: ProposedOperation::Reinforce {
            memory_id: memory_id.clone(),
            expected_version: 1,
            confidence: Some(0.9),
        },
        // ids[0] cited twice -- exactly the shape run 01a01648 hit.
        evidence_observation_ids: vec![ids[0].clone(), ids[1].clone(), ids[0].clone()],
    }];
    let generated = ops.clone();
    let outcome = run_once(
        &db,
        window.clone(),
        lease_until,
        LEASE_DURATION_MS,
        LEASE_RENEW_INTERVAL_MS,
        1_000,
        "build-test",
        move |_w| async move { Ok::<_, ClassifiedFailure>(generated) },
    )
    .await
    .expect("run_once");
    assert!(matches!(outcome, RunOutcome::Applied(_)), "{outcome:?}");

    let read = db.open_read().expect("read conn");
    let mut expected = vec![ids[0].clone(), ids[1].clone()];
    expected.sort();
    assert_eq!(
        memory_evidence_for(&read, &memory_id).expect("evidence"),
        expected,
        "the repeated citation collapses to one row, the distinct one survives"
    );
    assert_eq!(
        processing_cursor(&read, "sess-1").expect("cursor"),
        Some(window.to_received_seq),
        "the window really consolidated"
    );
    assert_eq!(
        consolidation_run_state(&read, &run_id).expect("state"),
        Some(RunState::Applied)
    );
    drop(read);

    // Idempotence: replaying the very same window writes no second evidence
    // row (lease fencing rejects it -- the run is terminal).
    let err = commit_apply_run(&db, window, vec![], lease_until, ops, 3_000)
        .await
        .expect_err("an applied run can never be applied twice");
    assert!(matches!(err, RunOutcomeError::Rejected(_)), "{err:?}");
    let read = db.open_read().expect("read conn");
    assert_eq!(
        memory_evidence_for(&read, &memory_id).expect("evidence"),
        expected,
        "still exactly the same two rows"
    );
}

/// The same defect on the `ProposeCandidate` branch — the one the live
/// incident actually hit (`candidate_evidence.candidate_id,
/// candidate_evidence.observation_id`).
#[tokio::test]
async fn a_repeated_citation_inside_one_propose_candidate_op_still_applies() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let ids = seed_envelopes(&db, "sess-1", 70, 2).await;
    let run_id = uuid(72);
    let SnapshotOutcome::Opened(window) = open_run(&db, &run_id, "sess-1", 10, 1_000).await else {
        panic!("expected Opened");
    };
    let lease_until = 1_000 + LEASE_DURATION_MS;

    let candidate_id = uuid(73);
    let ops = vec![GeneratedOp::ProposeCandidate {
        candidate_id: candidate_id.clone(),
        operation: ProposedOperation::Create {
            memory_id: uuid(74),
            kind: "fact".to_string(),
            text: "some proposed durable text".to_string(),
            canonical_key: None,
            scope_kind: "worktree".to_string(),
            scope_owner_id: uuid(75),
            confidence: 0.5,
            importance: 0.5,
            valid_from_tree: None,
            last_verified_tree: None,
        },
        conflicts: vec![],
        evidence_observation_ids: vec![ids[0].clone(), ids[0].clone()],
    }];
    let outcome = run_once(
        &db,
        window,
        lease_until,
        LEASE_DURATION_MS,
        LEASE_RENEW_INTERVAL_MS,
        1_000,
        "build-test",
        move |_w| async move { Ok::<_, ClassifiedFailure>(ops) },
    )
    .await
    .expect("run_once");
    assert!(matches!(outcome, RunOutcome::Applied(_)), "{outcome:?}");

    let read = db.open_read().expect("read conn");
    assert_eq!(
        candidate_evidence_for(&read, &candidate_id).expect("evidence"),
        vec![ids[0].clone()],
        "one candidate_evidence row, not a rolled-back window"
    );
    assert_eq!(
        consolidation_run_state(&read, &run_id).expect("state"),
        Some(RunState::Applied)
    );
}

/// Per-op deduplication deliberately does not cover a duplicate *across* two
/// ops (here: two reinforces of the same entry citing the same observation),
/// so the constraint violation is still reachable — and that is the point:
/// a violation reproduces byte-for-byte on the same build, so it must be
/// classified `Mechanical` and dead-lettered after one attempt, never retried
/// on a 4s cap forever.
#[tokio::test]
async fn a_constraint_violation_dead_letters_instead_of_retrying_forever() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let ids = seed_envelopes(&db, "sess-1", 80, 2).await;
    let run_id = uuid(82);
    let SnapshotOutcome::Opened(window) = open_run(&db, &run_id, "sess-1", 10, 1_000).await else {
        panic!("expected Opened");
    };
    let lease_until = 1_000 + LEASE_DURATION_MS;

    let owner = uuid(83);
    let memory_id = uuid(84);
    create_memory(&db, &memory_id, &owner).await;

    let ops = vec![
        GeneratedOp::Materialize {
            operation: ProposedOperation::Reinforce {
                memory_id: memory_id.clone(),
                expected_version: 1,
                confidence: Some(0.9),
            },
            evidence_observation_ids: vec![ids[0].clone()],
        },
        GeneratedOp::Materialize {
            operation: ProposedOperation::Reinforce {
                memory_id: memory_id.clone(),
                expected_version: 2,
                confidence: Some(0.95),
            },
            // Same (memory_id, observation_id) pair as op 0 -> PRIMARY KEY.
            evidence_observation_ids: vec![ids[0].clone()],
        },
    ];
    let outcome = run_once(
        &db,
        window,
        lease_until,
        LEASE_DURATION_MS,
        LEASE_RENEW_INTERVAL_MS,
        1_000,
        "build-test",
        move |_w| async move { Ok::<_, ClassifiedFailure>(ops) },
    )
    .await
    .expect("run_once");
    let RunOutcome::Failed(reason) = outcome else {
        panic!("expected Failed, got {outcome:?}");
    };
    assert!(
        reason.contains("UNIQUE constraint failed"),
        "the whole window rolled back on the constraint: {reason}"
    );

    let read = db.open_read().expect("read conn");
    let (kind, fingerprint, next_retry_at, attempts) = failure_row(&read, &run_id);
    assert_eq!(
        kind.as_deref(),
        Some("mechanical"),
        "a constraint violation reproduces identically on the same build"
    );
    assert_eq!(fingerprint.as_deref(), Some("build-test"));
    assert_eq!(
        next_retry_at, None,
        "mechanical failures are fingerprint-gated, never time-gated"
    );
    assert_eq!(attempts, 1);
    assert_eq!(
        entry_version(&read, &memory_id),
        1,
        "nothing committed -- the whole batch rolled back"
    );

    assert!(
        stale_runs(&read, 100_000, "build-test")
            .expect("stale runs")
            .is_empty(),
        "dead-lettered on this build: no second attempt, no matter how long"
    );
    let after_rebuild = stale_runs(&read, 100_000, "build-next").expect("stale runs");
    assert_eq!(
        after_rebuild.len(),
        1,
        "a rebuild (the fix) grants exactly one more attempt"
    );
    assert_eq!(after_rebuild[0].run_id, run_id);
}
