//! T14-02/T14-03 acceptance tests for the transactional memory-op engine
//! (spec 08 §3): the operation contracts (`create`/`reinforce`/`noop`/
//! `resolve`/`retract`/`supersede`/`edit`), optimistic conflict, illegal
//! kind/state transitions, idempotency-key replay ("same key returns the
//! original result"), rollback-under-failpoint, audit-version contiguity,
//! and user/router actor recording.
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
    Actor, CreateMemoryOp, EditMemoryOp, EvidenceInput, IllegalMemoryTransition, MemoryOpError,
    MemoryOpOutcome, ReinforceMemoryOp, ResolveMemoryOp, RetractMemoryOp, ScopeKind,
    SupersedeMemoryOp, apply_create, apply_edit, apply_noop, apply_reinforce, apply_resolve,
    apply_retract, apply_supersede, memory_entry_state, memory_evidence_for,
    read_audit_events_for_entity,
};
use local_rag_store::rusqlite::{Connection, params};
use local_rag_store::{MemoryKind, MemoryState, StateDb};
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

async fn resolve(
    db: &StateDb,
    memory_id: &str,
    expected_version: i64,
    actor: Actor,
    idempotency_key: Option<&str>,
) -> Result<MemoryOpOutcome, MemoryOpError> {
    let (id, idem) = (memory_id.to_string(), idempotency_key.map(str::to_string));
    db.writer()
        .transaction(move |tx| {
            apply_resolve(
                tx,
                &ResolveMemoryOp {
                    memory_id: &id,
                    expected_version,
                    evidence: &[],
                    actor,
                    idempotency_key: idem.as_deref(),
                },
                3000,
            )
        })
        .await
        .expect("resolve tx (infrastructure)")
}

async fn retract(
    db: &StateDb,
    memory_id: &str,
    expected_version: i64,
    actor: Actor,
    idempotency_key: Option<&str>,
) -> Result<MemoryOpOutcome, MemoryOpError> {
    let (id, idem) = (memory_id.to_string(), idempotency_key.map(str::to_string));
    db.writer()
        .transaction(move |tx| {
            apply_retract(
                tx,
                &RetractMemoryOp {
                    memory_id: &id,
                    expected_version,
                    evidence: &[],
                    actor,
                    idempotency_key: idem.as_deref(),
                },
                3000,
            )
        })
        .await
        .expect("retract tx (infrastructure)")
}

#[allow(clippy::too_many_arguments)]
async fn supersede(
    db: &StateDb,
    old_memory_id: &str,
    old_expected_version: i64,
    new_memory_id: &str,
    new_kind: MemoryKind,
    new_scope_owner_id: &str,
    actor: Actor,
    idempotency_key: Option<&str>,
) -> Result<MemoryOpOutcome, MemoryOpError> {
    let (old_id, new_id, owner, idem) = (
        old_memory_id.to_string(),
        new_memory_id.to_string(),
        new_scope_owner_id.to_string(),
        idempotency_key.map(str::to_string),
    );
    db.writer()
        .transaction(move |tx| {
            apply_supersede(
                tx,
                &SupersedeMemoryOp {
                    old_memory_id: &old_id,
                    old_expected_version,
                    new_memory_id: &new_id,
                    new_kind,
                    new_text: "promoted durable text",
                    new_canonical_key: None,
                    new_scope_kind: ScopeKind::Worktree,
                    new_scope_owner_id: &owner,
                    new_confidence: 0.6,
                    new_importance: 0.5,
                    new_valid_from_tree: None,
                    new_last_verified_tree: None,
                    evidence: &[],
                    actor,
                    idempotency_key: idem.as_deref(),
                },
                4000,
            )
        })
        .await
        .expect("supersede tx (infrastructure)")
}

async fn edit(
    db: &StateDb,
    memory_id: &str,
    expected_version: i64,
    text: Option<&str>,
    importance: Option<f64>,
    actor: Actor,
    idempotency_key: Option<&str>,
) -> Result<MemoryOpOutcome, MemoryOpError> {
    let (id, text_owned, idem) = (
        memory_id.to_string(),
        text.map(str::to_string),
        idempotency_key.map(str::to_string),
    );
    db.writer()
        .transaction(move |tx| {
            apply_edit(
                tx,
                &EditMemoryOp {
                    memory_id: &id,
                    expected_version,
                    text: text_owned.as_deref(),
                    importance,
                    actor,
                    idempotency_key: idem.as_deref(),
                },
                5000,
            )
        })
        .await
        .expect("edit tx (infrastructure)")
}

