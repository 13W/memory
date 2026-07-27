//! T14-02 acceptance tests for the transactional memory-op engine (spec 08
//! §3): the three operation contracts (`create`/`reinforce`/`noop`),
//! optimistic conflict, idempotency-key replay ("same key returns the
//! original result"), rollback-under-failpoint, and audit-version
//! contiguity.
//!
//! All tests are deterministic: an isolated [`TempHome`], fixed `now_ms`
//! literals, and ids minted from [`uuidv7_from`] with fixed entropy.
//!
//! Failpoint tests share [`SERIAL`] with every other test that calls
//! `apply_create`/`apply_reinforce`: the failpoint registry
//! (`local_rag_test_support::failpoint::global()`) is process-global, so an
//! armed-but-not-yet-disarmed failpoint in one test could otherwise fire in a
//! concurrently running test that hits the same injection site (the same
//! class of hazard `crates/store/tests/fts_materialize.rs`'s `SERIAL` guards
//! against).

use local_rag_core::identity::uuidv7_from;
use local_rag_core::paths::StoreLayout;
use local_rag_store::memory::{
    Actor, CreateMemoryOp, EvidenceInput, MemoryOpError, MemoryOpOutcome, ReinforceMemoryOp,
    ScopeKind, apply_create, apply_noop, apply_reinforce, memory_entry_state, memory_evidence_for,
    read_audit_events_for_entity,
};
use local_rag_store::rusqlite::{Connection, params};
use local_rag_store::{MemoryKind, StateDb};
use local_rag_test_support::TempHome;
use tokio::sync::Mutex;

#[cfg(feature = "failpoints")]
use local_rag_test_support::Action;

static SERIAL: Mutex<()> = Mutex::const_new(());

/// A temporary store with an ensured tree and an opened [`StateDb`].
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

/// Insert a minimal, standalone `observation_envelope` row (mirrors
/// `tests/memory.rs`'s helper) so evidence-input tests have a real
/// `observation_id` to point at.
async fn seed_observation(db: &StateDb, seed: u8) -> String {
    let observation_id = uuid(seed);
    let oid = observation_id.clone();
    db.writer()
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO observation_envelope \
                   (observation_id, source_event_id, payload_hash, event_type, evidence_kind, \
                    trust, session_id) \
                 VALUES (?1, 'evt-1', 'deadbeef', 'Stop', 'user_statement', 'normal', 'sess-1')",
                [&oid],
            )
        })
        .await
        .expect("seed observation envelope");
    observation_id
}

fn read_text(conn: &Connection, memory_id: &str) -> String {
    conn.query_row(
        "SELECT text FROM memory_entry WHERE memory_id = ?1",
        params![memory_id],
        |r| r.get(0),
    )
    .expect("read text")
}

fn read_confidence(conn: &Connection, memory_id: &str) -> f64 {
    conn.query_row(
        "SELECT confidence FROM memory_entry WHERE memory_id = ?1",
        params![memory_id],
        |r| r.get(0),
    )
    .expect("read confidence")
}

fn row_count(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .expect("count")
}

