//! T16-02 acceptance tests for `purge --memory <id>|--session <id>|--all`
//! (spec 08 §3, 12 §3): the only hard-delete path, its audit tombstones,
//! authorization-shaped (`expected_version`) refusal, referential integrity
//! after `--all`, crash rollback, and the contrast with `retract` (row
//! survives) that motivates purge existing at all.
//!
//! All tests are deterministic: an isolated [`TempHome`], fixed `now_ms`
//! literals, and ids minted from [`uuidv7_from`] with fixed entropy.
//!
//! Failpoint tests share [`SERIAL`]: the failpoint registry
//! (`local_rag_test_support::failpoint::global()`) is process-global to this
//! test binary, so an armed-but-not-yet-disarmed failpoint in one test could
//! otherwise fire in a concurrently running test in this same file (the same
//! hazard `crates/store/tests/memory_op.rs`'s own `SERIAL` guards against).

use local_rag_core::identity::uuidv7_from;
use local_rag_core::paths::StoreLayout;
use local_rag_store::StateDb;
use local_rag_store::memory::{
    Actor, CreateMemoryOp, MemoryKind, MemoryOpOutcome, MergeLoser, MergeMemoryOp, NewCandidate,
    NormalizationStatus, NormalizationWrite, RetractMemoryOp, ScopeKind, SupersedeMemoryOp,
    UpsertOutcome, apply_create, apply_merge, apply_retract, apply_supersede,
    candidate_evidence_for, create_candidate, insert_candidate_evidence, memory_entry_state,
    normalization_for, read_audit_events_for_entity, upsert_normalization,
};
use local_rag_store::observation::{NewObservationEnvelope, insert_envelope};
use local_rag_store::privacy::{
    PurgeMemoryError, inspect_memory, preview_purge_all, preview_purge_memory,
    preview_purge_session, purge_all, purge_memory, purge_session,
};
use local_rag_store::rusqlite::{Connection, params};
use local_rag_test_support::TempHome;
use tokio::sync::Mutex;

#[cfg(feature = "failpoints")]
use local_rag_test_support::Action;

static SERIAL: Mutex<()> = Mutex::const_new(());

/// The text [`create`] writes; a normalization fixture must hash exactly this,
/// since `upsert_normalization` refuses a write whose source hash has moved.
const CREATED_TEXT: &str = "some durable text";

fn open_state() -> (TempHome, StateDb) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    (home, db)
}

fn uuid(seed: u8) -> String {
    let mut rand = [0u8; 10];
    rand[9] = seed;
    uuidv7_from(1000, rand).to_string()
}

fn row_count(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .expect("count")
}

fn foreign_key_violations(conn: &Connection) -> Vec<String> {
    let mut stmt = conn.prepare("PRAGMA foreign_key_check").expect("prepare");
    stmt.query_map([], |r| r.get::<_, String>(0))
        .expect("query")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("collect")
}

async fn seed_observation(db: &StateDb, seed: u8, session_id: &'static str) -> String {
    let observation_id = uuid(seed);
    let oid = observation_id.clone();
    db.writer()
        .transaction(move |tx| {
            insert_envelope(
                tx,
                &NewObservationEnvelope {
                    observation_id: &oid,
                    source_event_id: "evt-1",
                    dedup_key: None,
                    payload_hash: "deadbeef",
                    event_type: "Stop",
                    evidence_kind: "user_statement",
                    trust: "normal",
                    source_timestamp: Some(1000),
                    repo_id: None,
                    worktree_id: None,
                    session_id,
                    agent_id: None,
                    turn_id: None,
                    batch_id: None,
                    commit_hash: None,
                    short_evidence_excerpt: None,
                    redaction_version: Some(1),
                },
            )
            .map(|_| ())
        })
        .await
        .expect("seed observation");
    observation_id
}