fn read_importance(conn: &Connection, memory_id: &str) -> f64 {
    conn.query_row(
        "SELECT importance FROM memory_entry WHERE memory_id = ?1",
        params![memory_id],
        |r| r.get(0),
    )
    .expect("read importance")
}

fn read_supersedes_id(conn: &Connection, memory_id: &str) -> Option<String> {
    conn.query_row(
        "SELECT supersedes_id FROM memory_entry WHERE memory_id = ?1",
        params![memory_id],
        |r| r.get(0),
    )
    .expect("read supersedes_id")
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
// resolve / retract: contract, illegal transition, retract-not-delete
// ---------------------------------------------------------------------------

#[tokio::test]
async fn resolve_contract_writes_state_and_audit() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let owner = uuid(130);
    let id = uuid(131);
    create(&db, &id, MemoryKind::Task, &owner, None, vec![], None)
        .await
        .expect("create");

    let outcome = resolve(&db, &id, 1, Actor::User, None)
        .await
        .expect("resolve applies");
    let MemoryOpOutcome::Applied(result) = outcome else {
        panic!("expected Applied, got {outcome:?}");
    };
    assert_eq!(result.entry_version, 2);

    let read = db.open_read().expect("read conn");
    assert_eq!(
        memory_entry_state(&read, &id).expect("state"),
        Some((MemoryKind::Task, MemoryState::Resolved)),
    );
    let audit = read_audit_events_for_entity(&read, "memory_entry", &id).expect("audit");
    assert_eq!(audit.len(), 2, "create + resolve");
    assert_eq!(audit[1].op, "resolve");
    assert_eq!(
        audit[1].actor,
        Actor::User,
        "user/router actor recorded correctly"
    );
    assert_eq!(audit[1].audit_id, result.audit_id);
}

#[tokio::test]
async fn retract_contract_survives_as_retracted_not_deleted() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let owner = uuid(132);
    let id = uuid(133);
    create(&db, &id, MemoryKind::Fact, &owner, None, vec![], None)
        .await
        .expect("create");

    let outcome = retract(&db, &id, 1, Actor::Router, None)
        .await
        .expect("retract applies");
    let MemoryOpOutcome::Applied(result) = outcome else {
        panic!("expected Applied, got {outcome:?}");
    };
    assert_eq!(result.entry_version, 2);

    let read = db.open_read().expect("read conn");
    // Retract ≠ delete (spec 08 §3): the row survives with state=retracted.
    assert_eq!(
        memory_entry_state(&read, &id).expect("state"),
        Some((MemoryKind::Fact, MemoryState::Retracted)),
        "retract does not delete the row"
    );
    let audit = read_audit_events_for_entity(&read, "memory_entry", &id).expect("audit");
    assert_eq!(audit.len(), 2, "create + retract");
    assert_eq!(audit[1].op, "retract");
    assert_eq!(audit[1].actor, Actor::Router);
}

#[tokio::test]
async fn resolve_illegal_for_fact_is_typed_error_and_rolls_back() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let owner = uuid(134);
    let id = uuid(135);
    create(&db, &id, MemoryKind::Fact, &owner, None, vec![], None)
        .await
        .expect("create");

    let result = resolve(&db, &id, 1, Actor::Router, None)
        .await
        .expect_err("resolve illegal for fact");
    assert_eq!(
        result,
        MemoryOpError::IllegalTransition(IllegalMemoryTransition {
            kind: MemoryKind::Fact,
            from: MemoryState::Active,
            to: MemoryState::Resolved,
        })
    );

    let read = db.open_read().expect("read conn");
    assert_eq!(
        memory_entry_state(&read, &id).expect("state"),
        Some((MemoryKind::Fact, MemoryState::Active)),
        "state unchanged after the rejected resolve"
    );
    let audit = read_audit_events_for_entity(&read, "memory_entry", &id).expect("audit");
    assert_eq!(audit.len(), 1, "no new audit row from the rejected resolve");
}

#[tokio::test]
async fn retract_illegal_for_hypothesis_is_typed_error() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let owner = uuid(136);
    let id = uuid(137);
    create(&db, &id, MemoryKind::Hypothesis, &owner, None, vec![], None)
        .await
        .expect("create");

    let result = retract(&db, &id, 1, Actor::Router, None)
        .await
        .expect_err("retract illegal for hypothesis");
    assert_eq!(
        result,
        MemoryOpError::IllegalTransition(IllegalMemoryTransition {
            kind: MemoryKind::Hypothesis,
            from: MemoryState::Active,
            to: MemoryState::Retracted,
        })
    );
}