#[allow(clippy::too_many_arguments)]
async fn create(
    db: &StateDb,
    memory_id: &str,
    kind: MemoryKind,
    scope_owner_id: &str,
    canonical_key: Option<&str>,
    evidence: Vec<(String, &'static str)>, // (observation_id, session_id)
    idempotency_key: Option<&str>,
) -> Result<MemoryOpOutcome, MemoryOpError> {
    let (id, owner, key, idem) = (
        memory_id.to_string(),
        scope_owner_id.to_string(),
        canonical_key.map(str::to_string),
        idempotency_key.map(str::to_string),
    );
    db.writer()
        .transaction(move |tx| {
            let evidence_inputs: Vec<EvidenceInput<'_>> = evidence
                .iter()
                .map(|(oid, session)| EvidenceInput {
                    observation_id: oid,
                    evidence_kind: local_rag_store::EvidenceKind::ToolResult,
                    session_id: session,
                    agent_id: None,
                    commit_hash: None,
                })
                .collect();
            apply_create(
                tx,
                &CreateMemoryOp {
                    memory_id: &id,
                    kind,
                    text: "original durable text",
                    canonical_key: key.as_deref(),
                    scope_kind: ScopeKind::Worktree,
                    scope_owner_id: &owner,
                    confidence: 0.5,
                    importance: 0.5,
                    valid_from_tree: None,
                    last_verified_tree: None,
                    evidence: &evidence_inputs,
                    actor: Actor::Router,
                    idempotency_key: idem.as_deref(),
                },
                1000,
            )
        })
        .await
        .expect("create tx (infrastructure)")
}

async fn reinforce(
    db: &StateDb,
    memory_id: &str,
    expected_version: i64,
    confidence: Option<f64>,
    evidence: Vec<(String, &'static str)>,
    idempotency_key: Option<&str>,
) -> Result<MemoryOpOutcome, MemoryOpError> {
    let (id, idem) = (memory_id.to_string(), idempotency_key.map(str::to_string));
    db.writer()
        .transaction(move |tx| {
            let evidence_inputs: Vec<EvidenceInput<'_>> = evidence
                .iter()
                .map(|(oid, session)| EvidenceInput {
                    observation_id: oid,
                    evidence_kind: local_rag_store::EvidenceKind::ToolResult,
                    session_id: session,
                    agent_id: None,
                    commit_hash: None,
                })
                .collect();
            apply_reinforce(
                tx,
                &ReinforceMemoryOp {
                    memory_id: &id,
                    expected_version,
                    confidence,
                    evidence: &evidence_inputs,
                    actor: Actor::Router,
                    idempotency_key: idem.as_deref(),
                },
                2000,
            )
        })
        .await
        .expect("reinforce tx (infrastructure)")
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
// Three operation contracts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_contract_writes_entry_evidence_and_audit() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let owner = uuid(1);
    let observation_id = seed_observation(&db, 2).await;
    let id = uuid(3);

    let outcome = create(
        &db,
        &id,
        MemoryKind::Fact,
        &owner,
        None,
        vec![(observation_id.clone(), "sess-1")],
        None,
    )
    .await
    .expect("create applies");
    let MemoryOpOutcome::Applied(result) = outcome else {
        panic!("expected Applied, got {outcome:?}");
    };
    assert_eq!(result.memory_id, id);
    assert_eq!(result.entry_version, 1);

    let read = db.open_read().expect("read conn");
    assert_eq!(
        memory_entry_state(&read, &id).expect("state"),
        Some((MemoryKind::Fact, local_rag_store::MemoryState::Active)),
    );
    assert_eq!(
        memory_evidence_for(&read, &id).expect("evidence"),
        vec![observation_id],
    );
    let audit = read_audit_events_for_entity(&read, "memory_entry", &id).expect("audit");
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].op, "create");
    assert_eq!(audit[0].entity_version, 1);
    assert_eq!(audit[0].audit_id, result.audit_id);
}

#[tokio::test]
async fn reinforce_contract_writes_entry_update_evidence_and_audit_but_not_text() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let owner = uuid(4);
    let id = uuid(5);
    create(&db, &id, MemoryKind::Fact, &owner, None, vec![], None)
        .await
        .expect("create");

    let read = db.open_read().expect("read conn");
    let text_before = read_text(&read, &id);
    drop(read);

    let observation_id = seed_observation(&db, 6).await;
    let outcome = reinforce(
        &db,
        &id,
        1,
        Some(0.9),
        vec![(observation_id.clone(), "sess-2")],
        None,
    )
    .await
    .expect("reinforce applies");
    let MemoryOpOutcome::Applied(result) = outcome else {
        panic!("expected Applied, got {outcome:?}");
    };
    assert_eq!(result.entry_version, 2);

    let read = db.open_read().expect("read conn");
    assert_eq!(
        read_text(&read, &id),
        text_before,
        "reinforce never edits text"
    );
    assert!((read_confidence(&read, &id) - 0.9).abs() < f64::EPSILON);
    assert_eq!(
        memory_evidence_for(&read, &id).expect("evidence"),
        vec![observation_id],
    );
    let audit = read_audit_events_for_entity(&read, "memory_entry", &id).expect("audit");
    assert_eq!(audit.len(), 2, "create + reinforce audit rows");
    assert_eq!(audit[1].op, "reinforce");
    assert_eq!(audit[1].entity_version, 2);
}

