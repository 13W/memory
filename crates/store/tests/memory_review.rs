//! T14-05 acceptance tests for the candidate review operations (spec 04 §6,
//! 08 §3/§5/§8): `propose`/`edit`/`approve`/`reject` over
//! `pending_memory_candidate`, materialization through the same op engine as
//! the router, FK-derived evidence, double-approval idempotence, and the
//! state-based "conflicting edit" rejection. Candidate expiry
//! (`run_candidate_expiry_sweep`) is tested in `tests/housekeeping.rs`
//! alongside this crate's other GC sweeps, not here.
//!
//! All tests are deterministic: an isolated [`TempHome`], fixed `now_ms`
//! literals, and ids minted from [`uuidv7_from`] with fixed entropy.

use local_rag_core::identity::uuidv7_from;
use local_rag_core::paths::StoreLayout;
use local_rag_store::memory::{
    AUDIT_ENTITY_CANDIDATE, AUDIT_OP_FOLD_DUPLICATE, ApproveCandidateOutcome, CandidateCountRow,
    CandidateRow, FoldDuplicatesOutcome, FoldRetained, ProposeCandidateOutcome, ProposedOperation,
    ReviewError, approve_candidate, candidate_evidence_for, edit_candidate,
    fold_pending_duplicates, list_candidates, memory_entry_by_id, memory_entry_state,
    memory_evidence_for, pending_candidate_counts, pending_candidate_groups, propose_candidate,
    reject_candidate,
};
use local_rag_store::rusqlite::{Connection, params};
use local_rag_store::{
    Actor, AuditEventRow, CandidateState, EvidenceKind, IllegalCandidateTransition, MemoryKind,
    MemoryOpOutcome, MemoryState, NewCandidate, ScopeKind, StateDb, create_candidate,
    insert_candidate_evidence, read_audit_events_for_entity,
};
use local_rag_test_support::TempHome;

/// A temporary store with an ensured tree and an opened [`StateDb`].
fn open_state() -> (TempHome, StateDb) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    (home, db)
}

/// A distinct, deterministic UUIDv7 string keyed by `seed`.
fn uuid(seed: u8) -> String {
    let mut rand = [0u8; 10];
    rand[9] = seed;
    uuidv7_from(1000, rand).to_string()
}

/// Insert a standalone `observation_envelope` row with a caller-chosen
/// `evidence_kind`/`session_id`, so FK-evidence-derivation tests can assert
/// the materialized `memory_evidence` row copies these exact values, not a
/// hardcoded default.
async fn seed_observation(
    db: &StateDb,
    seed: u8,
    evidence_kind: EvidenceKind,
    session_id: &str,
) -> String {
    let observation_id = uuid(seed);
    let (oid, kind, session) = (
        observation_id.clone(),
        evidence_kind.as_str().to_string(),
        session_id.to_string(),
    );
    db.writer()
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO observation_envelope \
                   (observation_id, source_event_id, payload_hash, event_type, evidence_kind, \
                    trust, session_id) \
                 VALUES (?1, 'evt-1', 'deadbeef', 'Stop', ?2, 'normal', ?3)",
                params![oid, kind, session],
            )
        })
        .await
        .expect("seed observation envelope");
    observation_id
}

#[allow(clippy::too_many_arguments)]
async fn propose(
    db: &StateDb,
    candidate_id: &str,
    op: ProposedOperation,
    conflicts: Vec<String>,
    evidence_observation_ids: Vec<String>,
    now_ms: i64,
) -> ProposeCandidateOutcome {
    let (id, op, conflicts, evidence) = (
        candidate_id.to_string(),
        op,
        conflicts,
        evidence_observation_ids,
    );
    db.writer()
        .transaction(move |tx| {
            let conflict_refs: Vec<&str> = conflicts.iter().map(String::as_str).collect();
            let evidence_refs: Vec<&str> = evidence.iter().map(String::as_str).collect();
            propose_candidate(tx, &id, &op, &conflict_refs, &evidence_refs, now_ms)
        })
        .await
        .expect("propose tx")
}

async fn approve(
    db: &StateDb,
    candidate_id: &str,
    now_ms: i64,
) -> Result<ApproveCandidateOutcome, ReviewError> {
    let id = candidate_id.to_string();
    db.writer()
        .transaction(move |tx| approve_candidate(tx, &id, now_ms))
        .await
        .expect("approve tx (infrastructure)")
}

async fn reject(db: &StateDb, candidate_id: &str, now_ms: i64) -> Result<(), ReviewError> {
    let id = candidate_id.to_string();
    db.writer()
        .transaction(move |tx| reject_candidate(tx, &id, now_ms))
        .await
        .expect("reject tx (infrastructure)")
}

/// `T23-08`: fold `survivor_candidate_id`'s exact-duplicate group.
async fn fold(
    db: &StateDb,
    survivor_candidate_id: &str,
    now_ms: i64,
) -> Result<FoldDuplicatesOutcome, ReviewError> {
    let id = survivor_candidate_id.to_string();
    db.writer()
        .transaction(move |tx| fold_pending_duplicates(tx, &id, now_ms))
        .await
        .expect("fold tx (infrastructure)")
}

async fn edit(
    db: &StateDb,
    candidate_id: &str,
    new_op: Option<ProposedOperation>,
    new_conflicts: Option<Vec<String>>,
    now_ms: i64,
) -> Result<(), ReviewError> {
    let id = candidate_id.to_string();
    db.writer()
        .transaction(move |tx| {
            let conflict_refs: Option<Vec<&str>> = new_conflicts
                .as_ref()
                .map(|c| c.iter().map(String::as_str).collect());
            edit_candidate(tx, &id, new_op.as_ref(), conflict_refs.as_deref(), now_ms)
        })
        .await
        .expect("edit tx (infrastructure)")
}

fn create_op(memory_id: &str, kind: MemoryKind, scope_owner_id: &str) -> ProposedOperation {
    create_op_with_text(memory_id, kind, scope_owner_id, "candidate-proposed text")
}

/// Like [`create_op`], but with a caller-chosen `text` — needed wherever a
/// test proposes more than one candidate in the same `(kind, scope_owner_id)`
/// and wants each to survive as its own row: since `T23-07`, two proposals
/// that agree on kind/scope/text are the same proposal
/// (`local_rag_store::memory::candidate_dedup_key`) regardless of the
/// `memory_id` each one happens to carry, so a fixture testing something
/// else (pagination, counts) must give them genuinely different text or it
/// will silently collide into one row.
fn create_op_with_text(
    memory_id: &str,
    kind: MemoryKind,
    scope_owner_id: &str,
    text: &str,
) -> ProposedOperation {
    ProposedOperation::Create {
        memory_id: memory_id.to_string(),
        kind: kind.as_str().to_string(),
        text: text.to_string(),
        canonical_key: None,
        scope_kind: ScopeKind::Worktree.as_str().to_string(),
        scope_owner_id: scope_owner_id.to_string(),
        confidence: 0.5,
        importance: 0.5,
        valid_from_tree: None,
        last_verified_tree: None,
    }
}

fn read_memory_evidence_row(
    conn: &Connection,
    memory_id: &str,
    observation_id: &str,
) -> (String, String) {
    conn.query_row(
        "SELECT evidence_kind, session_id FROM memory_evidence \
         WHERE memory_id = ?1 AND observation_id = ?2",
        params![memory_id, observation_id],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
    )
    .expect("read memory_evidence row")
}

fn row_count(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .expect("count")
}

// ---------------------------------------------------------------------------
// propose / list / provenance
// ---------------------------------------------------------------------------

#[tokio::test]
async fn propose_then_list_exposes_state_and_provenance() {
    let (_home, db) = open_state();
    let owner = uuid(1);
    let memory_id = uuid(2);
    let observation_id = seed_observation(&db, 3, EvidenceKind::UserStatement, "sess-1").await;
    let op = create_op(&memory_id, MemoryKind::Fact, &owner);

    propose(
        &db,
        "cand-1",
        op.clone(),
        vec!["other-memory".to_string()],
        vec![observation_id.clone()],
        1_000,
    )
    .await;

    let read = db.open_read().expect("read conn");
    let rows = list_candidates(&read, None, i64::MAX, 0).expect("list");
    assert_eq!(rows.len(), 1);
    let row: &CandidateRow = &rows[0];
    assert_eq!(row.candidate_id, "cand-1");
    assert_eq!(row.review_state, CandidateState::Pending);
    assert_eq!(row.created_at, 1_000);
    assert_eq!(row.conflicts.as_deref(), Some("[\"other-memory\"]"));
    let round_tripped: ProposedOperation =
        serde_json::from_str(&row.proposed_operation).expect("parse stored proposal");
    assert_eq!(round_tripped, op);

    assert_eq!(
        candidate_evidence_for(&read, "cand-1").expect("evidence"),
        vec![observation_id],
    );
}

