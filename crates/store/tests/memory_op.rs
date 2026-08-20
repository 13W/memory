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
    MemoryOpOutcome, MergeLoser, MergeMemoryOp, NormalizationStatus, NormalizationWrite,
    ReinforceMemoryOp, ResolveMemoryOp, RetractMemoryOp, ScopeKind, SupersedeMemoryOp,
    UpsertOutcome, apply_create, apply_edit, apply_merge, apply_noop, apply_reinforce,
    apply_resolve, apply_retract, apply_supersede, memory_entry_state, memory_evidence_for,
    normalization_for, read_audit_events_for_entity, upsert_normalization,
};
use local_rag_store::rusqlite::{Connection, params};
use local_rag_store::{MemoryKind, MemoryState, StateDb};
use local_rag_test_support::TempHome;
use tokio::sync::Mutex;

#[cfg(feature = "failpoints")]
use local_rag_test_support::Action;

static SERIAL: Mutex<()> = Mutex::const_new(());

/// The text [`create`] writes. A normalization fixture must hash exactly this,
/// since `upsert_normalization` refuses a write whose source hash has moved.
const CREATED_TEXT: &str = "original durable text";

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
                    text: CREATED_TEXT,
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

async fn merge(
    db: &StateDb,
    survivor_id: &str,
    survivor_expected_version: i64,
    losers: Vec<(String, i64)>,
    actor: Actor,
    idempotency_key: Option<&str>,
) -> Result<MemoryOpOutcome, MemoryOpError> {
    let (survivor, idem) = (survivor_id.to_string(), idempotency_key.map(str::to_string));
    db.writer()
        .transaction(move |tx| {
            let loser_refs: Vec<MergeLoser<'_>> = losers
                .iter()
                .map(|(id, version)| MergeLoser {
                    memory_id: id,
                    expected_version: *version,
                })
                .collect();
            apply_merge(
                tx,
                &MergeMemoryOp {
                    survivor_id: &survivor,
                    survivor_expected_version,
                    losers: &loser_refs,
                    actor,
                    idempotency_key: idem.as_deref(),
                },
                6000,
            )
        })
        .await
        .expect("merge tx (infrastructure)")
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
// merge: survivor absorbs evidence, losers superseded, audit records the set
// ---------------------------------------------------------------------------

#[tokio::test]
async fn merge_contract_absorbs_evidence_and_supersedes_losers() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let owner = uuid(170);
    let survivor = uuid(171);
    let loser1 = uuid(172);
    let loser2 = uuid(173);
    let obs1 = seed_observation(&db, 174).await;
    let obs2 = seed_observation(&db, 175).await;

    create(&db, &survivor, MemoryKind::Fact, &owner, None, vec![], None)
        .await
        .expect("create survivor");
    create(
        &db,
        &loser1,
        MemoryKind::Fact,
        &owner,
        None,
        vec![(obs1.clone(), "sess-1")],
        None,
    )
    .await
    .expect("create loser1");
    create(
        &db,
        &loser2,
        MemoryKind::Decision,
        &owner,
        None,
        vec![(obs2.clone(), "sess-1")],
        None,
    )
    .await
    .expect("create loser2");

    let outcome = merge(
        &db,
        &survivor,
        1,
        vec![(loser1.clone(), 1), (loser2.clone(), 1)],
        Actor::Router,
        None,
    )
    .await
    .expect("merge applies");
    let MemoryOpOutcome::Applied(result) = outcome else {
        panic!("expected Applied, got {outcome:?}");
    };
    assert_eq!(result.memory_id, survivor);
    assert_eq!(
        result.entry_version, 2,
        "survivor absorbing evidence is a real mutation"
    );

    let read = db.open_read().expect("read conn");
    // Survivor absorbed both losers' evidence.
    let mut survivor_evidence = memory_evidence_for(&read, &survivor).expect("evidence");
    survivor_evidence.sort();
    let mut expected = vec![obs1.clone(), obs2.clone()];
    expected.sort();
    assert_eq!(survivor_evidence, expected);

    // Both losers are superseded, pointing back at the survivor.
    for loser in [&loser1, &loser2] {
        assert!(
            memory_evidence_for(&read, loser)
                .expect("evidence")
                .is_empty(),
            "evidence moved off the loser"
        );
        assert_eq!(read_supersedes_id(&read, loser), Some(survivor.clone()));
    }
    assert_eq!(
        memory_entry_state(&read, &loser1).expect("state"),
        Some((MemoryKind::Fact, MemoryState::Superseded)),
    );
    assert_eq!(
        memory_entry_state(&read, &loser2).expect("state"),
        Some((MemoryKind::Decision, MemoryState::Superseded)),
    );

    // Audit: 1 create each (3) + 1 merge each (3) = 6 total across the set;
    // only the survivor's merge row carries the payload.
    let survivor_audit =
        read_audit_events_for_entity(&read, "memory_entry", &survivor).expect("audit");
    assert_eq!(survivor_audit.len(), 2);
    assert_eq!(survivor_audit[1].op, "merge");
    assert_eq!(survivor_audit[1].entity_version, 2);
    assert_eq!(survivor_audit[1].audit_id, result.audit_id);
    let payload = survivor_audit[1].payload.as_deref().expect("merge payload");
    let merged: Vec<String> = serde_json::from_str(payload).expect("payload is a JSON array");
    let mut merged_sorted = merged.clone();
    merged_sorted.sort();
    let mut losers_sorted = vec![loser1.clone(), loser2.clone()];
    losers_sorted.sort();
    assert_eq!(
        merged_sorted, losers_sorted,
        "audit records the exact merge set"
    );

    for loser in [&loser1, &loser2] {
        let loser_audit =
            read_audit_events_for_entity(&read, "memory_entry", loser).expect("audit");
        assert_eq!(loser_audit.len(), 2, "create + merge");
        assert_eq!(loser_audit[1].op, "merge");
        assert_eq!(
            loser_audit[1].idempotency_key, None,
            "only the survivor's row carries it"
        );
    }
}