/// `apply_create` with no evidence, returning the created `entry_version`
/// (always 1).
async fn create(db: &StateDb, memory_id: &str, scope_owner_id: &str) -> i64 {
    let (id, owner) = (memory_id.to_string(), scope_owner_id.to_string());
    let outcome = db
        .writer()
        .transaction(move |tx| {
            apply_create(
                tx,
                &CreateMemoryOp {
                    memory_id: &id,
                    kind: MemoryKind::Fact,
                    text: CREATED_TEXT,
                    canonical_key: None,
                    scope_kind: ScopeKind::Worktree,
                    scope_owner_id: &owner,
                    confidence: 0.5,
                    importance: 0.5,
                    valid_from_tree: None,
                    last_verified_tree: None,
                    evidence: &[],
                    actor: Actor::User,
                    idempotency_key: None,
                },
                1000,
            )
        })
        .await
        .expect("infra")
        .expect("create applies");
    let MemoryOpOutcome::Applied(applied) = outcome else {
        panic!("expected Applied");
    };
    applied.entry_version
}

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

// ---------------------------------------------------------------------------
// purge_memory
// ---------------------------------------------------------------------------

#[tokio::test]
async fn purge_memory_deletes_the_row_evidence_and_relinks_descendants() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let owner = uuid(1);
    let old_id = uuid(2);
    let new_id = uuid(3);
    create(&db, &old_id, &owner).await;

    // `new_id` supersedes `old_id`: the successor's own `supersedes_id`
    // points back at the row it replaced (spec 04 §5).
    let (old, new, own) = (old_id.clone(), new_id.clone(), owner.clone());
    db.writer()
        .transaction(move |tx| {
            apply_supersede(
                tx,
                &SupersedeMemoryOp {
                    old_memory_id: &old,
                    old_expected_version: 1,
                    new_memory_id: &new,
                    new_kind: MemoryKind::Fact,
                    new_text: "superseding text",
                    new_canonical_key: None,
                    new_scope_kind: ScopeKind::Worktree,
                    new_scope_owner_id: &own,
                    new_confidence: 0.5,
                    new_importance: 0.5,
                    new_valid_from_tree: None,
                    new_last_verified_tree: None,
                    evidence: &[],
                    actor: Actor::User,
                    idempotency_key: None,
                },
                1000,
            )
        })
        .await
        .expect("infra")
        .expect("supersede applies");

    let read = db.open_read().expect("read conn");
    assert_eq!(
        inspect_memory(&read, &new_id, 1000)
            .unwrap()
            .unwrap()
            .entry
            .supersedes_id
            .as_deref(),
        Some(old_id.as_str()),
        "sanity: the successor really does point at the predecessor before purge"
    );
    let old_version = inspect_memory(&read, &old_id, 1000)
        .unwrap()
        .unwrap()
        .entry
        .entry_version;
    assert_eq!(
        old_version, 2,
        "the transition to superseded bumped its version too"
    );
    drop(read);

    // Purge the now-historical predecessor.
    let id_arg = old_id.clone();
    let report = db
        .writer()
        .transaction(move |tx| purge_memory(tx, &id_arg, old_version, 1000))
        .await
        .expect("infra")
        .expect("purge applies");
    assert_eq!(report.descendants_relinked, 1);

    let read = db.open_read().expect("read conn");
    assert_eq!(
        inspect_memory(&read, &old_id, 1000).unwrap(),
        None,
        "the purged row is gone"
    );
    assert_eq!(
        inspect_memory(&read, &new_id, 1000)
            .unwrap()
            .unwrap()
            .entry
            .supersedes_id,
        None,
        "the successor's dangling supersedes_id was relinked to NULL, not left dangling"
    );
}

#[tokio::test]
async fn purge_memory_unknown_id_is_typed_error_with_no_mutation() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let before = preview_purge_memory(&db.open_read().unwrap(), "unknown").unwrap();
    assert!(!before.exists);

    let result = db
        .writer()
        .transaction(move |tx| purge_memory(tx, "unknown", 1, 1000))
        .await
        .expect("infra");
    assert_eq!(result, Err(PurgeMemoryError::UnknownMemory));
}

#[tokio::test]
async fn purge_memory_stale_expected_version_surfaces_both_numbers_with_no_mutation() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let owner = uuid(4);
    let id = uuid(5);
    create(&db, &id, &owner).await;

    let id_arg = id.clone();
    let result = db
        .writer()
        .transaction(move |tx| purge_memory(tx, &id_arg, 99, 1000))
        .await
        .expect("infra");
    assert_eq!(
        result,
        Err(PurgeMemoryError::OptimisticConflict {
            expected: 99,
            actual: 1,
        }),
        "both numbers surfaced, the same contract memory edit/retract already give"
    );

    let read = db.open_read().expect("read conn");
    assert!(
        inspect_memory(&read, &id, 1000).unwrap().is_some(),
        "no mutation happened"
    );
}