#[tokio::test]
async fn list_candidates_filters_by_review_state() {
    let (_home, db) = open_state();
    let owner = uuid(10);
    propose(
        &db,
        "cand-a",
        create_op_with_text(&uuid(11), MemoryKind::Fact, &owner, "claim a"),
        vec![],
        vec![],
        1_000,
    )
    .await;
    propose(
        &db,
        "cand-b",
        create_op_with_text(&uuid(12), MemoryKind::Fact, &owner, "claim b"),
        vec![],
        vec![],
        1_100,
    )
    .await;
    reject(&db, "cand-b", 1_000).await.expect("reject");

    let read = db.open_read().expect("read conn");
    let pending =
        list_candidates(&read, Some(CandidateState::Pending), i64::MAX, 0).expect("list pending");
    assert_eq!(
        pending
            .iter()
            .map(|r| r.candidate_id.as_str())
            .collect::<Vec<_>>(),
        vec!["cand-a"],
    );
    let rejected =
        list_candidates(&read, Some(CandidateState::Rejected), i64::MAX, 0).expect("list rejected");
    assert_eq!(
        rejected
            .iter()
            .map(|r| r.candidate_id.as_str())
            .collect::<Vec<_>>(),
        vec!["cand-b"],
    );
    let all = list_candidates(&read, None, i64::MAX, 0).expect("list all");
    assert_eq!(all.len(), 2);
}

// ---------------------------------------------------------------------------
// approve materializes each op kind, FK evidence is derived
// ---------------------------------------------------------------------------

#[tokio::test]
async fn approve_materializes_create_and_derives_evidence_from_observation() {
    let (_home, db) = open_state();
    let owner = uuid(20);
    let memory_id = uuid(21);
    let observation_id = seed_observation(&db, 22, EvidenceKind::TestResult, "sess-derived").await;

    propose(
        &db,
        "cand-create",
        create_op(&memory_id, MemoryKind::Fact, &owner),
        vec![],
        vec![observation_id.clone()],
        1_000,
    )
    .await;

    let outcome = approve(&db, "cand-create", 2_000).await.expect("approve");
    let ApproveCandidateOutcome::Materialized(MemoryOpOutcome::Applied(result)) = outcome else {
        panic!("expected Materialized(Applied), got {outcome:?}");
    };
    assert_eq!(result.memory_id, memory_id);
    assert_eq!(result.entry_version, 1);

    let read = db.open_read().expect("read conn");
    assert_eq!(
        memory_entry_state(&read, &memory_id).expect("state"),
        Some((MemoryKind::Fact, MemoryState::Active)),
    );
    assert_eq!(
        memory_evidence_for(&read, &memory_id).expect("evidence"),
        vec![observation_id.clone()],
    );
    let (evidence_kind, session_id) = read_memory_evidence_row(&read, &memory_id, &observation_id);
    assert_eq!(
        evidence_kind, "test_result",
        "evidence_kind derived from the observation, not hardcoded"
    );
    assert_eq!(session_id, "sess-derived");
}

#[tokio::test]
async fn approve_materializes_reinforce() {
    let (_home, db) = open_state();
    let owner = uuid(30);
    let memory_id = uuid(31);
    propose(
        &db,
        "cand-base",
        create_op(&memory_id, MemoryKind::Fact, &owner),
        vec![],
        vec![],
        1_000,
    )
    .await;
    approve(&db, "cand-base", 1_000)
        .await
        .expect("approve create");

    propose(
        &db,
        "cand-reinforce",
        ProposedOperation::Reinforce {
            memory_id: memory_id.clone(),
            expected_version: 1,
            confidence: Some(0.9),
        },
        vec![],
        vec![],
        2_000,
    )
    .await;
    let outcome = approve(&db, "cand-reinforce", 2_000)
        .await
        .expect("approve reinforce");
    let ApproveCandidateOutcome::Materialized(MemoryOpOutcome::Applied(result)) = outcome else {
        panic!("expected Materialized(Applied), got {outcome:?}");
    };
    assert_eq!(result.entry_version, 2);
}

#[tokio::test]
async fn approve_materializes_resolve() {
    let (_home, db) = open_state();
    let owner = uuid(40);
    let memory_id = uuid(41);
    propose(
        &db,
        "cand-base",
        create_op(&memory_id, MemoryKind::Task, &owner),
        vec![],
        vec![],
        1_000,
    )
    .await;
    approve(&db, "cand-base", 1_000)
        .await
        .expect("approve create");

    propose(
        &db,
        "cand-resolve",
        ProposedOperation::Resolve {
            memory_id: memory_id.clone(),
            expected_version: 1,
        },
        vec![],
        vec![],
        2_000,
    )
    .await;
    approve(&db, "cand-resolve", 2_000)
        .await
        .expect("approve resolve");

    let read = db.open_read().expect("read conn");
    assert_eq!(
        memory_entry_state(&read, &memory_id).expect("state"),
        Some((MemoryKind::Task, MemoryState::Resolved)),
    );
}

#[tokio::test]
async fn approve_materializes_retract() {
    let (_home, db) = open_state();
    let owner = uuid(50);
    let memory_id = uuid(51);
    propose(
        &db,
        "cand-base",
        create_op(&memory_id, MemoryKind::Fact, &owner),
        vec![],
        vec![],
        1_000,
    )
    .await;
    approve(&db, "cand-base", 1_000)
        .await
        .expect("approve create");

    propose(
        &db,
        "cand-retract",
        ProposedOperation::Retract {
            memory_id: memory_id.clone(),
            expected_version: 1,
        },
        vec![],
        vec![],
        2_000,
    )
    .await;
    approve(&db, "cand-retract", 2_000)
        .await
        .expect("approve retract");

    let read = db.open_read().expect("read conn");
    assert_eq!(
        memory_entry_state(&read, &memory_id).expect("state"),
        Some((MemoryKind::Fact, MemoryState::Retracted)),
    );
}

#[tokio::test]
async fn approve_materializes_supersede() {
    let (_home, db) = open_state();
    let owner = uuid(60);
    let old_id = uuid(61);
    let new_id = uuid(62);

    propose(
        &db,
        "cand-base",
        create_op(&old_id, MemoryKind::Hypothesis, &owner),
        vec![],
        vec![],
        1_000,
    )
    .await;
    approve(&db, "cand-base", 1_000)
        .await
        .expect("approve create");
    // Promote the hypothesis to `confirmed` first, matching D-020's legal path.
    db.writer()
        .transaction({
            let old_id = old_id.clone();
            move |tx| local_rag_store::transition_memory_entry(tx, &old_id, MemoryState::Confirmed)
        })
        .await
        .expect("confirm tx")
        .expect("legal confirm");

    propose(
        &db,
        "cand-supersede",
        ProposedOperation::Supersede {
            old_memory_id: old_id.clone(),
            old_expected_version: 1,
            new_memory_id: new_id.clone(),
            new_kind: MemoryKind::Fact.as_str().to_string(),
            new_text: "promoted by review".to_string(),
            new_canonical_key: None,
            new_scope_kind: ScopeKind::Worktree.as_str().to_string(),
            new_scope_owner_id: owner.clone(),
            new_confidence: 0.8,
            new_importance: 0.6,
            new_valid_from_tree: None,
            new_last_verified_tree: None,
        },
        vec![],
        vec![],
        2_000,
    )
    .await;
    let outcome = approve(&db, "cand-supersede", 2_000)
        .await
        .expect("approve supersede");
    let ApproveCandidateOutcome::Materialized(MemoryOpOutcome::Applied(result)) = outcome else {
        panic!("expected Materialized(Applied), got {outcome:?}");
    };
    assert_eq!(result.memory_id, new_id);

    let read = db.open_read().expect("read conn");
    assert_eq!(
        memory_entry_state(&read, &old_id).expect("old state"),
        Some((MemoryKind::Hypothesis, MemoryState::Superseded)),
    );
    assert_eq!(
        memory_entry_state(&read, &new_id).expect("new state"),
        Some((MemoryKind::Fact, MemoryState::Active)),
    );
}