#[tokio::test]
async fn merge_duplicate_evidence_is_not_duplicated_and_stays_with_loser() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let owner = uuid(176);
    let survivor = uuid(177);
    let loser = uuid(178);
    let shared_obs = seed_observation(&db, 179).await;

    create(
        &db,
        &survivor,
        MemoryKind::Fact,
        &owner,
        None,
        vec![(shared_obs.clone(), "sess-1")],
        None,
    )
    .await
    .expect("create survivor");
    create(
        &db,
        &loser,
        MemoryKind::Fact,
        &owner,
        None,
        vec![(shared_obs.clone(), "sess-2")],
        None,
    )
    .await
    .expect("create loser");

    merge(
        &db,
        &survivor,
        1,
        vec![(loser.clone(), 1)],
        Actor::Router,
        None,
    )
    .await
    .expect("merge applies");

    let read = db.open_read().expect("read conn");
    assert_eq!(
        memory_evidence_for(&read, &survivor).expect("evidence"),
        vec![shared_obs.clone()],
        "still exactly one row for the shared observation, not two"
    );
    assert_eq!(
        memory_evidence_for(&read, &loser).expect("evidence"),
        vec![shared_obs],
        "the duplicate stays attached to the (superseded) loser rather than erroring"
    );
}

#[tokio::test]
async fn merge_incompatible_scope_is_typed_error_and_rolls_back() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let owner_a = uuid(180);
    let owner_b = uuid(181);
    let survivor = uuid(182);
    let loser = uuid(183);
    create(
        &db,
        &survivor,
        MemoryKind::Fact,
        &owner_a,
        None,
        vec![],
        None,
    )
    .await
    .expect("create survivor");
    create(&db, &loser, MemoryKind::Fact, &owner_b, None, vec![], None)
        .await
        .expect("create loser");

    let result = merge(
        &db,
        &survivor,
        1,
        vec![(loser.clone(), 1)],
        Actor::Router,
        None,
    )
    .await
    .expect_err("incompatible scope rejected");
    assert_eq!(result, MemoryOpError::IncompatibleScope);

    let read = db.open_read().expect("read conn");
    assert_eq!(
        memory_entry_state(&read, &loser).expect("state"),
        Some((MemoryKind::Fact, MemoryState::Active)),
        "loser unchanged"
    );
    assert_eq!(
        read_audit_events_for_entity(&read, "memory_entry", &survivor)
            .expect("audit")
            .len(),
        1,
        "no new audit row on the survivor"
    );
}