#[tokio::test]
async fn purge_memory_tombstones_prior_audit_payload_and_appends_a_purge_marker() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let owner = uuid(6);
    let survivor = uuid(7);
    let loser = uuid(8);
    create(&db, &survivor, &owner).await;
    create(&db, &loser, &owner).await;

    let (surv, lose) = (survivor.clone(), loser.clone());
    db.writer()
        .transaction(move |tx| {
            apply_merge(
                tx,
                &MergeMemoryOp {
                    survivor_id: &surv,
                    survivor_expected_version: 1,
                    losers: &[MergeLoser {
                        memory_id: &lose,
                        expected_version: 1,
                    }],
                    actor: Actor::User,
                    idempotency_key: None,
                },
                1000,
            )
        })
        .await
        .expect("infra")
        .expect("merge applies");

    let read = db.open_read().expect("read conn");
    let before_audit = read_audit_events_for_entity(&read, "memory_entry", &survivor).unwrap();
    assert_eq!(before_audit.len(), 2, "create + merge");
    assert!(
        before_audit[1].payload.is_some(),
        "sanity: the merge audit row really does carry a payload before purge"
    );
    let survivor_version = inspect_memory(&read, &survivor, 1000)
        .unwrap()
        .unwrap()
        .entry
        .entry_version;
    assert_eq!(survivor_version, 2);
    drop(read);

    let (id_arg, version_arg) = (survivor.clone(), survivor_version);
    let report = db
        .writer()
        .transaction(move |tx| purge_memory(tx, &id_arg, version_arg, 1000))
        .await
        .expect("infra")
        .expect("purge applies");
    assert_eq!(report.audit_rows_tombstoned, 2);

    let read = db.open_read().expect("read conn");
    let after_audit = read_audit_events_for_entity(&read, "memory_entry", &survivor).unwrap();
    assert_eq!(
        after_audit.len(),
        3,
        "create + merge + the new purge marker"
    );
    assert_eq!(after_audit[0].op, "create");
    assert_eq!(after_audit[0].payload, None);
    assert_eq!(after_audit[1].op, "merge");
    assert_eq!(
        after_audit[1].payload, None,
        "the prior merge payload (a JSON array of loser ids) is tombstoned to NULL"
    );
    assert_eq!(after_audit[2].op, "purge");
    assert_eq!(after_audit[2].entity_version, 3);
    assert_eq!(after_audit[2].payload, None);
    assert_eq!(after_audit[2].actor, Actor::User);
}

// ---------------------------------------------------------------------------
// purge_session
// ---------------------------------------------------------------------------