#[tokio::test]
async fn reinforce_without_confidence_change_still_bumps_version() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let owner = uuid(7);
    let id = uuid(8);
    create(&db, &id, MemoryKind::Fact, &owner, None, vec![], None)
        .await
        .expect("create");

    let read = db.open_read().expect("read conn");
    let confidence_before = read_confidence(&read, &id);
    drop(read);

    let outcome = reinforce(&db, &id, 1, None, vec![], None)
        .await
        .expect("reinforce applies");
    let MemoryOpOutcome::Applied(result) = outcome else {
        panic!("expected Applied, got {outcome:?}");
    };
    assert_eq!(
        result.entry_version, 2,
        "version bumps even without a confidence change"
    );

    let read = db.open_read().expect("read conn");
    assert_eq!(
        read_confidence(&read, &id),
        confidence_before,
        "confidence untouched"
    );
}

#[tokio::test]
async fn noop_contract_writes_nothing() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    // Seed one real entry so the tables are non-empty before the noop.
    let owner = uuid(9);
    let id = uuid(10);
    create(&db, &id, MemoryKind::Fact, &owner, None, vec![], None)
        .await
        .expect("create");

    let read = db.open_read().expect("read conn");
    let (entries_before, evidence_before, audit_before) = (
        row_count(&read, "memory_entry"),
        row_count(&read, "memory_evidence"),
        row_count(&read, "audit_event"),
    );
    drop(read);

    apply_noop();

    let read = db.open_read().expect("read conn");
    assert_eq!(row_count(&read, "memory_entry"), entries_before);
    assert_eq!(row_count(&read, "memory_evidence"), evidence_before);
    assert_eq!(row_count(&read, "audit_event"), audit_before);
}

// ---------------------------------------------------------------------------
// Optimistic conflict
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reinforce_optimistic_conflict_is_typed_error_and_rolls_back() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let owner = uuid(11);
    let id = uuid(12);
    create(&db, &id, MemoryKind::Fact, &owner, None, vec![], None)
        .await
        .expect("create");

    let result = reinforce(&db, &id, 99, Some(0.9), vec![], None)
        .await
        .expect_err("stale expected_version rejected");
    assert_eq!(
        result,
        MemoryOpError::OptimisticConflict {
            expected: 99,
            actual: 1
        }
    );

    let read = db.open_read().expect("read conn");
    assert_eq!(
        memory_entry_state(&read, &id).expect("state"),
        Some((MemoryKind::Fact, local_rag_store::MemoryState::Active)),
    );
    let audit = read_audit_events_for_entity(&read, "memory_entry", &id).expect("audit");
    assert_eq!(
        audit.len(),
        1,
        "no new audit row from the rejected reinforce"
    );
}

#[tokio::test]
async fn reinforce_unknown_memory_is_typed_error() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let ghost = uuid(13);
    let result = reinforce(&db, &ghost, 1, Some(0.9), vec![], None)
        .await
        .expect_err("unknown memory rejected");
    assert_eq!(result, MemoryOpError::UnknownMemory);
}

#[tokio::test]
async fn create_canonical_key_conflict_is_typed_error_and_rolls_back() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let owner = uuid(14);
    let first = uuid(15);
    create(
        &db,
        &first,
        MemoryKind::Fact,
        &owner,
        Some("dup-key"),
        vec![],
        None,
    )
    .await
    .expect("first create");

    let second = uuid(16);
    let result = create(
        &db,
        &second,
        MemoryKind::Fact,
        &owner,
        Some("dup-key"),
        vec![],
        None,
    )
    .await
    .expect_err("duplicate canonical_key rejected");
    assert_eq!(result, MemoryOpError::CanonicalKeyConflict);

    let read = db.open_read().expect("read conn");
    assert_eq!(
        memory_entry_state(&read, &second).expect("state"),
        None,
        "the rejected create never wrote a row"
    );
}