#[tokio::test]
async fn merge_illegal_for_task_loser_is_typed_error_and_rolls_back() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let owner = uuid(184);
    let survivor = uuid(185);
    let loser = uuid(186);
    create(&db, &survivor, MemoryKind::Fact, &owner, None, vec![], None)
        .await
        .expect("create survivor");
    create(&db, &loser, MemoryKind::Task, &owner, None, vec![], None)
        .await
        .expect("create loser");

    let result = merge(
        &db,
        &survivor,
        1,
        vec![(loser.clone(), 1)],
        Actor::Router,
        None,
    )
    .await
    .expect_err("task loser cannot be superseded");
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
        memory_entry_state(&read, &loser).expect("state"),
        Some((MemoryKind::Task, MemoryState::Active)),
    );
}

#[tokio::test]
async fn merge_empty_losers_is_typed_error() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let owner = uuid(187);
    let survivor = uuid(188);
    create(&db, &survivor, MemoryKind::Fact, &owner, None, vec![], None)
        .await
        .expect("create survivor");

    let result = merge(&db, &survivor, 1, vec![], Actor::Router, None)
        .await
        .expect_err("empty losers rejected");
    assert_eq!(result, MemoryOpError::EmptyMergeSet);
}

#[tokio::test]
async fn merge_survivor_optimistic_conflict_is_typed_error_and_rolls_back() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let owner = uuid(189);
    let survivor = uuid(190);
    let loser = uuid(191);
    create(&db, &survivor, MemoryKind::Fact, &owner, None, vec![], None)
        .await
        .expect("create survivor");
    create(&db, &loser, MemoryKind::Fact, &owner, None, vec![], None)
        .await
        .expect("create loser");

    let result = merge(
        &db,
        &survivor,
        99,
        vec![(loser.clone(), 1)],
        Actor::Router,
        None,
    )
    .await
    .expect_err("stale survivor expected_version rejected");
    assert_eq!(
        result,
        MemoryOpError::OptimisticConflict {
            expected: 99,
            actual: 1
        }
    );

    let read = db.open_read().expect("read conn");
    assert_eq!(
        memory_entry_state(&read, &loser).expect("state"),
        Some((MemoryKind::Fact, MemoryState::Active)),
        "loser untouched when the survivor precondition fails first"
    );
}

#[tokio::test]
async fn merge_loser_optimistic_conflict_is_typed_error_and_rolls_back() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let owner = uuid(192);
    let survivor = uuid(193);
    let good_loser = uuid(194);
    let bad_loser = uuid(195);
    create(&db, &survivor, MemoryKind::Fact, &owner, None, vec![], None)
        .await
        .expect("create survivor");
    create(
        &db,
        &good_loser,
        MemoryKind::Fact,
        &owner,
        None,
        vec![],
        None,
    )
    .await
    .expect("create good_loser");
    create(
        &db,
        &bad_loser,
        MemoryKind::Fact,
        &owner,
        None,
        vec![],
        None,
    )
    .await
    .expect("create bad_loser");

    let result = merge(
        &db,
        &survivor,
        1,
        vec![(good_loser.clone(), 1), (bad_loser.clone(), 99)],
        Actor::Router,
        None,
    )
    .await
    .expect_err("stale loser expected_version rejected");
    assert_eq!(
        result,
        MemoryOpError::OptimisticConflict {
            expected: 99,
            actual: 1
        }
    );

    let read = db.open_read().expect("read conn");
    // Pre-validate-all-then-mutate: the valid `good_loser` must NOT have been
    // touched just because a later loser in the same request failed.
    assert_eq!(
        memory_entry_state(&read, &good_loser).expect("state"),
        Some((MemoryKind::Fact, MemoryState::Active)),
        "good_loser untouched"
    );
    assert_eq!(
        memory_entry_state(&read, &survivor).expect("state"),
        Some((MemoryKind::Fact, MemoryState::Active)),
    );
    assert_eq!(
        read_audit_events_for_entity(&read, "memory_entry", &survivor)
            .expect("audit")
            .len(),
        1
    );
}