// ---------------------------------------------------------------------------
// double-approval idempotence, rejected never materializes, conflicting edit
// ---------------------------------------------------------------------------

#[tokio::test]
async fn double_approval_is_idempotent_no_duplicate_entry() {
    let (_home, db) = open_state();
    let owner = uuid(70);
    let memory_id = uuid(71);
    propose(
        &db,
        "cand-1",
        create_op(&memory_id, MemoryKind::Fact, &owner),
        vec![],
        vec![],
        1_000,
    )
    .await;

    let first = approve(&db, "cand-1", 2_000).await.expect("first approve");
    assert!(matches!(
        first,
        ApproveCandidateOutcome::Materialized(MemoryOpOutcome::Applied(_))
    ));

    let second = approve(&db, "cand-1", 3_000).await.expect("second approve");
    assert_eq!(second, ApproveCandidateOutcome::AlreadyApproved);

    let read = db.open_read().expect("read conn");
    assert_eq!(row_count(&read, "memory_entry"), 1, "no duplicate entry");
    assert_eq!(row_count(&read, "audit_event"), 1, "no duplicate audit row");
}

#[tokio::test]
async fn rejected_candidate_never_materializes() {
    let (_home, db) = open_state();
    let owner = uuid(80);
    let memory_id = uuid(81);
    propose(
        &db,
        "cand-1",
        create_op(&memory_id, MemoryKind::Fact, &owner),
        vec![],
        vec![],
        1_000,
    )
    .await;

    reject(&db, "cand-1", 1_000).await.expect("reject");

    let read = db.open_read().expect("read conn");
    assert_eq!(
        row_count(&read, "memory_entry"),
        0,
        "rejected never materializes"
    );
    assert_eq!(memory_entry_state(&read, &memory_id).expect("state"), None,);
}

/// `T23-11`/`D-132`: an operator's own reject now leaves the same kind of
/// trace a fold already did.
#[tokio::test]
async fn reject_candidate_writes_one_audit_event() {
    let (_home, db) = open_state();
    let owner = uuid(103);
    propose(
        &db,
        "cand-1",
        create_op(&uuid(104), MemoryKind::Fact, &owner),
        vec![],
        vec![],
        1_000,
    )
    .await;

    reject(&db, "cand-1", 2_000).await.expect("reject");

    let read = db.open_read().expect("read conn");
    let rows = audit_rows_for_candidate(&read, "cand-1");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].op, "reject");
    assert_eq!(rows[0].actor, Actor::User);
    assert_eq!(rows[0].entity_version, 1);
    assert_eq!(rows[0].created_at, 2_000);
}

/// `T23-11`: `transition_candidate`'s own self-transition-is-legal no-op
/// (`reject_memory_candidate_retry_on_the_same_already_rejected_is_a_success_no_op`,
/// `crates/local-rag/tests/mcp_memory_write_tools.rs`) must stay a silent
/// no-op at the audit layer too — a retried reject must not write a second,
/// false "reject happened again" row.
#[tokio::test]
async fn a_retried_reject_writes_no_second_audit_row() {
    let (_home, db) = open_state();
    let owner = uuid(105);
    propose(
        &db,
        "cand-1",
        create_op(&uuid(106), MemoryKind::Fact, &owner),
        vec![],
        vec![],
        1_000,
    )
    .await;

    reject(&db, "cand-1", 2_000).await.expect("first reject");
    reject(&db, "cand-1", 3_000)
        .await
        .expect("retried reject is a success no-op, not an error");

    let read = db.open_read().expect("read conn");
    assert_eq!(
        audit_rows_for_candidate(&read, "cand-1").len(),
        1,
        "the retry must not write a second audit row"
    );
}

#[tokio::test]
async fn edit_while_pending_updates_proposal_and_conflicts() {
    let (_home, db) = open_state();
    let owner = uuid(90);
    let original_target = uuid(91);
    let new_target = uuid(92);
    propose(
        &db,
        "cand-1",
        create_op(&original_target, MemoryKind::Fact, &owner),
        vec![],
        vec![],
        1_000,
    )
    .await;

    let new_op = create_op(&new_target, MemoryKind::Decision, &owner);
    edit(
        &db,
        "cand-1",
        Some(new_op.clone()),
        Some(vec!["conflict-1".to_string()]),
        1_000,
    )
    .await
    .expect("edit");

    let read = db.open_read().expect("read conn");
    let rows = list_candidates(&read, None, i64::MAX, 0).expect("list");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].conflicts.as_deref(), Some("[\"conflict-1\"]"));
    let round_tripped: ProposedOperation =
        serde_json::from_str(&rows[0].proposed_operation).expect("parse");
    assert_eq!(round_tripped, new_op);

    // `T23-11`/`D-132`: the edit itself now leaves a trace too.
    let events = audit_rows_for_candidate(&read, "cand-1");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].op, "edit");
    assert_eq!(events[0].actor, Actor::User);
    assert_eq!(events[0].entity_version, 1);
    assert_eq!(
        events[0].payload, None,
        "matching apply_edit's own precedent"
    );
}

/// `T23-11`: `edit_candidate` is legally repeatable while `pending` (unlike
/// `reject`/`expire`, which are terminal) — so its `entity_version` must come
/// from [`next_candidate_audit_version`], not a constant, or a second edit on
/// one candidate would collide with the first's own row under
/// `UNIQUE (entity_kind, entity_id, entity_version)`.
#[tokio::test]
async fn two_edits_on_one_pending_candidate_write_two_ordered_audit_rows() {
    let (_home, db) = open_state();
    let owner = uuid(107);
    propose(
        &db,
        "cand-1",
        create_op(&uuid(108), MemoryKind::Fact, &owner),
        vec![],
        vec![],
        1_000,
    )
    .await;

    edit(
        &db,
        "cand-1",
        Some(create_op(&uuid(109), MemoryKind::Fact, &owner)),
        None,
        2_000,
    )
    .await
    .expect("first edit");
    edit(
        &db,
        "cand-1",
        Some(create_op(&uuid(110), MemoryKind::Decision, &owner)),
        None,
        3_000,
    )
    .await
    .expect("second edit");

    let read = db.open_read().expect("read conn");
    let events = audit_rows_for_candidate(&read, "cand-1");
    assert_eq!(events.len(), 2, "two edits, two rows, no collision");
    assert_eq!(events[0].entity_version, 1);
    assert_eq!(events[0].created_at, 2_000);
    assert_eq!(events[1].entity_version, 2);
    assert_eq!(events[1].created_at, 3_000);
    assert!(events.iter().all(|e| e.op == "edit"));
}

#[tokio::test]
async fn edit_non_pending_candidate_is_conflicting_edit_with_no_mutation() {
    let (_home, db) = open_state();
    let owner = uuid(100);
    let memory_id = uuid(101);
    propose(
        &db,
        "cand-1",
        create_op(&memory_id, MemoryKind::Fact, &owner),
        vec![],
        vec![],
        1_000,
    )
    .await;
    reject(&db, "cand-1", 1_000).await.expect("reject");

    let attempted_op = create_op(&uuid(102), MemoryKind::Fact, &owner);
    let result = edit(&db, "cand-1", Some(attempted_op), None, 1_000).await;
    assert_eq!(result, Err(ReviewError::NotPending));

    let read = db.open_read().expect("read conn");
    let rows = list_candidates(&read, None, i64::MAX, 0).expect("list");
    assert_eq!(
        rows[0].review_state,
        CandidateState::Rejected,
        "still rejected"
    );
    let round_tripped: ProposedOperation =
        serde_json::from_str(&rows[0].proposed_operation).expect("parse");
    assert_eq!(
        round_tripped,
        create_op(&memory_id, MemoryKind::Fact, &owner),
        "proposal untouched by the rejected edit attempt",
    );
    // `T23-11`: the one row is the reject's own — the rejected edit attempt
    // added no second one.
    assert_eq!(
        audit_rows_for_candidate(&read, "cand-1").len(),
        1,
        "a NotPending edit writes no audit row of its own"
    );
}

// ---------------------------------------------------------------------------
// unknown candidate / illegal transition
// ---------------------------------------------------------------------------

#[tokio::test]
async fn approve_unknown_candidate_is_typed_error() {
    let (_home, db) = open_state();
    let result = approve(&db, "does-not-exist", 1_000).await;
    assert_eq!(result, Err(ReviewError::UnknownCandidate));
}