#[tokio::test]
async fn create_invalid_global_scope_owner_is_typed_error() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let wrong_owner = uuid(17);
    let id = uuid(18);
    let (mid, owner) = (id.clone(), wrong_owner.clone());
    let outcome = db
        .writer()
        .transaction(move |tx| {
            apply_create(
                tx,
                &CreateMemoryOp {
                    memory_id: &mid,
                    kind: MemoryKind::Convention,
                    text: "text",
                    canonical_key: None,
                    scope_kind: ScopeKind::Global,
                    scope_owner_id: &owner,
                    confidence: 0.5,
                    importance: 0.5,
                    valid_from_tree: None,
                    last_verified_tree: None,
                    evidence: &[],
                    actor: Actor::Router,
                    idempotency_key: None,
                },
                1000,
            )
        })
        .await
        .expect("create tx (infrastructure)");
    assert_eq!(outcome, Err(MemoryOpError::InvalidGlobalScopeOwner));
}

// ---------------------------------------------------------------------------
// Idempotency-key replay: "same key returns original result"
// ---------------------------------------------------------------------------

#[tokio::test]
async fn same_idempotency_key_replays_the_original_create_result() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let owner = uuid(19);
    let id = uuid(20);

    let first = create(
        &db,
        &id,
        MemoryKind::Fact,
        &owner,
        None,
        vec![],
        Some("idem-create-1"),
    )
    .await
    .expect("first create applies");
    let MemoryOpOutcome::Applied(first_result) = first else {
        panic!("expected Applied, got {first:?}");
    };

    let second = create(
        &db,
        &id,
        MemoryKind::Fact,
        &owner,
        None,
        vec![],
        Some("idem-create-1"),
    )
    .await
    .expect("replay applies");
    let MemoryOpOutcome::Replayed(second_result) = second else {
        panic!("expected Replayed, got {second:?}");
    };
    assert_eq!(
        second_result, first_result,
        "replay returns the original result"
    );

    let read = db.open_read().expect("read conn");
    assert_eq!(row_count(&read, "memory_entry"), 1, "no duplicate entry");
    assert_eq!(row_count(&read, "audit_event"), 1, "no duplicate audit row");
}

#[tokio::test]
async fn same_idempotency_key_replays_the_original_reinforce_result() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let owner = uuid(21);
    let id = uuid(22);
    create(&db, &id, MemoryKind::Fact, &owner, None, vec![], None)
        .await
        .expect("create");

    let first = reinforce(&db, &id, 1, Some(0.9), vec![], Some("idem-reinforce-1"))
        .await
        .expect("first reinforce applies");
    let MemoryOpOutcome::Applied(first_result) = first else {
        panic!("expected Applied, got {first:?}");
    };
    assert_eq!(first_result.entry_version, 2);

    // Same key, even with a (deliberately wrong) different expected_version —
    // the replay short-circuits before the version check ever runs.
    let second = reinforce(&db, &id, 99, Some(0.1), vec![], Some("idem-reinforce-1"))
        .await
        .expect("replay applies");
    let MemoryOpOutcome::Replayed(second_result) = second else {
        panic!("expected Replayed, got {second:?}");
    };
    assert_eq!(second_result, first_result);

    let read = db.open_read().expect("read conn");
    assert_eq!(
        read_confidence(&read, &id),
        0.9,
        "the replay did not re-apply the (different) second confidence"
    );
    assert_eq!(
        row_count(&read, "audit_event"),
        2,
        "create + one reinforce, no duplicate"
    );
}

// ---------------------------------------------------------------------------
// Audit versions contiguous
// ---------------------------------------------------------------------------

/// Mirrors `migrate::validate_set`'s enumerate-and-compare-to-expected shape.
fn assert_contiguous_from(versions: &[i64], start: i64) {
    for (i, v) in versions.iter().enumerate() {
        let expected = start + i as i64;
        assert_eq!(
            *v, expected,
            "position {i}: expected version {expected}, found {v}"
        );
    }
}

#[tokio::test]
async fn audit_versions_are_contiguous_across_create_then_two_reinforces() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let owner = uuid(23);
    let id = uuid(24);

    create(&db, &id, MemoryKind::Fact, &owner, None, vec![], None)
        .await
        .expect("create");
    reinforce(&db, &id, 1, Some(0.6), vec![], None)
        .await
        .expect("reinforce 1");
    reinforce(&db, &id, 2, Some(0.7), vec![], None)
        .await
        .expect("reinforce 2");

    let read = db.open_read().expect("read conn");
    let audit = read_audit_events_for_entity(&read, "memory_entry", &id).expect("audit");
    let versions: Vec<i64> = audit.iter().map(|a| a.entity_version).collect();
    assert_contiguous_from(&versions, 1);
    assert_eq!(versions, vec![1, 2, 3]);
}