#[tokio::test]
async fn same_idempotency_key_replays_the_original_merge_result() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let owner = uuid(196);
    let survivor = uuid(197);
    let loser = uuid(198);
    create(&db, &survivor, MemoryKind::Fact, &owner, None, vec![], None)
        .await
        .expect("create survivor");
    create(&db, &loser, MemoryKind::Fact, &owner, None, vec![], None)
        .await
        .expect("create loser");

    let first = merge(
        &db,
        &survivor,
        1,
        vec![(loser.clone(), 1)],
        Actor::Router,
        Some("idem-merge-1"),
    )
    .await
    .expect("first merge applies");
    let MemoryOpOutcome::Applied(first_result) = first else {
        panic!("expected Applied, got {first:?}");
    };

    // Same key, a different (never-created) bogus loser — the replay
    // short-circuits before any precondition on it runs.
    let bogus_loser = uuid(199);
    let second = merge(
        &db,
        &survivor,
        99,
        vec![(bogus_loser.clone(), 1)],
        Actor::Router,
        Some("idem-merge-1"),
    )
    .await
    .expect("replay applies");
    let MemoryOpOutcome::Replayed(second_result) = second else {
        panic!("expected Replayed, got {second:?}");
    };
    assert_eq!(second_result, first_result);

    let read = db.open_read().expect("read conn");
    assert_eq!(
        memory_entry_state(&read, &bogus_loser).expect("state"),
        None,
        "the replay never touched the bogus loser"
    );
}

#[cfg(feature = "failpoints")]
#[tokio::test]
async fn merge_rolls_back_completely_on_failpoint() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let owner = uuid(200);
    let survivor = uuid(201);
    let loser = uuid(202);
    let obs = seed_observation(&db, 203).await;
    create(&db, &survivor, MemoryKind::Fact, &owner, None, vec![], None)
        .await
        .expect("create survivor");
    create(
        &db,
        &loser,
        MemoryKind::Fact,
        &owner,
        None,
        vec![(obs.clone(), "sess-1")],
        None,
    )
    .await
    .expect("create loser");

    arm("memory.op.merge.before_survivor_audit");
    let (survivor_arg, loser_id) = (survivor.clone(), loser.clone());
    let result = db
        .writer()
        .transaction(move |tx| {
            apply_merge(
                tx,
                &MergeMemoryOp {
                    survivor_id: &survivor_arg,
                    survivor_expected_version: 1,
                    losers: &[MergeLoser {
                        memory_id: &loser_id,
                        expected_version: 1,
                    }],
                    actor: Actor::Router,
                    idempotency_key: None,
                },
                6000,
            )
        })
        .await;
    assert!(
        matches!(result, Err(local_rag_store::WriteError::Sqlite(_))),
        "the failpoint must fail the call: {result:?}"
    );
    disarm("memory.op.merge.before_survivor_audit");

    let read = db.open_read().expect("read conn");
    assert_eq!(
        memory_entry_state(&read, &loser).expect("state"),
        Some((MemoryKind::Fact, MemoryState::Active)),
        "no loser transition survives a failure before the survivor's final audit insert"
    );
    assert_eq!(
        memory_evidence_for(&read, &survivor).expect("evidence"),
        Vec::<String>::new(),
        "no evidence was absorbed"
    );
    assert_eq!(
        read_audit_events_for_entity(&read, "memory_entry", &survivor)
            .expect("audit")
            .len(),
        1,
        "no merge audit row on the survivor"
    );
    drop(read);

    // Retrying with the failpoint disarmed converges cleanly.
    merge(
        &db,
        &survivor,
        1,
        vec![(loser.clone(), 1)],
        Actor::Router,
        None,
    )
    .await
    .expect("retry applies");
    let read = db.open_read().expect("read conn");
    assert_eq!(
        memory_entry_state(&read, &loser).expect("state"),
        Some((MemoryKind::Fact, MemoryState::Superseded)),
    );
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
                    text: CREATED_TEXT,
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