#[tokio::test]
async fn purge_session_removes_envelopes_paths_payloads_and_both_evidence_kinds() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let owner = uuid(10);
    let obs = seed_observation(&db, 11, "sess-target").await;
    let other_session_obs = seed_observation(&db, 12, "sess-other").await;
    let memory_id = uuid(13);
    create(&db, &memory_id, &owner).await;
    let candidate_id = uuid(14);

    let (oid, mid, cid) = (obs.clone(), memory_id.clone(), candidate_id.clone());
    db.writer()
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO observation_path (observation_id, normalized_path) VALUES (?1, 'src/a.rs')",
                params![oid],
            )?;
            tx.execute(
                "INSERT INTO observation_payload \
                   (observation_id, redacted_payload, byte_size, expires_at) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![oid, b"hi".to_vec(), 2_i64, 5000_i64],
            )?;
            tx.execute(
                "INSERT INTO memory_evidence \
                   (memory_id, observation_id, evidence_kind, session_id) \
                 VALUES (?1, ?2, 'user_statement', 'sess-target')",
                params![mid, oid],
            )?;
            create_candidate(
                tx,
                &NewCandidate {
                    candidate_id: &cid,
                    proposed_operation: "{}",
                    conflicts: None,
                },
                1000,
            )?;
            insert_candidate_evidence(tx, &cid, &oid)?;
            Ok(())
        })
        .await
        .expect("seed session fixture");

    let read = db.open_read().expect("read conn");
    let preview = preview_purge_session(&read, "sess-target").unwrap();
    assert_eq!(preview.observations, 1);
    drop(read);

    let report = db
        .writer()
        .transaction(|tx| purge_session(tx, "sess-target"))
        .await
        .expect("purge session");
    assert_eq!(report.observations_purged, 1);
    assert_eq!(report.candidate_evidence_rows_removed, 1);
    assert_eq!(report.memory_evidence_rows_removed, 1);

    let count = |sql: &str, id: &str| -> i64 {
        db.open_read()
            .expect("read conn")
            .query_row(sql, params![id], |r| r.get(0))
            .expect("count")
    };
    assert_eq!(
        count(
            "SELECT count(*) FROM observation_envelope WHERE observation_id = ?1",
            &obs,
        ),
        0,
        "the target session's envelope is gone"
    );
    assert_eq!(
        count(
            "SELECT count(*) FROM observation_path WHERE observation_id = ?1",
            &obs,
        ),
        0,
        "the cascaded path row is gone too"
    );
    assert_eq!(
        count(
            "SELECT count(*) FROM observation_payload WHERE observation_id = ?1",
            &obs,
        ),
        0,
        "the cascaded payload row is gone too"
    );
    assert_eq!(
        count(
            "SELECT count(*) FROM candidate_evidence WHERE candidate_id = ?1",
            &candidate_id,
        ),
        0,
        "the pending candidate's evidence link (an observation-side FK) is gone"
    );
    assert_eq!(
        count(
            "SELECT count(*) FROM observation_envelope WHERE observation_id = ?1",
            &other_session_obs,
        ),
        1,
        "a different session's envelope is untouched"
    );
    let read = db.open_read().expect("read conn");
    assert_eq!(row_count(&read, "memory_evidence"), 0);
    assert_eq!(
        row_count(&read, "pending_memory_candidate"),
        1,
        "the candidate row itself survives -- only its evidence link was removed"
    );
}

#[tokio::test]
async fn purge_session_of_unknown_session_is_a_harmless_no_op_report() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let report = db
        .writer()
        .transaction(|tx| purge_session(tx, "no-such-session"))
        .await
        .expect("purge session");
    assert_eq!(report.observations_purged, 0);
    assert_eq!(report.candidate_evidence_rows_removed, 0);
    assert_eq!(report.memory_evidence_rows_removed, 0);
}

// ---------------------------------------------------------------------------
// purge_all
// ---------------------------------------------------------------------------