#[tokio::test]
async fn reject_unknown_candidate_is_typed_error() {
    let (_home, db) = open_state();
    let result = reject(&db, "does-not-exist", 1_000).await;
    assert_eq!(result, Err(ReviewError::UnknownCandidate));
}

#[tokio::test]
async fn approve_already_rejected_candidate_is_illegal_transition() {
    let (_home, db) = open_state();
    let owner = uuid(110);
    let memory_id = uuid(111);
    propose(
        &db,
        "cand-1",
        create_op(&memory_id, MemoryKind::Fact, &owner),
        vec![],
        vec![],
        1_000,
    )
    .await;
    reject(&db, "cand-1", 1_000).await.expect("reject");

    let result = approve(&db, "cand-1", 2_000).await;
    assert_eq!(
        result,
        Err(ReviewError::IllegalTransition(IllegalCandidateTransition {
            from: CandidateState::Rejected,
            to: CandidateState::Approved,
        })),
    );
    let read = db.open_read().expect("read conn");
    assert_eq!(
        row_count(&read, "memory_entry"),
        0,
        "no materialization on illegal approve"
    );
}

// ---------------------------------------------------------------------------
// T15-04: list_candidates pagination / pending_candidate_counts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_candidates_limit_and_offset_window_the_ordered_result() {
    let (_home, db) = open_state();
    let owner = uuid(180);
    for (i, seed) in (0u8..4).enumerate() {
        propose(
            &db,
            &format!("cand-{i}"),
            create_op_with_text(
                &uuid(190 + seed),
                MemoryKind::Fact,
                &owner,
                &format!("claim {i}"),
            ),
            vec![],
            vec![],
            1_000 + i64::from(seed),
        )
        .await;
    }

    let read = db.open_read().expect("read conn");
    let first_page = list_candidates(&read, None, 2, 0).expect("page 1");
    assert_eq!(
        first_page
            .iter()
            .map(|r| r.candidate_id.as_str())
            .collect::<Vec<_>>(),
        vec!["cand-0", "cand-1"]
    );
    let second_page = list_candidates(&read, None, 2, 2).expect("page 2");
    assert_eq!(
        second_page
            .iter()
            .map(|r| r.candidate_id.as_str())
            .collect::<Vec<_>>(),
        vec!["cand-2", "cand-3"]
    );
    let past_the_end = list_candidates(&read, None, 2, 4).expect("page 3");
    assert!(past_the_end.is_empty());
}

#[tokio::test]
async fn pending_candidate_counts_groups_by_review_state() {
    let (_home, db) = open_state();
    let owner = uuid(181);
    propose(
        &db,
        "cand-pending-1",
        create_op_with_text(&uuid(182), MemoryKind::Fact, &owner, "claim pending 1"),
        vec![],
        vec![],
        1_000,
    )
    .await;
    propose(
        &db,
        "cand-pending-2",
        create_op_with_text(&uuid(183), MemoryKind::Fact, &owner, "claim pending 2"),
        vec![],
        vec![],
        1_100,
    )
    .await;
    propose(
        &db,
        "cand-rejected",
        create_op_with_text(&uuid(184), MemoryKind::Fact, &owner, "claim rejected"),
        vec![],
        vec![],
        1_200,
    )
    .await;
    reject(&db, "cand-rejected", 1_000).await.expect("reject");

    let read = db.open_read().expect("read conn");
    let counts = pending_candidate_counts(&read).expect("counts");
    assert_eq!(
        counts,
        vec![
            CandidateCountRow {
                state: CandidateState::Pending,
                count: 2,
            },
            CandidateCountRow {
                state: CandidateState::Rejected,
                count: 1,
            },
        ],
        "ordered by review_state; empty buckets are omitted"
    );
}

// -----------------------------------------------------------------
// T23-07 / ADR-0014 Decision 2 / D-118 / D-127: a proposal identical to one
// already pending, or already an entry, writes no row.
// -----------------------------------------------------------------

/// The card's own wording, as an assertion.
#[tokio::test]
async fn the_same_proposal_twice_yields_one_row() {
    let (_home, db) = open_state();
    let owner = uuid(200);

    let first = propose(
        &db,
        "cand-first",
        create_op_with_text(&uuid(201), MemoryKind::Fact, &owner, "the same claim"),
        vec![],
        vec![],
        1_000,
    )
    .await;
    assert_eq!(first, ProposeCandidateOutcome::Proposed);

    let second = propose(
        &db,
        "cand-second",
        create_op_with_text(&uuid(202), MemoryKind::Fact, &owner, "the same claim"),
        vec![],
        vec![],
        1_100,
    )
    .await;
    assert_eq!(
        second,
        ProposeCandidateOutcome::DuplicateOfPending {
            candidate_id: "cand-first".to_string()
        },
        "the oldest pending twin wins"
    );

    let read = db.open_read().expect("read conn");
    let rows = list_candidates(&read, None, i64::MAX, 0).expect("list");
    assert_eq!(
        rows.iter()
            .map(|r| r.candidate_id.as_str())
            .collect::<Vec<_>>(),
        vec!["cand-first"],
        "one row, not two"
    );
}

/// A proposal differing in exactly one of kind/scope_owner_id/text is a
/// genuinely different claim and is unaffected by the check.
#[tokio::test]
async fn a_genuinely_different_proposal_is_unaffected() {
    for (label, kind, scope_owner, text) in [
        ("different kind", MemoryKind::Decision, "owner-a", "claim"),
        (
            "different scope_owner_id",
            MemoryKind::Fact,
            "owner-b",
            "claim",
        ),
        (
            "different text",
            MemoryKind::Fact,
            "owner-a",
            "a different claim",
        ),
    ] {
        let (_home, db) = open_state();
        propose(
            &db,
            "cand-base",
            create_op_with_text(&uuid(210), MemoryKind::Fact, "owner-a", "claim"),
            vec![],
            vec![],
            1_000,
        )
        .await;

        let outcome = propose(
            &db,
            "cand-other",
            create_op_with_text(&uuid(211), kind, scope_owner, text),
            vec![],
            vec![],
            1_100,
        )
        .await;
        assert_eq!(
            outcome,
            ProposeCandidateOutcome::Proposed,
            "{label}: a genuinely different proposal must still be proposed"
        );

        let read = db.open_read().expect("read conn");
        let rows = list_candidates(&read, None, i64::MAX, 0).expect("list");
        assert_eq!(rows.len(), 2, "{label}: both rows survive");
    }
}

/// Evidence from a dropped duplicate is not lost — it lands on the survivor
/// (the same "carry the evidence" reasoning D-078's reinforce rewrite uses),
/// and re-citing an observation the survivor already cites does not trip
/// `candidate_evidence`'s primary key.
#[tokio::test]
async fn the_duplicates_evidence_lands_on_the_survivor() {
    let (_home, db) = open_state();
    let owner = uuid(220);
    let o1 = seed_observation(&db, 221, EvidenceKind::UserStatement, "sess-1").await;
    let o2 = seed_observation(&db, 222, EvidenceKind::UserStatement, "sess-1").await;

    propose(
        &db,
        "cand-first",
        create_op_with_text(&uuid(223), MemoryKind::Fact, &owner, "claim"),
        vec![],
        vec![o1.clone()],
        1_000,
    )
    .await;

    // A second, identical proposal citing the same observation again plus a
    // new one: the repeat must not be a PK violation, and the new one must
    // land.
    let outcome = propose(
        &db,
        "cand-second",
        create_op_with_text(&uuid(224), MemoryKind::Fact, &owner, "claim"),
        vec![],
        vec![o1.clone(), o2.clone()],
        1_100,
    )
    .await;
    assert_eq!(
        outcome,
        ProposeCandidateOutcome::DuplicateOfPending {
            candidate_id: "cand-first".to_string()
        }
    );

    let read = db.open_read().expect("read conn");
    let mut evidence = candidate_evidence_for(&read, "cand-first").expect("evidence");
    evidence.sort();
    let mut expected = vec![o1, o2];
    expected.sort();
    assert_eq!(evidence, expected, "the survivor carries the union");
}

