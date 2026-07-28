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
    ApproveCandidateOutcome, CandidateRow, ProposedOperation, ReviewError, approve_candidate,
    candidate_evidence_for, edit_candidate, list_candidates, memory_entry_state,
    memory_evidence_for, propose_candidate, reject_candidate,
};
use local_rag_store::rusqlite::{Connection, params};
use local_rag_store::{
    CandidateState, EvidenceKind, IllegalCandidateTransition, MemoryKind, MemoryOpOutcome,
    MemoryState, ScopeKind, StateDb,
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
) {
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

async fn reject(db: &StateDb, candidate_id: &str) -> Result<(), ReviewError> {
    let id = candidate_id.to_string();
    db.writer()
        .transaction(move |tx| reject_candidate(tx, &id))
        .await
        .expect("reject tx (infrastructure)")
}

async fn edit(
    db: &StateDb,
    candidate_id: &str,
    new_op: Option<ProposedOperation>,
    new_conflicts: Option<Vec<String>>,
) -> Result<(), ReviewError> {
    let id = candidate_id.to_string();
    db.writer()
        .transaction(move |tx| {
            let conflict_refs: Option<Vec<&str>> = new_conflicts
                .as_ref()
                .map(|c| c.iter().map(String::as_str).collect());
            edit_candidate(tx, &id, new_op.as_ref(), conflict_refs.as_deref())
        })
        .await
        .expect("edit tx (infrastructure)")
}

fn create_op(memory_id: &str, kind: MemoryKind, scope_owner_id: &str) -> ProposedOperation {
    ProposedOperation::Create {
        memory_id: memory_id.to_string(),
        kind: kind.as_str().to_string(),
        text: "candidate-proposed text".to_string(),
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
    let rows = list_candidates(&read, None).expect("list");
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
        create_op(&uuid(11), MemoryKind::Fact, &owner),
        vec![],
        vec![],
        1_000,
    )
    .await;
    propose(
        &db,
        "cand-b",
        create_op(&uuid(12), MemoryKind::Fact, &owner),
        vec![],
        vec![],
        1_100,
    )
    .await;
    reject(&db, "cand-b").await.expect("reject");

    let read = db.open_read().expect("read conn");
    let pending = list_candidates(&read, Some(CandidateState::Pending)).expect("list pending");
    assert_eq!(
        pending
            .iter()
            .map(|r| r.candidate_id.as_str())
            .collect::<Vec<_>>(),
        vec!["cand-a"],
    );
    let rejected = list_candidates(&read, Some(CandidateState::Rejected)).expect("list rejected");
    assert_eq!(
        rejected
            .iter()
            .map(|r| r.candidate_id.as_str())
            .collect::<Vec<_>>(),
        vec!["cand-b"],
    );
    let all = list_candidates(&read, None).expect("list all");
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

    reject(&db, "cand-1").await.expect("reject");

    let read = db.open_read().expect("read conn");
    assert_eq!(
        row_count(&read, "memory_entry"),
        0,
        "rejected never materializes"
    );
    assert_eq!(memory_entry_state(&read, &memory_id).expect("state"), None,);
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
    )
    .await
    .expect("edit");

    let read = db.open_read().expect("read conn");
    let rows = list_candidates(&read, None).expect("list");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].conflicts.as_deref(), Some("[\"conflict-1\"]"));
    let round_tripped: ProposedOperation =
        serde_json::from_str(&rows[0].proposed_operation).expect("parse");
    assert_eq!(round_tripped, new_op);
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
    reject(&db, "cand-1").await.expect("reject");

    let attempted_op = create_op(&uuid(102), MemoryKind::Fact, &owner);
    let result = edit(&db, "cand-1", Some(attempted_op), None).await;
    assert_eq!(result, Err(ReviewError::NotPending));

    let read = db.open_read().expect("read conn");
    let rows = list_candidates(&read, None).expect("list");
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
    let result = reject(&db, "does-not-exist").await;
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
    reject(&db, "cand-1").await.expect("reject");

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