// ---------------------------------------------------------------------------
// T14-07: ModelClaimOnlyProvenance backstop (spec 12 §4 `[FIXED]`)
// ---------------------------------------------------------------------------

/// Like [`create`], but with explicit control over `actor` and each evidence
/// row's `evidence_kind` — what the model-claim-only-provenance backstop
/// tests need that the narrower [`create`] helper (fixed `ToolResult`/
/// `Router`) does not give.
async fn create_with_evidence(
    db: &StateDb,
    memory_id: &str,
    kind: MemoryKind,
    scope_owner_id: &str,
    actor: Actor,
    evidence: Vec<(String, local_rag_store::EvidenceKind)>,
) -> Result<MemoryOpOutcome, MemoryOpError> {
    let (id, owner) = (memory_id.to_string(), scope_owner_id.to_string());
    db.writer()
        .transaction(move |tx| {
            let evidence_inputs: Vec<EvidenceInput<'_>> = evidence
                .iter()
                .map(|(oid, evidence_kind)| EvidenceInput {
                    observation_id: oid,
                    evidence_kind: *evidence_kind,
                    session_id: "sess-1",
                    agent_id: None,
                    commit_hash: None,
                })
                .collect();
            apply_create(
                tx,
                &CreateMemoryOp {
                    memory_id: &id,
                    kind,
                    text: "candidate durable text",
                    canonical_key: None,
                    scope_kind: ScopeKind::Worktree,
                    scope_owner_id: &owner,
                    confidence: 0.5,
                    importance: 0.5,
                    valid_from_tree: None,
                    last_verified_tree: None,
                    evidence: &evidence_inputs,
                    actor,
                    idempotency_key: None,
                },
                1000,
            )
        })
        .await
        .expect("create tx (infrastructure)")
}

#[tokio::test]
async fn router_promotion_with_only_model_claim_evidence_is_rejected() {
    let (_home, db) = open_state();
    let owner = uuid(220);
    let id = uuid(221);
    let obs = seed_observation(&db, 222).await;

    let result = create_with_evidence(
        &db,
        &id,
        MemoryKind::Fact,
        &owner,
        Actor::Router,
        vec![(obs, local_rag_store::EvidenceKind::ModelClaim)],
    )
    .await;
    assert_eq!(result, Err(MemoryOpError::ModelClaimOnlyProvenance));

    let read = db.open_read().expect("read conn");
    assert_eq!(
        memory_entry_state(&read, &id).expect("state"),
        None,
        "no row created"
    );
}

#[tokio::test]
async fn router_promotion_with_no_evidence_at_all_is_not_the_backstops_concern() {
    // Empty evidence is a different, pre-existing concern (every earlier
    // T14-0N op-engine test already exercises evidence-less router ops for
    // reasons unrelated to trust) -- the backstop triggers specifically on
    // the `model_claim` *marking*, not on evidence being absent.
    let (_home, db) = open_state();
    let owner = uuid(223);
    let id = uuid(224);

    let result = create_with_evidence(
        &db,
        &id,
        MemoryKind::Decision,
        &owner,
        Actor::Router,
        vec![],
    )
    .await;
    assert!(
        matches!(result, Ok(MemoryOpOutcome::Applied(_))),
        "{result:?}"
    );
}