/// A candidate the owner already rejected does not blacklist the claim: the
/// router re-deriving it from new evidence is a legitimate new proposal.
#[tokio::test]
async fn a_rejected_twin_does_not_block_a_new_proposal() {
    let (_home, db) = open_state();
    let owner = uuid(230);

    propose(
        &db,
        "cand-first",
        create_op_with_text(&uuid(231), MemoryKind::Fact, &owner, "claim"),
        vec![],
        vec![],
        1_000,
    )
    .await;
    reject(&db, "cand-first", 1_000).await.expect("reject");

    let outcome = propose(
        &db,
        "cand-second",
        create_op_with_text(&uuid(232), MemoryKind::Fact, &owner, "claim"),
        vec![],
        vec![],
        1_100,
    )
    .await;
    assert_eq!(
        outcome,
        ProposeCandidateOutcome::Proposed,
        "a rejected twin must not block the new proposal"
    );
}

/// A `create` proposal whose exact text is already a non-terminal entry in
/// that scope has nothing left to review: no row, and the entry itself is
/// left untouched (no evidence, no confidence change).
#[tokio::test]
async fn a_proposal_whose_text_is_already_an_active_entry_writes_no_row_and_touches_no_entry() {
    let (_home, db) = open_state();
    let owner = uuid(240);
    let memory_id = uuid(241);

    propose(
        &db,
        "cand-materialize",
        create_op_with_text(&memory_id, MemoryKind::Fact, &owner, "an existing claim"),
        vec![],
        vec![],
        1_000,
    )
    .await;
    approve(&db, "cand-materialize", 1_500)
        .await
        .expect("materializes");

    let outcome = propose(
        &db,
        "cand-duplicate",
        create_op_with_text(&uuid(242), MemoryKind::Fact, &owner, "an existing claim"),
        vec![],
        vec![],
        2_000,
    )
    .await;
    assert_eq!(
        outcome,
        ProposeCandidateOutcome::AlreadyAnEntry {
            memory_id: memory_id.clone()
        }
    );

    let read = db.open_read().expect("read conn");
    let rows = list_candidates(&read, None, i64::MAX, 0).expect("list");
    assert_eq!(
        rows.len(),
        1,
        "no second row -- the only candidate is the one already approved"
    );
    assert_eq!(
        memory_entry_state(&read, &memory_id).expect("state"),
        Some((MemoryKind::Fact, MemoryState::Active)),
        "the entry itself is untouched"
    );
}

// -----------------------------------------------------------------
// T23-08 / ADR-0014 Decision 2: folding a pre-existing duplicate group is a
// recorded act, not a rejection.
// -----------------------------------------------------------------

/// Seed one exact-duplicate group of `ids.len()` pending candidates directly
/// via `create_candidate`, bypassing `propose_candidate` — since `T23-07`,
/// that check declines a second row sharing one identity, so a fixture
/// wanting more than one still has to arise the same way the pre-T23-07
/// backlog itself did. `ids[0]` gets the oldest `created_at`
/// (`base_created_at`), each later id one millisecond later — so `ids[0]` is
/// always the deterministic survivor `fold_pending_duplicates` should pick.
async fn duplicate_group(db: &StateDb, ids: &[&str], op: &ProposedOperation, base_created_at: i64) {
    let json = serde_json::to_string(op).expect("op serializes");
    for (i, id) in ids.iter().enumerate() {
        let (cid, json) = (id.to_string(), json.clone());
        let created_at = base_created_at + i as i64;
        db.writer()
            .transaction(move |tx| {
                create_candidate(
                    tx,
                    &NewCandidate {
                        candidate_id: &cid,
                        proposed_operation: &json,
                        conflicts: None,
                    },
                    created_at,
                )
            })
            .await
            .expect("seed duplicate candidate");
    }
}

fn audit_rows_for_candidate(conn: &Connection, candidate_id: &str) -> Vec<AuditEventRow> {
    read_audit_events_for_entity(conn, AUDIT_ENTITY_CANDIDATE, candidate_id)
        .expect("read audit rows")
}

#[tokio::test]
async fn folding_rejects_every_twin_and_leaves_the_survivor_pending() {
    let (_home, db) = open_state();
    let owner = uuid(44);
    let op = create_op_with_text(&uuid(45), MemoryKind::Fact, &owner, "the same claim");
    duplicate_group(&db, &["survivor", "twin-a", "twin-b"], &op, 1_000).await;

    let outcome = fold(&db, "survivor", 5_000).await.expect("fold");
    let mut folded = outcome.folded.clone();
    folded.sort();
    assert_eq!(folded, vec!["twin-a".to_string(), "twin-b".to_string()]);
    assert!(outcome.retained.is_empty(), "{:?}", outcome.retained);
    assert_eq!(outcome.survivor_candidate_id, "survivor");

    let read = db.open_read().expect("read conn");
    assert_eq!(
        candidate_state_of(&read, "survivor"),
        CandidateState::Pending
    );
    assert_eq!(
        candidate_state_of(&read, "twin-a"),
        CandidateState::Rejected
    );
    assert_eq!(
        candidate_state_of(&read, "twin-b"),
        CandidateState::Rejected
    );
}

#[tokio::test]
async fn folding_touches_no_candidate_outside_the_named_group() {
    let (_home, db) = open_state();
    let owner = uuid(54);
    let claim_a = create_op_with_text(&uuid(55), MemoryKind::Fact, &owner, "claim a");
    let claim_b = create_op_with_text(&uuid(56), MemoryKind::Fact, &owner, "claim b");
    duplicate_group(&db, &["a-survivor", "a-twin"], &claim_a, 1_000).await;
    duplicate_group(&db, &["b-survivor", "b-twin"], &claim_b, 1_000).await;

    let outcome = fold(&db, "a-survivor", 5_000).await.expect("fold");
    assert_eq!(outcome.folded, vec!["a-twin".to_string()]);

    let read = db.open_read().expect("read conn");
    assert_eq!(
        candidate_state_of(&read, "a-twin"),
        CandidateState::Rejected
    );
    assert_eq!(
        candidate_state_of(&read, "b-survivor"),
        CandidateState::Pending,
        "a different group must be untouched"
    );
    assert_eq!(
        candidate_state_of(&read, "b-twin"),
        CandidateState::Pending,
        "a different group must be untouched"
    );
}

/// The card's second test, restated: a candidate with no twins folds
/// nothing, and the existing per-candidate path still materializes it.
#[tokio::test]
async fn folding_a_group_of_one_writes_nothing_and_the_existing_path_still_works() {
    let (_home, db) = open_state();
    let owner = uuid(64);
    let memory_id = uuid(65);
    let op = create_op_with_text(&memory_id, MemoryKind::Fact, &owner, "a lone claim");
    duplicate_group(&db, &["lonely"], &op, 1_000).await;

    let outcome = fold(&db, "lonely", 5_000).await.expect("fold");
    assert!(outcome.folded.is_empty());
    assert!(outcome.retained.is_empty());
    assert_eq!(outcome.evidence_linked, 0);

    let read = db.open_read().expect("read conn");
    assert_eq!(
        candidate_state_of(&read, "lonely"),
        CandidateState::Pending,
        "folding a group of one must not touch the candidate itself"
    );
    assert!(
        audit_rows_for_candidate(&read, "lonely").is_empty(),
        "no fold happened, so no audit row"
    );
    drop(read);

    let approved = approve(&db, "lonely", 6_000)
        .await
        .expect("still approvable");
    assert!(matches!(
        approved,
        ApproveCandidateOutcome::Materialized(MemoryOpOutcome::Applied(_))
    ));
}