#[tokio::test]
async fn purge_all_leaves_no_orphaned_foreign_keys() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let owner = uuid(20);

    // Supersede chain.
    let old_id = uuid(21);
    let new_id = uuid(22);
    create(&db, &old_id, &owner).await;
    let (old, new, own) = (old_id.clone(), new_id.clone(), owner.clone());
    db.writer()
        .transaction(move |tx| {
            apply_supersede(
                tx,
                &SupersedeMemoryOp {
                    old_memory_id: &old,
                    old_expected_version: 1,
                    new_memory_id: &new,
                    new_kind: MemoryKind::Fact,
                    new_text: "new text",
                    new_canonical_key: None,
                    new_scope_kind: ScopeKind::Worktree,
                    new_scope_owner_id: &own,
                    new_confidence: 0.5,
                    new_importance: 0.5,
                    new_valid_from_tree: None,
                    new_last_verified_tree: None,
                    evidence: &[],
                    actor: Actor::User,
                    idempotency_key: None,
                },
                1000,
            )
        })
        .await
        .expect("infra")
        .expect("supersede applies");

    // Merge survivor + loser.
    let survivor = uuid(23);
    let loser = uuid(24);
    create(&db, &survivor, &owner).await;
    create(&db, &loser, &owner).await;
    let (surv, lose) = (survivor.clone(), loser.clone());
    db.writer()
        .transaction(move |tx| {
            apply_merge(
                tx,
                &MergeMemoryOp {
                    survivor_id: &surv,
                    survivor_expected_version: 1,
                    losers: &[MergeLoser {
                        memory_id: &lose,
                        expected_version: 1,
                    }],
                    actor: Actor::User,
                    idempotency_key: None,
                },
                1000,
            )
        })
        .await
        .expect("infra")
        .expect("merge applies");

    // Two sessions, each with evidence linked to the merged entries, plus a
    // pending candidate.
    let obs_a = seed_observation(&db, 25, "sess-a").await;
    let obs_b = seed_observation(&db, 26, "sess-b").await;
    let candidate_id = uuid(27);
    let (oa, ob, mid, cid) = (
        obs_a.clone(),
        obs_b.clone(),
        survivor.clone(),
        candidate_id.clone(),
    );
    db.writer()
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO memory_evidence \
                   (memory_id, observation_id, evidence_kind, session_id) \
                 VALUES (?1, ?2, 'user_statement', 'sess-a')",
                params![mid, oa],
            )?;
            create_candidate(
                tx,
                &NewCandidate {
                    candidate_id: &cid,
                    proposed_operation: "{}",
                    conflicts: None,
                },
                1000,
            )?;
            insert_candidate_evidence(tx, &cid, &ob)?;
            Ok(())
        })
        .await
        .expect("seed evidence links");

    let read = db.open_read().expect("read conn");
    let preview = preview_purge_all(&read).unwrap();
    assert_eq!(preview.memory_entries, 4, "old, new, survivor, loser");
    assert_eq!(preview.sessions, 2);
    drop(read);

    db.writer()
        .transaction(move |tx| purge_all(tx, 2000))
        .await
        .expect("purge all");

    let read = db.open_read().expect("read conn");
    assert_eq!(
        foreign_key_violations(&read),
        Vec::<String>::new(),
        "no dangling FK reference survives purge_all"
    );
    assert_eq!(row_count(&read, "memory_entry"), 0);
    assert_eq!(row_count(&read, "observation_envelope"), 0);
    assert_eq!(row_count(&read, "memory_evidence"), 0);
    assert_eq!(row_count(&read, "candidate_evidence"), 0);
    assert_eq!(
        row_count(&read, "pending_memory_candidate"),
        1,
        "the candidate row itself survives (only its evidence link was FK-blocking)"
    );
    assert_eq!(
        candidate_evidence_for(&read, &candidate_id).unwrap(),
        Vec::<String>::new(),
        "the surviving candidate is now evidence-less, a pre-existing legal state"
    );
}

#[tokio::test]
async fn purge_all_purges_every_memory_entry_and_every_session() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let owner = uuid(30);
    create(&db, &uuid(31), &owner).await;
    create(&db, &uuid(32), &owner).await;
    seed_observation(&db, 33, "sess-x").await;
    seed_observation(&db, 34, "sess-y").await;

    let report = db
        .writer()
        .transaction(move |tx| purge_all(tx, 1000))
        .await
        .expect("purge all");
    assert_eq!(report.memory_entries_purged, 2);
    assert_eq!(report.sessions_purged, 2);
    assert_eq!(report.observations_purged, 2);

    let read = db.open_read().expect("read conn");
    assert_eq!(row_count(&read, "memory_entry"), 0);
    assert_eq!(row_count(&read, "observation_envelope"), 0);
}

#[tokio::test]
async fn purge_all_on_an_already_empty_store_is_idempotent() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let report = db
        .writer()
        .transaction(move |tx| purge_all(tx, 1000))
        .await
        .expect("purge all on an empty store");
    assert_eq!(report.memory_entries_purged, 0);
    assert_eq!(report.sessions_purged, 0);
    assert_eq!(report.observations_purged, 0);
}

// ---------------------------------------------------------------------------
// retract vs purge — the card's own "retract remains non-delete" contrast
// ---------------------------------------------------------------------------

#[tokio::test]
async fn retract_leaves_the_row_inspectable_purge_does_not() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let owner = uuid(40);
    let retracted_id = uuid(41);
    let purged_id = uuid(42);
    create(&db, &retracted_id, &owner).await;
    create(&db, &purged_id, &owner).await;

    let id_arg = retracted_id.clone();
    db.writer()
        .transaction(move |tx| {
            apply_retract(
                tx,
                &RetractMemoryOp {
                    memory_id: &id_arg,
                    expected_version: 1,
                    evidence: &[],
                    actor: Actor::User,
                    idempotency_key: None,
                },
                1000,
            )
        })
        .await
        .expect("infra")
        .expect("retract applies");

    let id_arg = purged_id.clone();
    db.writer()
        .transaction(move |tx| purge_memory(tx, &id_arg, 1, 1000))
        .await
        .expect("infra")
        .expect("purge applies");

    let read = db.open_read().expect("read conn");
    let retracted = inspect_memory(&read, &retracted_id, 1000)
        .unwrap()
        .expect("retract ≠ delete: the row still exists");
    assert_eq!(
        memory_entry_state(&read, &retracted_id).unwrap(),
        Some((MemoryKind::Fact, retracted.entry.state)),
    );
    assert_eq!(
        inspect_memory(&read, &purged_id, 1000).unwrap(),
        None,
        "purge is the only hard-delete path: the row is actually gone"
    );
}