#[tokio::test]
async fn resolve_unknown_memory_is_typed_error() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let ghost = uuid(138);
    let result = resolve(&db, &ghost, 1, Actor::Router, None)
        .await
        .expect_err("unknown memory rejected");
    assert_eq!(result, MemoryOpError::UnknownMemory);
}

#[tokio::test]
async fn resolve_optimistic_conflict_is_typed_error_and_rolls_back() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let owner = uuid(139);
    let id = uuid(140);
    create(&db, &id, MemoryKind::Question, &owner, None, vec![], None)
        .await
        .expect("create");

    let result = resolve(&db, &id, 99, Actor::Router, None)
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
        Some((MemoryKind::Question, MemoryState::Active)),
    );
}

#[tokio::test]
async fn same_idempotency_key_replays_the_original_resolve_result() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let owner = uuid(141);
    let id = uuid(142);
    create(&db, &id, MemoryKind::Task, &owner, None, vec![], None)
        .await
        .expect("create");

    let first = resolve(&db, &id, 1, Actor::Router, Some("idem-resolve-1"))
        .await
        .expect("first resolve applies");
    let MemoryOpOutcome::Applied(first_result) = first else {
        panic!("expected Applied, got {first:?}");
    };

    // Same key, wrong expected_version — the replay short-circuits before
    // the version check ever runs.
    let second = resolve(&db, &id, 99, Actor::Router, Some("idem-resolve-1"))
        .await
        .expect("replay applies");
    let MemoryOpOutcome::Replayed(second_result) = second else {
        panic!("expected Replayed, got {second:?}");
    };
    assert_eq!(second_result, first_result);

    let read = db.open_read().expect("read conn");
    assert_eq!(
        read_audit_events_for_entity(&read, "memory_entry", &id)
            .expect("audit")
            .len(),
        2,
        "create + one resolve, no duplicate"
    );
}

// ---------------------------------------------------------------------------
// supersede: promotion, illegal transition, optimistic conflict, replay
// ---------------------------------------------------------------------------

/// No `confirm` op exists in the transactional engine — spec 08 §3/§4's op
/// vocabulary has no "confirm"/"reject" entry, out of this task's scope — so
/// tests reach `confirmed` via the raw T14-01 transition primitive directly,
/// the same way `tests/memory.rs`'s hypothesis tests already do. It writes
/// no `audit_event` (that composition is the op engine's job); only `state`
/// changes, `entry_version` stays put.
async fn confirm_hypothesis(db: &StateDb, memory_id: &str) {
    let id = memory_id.to_string();
    db.writer()
        .transaction(move |tx| {
            local_rag_store::transition_memory_entry(tx, &id, MemoryState::Confirmed)
        })
        .await
        .expect("transition tx (infrastructure)")
        .expect("confirm transition legal");
}

/// The flagship promotion scenario (group card: "promotion creates fact via
/// supersede") and the D-020 regression at the op-engine layer: a
/// **confirmed** hypothesis (not merely `active`) is promoted into a new
/// `fact`, in one transaction.
#[tokio::test]
async fn supersede_promotes_a_confirmed_hypothesis_into_a_fact() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let owner = uuid(143);
    let hyp = uuid(144);
    create(
        &db,
        &hyp,
        MemoryKind::Hypothesis,
        &owner,
        None,
        vec![],
        None,
    )
    .await
    .expect("create hypothesis");
    confirm_hypothesis(&db, &hyp).await;

    let fact = uuid(145);
    let outcome = supersede(
        &db,
        &hyp,
        1,
        &fact,
        MemoryKind::Fact,
        &owner,
        Actor::Router,
        None,
    )
    .await
    .expect("supersede applies");
    let MemoryOpOutcome::Applied(result) = outcome else {
        panic!("expected Applied, got {outcome:?}");
    };
    assert_eq!(result.memory_id, fact);
    assert_eq!(result.entry_version, 1, "the new entry starts at version 1");

    let read = db.open_read().expect("read conn");
    assert_eq!(
        memory_entry_state(&read, &fact).expect("state"),
        Some((MemoryKind::Fact, MemoryState::Active)),
    );
    assert_eq!(
        read_supersedes_id(&read, &fact),
        Some(hyp.clone()),
        "the new fact points back at the promoted hypothesis"
    );
    assert_eq!(
        memory_entry_state(&read, &hyp).expect("state"),
        Some((MemoryKind::Hypothesis, MemoryState::Superseded)),
        "the old (confirmed) hypothesis is now superseded — D-020"
    );

    let new_audit = read_audit_events_for_entity(&read, "memory_entry", &fact).expect("new audit");
    assert_eq!(new_audit.len(), 1);
    assert_eq!(new_audit[0].op, "supersede");
    assert_eq!(new_audit[0].entity_version, 1);
    assert_eq!(new_audit[0].audit_id, result.audit_id);

    let old_audit = read_audit_events_for_entity(&read, "memory_entry", &hyp).expect("old audit");
    assert_eq!(
        old_audit.len(),
        2,
        "create + supersede-transition (the raw-primitive confirm wrote no audit row)"
    );
    assert_eq!(old_audit[1].op, "supersede");
    assert_eq!(old_audit[1].entity_version, 2);
}