#[tokio::test]
async fn folding_links_every_twins_evidence_onto_the_survivor_and_a_later_approve_carries_it() {
    let (_home, db) = open_state();
    let owner = uuid(74);
    let memory_id = uuid(75);
    let o1 = seed_observation(&db, 76, EvidenceKind::UserStatement, "sess-1").await;
    let o2 = seed_observation(&db, 77, EvidenceKind::UserStatement, "sess-1").await;
    let op = create_op_with_text(&memory_id, MemoryKind::Fact, &owner, "claim");
    duplicate_group(&db, &["survivor", "twin"], &op, 1_000).await;

    db.writer()
        .transaction({
            let survivor = "survivor".to_string();
            let o1 = o1.clone();
            move |tx| insert_candidate_evidence(tx, &survivor, &o1)
        })
        .await
        .expect("seed survivor evidence");
    db.writer()
        .transaction({
            let twin = "twin".to_string();
            let o1 = o1.clone();
            let o2 = o2.clone();
            move |tx| {
                insert_candidate_evidence(tx, &twin, &o1)?;
                insert_candidate_evidence(tx, &twin, &o2)
            }
        })
        .await
        .expect("seed twin evidence");

    let outcome = fold(&db, "survivor", 5_000).await.expect("fold");
    assert_eq!(
        outcome.evidence_linked, 1,
        "only o2 is new; o1 was already on the survivor"
    );

    let read = db.open_read().expect("read conn");
    let mut evidence = candidate_evidence_for(&read, "survivor").expect("evidence");
    evidence.sort();
    let mut expected = vec![o1, o2];
    expected.sort();
    assert_eq!(evidence, expected, "the survivor carries the union");
    drop(read);

    let approved = approve(&db, "survivor", 6_000).await.expect("materializes");
    let ApproveCandidateOutcome::Materialized(MemoryOpOutcome::Applied(_)) = approved else {
        panic!("expected Materialized(Applied), got {approved:?}");
    };
    let read = db.open_read().expect("read conn");
    let mut materialized = memory_evidence_for(&read, &memory_id).expect("evidence");
    materialized.sort();
    assert_eq!(
        materialized, expected,
        "the approved entry carries every observation the fold rescued"
    );
}

#[tokio::test]
async fn folding_writes_one_candidate_audit_row_per_twin_naming_the_survivor() {
    let (_home, db) = open_state();
    let owner = uuid(84);
    let op = create_op_with_text(&uuid(85), MemoryKind::Fact, &owner, "claim");
    duplicate_group(&db, &["survivor", "twin-a", "twin-b"], &op, 1_000).await;

    fold(&db, "survivor", 5_000).await.expect("fold");

    let read = db.open_read().expect("read conn");
    for twin in ["twin-a", "twin-b"] {
        let rows = audit_rows_for_candidate(&read, twin);
        assert_eq!(rows.len(), 1, "{twin}: {rows:?}");
        let row = &rows[0];
        assert_eq!(row.entity_kind, AUDIT_ENTITY_CANDIDATE);
        assert_eq!(row.entity_id, twin);
        assert_eq!(row.entity_version, 1);
        assert_eq!(row.op, AUDIT_OP_FOLD_DUPLICATE);
        assert_eq!(row.actor, Actor::User);
        assert_eq!(row.created_at, 5_000);
        let payload = row.payload.as_deref().expect("payload");
        assert!(
            payload.contains("\"survivor_candidate_id\":\"survivor\""),
            "{payload}"
        );
    }
    assert!(
        audit_rows_for_candidate(&read, "survivor").is_empty(),
        "the survivor is never audited — it was not acted on"
    );
}

/// `T23-11`: the one scenario that would have collided under the old
/// hard-coded `entity_version: 1` in [`fold_pending_duplicates`] — a twin
/// edited (and thus already audited) once while still `pending`, then later
/// folded. Proves the fold's retrofit onto [`next_candidate_audit_version`]
/// is load-bearing, not cosmetic: without it this would abort the fold's own
/// transaction with a `UNIQUE (entity_kind, entity_id, entity_version)`
/// violation instead of writing a clean second row.
#[tokio::test]
async fn an_edited_then_folded_candidate_accumulates_two_ordered_audit_rows() {
    let (_home, db) = open_state();
    let owner = uuid(111);
    let op = create_op_with_text(&uuid(112), MemoryKind::Fact, &owner, "claim");
    duplicate_group(&db, &["survivor", "twin-a"], &op, 1_000).await;

    // An edit that changes nothing about the proposal's identity (same op,
    // same dedup key) — it still counts as a real edit request and is still
    // audited, the same "the caller asked, regardless of old vs. new" rule
    // `apply_edit` already uses for a memory entry.
    edit(&db, "twin-a", Some(op.clone()), None, 2_000)
        .await
        .expect("edit twin-a while still pending");

    fold(&db, "survivor", 5_000).await.expect("fold");

    let read = db.open_read().expect("read conn");
    let events = audit_rows_for_candidate(&read, "twin-a");
    assert_eq!(events.len(), 2, "the edit and the fold, not a collision");
    assert_eq!(events[0].entity_version, 1);
    assert_eq!(events[0].op, "edit");
    assert_eq!(events[1].entity_version, 2);
    assert_eq!(events[1].op, AUDIT_OP_FOLD_DUPLICATE);
    assert_eq!(
        candidate_state_of(&read, "twin-a"),
        CandidateState::Rejected,
        "the edit did not change the dedup key, so the fold still finds it"
    );
}

/// The sharpest test in the set: an operator's own rejection and a
/// machine-driven fold both leave `review_state = 'rejected'`, and — since
/// `T23-11` gave `reject_candidate` its own audit row too — the only thing
/// that still tells them apart is the `op` each one carries, `"reject"`
/// versus `"fold_duplicate"`, never their mere presence.
#[tokio::test]
async fn an_operator_reject_and_a_fold_are_distinguishable_in_the_audit() {
    let (_home, db) = open_state();
    let owner = uuid(94);
    let judged_op = create_op_with_text(&uuid(95), MemoryKind::Fact, &owner, "judged claim");
    propose(&db, "judged", judged_op, vec![], vec![], 1_000).await;
    reject(&db, "judged", 1_000).await.expect("operator reject");

    let folded_op = create_op_with_text(&uuid(96), MemoryKind::Fact, &owner, "folded claim");
    duplicate_group(&db, &["survivor", "folded"], &folded_op, 2_000).await;
    fold(&db, "survivor", 5_000).await.expect("fold");

    let read = db.open_read().expect("read conn");
    assert_eq!(
        candidate_state_of(&read, "judged"),
        CandidateState::Rejected
    );
    assert_eq!(
        candidate_state_of(&read, "folded"),
        CandidateState::Rejected
    );
    let judged_events = audit_rows_for_candidate(&read, "judged");
    assert_eq!(
        judged_events.len(),
        1,
        "an operator's own reject now writes exactly one audit row"
    );
    assert_eq!(judged_events[0].op, "reject", "not the fold's own op");
    let folded_events = audit_rows_for_candidate(&read, "folded");
    assert_eq!(
        folded_events.len(),
        1,
        "a fold writes exactly one, naming the survivor it was folded into"
    );
    assert_eq!(
        folded_events[0].op, AUDIT_OP_FOLD_DUPLICATE,
        "not an operator's own reject"
    );
}

/// `pending_candidate_groups`'s own survivor choice — the one `T23-08`'s CLI
/// `dedup --all` and the "oldest wins" claim rest on. Distinct from
/// [`fold_pending_duplicates`] itself, which takes the survivor as an
/// explicit parameter and honors whichever member the caller names (the
/// CLI's `--candidate <id>` mode: "any member of the group, not necessarily
/// the group's oldest") — proven below by folding around a *non*-oldest,
/// deliberately caller-named survivor.
#[tokio::test]
async fn the_survivor_is_the_oldest_member_deterministically_with_id_tiebreak() {
    let (_home, db) = open_state();
    let owner = uuid(104);
    let op = create_op_with_text(&uuid(105), MemoryKind::Fact, &owner, "claim");
    // Same `created_at`: the tie-break falls to `candidate_id` ordering.
    let json = serde_json::to_string(&op).expect("op serializes");
    for id in ["cand-z", "cand-a", "cand-m"] {
        let (cid, json) = (id.to_string(), json.clone());
        db.writer()
            .transaction(move |tx| {
                create_candidate(
                    tx,
                    &NewCandidate {
                        candidate_id: &cid,
                        proposed_operation: &json,
                        conflicts: None,
                    },
                    1_000,
                )
            })
            .await
            .expect("seed");
    }

    let read = db.open_read().expect("read conn");
    let groups = pending_candidate_groups(&read).expect("groups");
    assert_eq!(groups.len(), 1, "{groups:?}");
    assert_eq!(
        groups[0].survivor.candidate_id, "cand-a",
        "the earliest created_at wins; a tie falls to candidate_id order"
    );
    drop(read);

    // `fold_pending_duplicates` itself does not second-guess the caller:
    // naming "cand-z" (not the group's oldest) still folds everyone else,
    // "cand-a" included, into "cand-z".
    let outcome = fold(&db, "cand-z", 5_000).await.expect("fold");
    let mut folded = outcome.folded.clone();
    folded.sort();
    assert_eq!(
        folded,
        vec!["cand-a".to_string(), "cand-m".to_string()],
        "the caller's chosen survivor is honored, not overridden"
    );
    let read = db.open_read().expect("read conn");
    assert_eq!(candidate_state_of(&read, "cand-z"), CandidateState::Pending);
    assert_eq!(
        candidate_state_of(&read, "cand-a"),
        CandidateState::Rejected
    );
    assert_eq!(
        candidate_state_of(&read, "cand-m"),
        CandidateState::Rejected
    );
}