// ---------------------------------------------------------------------------
// Crash rollback under failpoint
// ---------------------------------------------------------------------------

#[cfg(feature = "failpoints")]
#[tokio::test]
async fn purge_memory_rolls_back_completely_on_failpoint() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let owner = uuid(50);
    let old_id = uuid(51);
    let new_id = uuid(52);
    create(&db, &old_id, &owner).await;
    let (old, new, own) = (old_id.clone(), new_id.clone(), owner.clone());
    db.writer()
        .transaction(move |tx| {
            apply_supersede(
                tx,
                &SupersedeMemoryOp {
                    old_memory_id: &old,
                    old_expected_version: 1,
                    new_memory_id: &new,
                    new_kind: MemoryKind::Fact,
                    new_text: "new text",
                    new_canonical_key: None,
                    new_scope_kind: ScopeKind::Worktree,
                    new_scope_owner_id: &own,
                    new_confidence: 0.5,
                    new_importance: 0.5,
                    new_valid_from_tree: None,
                    new_last_verified_tree: None,
                    evidence: &[],
                    actor: Actor::User,
                    idempotency_key: None,
                },
                1000,
            )
        })
        .await
        .expect("infra")
        .expect("supersede applies");

    let old_version = inspect_memory(&db.open_read().unwrap(), &old_id, 1000)
        .unwrap()
        .unwrap()
        .entry
        .entry_version;
    assert_eq!(
        old_version, 2,
        "the transition to superseded bumped its version too"
    );

    arm("privacy.purge.memory.before_final_audit");
    let id_arg = old_id.clone();
    let result = db
        .writer()
        .transaction(move |tx| purge_memory(tx, &id_arg, old_version, 1000))
        .await;
    assert!(
        matches!(result, Err(local_rag_store::WriteError::Sqlite(_))),
        "the failpoint must fail the call: {result:?}"
    );
    disarm("privacy.purge.memory.before_final_audit");

    let read = db.open_read().expect("read conn");
    assert!(
        inspect_memory(&read, &old_id, 1000).unwrap().is_some(),
        "the purge target survives a failure before its final audit insert"
    );
    assert_eq!(
        inspect_memory(&read, &new_id, 1000)
            .unwrap()
            .unwrap()
            .entry
            .supersedes_id
            .as_deref(),
        Some(old_id.as_str()),
        "the relink never happened either -- the whole transaction rolled back"
    );
    let audit = read_audit_events_for_entity(&read, "memory_entry", &old_id).unwrap();
    assert_eq!(
        audit.len(),
        2,
        "create + the supersede transition -- no purge marker row was left behind"
    );
    drop(read);

    // Retrying with the failpoint disarmed converges cleanly.
    let id_arg = old_id.clone();
    db.writer()
        .transaction(move |tx| purge_memory(tx, &id_arg, old_version, 1000))
        .await
        .expect("infra")
        .expect("retry applies");
    let read = db.open_read().expect("read conn");
    assert_eq!(inspect_memory(&read, &old_id, 1000).unwrap(), None);
}