#[tokio::test]
async fn router_promotion_with_at_least_one_non_model_claim_row_succeeds() {
    let (_home, db) = open_state();
    let owner = uuid(225);
    let id = uuid(226);
    let obs_claim = seed_observation(&db, 227).await;
    let obs_real = seed_observation(&db, 228).await;

    let result = create_with_evidence(
        &db,
        &id,
        MemoryKind::Convention,
        &owner,
        Actor::Router,
        vec![
            (obs_claim, local_rag_store::EvidenceKind::ModelClaim),
            (obs_real, local_rag_store::EvidenceKind::ToolResult),
        ],
    )
    .await;
    assert!(
        matches!(result, Ok(MemoryOpOutcome::Applied(_))),
        "{result:?}"
    );
}

#[tokio::test]
async fn router_task_and_hypothesis_are_exempt_from_the_backstop() {
    let (_home, db) = open_state();
    let owner = uuid(229);

    for (seed, kind) in [(230, MemoryKind::Task), (231, MemoryKind::Hypothesis)] {
        let id = uuid(seed);
        let obs = seed_observation(&db, seed + 10).await;
        let result = create_with_evidence(
            &db,
            &id,
            kind,
            &owner,
            Actor::Router,
            vec![(obs, local_rag_store::EvidenceKind::ModelClaim)],
        )
        .await;
        assert!(
            matches!(result, Ok(MemoryOpOutcome::Applied(_))),
            "{kind:?}: {result:?}"
        );
    }
}

#[tokio::test]
async fn user_actor_promotion_with_only_model_claim_evidence_is_allowed() {
    let (_home, db) = open_state();
    let owner = uuid(232);
    let id = uuid(233);
    let obs = seed_observation(&db, 234).await;

    // spec 08 §5's carve-out: `remember`/candidate-approval carries
    // user-equivalent trust even when its own evidence is `model_claim`.
    let result = create_with_evidence(
        &db,
        &id,
        MemoryKind::Procedure,
        &owner,
        Actor::User,
        vec![(obs, local_rag_store::EvidenceKind::ModelClaim)],
    )
    .await;
    assert!(
        matches!(result, Ok(MemoryOpOutcome::Applied(_))),
        "{result:?}"
    );
}

#[tokio::test]
async fn supersede_new_entry_model_claim_only_is_rejected_and_old_entry_is_untouched() {
    let (_home, db) = open_state();
    let owner = uuid(235);
    let old_id = uuid(236);
    let new_id = uuid(237);
    let obs = seed_observation(&db, 238).await;

    // A real, user-backed old fact -- supersedable per the kind/state machine.
    create_with_evidence(
        &db,
        &old_id,
        MemoryKind::Fact,
        &owner,
        Actor::User,
        vec![(obs.clone(), local_rag_store::EvidenceKind::UserStatement)],
    )
    .await
    .expect("old entry create");

    let (old, new, own, ob) = (old_id.clone(), new_id.clone(), owner.clone(), obs.clone());
    let result = db
        .writer()
        .transaction(move |tx| {
            let evidence = [EvidenceInput {
                observation_id: &ob,
                evidence_kind: local_rag_store::EvidenceKind::ModelClaim,
                session_id: "sess-1",
                agent_id: None,
                commit_hash: None,
            }];
            apply_supersede(
                tx,
                &SupersedeMemoryOp {
                    old_memory_id: &old,
                    old_expected_version: 1,
                    new_memory_id: &new,
                    new_kind: MemoryKind::Fact,
                    new_text: "router-claimed replacement",
                    new_canonical_key: None,
                    new_scope_kind: ScopeKind::Worktree,
                    new_scope_owner_id: &own,
                    new_confidence: 0.5,
                    new_importance: 0.5,
                    new_valid_from_tree: None,
                    new_last_verified_tree: None,
                    evidence: &evidence,
                    actor: Actor::Router,
                    idempotency_key: None,
                },
                2000,
            )
        })
        .await
        .expect("supersede tx (infrastructure)");
    assert_eq!(result, Err(MemoryOpError::ModelClaimOnlyProvenance));

    let read = db.open_read().expect("read conn");
    assert_eq!(
        memory_entry_state(&read, &old_id).expect("state"),
        Some((MemoryKind::Fact, MemoryState::Active)),
        "the old entry must be completely untouched -- not just the new one refused"
    );
    assert_eq!(memory_entry_state(&read, &new_id).expect("state"), None);
}