#[tokio::test]
async fn a_twin_with_unparsable_proposed_operation_is_retained_not_folded() {
    let (_home, db) = open_state();
    let owner = uuid(114);
    let op = create_op_with_text(&uuid(115), MemoryKind::Fact, &owner, "claim");
    duplicate_group(&db, &["survivor"], &op, 1_000).await;
    // A twin whose `proposed_operation` is well-formed JSON (so the coarse
    // SQL narrowing's own `json_extract` calls, which run over every row the
    // scan visits, do not themselves error) but does not deserialize as
    // `ProposedOperation` (missing required fields) — a future format
    // neither this key version nor this enum recognizes, per the module
    // doc's own "row this binary cannot deserialize" wording. Genuinely
    // malformed JSON syntax is a different, SQL-layer failure this function
    // does not classify at all (`json_extract` itself errors before Rust
    // ever sees the row) — out of scope here, and already true of
    // `propose_candidate`'s own narrowing query since `T23-07`.
    let corrupt_json =
        format!("{{\"op\":\"create\",\"scope_kind\":\"worktree\",\"scope_owner_id\":\"{owner}\"}}");
    db.writer()
        .transaction(move |tx| {
            create_candidate(
                tx,
                &NewCandidate {
                    candidate_id: "corrupt-twin",
                    proposed_operation: &corrupt_json,
                    conflicts: None,
                },
                1_001,
            )
        })
        .await
        .expect("seed corrupt twin");

    let outcome = fold(&db, "survivor", 5_000).await.expect("fold");
    assert!(outcome.folded.is_empty());
    assert_eq!(
        outcome.retained,
        vec![("corrupt-twin".to_string(), FoldRetained::UnparsableProposal)]
    );
    let read = db.open_read().expect("read conn");
    assert_eq!(
        candidate_state_of(&read, "corrupt-twin"),
        CandidateState::Pending,
        "an unparsable row is retained, never folded"
    );
}

#[tokio::test]
async fn a_twin_whose_conflicts_differ_is_retained_not_folded() {
    let (_home, db) = open_state();
    let owner = uuid(124);
    let op = create_op_with_text(&uuid(125), MemoryKind::Fact, &owner, "claim");
    let json = serde_json::to_string(&op).expect("op serializes");
    db.writer()
        .transaction({
            let json = json.clone();
            move |tx| {
                create_candidate(
                    tx,
                    &NewCandidate {
                        candidate_id: "survivor",
                        proposed_operation: &json,
                        conflicts: None,
                    },
                    1_000,
                )
            }
        })
        .await
        .expect("seed survivor");
    db.writer()
        .transaction(move |tx| {
            create_candidate(
                tx,
                &NewCandidate {
                    candidate_id: "twin-conflicted",
                    proposed_operation: &json,
                    conflicts: Some("[\"some-other-memory\"]"),
                },
                1_001,
            )
        })
        .await
        .expect("seed conflicted twin");

    let outcome = fold(&db, "survivor", 5_000).await.expect("fold");
    assert!(outcome.folded.is_empty());
    assert_eq!(
        outcome.retained,
        vec![("twin-conflicted".to_string(), FoldRetained::ConflictsDiffer)]
    );
    let read = db.open_read().expect("read conn");
    assert_eq!(
        candidate_state_of(&read, "twin-conflicted"),
        CandidateState::Pending,
        "merging conflict sets is a judgement call this layer does not make"
    );
}

#[tokio::test]
async fn folding_an_unknown_candidate_is_typed_unknown_candidate() {
    let (_home, db) = open_state();
    let outcome = fold(&db, "no-such-id", 1_000).await;
    assert_eq!(outcome, Err(ReviewError::UnknownCandidate));
}

#[tokio::test]
async fn folding_a_non_pending_candidate_is_typed_not_pending() {
    let (_home, db) = open_state();
    let owner = uuid(134);
    let op = create_op_with_text(&uuid(135), MemoryKind::Fact, &owner, "claim");
    duplicate_group(&db, &["already-rejected"], &op, 1_000).await;
    reject(&db, "already-rejected", 1_000)
        .await
        .expect("reject it first");

    let outcome = fold(&db, "already-rejected", 5_000).await;
    assert_eq!(outcome, Err(ReviewError::NotPending));
}

#[tokio::test]
async fn folding_a_candidate_with_corrupt_json_is_typed_invalid_proposed_operation() {
    let (_home, db) = open_state();
    db.writer()
        .transaction(move |tx| {
            create_candidate(
                tx,
                &NewCandidate {
                    candidate_id: "corrupt-survivor",
                    proposed_operation: "not json at all",
                    conflicts: None,
                },
                1_000,
            )
        })
        .await
        .expect("seed corrupt survivor");

    let outcome = fold(&db, "corrupt-survivor", 5_000).await;
    assert!(
        matches!(outcome, Err(ReviewError::InvalidProposedOperation(_))),
        "{outcome:?}"
    );
}

/// Pins every op shape's own key form (`local_rag_store::memory::dedup`):
/// `reinforce`/`resolve`/`retract` on one `memory_id` are three distinct
/// groups (the op tag is part of the key), and two `supersede`s of one
/// `old_memory_id` proposing different `new_text` are two distinct groups.
#[tokio::test]
async fn non_create_op_shapes_group_by_their_own_key_forms() {
    let (_home, db) = open_state();
    let target = uuid(144);

    let reinforce = ProposedOperation::Reinforce {
        memory_id: target.clone(),
        expected_version: 1,
        confidence: Some(0.9),
    };
    let resolve = ProposedOperation::Resolve {
        memory_id: target.clone(),
        expected_version: 1,
    };
    let retract = ProposedOperation::Retract {
        memory_id: target.clone(),
        expected_version: 1,
    };
    duplicate_group(&db, &["r-1"], &reinforce, 1_000).await;
    duplicate_group(&db, &["r-2"], &resolve, 1_000).await;
    duplicate_group(&db, &["r-3"], &retract, 1_000).await;

    let old_id = uuid(145);
    let supersede_a = ProposedOperation::Supersede {
        old_memory_id: old_id.clone(),
        old_expected_version: 1,
        new_memory_id: uuid(146),
        new_kind: MemoryKind::Fact.as_str().to_string(),
        new_text: "replacement a".to_string(),
        new_canonical_key: None,
        new_scope_kind: ScopeKind::Worktree.as_str().to_string(),
        new_scope_owner_id: "owner".to_string(),
        new_confidence: 0.5,
        new_importance: 0.5,
        new_valid_from_tree: None,
        new_last_verified_tree: None,
    };
    let supersede_b = ProposedOperation::Supersede {
        old_memory_id: old_id.clone(),
        old_expected_version: 1,
        new_memory_id: uuid(147),
        new_kind: MemoryKind::Fact.as_str().to_string(),
        new_text: "replacement b".to_string(),
        new_canonical_key: None,
        new_scope_kind: ScopeKind::Worktree.as_str().to_string(),
        new_scope_owner_id: "owner".to_string(),
        new_confidence: 0.5,
        new_importance: 0.5,
        new_valid_from_tree: None,
        new_last_verified_tree: None,
    };
    duplicate_group(&db, &["s-a"], &supersede_a, 1_000).await;
    duplicate_group(&db, &["s-b"], &supersede_b, 1_000).await;

    let read = db.open_read().expect("read conn");
    let groups = pending_candidate_groups(&read).expect("groups");
    assert_eq!(
        groups.len(),
        5,
        "five distinct claims, none merged: {groups:?}"
    );
    for id in ["r-1", "r-2", "r-3", "s-a", "s-b"] {
        let group = groups
            .iter()
            .find(|g| g.survivor.candidate_id == id)
            .unwrap_or_else(|| panic!("{id} missing from groups: {groups:?}"));
        assert!(group.duplicate_ids.is_empty(), "{id}: {group:?}");
    }
}