#[tokio::test]
async fn supersede_illegal_for_task_is_typed_error_and_rolls_back() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let owner = uuid(146);
    let old_id = uuid(147);
    create(&db, &old_id, MemoryKind::Task, &owner, None, vec![], None)
        .await
        .expect("create");

    let new_id = uuid(148);
    let result = supersede(
        &db,
        &old_id,
        1,
        &new_id,
        MemoryKind::Fact,
        &owner,
        Actor::Router,
        None,
    )
    .await
    .expect_err("supersede illegal for task");
    assert_eq!(
        result,
        MemoryOpError::IllegalTransition(IllegalMemoryTransition {
            kind: MemoryKind::Task,
            from: MemoryState::Active,
            to: MemoryState::Superseded,
        })
    );

    let read = db.open_read().expect("read conn");
    assert_eq!(
        memory_entry_state(&read, &old_id).expect("state"),
        Some((MemoryKind::Task, MemoryState::Active)),
        "old entry unchanged"
    );
    assert_eq!(
        memory_entry_state(&read, &new_id).expect("state"),
        None,
        "no new entry created when the old-half is rejected"
    );
}

#[tokio::test]
async fn supersede_unknown_old_memory_is_typed_error() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let owner = uuid(149);
    let ghost = uuid(150);
    let new_id = uuid(151);
    let result = supersede(
        &db,
        &ghost,
        1,
        &new_id,
        MemoryKind::Fact,
        &owner,
        Actor::Router,
        None,
    )
    .await
    .expect_err("unknown old memory rejected");
    assert_eq!(result, MemoryOpError::UnknownMemory);

    let read = db.open_read().expect("read conn");
    assert_eq!(memory_entry_state(&read, &new_id).expect("state"), None);
}

#[tokio::test]
async fn supersede_optimistic_conflict_is_typed_error_and_rolls_back() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let owner = uuid(152);
    let old_id = uuid(153);
    create(&db, &old_id, MemoryKind::Fact, &owner, None, vec![], None)
        .await
        .expect("create");

    let new_id = uuid(154);
    let result = supersede(
        &db,
        &old_id,
        99,
        &new_id,
        MemoryKind::Fact,
        &owner,
        Actor::Router,
        None,
    )
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
    assert_eq!(memory_entry_state(&read, &new_id).expect("state"), None);
}

#[tokio::test]
async fn same_idempotency_key_replays_the_original_supersede_result() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let owner = uuid(155);
    let old_id = uuid(156);
    create(&db, &old_id, MemoryKind::Fact, &owner, None, vec![], None)
        .await
        .expect("create");

    let new_id = uuid(157);
    let first = supersede(
        &db,
        &old_id,
        1,
        &new_id,
        MemoryKind::Fact,
        &owner,
        Actor::Router,
        Some("idem-supersede-1"),
    )
    .await
    .expect("first supersede applies");
    let MemoryOpOutcome::Applied(first_result) = first else {
        panic!("expected Applied, got {first:?}");
    };

    // Same key, a different (bogus, never-created) old/new pair — the replay
    // short-circuits before any of the old/new-side preconditions run.
    let other_old = uuid(158);
    let other_new = uuid(159);
    let second = supersede(
        &db,
        &other_old,
        1,
        &other_new,
        MemoryKind::Fact,
        &owner,
        Actor::Router,
        Some("idem-supersede-1"),
    )
    .await
    .expect("replay applies");
    let MemoryOpOutcome::Replayed(second_result) = second else {
        panic!("expected Replayed, got {second:?}");
    };
    assert_eq!(second_result, first_result);

    let read = db.open_read().expect("read conn");
    assert_eq!(
        memory_entry_state(&read, &other_new).expect("state"),
        None,
        "the replay never created the bogus new entry"
    );
}