#[cfg(feature = "failpoints")]
#[tokio::test]
async fn purge_session_rolls_back_completely_on_failpoint() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let obs = seed_observation(&db, 60, "sess-crash").await;
    let memory_id = uuid(61);
    let owner = uuid(62);
    create(&db, &memory_id, &owner).await;
    let candidate_id = uuid(63);
    let (oid, mid, cid) = (obs.clone(), memory_id.clone(), candidate_id.clone());
    db.writer()
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO memory_evidence \
                   (memory_id, observation_id, evidence_kind, session_id) \
                 VALUES (?1, ?2, 'user_statement', 'sess-crash')",
                params![mid, oid],
            )?;
            create_candidate(
                tx,
                &NewCandidate {
                    candidate_id: &cid,
                    proposed_operation: "{}",
                    conflicts: None,
                },
                1000,
            )?;
            insert_candidate_evidence(tx, &cid, &oid)?;
            Ok(())
        })
        .await
        .expect("seed session fixture");

    arm("privacy.purge.session.before_envelope_delete");
    let result = db
        .writer()
        .transaction(|tx| purge_session(tx, "sess-crash"))
        .await;
    assert!(
        matches!(result, Err(local_rag_store::WriteError::Sqlite(_))),
        "the failpoint must fail the call: {result:?}"
    );
    disarm("privacy.purge.session.before_envelope_delete");

    let read = db.open_read().expect("read conn");
    assert_eq!(
        preview_purge_session(&read, "sess-crash")
            .unwrap()
            .observations,
        1,
        "the envelope survives a failure before its own delete"
    );
    assert_eq!(
        candidate_evidence_for(&read, &candidate_id).unwrap(),
        vec![obs.clone()],
        "the candidate_evidence delete rolled back too -- the whole transaction is one unit"
    );
    drop(read);

    db.writer()
        .transaction(|tx| purge_session(tx, "sess-crash"))
        .await
        .expect("retry converges");
    let read = db.open_read().expect("read conn");
    assert_eq!(
        preview_purge_session(&read, "sess-crash")
            .unwrap()
            .observations,
        0
    );
}

#[cfg(feature = "failpoints")]
#[tokio::test]
async fn purge_all_rolls_back_every_purged_entity_together_on_a_single_failpoint() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let owner = uuid(70);
    let first = uuid(71);
    let second = uuid(72);
    create(&db, &first, &owner).await;
    create(&db, &second, &owner).await;
    seed_observation(&db, 73, "sess-both").await;

    arm("privacy.purge.memory.before_final_audit");
    let result = db.writer().transaction(move |tx| purge_all(tx, 1000)).await;
    assert!(
        matches!(result, Err(local_rag_store::WriteError::Sqlite(_))),
        "the failpoint must fail the call: {result:?}"
    );
    disarm("privacy.purge.memory.before_final_audit");

    let read = db.open_read().expect("read conn");
    assert_eq!(
        row_count(&read, "memory_entry"),
        2,
        "neither memory entry was purged -- one transaction, all or nothing"
    );
    assert_eq!(
        row_count(&read, "observation_envelope"),
        1,
        "the session that hadn't even been reached yet is untouched too"
    );
    drop(read);

    db.writer()
        .transaction(move |tx| purge_all(tx, 1000))
        .await
        .expect("retry converges");
    let read = db.open_read().expect("read conn");
    assert_eq!(row_count(&read, "memory_entry"), 0);
    assert_eq!(row_count(&read, "observation_envelope"), 0);
}

// ---------------------------------------------------------------------------
// T21-07: the entry's English variant dies with it
// ---------------------------------------------------------------------------

/// Give `memory_id` a `ready` normalization row derived from the text `create`
/// wrote, so it is the row a purge/edit is actually supposed to remove (an
/// upsert whose `source_text_sha256` did not match would be refused outright).
async fn seed_normalization(db: &StateDb, memory_id: &str, normalized: &'static str) {
    let id = memory_id.to_string();
    let sha = local_rag_core::hash::sha256_hex(CREATED_TEXT.as_bytes());
    let outcome = db
        .writer()
        .transaction(move |tx| {
            upsert_normalization(
                tx,
                &NormalizationWrite {
                    memory_id: &id,
                    status: NormalizationStatus::Ready,
                    source_text_sha256: &sha,
                    normalized_text: Some(normalized),
                    source_language: Some("ru"),
                    normalizer_model_id: Some("test-normalizer"),
                    prompt_version: Some(1),
                    normalizer_version: 1,
                    attempt_count: 1,
                    last_error: None,
                    next_attempt_at: None,
                },
                1000,
            )
        })
        .await
        .expect("infra");
    assert_eq!(
        outcome,
        UpsertOutcome::Written,
        "fixture must actually land"
    );
}