#[tokio::test]
async fn groups_never_include_a_non_pending_candidate() {
    let (_home, db) = open_state();
    let owner = uuid(154);
    let op = create_op_with_text(&uuid(155), MemoryKind::Fact, &owner, "claim");
    duplicate_group(
        &db,
        &["pending-a", "pending-b", "approved-c", "rejected-d"],
        &op,
        1_000,
    )
    .await;
    approve(&db, "approved-c", 2_000)
        .await
        .expect("approve one twin out of band");
    reject(&db, "rejected-d", 1_000)
        .await
        .expect("reject another");

    let read = db.open_read().expect("read conn");
    let groups = pending_candidate_groups(&read).expect("groups");
    assert_eq!(groups.len(), 1, "{groups:?}");
    let group = &groups[0];
    assert_eq!(group.survivor.candidate_id, "pending-a");
    assert_eq!(group.duplicate_ids, vec!["pending-b".to_string()]);

    let outcome = fold(&db, "pending-a", 5_000).await.expect("fold");
    assert_eq!(outcome.folded, vec!["pending-b".to_string()]);
}

#[tokio::test]
async fn pending_candidate_groups_is_deterministic_and_skips_unparsable_rows() {
    let (_home, db) = open_state();
    let owner = uuid(164);
    let claim_a = create_op_with_text(&uuid(165), MemoryKind::Fact, &owner, "claim a");
    let claim_b = create_op_with_text(&uuid(166), MemoryKind::Fact, &owner, "claim b");
    duplicate_group(&db, &["a-1", "a-2"], &claim_a, 1_000).await;
    duplicate_group(&db, &["b-1"], &claim_b, 2_000).await;
    db.writer()
        .transaction(move |tx| {
            create_candidate(
                tx,
                &NewCandidate {
                    candidate_id: "unparsable",
                    proposed_operation: "not json",
                    conflicts: None,
                },
                3_000,
            )
        })
        .await
        .expect("seed unparsable row");

    let read = db.open_read().expect("read conn");
    let first = pending_candidate_groups(&read).expect("groups");
    let second = pending_candidate_groups(&read).expect("groups again");
    assert_eq!(first, second, "deterministic across repeated reads");
    assert_eq!(
        first.len(),
        2,
        "the unparsable row forms no group: {first:?}"
    );
    assert!(
        first.iter().all(|g| g.survivor.candidate_id != "unparsable"
            && !g.duplicate_ids.contains(&"unparsable".to_string())),
        "{first:?}"
    );
}

fn candidate_state_of(conn: &Connection, candidate_id: &str) -> CandidateState {
    conn.query_row(
        "SELECT review_state FROM pending_memory_candidate WHERE candidate_id = ?1",
        params![candidate_id],
        |r| r.get::<_, String>(0),
    )
    .map(|raw| CandidateState::from_db(&raw).expect("valid review_state"))
    .expect("candidate exists")
}

// ---------------------------------------------------------------------------
// D-131: approving a duplicate create reinforces the entry it duplicates
// ---------------------------------------------------------------------------

/// Write a pending candidate the way rows were written before `T23-07`:
/// straight through `create_candidate`, with no duplicate check, since
/// `propose_candidate` now refuses exactly the rows these tests need.
async fn seed_unchecked_candidate(
    db: &StateDb,
    candidate_id: &str,
    op: &ProposedOperation,
    evidence_observation_ids: Vec<String>,
    now_ms: i64,
) {
    let (id, json) = (
        candidate_id.to_string(),
        serde_json::to_string(op).expect("serialize proposal"),
    );
    db.writer()
        .transaction(move |tx| {
            create_candidate(
                tx,
                &NewCandidate {
                    candidate_id: &id,
                    proposed_operation: &json,
                    conflicts: None,
                },
                now_ms,
            )?;
            for observation_id in &evidence_observation_ids {
                insert_candidate_evidence(tx, &id, observation_id)?;
            }
            Ok(())
        })
        .await
        .expect("seed unchecked candidate");
}

fn with_confidence(op: ProposedOperation, value: f64) -> ProposedOperation {
    let ProposedOperation::Create {
        memory_id,
        kind,
        text,
        canonical_key,
        scope_kind,
        scope_owner_id,
        importance,
        valid_from_tree,
        last_verified_tree,
        ..
    } = op
    else {
        panic!("with_confidence takes a create")
    };
    ProposedOperation::Create {
        memory_id,
        kind,
        text,
        canonical_key,
        scope_kind,
        scope_owner_id,
        confidence: value,
        importance,
        valid_from_tree,
        last_verified_tree,
    }
}

#[tokio::test]
async fn approving_a_create_whose_text_is_already_an_entry_reinforces_that_entry() {
    let (_home, db) = open_state();
    let owner = uuid(230);
    let first_id = uuid(231);
    let twin_id = uuid(232);
    let first_obs = seed_observation(&db, 233, EvidenceKind::TestResult, "sess-a").await;
    let twin_obs = seed_observation(&db, 234, EvidenceKind::TestResult, "sess-b").await;

    propose(
        &db,
        "cand-first",
        create_op(&first_id, MemoryKind::Fact, &owner),
        vec![],
        vec![first_obs.clone()],
        1_000,
    )
    .await;
    // The twin carries a different confidence, so the assertion below can
    // tell "left alone" from "overwritten with the candidate's number".
    seed_unchecked_candidate(
        &db,
        "cand-twin",
        &with_confidence(create_op(&twin_id, MemoryKind::Fact, &owner), 0.9),
        vec![twin_obs.clone()],
        1_001,
    )
    .await;

    approve(&db, "cand-first", 2_000)
        .await
        .expect("approve first");
    let outcome = approve(&db, "cand-twin", 3_000)
        .await
        .expect("approve twin");
    let ApproveCandidateOutcome::Materialized(MemoryOpOutcome::Applied(result)) = outcome else {
        panic!("expected Materialized(Applied), got {outcome:?}");
    };
    assert_eq!(
        result.memory_id, first_id,
        "the twin names the entry it duplicates"
    );
    assert_eq!(result.entry_version, 2, "a reinforce bumps the version");

    let read = db.open_read().expect("read conn");
    assert_eq!(row_count(&read, "memory_entry"), 1, "no second copy");
    assert_eq!(memory_entry_state(&read, &twin_id).expect("state"), None);
    let entry = memory_entry_by_id(&read, &first_id)
        .expect("read entry")
        .expect("entry exists");
    assert_eq!(entry.confidence, 0.5, "confidence is left alone");
    let mut evidence = memory_evidence_for(&read, &first_id).expect("evidence");
    evidence.sort();
    let mut expected = vec![first_obs, twin_obs];
    expected.sort();
    assert_eq!(evidence, expected, "the twin's evidence is kept");
    assert_eq!(
        candidate_state_of(&read, "cand-first"),
        CandidateState::Approved
    );
    assert_eq!(
        candidate_state_of(&read, "cand-twin"),
        CandidateState::Approved
    );

    let again = approve(&db, "cand-twin", 4_000).await.expect("re-approve");
    assert_eq!(again, ApproveCandidateOutcome::AlreadyApproved);
    let read = db.open_read().expect("read conn");
    let entry = memory_entry_by_id(&read, &first_id)
        .expect("read entry")
        .expect("entry exists");
    assert_eq!(
        entry.entry_version, 2,
        "a replayed approval changes nothing"
    );
}

#[tokio::test]
async fn the_same_text_in_another_scope_is_still_a_new_entry() {
    let (_home, db) = open_state();
    let (owner_a, owner_b) = (uuid(240), uuid(241));
    let (id_a, id_b) = (uuid(242), uuid(243));

    propose(
        &db,
        "cand-a",
        create_op(&id_a, MemoryKind::Fact, &owner_a),
        vec![],
        vec![],
        1_000,
    )
    .await;
    propose(
        &db,
        "cand-b",
        create_op(&id_b, MemoryKind::Fact, &owner_b),
        vec![],
        vec![],
        1_001,
    )
    .await;

    approve(&db, "cand-a", 2_000).await.expect("approve a");
    let outcome = approve(&db, "cand-b", 3_000).await.expect("approve b");
    let ApproveCandidateOutcome::Materialized(MemoryOpOutcome::Applied(result)) = outcome else {
        panic!("expected Materialized(Applied), got {outcome:?}");
    };
    assert_eq!(result.memory_id, id_b);
    assert_eq!(result.entry_version, 1);
    let read = db.open_read().expect("read conn");
    assert_eq!(row_count(&read, "memory_entry"), 2);
}