// ---------------------------------------------------------------------------
// Rollback under failpoint: the multi-statement transaction is atomic
// ---------------------------------------------------------------------------

#[cfg(feature = "failpoints")]
#[tokio::test]
async fn create_rolls_back_completely_on_failpoint() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let owner = uuid(25);
    let id = uuid(26);

    // Bypass the `create()` convenience helper here: it `.expect()`s the
    // outer infrastructure result, but a fired failpoint IS an outer
    // `WriteError` — panicking would skip `disarm()` below and leave the
    // process-global failpoint armed for every later test in this binary.
    arm("memory.op.create.before_audit");
    let (mid, owner_arg) = (id.clone(), owner.clone());
    let result = db
        .writer()
        .transaction(move |tx| {
            apply_create(
                tx,
                &CreateMemoryOp {
                    memory_id: &mid,
                    kind: MemoryKind::Fact,
                    text: "original durable text",
                    canonical_key: None,
                    scope_kind: ScopeKind::Worktree,
                    scope_owner_id: &owner_arg,
                    confidence: 0.5,
                    importance: 0.5,
                    valid_from_tree: None,
                    last_verified_tree: None,
                    evidence: &[],
                    actor: Actor::Router,
                    idempotency_key: None,
                },
                1000,
            )
        })
        .await;
    assert!(
        matches!(result, Err(local_rag_store::WriteError::Sqlite(_))),
        "the failpoint must fail the call: {result:?}"
    );
    disarm("memory.op.create.before_audit");

    let read = db.open_read().expect("read conn");
    assert_eq!(
        memory_entry_state(&read, &id).expect("state"),
        None,
        "no memory_entry row survives a failure before the audit insert"
    );
    assert_eq!(row_count(&read, "audit_event"), 0);
    drop(read);

    // Retrying with the failpoint disarmed converges cleanly.
    create(&db, &id, MemoryKind::Fact, &owner, None, vec![], None)
        .await
        .expect("retry applies");
    let read = db.open_read().expect("read conn");
    assert_eq!(row_count(&read, "memory_entry"), 1);
}

#[cfg(feature = "failpoints")]
#[tokio::test]
async fn reinforce_rolls_back_completely_on_failpoint() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let owner = uuid(27);
    let id = uuid(28);
    create(&db, &id, MemoryKind::Fact, &owner, None, vec![], None)
        .await
        .expect("create");

    let read = db.open_read().expect("read conn");
    let confidence_before = read_confidence(&read, &id);
    drop(read);

    // Bypass the `reinforce()` convenience helper — see the comment in
    // `create_rolls_back_completely_on_failpoint` for why.
    arm("memory.op.reinforce.before_audit");
    let mid = id.clone();
    let result = db
        .writer()
        .transaction(move |tx| {
            apply_reinforce(
                tx,
                &ReinforceMemoryOp {
                    memory_id: &mid,
                    expected_version: 1,
                    confidence: Some(0.99),
                    evidence: &[],
                    actor: Actor::Router,
                    idempotency_key: None,
                },
                2000,
            )
        })
        .await;
    assert!(
        matches!(result, Err(local_rag_store::WriteError::Sqlite(_))),
        "the failpoint must fail the call: {result:?}"
    );
    disarm("memory.op.reinforce.before_audit");

    let read = db.open_read().expect("read conn");
    assert_eq!(
        memory_entry_state(&read, &id).expect("state"),
        Some((MemoryKind::Fact, local_rag_store::MemoryState::Active)),
    );
    assert_eq!(
        read_confidence(&read, &id),
        confidence_before,
        "confidence rolled back"
    );
    let audit = read_audit_events_for_entity(&read, "memory_entry", &id).expect("audit");
    assert_eq!(
        audit.len(),
        1,
        "no reinforce audit row survives the rollback"
    );
    drop(read);

    // Retrying with the failpoint disarmed converges cleanly.
    reinforce(&db, &id, 1, Some(0.99), vec![], None)
        .await
        .expect("retry applies");
    let read = db.open_read().expect("read conn");
    assert_eq!(read_confidence(&read, &id), 0.99);
}