#[tokio::test]
async fn purge_memory_removes_the_normalization_row_and_counts_it() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let owner = uuid(1);
    let id = uuid(2);
    create(&db, &id, &owner).await;
    seed_normalization(&db, &id, "the English variant").await;

    let read = db.open_read().expect("read conn");
    assert!(
        normalization_for(&read, &id).unwrap().is_some(),
        "sanity: the translation is there before the purge"
    );
    drop(read);

    let id_arg = id.clone();
    let report = db
        .writer()
        .transaction(move |tx| purge_memory(tx, &id_arg, 1, 1000))
        .await
        .expect("infra")
        .expect("purge applies");

    assert_eq!(report.normalization_rows_removed, 1);
    let read = db.open_read().expect("read conn");
    assert!(
        normalization_for(&read, &id).unwrap().is_none(),
        "the English variant does not outlive the text it came from",
    );
    assert_eq!(row_count(&read, "memory_text_normalization"), 0);
}

#[tokio::test]
async fn an_entry_with_no_translation_reports_zero() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let owner = uuid(1);
    let id = uuid(2);
    create(&db, &id, &owner).await;

    let id_arg = id.clone();
    let report = db
        .writer()
        .transaction(move |tx| purge_memory(tx, &id_arg, 1, 1000))
        .await
        .expect("infra")
        .expect("purge applies");

    assert_eq!(
        report.normalization_rows_removed, 0,
        "the counter reports what was removed, not what could have been",
    );
}

#[tokio::test]
async fn purge_all_leaves_no_normalization_rows() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let owner = uuid(1);
    let first = uuid(2);
    let second = uuid(3);
    create(&db, &first, &owner).await;
    create(&db, &second, &owner).await;
    seed_normalization(&db, &first, "first English variant").await;
    seed_normalization(&db, &second, "second English variant").await;

    let read = db.open_read().expect("read conn");
    assert_eq!(row_count(&read, "memory_text_normalization"), 2);
    drop(read);

    let report = db
        .writer()
        .transaction(move |tx| purge_all(tx, 1000))
        .await
        .expect("infra");
    assert_eq!(report.memory_entries_purged, 2);

    let read = db.open_read().expect("read conn");
    assert_eq!(
        row_count(&read, "memory_text_normalization"),
        0,
        "an all-or-nothing privacy purge leaves no derived text behind",
    );
    assert!(foreign_key_violations(&read).is_empty());
}

/// The explicit `DELETE` is what the report counts; the schema's
/// `ON DELETE CASCADE` is the safety net for any delete path that never comes
/// through `purge_memory_rows`. This test exercises that net directly.
#[tokio::test]
async fn deleting_the_entry_outside_the_purge_path_still_cascades() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let owner = uuid(1);
    let id = uuid(2);
    create(&db, &id, &owner).await;
    seed_normalization(&db, &id, "the English variant").await;

    let id_arg = id.clone();
    db.writer()
        .transaction(move |tx| {
            tx.execute(
                "DELETE FROM memory_entry WHERE memory_id = ?1",
                params![id_arg],
            )
            .map(|_| ())
        })
        .await
        .expect("raw delete");

    let read = db.open_read().expect("read conn");
    assert_eq!(
        row_count(&read, "memory_text_normalization"),
        0,
        "the FK cascade removes it even with no explicit DELETE",
    );
    assert!(foreign_key_violations(&read).is_empty());
}

#[tokio::test]
async fn purging_the_same_entry_twice_is_idempotent() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let owner = uuid(1);
    let id = uuid(2);
    create(&db, &id, &owner).await;
    seed_normalization(&db, &id, "the English variant").await;

    let id_arg = id.clone();
    let first = db
        .writer()
        .transaction(move |tx| purge_memory(tx, &id_arg, 1, 1000))
        .await
        .expect("infra")
        .expect("purge applies");
    assert_eq!(first.normalization_rows_removed, 1);

    let id_arg = id.clone();
    let second = db
        .writer()
        .transaction(move |tx| purge_memory(tx, &id_arg, 1, 1000))
        .await
        .expect("infra");
    assert_eq!(
        second,
        Err(PurgeMemoryError::UnknownMemory),
        "a second purge finds nothing left to purge",
    );

    let read = db.open_read().expect("read conn");
    assert_eq!(row_count(&read, "memory_text_normalization"), 0);
}