// ---------------------------------------------------------------------------
// T21-07: an edit invalidates the entry's English variant — but only a real one
// ---------------------------------------------------------------------------

/// Give `memory_id` a `ready` normalization row over the text [`create`] wrote.
async fn seed_normalization(db: &StateDb, memory_id: &str) {
    let id = memory_id.to_string();
    let sha = local_rag_core::hash::sha256_hex(CREATED_TEXT.as_bytes());
    let outcome = db
        .writer()
        .transaction(move |tx| {
            upsert_normalization(
                tx,
                &NormalizationWrite {
                    memory_id: &id,
                    status: NormalizationStatus::Translated,
                    expected_text_sha256: &sha,
                    canon_text_sha256: &sha,
                    source_text: Some("the English variant"),
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

async fn has_normalization(db: &StateDb, memory_id: &str) -> bool {
    let read = db.open_read().expect("read conn");
    normalization_for(&read, memory_id)
        .expect("read normalization")
        .is_some()
}

#[tokio::test]
async fn edit_with_new_text_drops_the_translation() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let owner = uuid(200);
    let id = uuid(201);
    create(&db, &id, MemoryKind::Fact, &owner, None, vec![], None)
        .await
        .expect("create");
    seed_normalization(&db, &id).await;

    edit(
        &db,
        &id,
        1,
        Some("a genuinely different durable text"),
        None,
        Actor::User,
        None,
    )
    .await
    .expect("edit applies");

    assert!(
        !has_normalization(&db, &id).await,
        "a translation of text the user replaced must not survive the edit",
    );
}

#[tokio::test]
async fn edit_resubmitting_the_same_text_keeps_the_translation() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let owner = uuid(202);
    let id = uuid(203);
    create(&db, &id, MemoryKind::Fact, &owner, None, vec![], None)
        .await
        .expect("create");
    seed_normalization(&db, &id).await;

    edit(&db, &id, 1, Some(CREATED_TEXT), None, Actor::User, None)
        .await
        .expect("edit applies");

    assert!(
        has_normalization(&db, &id).await,
        "nothing changed, so nothing is stale — re-translating would be paid for twice",
    );
}

#[tokio::test]
async fn edit_of_importance_alone_keeps_the_translation() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let owner = uuid(204);
    let id = uuid(205);
    create(&db, &id, MemoryKind::Fact, &owner, None, vec![], None)
        .await
        .expect("create");
    seed_normalization(&db, &id).await;

    edit(&db, &id, 1, None, Some(0.9), Actor::User, None)
        .await
        .expect("edit applies");

    assert!(
        has_normalization(&db, &id).await,
        "importance is not text; the English variant is still accurate",
    );
}

#[tokio::test]
async fn reinforce_keeps_the_translation() {
    let _serial = SERIAL.lock().await;
    let (_home, db) = open_state();
    let owner = uuid(206);
    let id = uuid(207);
    create(&db, &id, MemoryKind::Fact, &owner, None, vec![], None)
        .await
        .expect("create");
    seed_normalization(&db, &id).await;

    reinforce(&db, &id, 1, Some(0.8), vec![], None)
        .await
        .expect("reinforce applies");

    assert!(
        has_normalization(&db, &id).await,
        "reinforce may not touch the text, so it may not invalidate its translation",
    );
}