// ---------------------------------------------------------------------------
// edit: text/importance change, terminal guard, actor
// ---------------------------------------------------------------------------

#[tokio::test]
async fn edit_contract_changes_text_and_importance_and_bumps_version() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let owner = uuid(160);
    let id = uuid(161);
    create(&db, &id, MemoryKind::Convention, &owner, None, vec![], None)
        .await
        .expect("create");

    let outcome = edit(
        &db,
        &id,
        1,
        Some("edited durable text"),
        Some(0.8),
        Actor::User,
        None,
    )
    .await
    .expect("edit applies");
    let MemoryOpOutcome::Applied(result) = outcome else {
        panic!("expected Applied, got {outcome:?}");
    };
    assert_eq!(result.entry_version, 2);

    let read = db.open_read().expect("read conn");
    assert_eq!(read_text(&read, &id), "edited durable text");
    assert!((read_importance(&read, &id) - 0.8).abs() < f64::EPSILON);
    let audit = read_audit_events_for_entity(&read, "memory_entry", &id).expect("audit");
    assert_eq!(audit.len(), 2);
    assert_eq!(audit[1].op, "edit");
    assert_eq!(audit[1].actor, Actor::User, "user-edit recorded");
}

#[tokio::test]
async fn edit_router_actor_is_recorded() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let owner = uuid(162);
    let id = uuid(163);
    create(&db, &id, MemoryKind::Procedure, &owner, None, vec![], None)
        .await
        .expect("create");

    edit(&db, &id, 1, None, Some(0.3), Actor::Router, None)
        .await
        .expect("edit applies");

    let read = db.open_read().expect("read conn");
    let audit = read_audit_events_for_entity(&read, "memory_entry", &id).expect("audit");
    assert_eq!(audit[1].actor, Actor::Router, "router-edit recorded");
}

#[tokio::test]
async fn edit_terminal_entry_is_typed_error_and_rolls_back() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let owner = uuid(164);
    let id = uuid(165);
    create(&db, &id, MemoryKind::Fact, &owner, None, vec![], None)
        .await
        .expect("create");
    retract(&db, &id, 1, Actor::Router, None)
        .await
        .expect("retract");

    let read = db.open_read().expect("read conn");
    let text_before = read_text(&read, &id);
    drop(read);

    let result = edit(
        &db,
        &id,
        2,
        Some("should not apply"),
        None,
        Actor::User,
        None,
    )
    .await
    .expect_err("editing a terminal entry rejected");
    assert_eq!(result, MemoryOpError::EntryTerminal);

    let read = db.open_read().expect("read conn");
    assert_eq!(read_text(&read, &id), text_before, "text unchanged");
}

#[tokio::test]
async fn edit_optimistic_conflict_is_typed_error_and_rolls_back() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let owner = uuid(166);
    let id = uuid(167);
    create(&db, &id, MemoryKind::Decision, &owner, None, vec![], None)
        .await
        .expect("create");

    let result = edit(&db, &id, 99, Some("nope"), None, Actor::User, None)
        .await
        .expect_err("stale expected_version rejected");
    assert_eq!(
        result,
        MemoryOpError::OptimisticConflict {
            expected: 99,
            actual: 1
        }
    );
}

#[tokio::test]
async fn same_idempotency_key_replays_the_original_edit_result() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let owner = uuid(168);
    let id = uuid(169);
    create(&db, &id, MemoryKind::Fact, &owner, None, vec![], None)
        .await
        .expect("create");

    let first = edit(
        &db,
        &id,
        1,
        Some("first edit"),
        None,
        Actor::User,
        Some("idem-edit-1"),
    )
    .await
    .expect("first edit applies");
    let MemoryOpOutcome::Applied(first_result) = first else {
        panic!("expected Applied, got {first:?}");
    };

    let second = edit(
        &db,
        &id,
        99,
        Some("second edit — must not apply"),
        None,
        Actor::User,
        Some("idem-edit-1"),
    )
    .await
    .expect("replay applies");
    let MemoryOpOutcome::Replayed(second_result) = second else {
        panic!("expected Replayed, got {second:?}");
    };
    assert_eq!(second_result, first_result);

    let read = db.open_read().expect("read conn");
    assert_eq!(read_text(&read, &id), "first edit");
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
