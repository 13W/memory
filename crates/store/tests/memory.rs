//! T14-01 acceptance tests for the durable-memory schema (spec 03 §2.5, 04
//! §4-6): every legal/illegal transition for all three state machines
//! (`memory_entry`, `pending_memory_candidate`, `consolidation_run`),
//! scope/canonical uniqueness including the global singleton, terminal-state
//! exclusion, hypothesis-confirm-vs-fact-supersede, and FK/uniqueness
//! constraints over `memory_evidence`/`candidate_evidence`/`audit_event`.
//!
//! Pure state-machine coverage (the full `check_transition` matrix per
//! machine, corrupt-enum reads) lives in the `memory::{entry,candidate,
//! consolidation}` modules' own unit tests; these exercise the DB operations,
//! the schema constraints, and round-trips.
//!
//! All tests are deterministic: an isolated [`TempHome`], fixed `now_ms`
//! literals, and ids minted from [`uuidv7_from`] with fixed entropy.

use local_rag_core::identity::uuidv7_from;
use local_rag_core::paths::StoreLayout;
use local_rag_store::memory::{
    Actor, CandidateState, CandidateTransitionError, CreateMemoryEntryError, GLOBAL_SCOPE_OWNER_ID,
    IllegalCandidateTransition, IllegalMemoryTransition, IllegalRunTransition, MemoryCountRow,
    MemoryKind, MemoryState, MemoryTransitionError, NewAuditEvent, NewCandidate,
    NewConsolidationRun, NewMemoryEntry, NewMemoryEvidence, RunState, RunTransitionError,
    ScopeKind, active_entries_for_scope, candidate_state, canonical_key_owner,
    consolidation_run_state, create_candidate, create_consolidation_run, create_memory_entry,
    insert_audit_event, insert_candidate_evidence, insert_memory_evidence,
    list_memory_entries_for_scope, memory_entry_counts, memory_entry_state, memory_entry_summary,
    memory_evidence_for, processing_cursor, read_audit_events_for_entity,
    recall_candidates_for_scope, transition_candidate, transition_memory_entry, transition_run,
    upsert_processing_cursor,
};
use local_rag_store::{StateDb, WriteError};
use local_rag_test_support::TempHome;

/// A temporary store with an ensured tree and an opened [`StateDb`] (runs the
/// full production migration set, v1..v9).
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

/// Insert a minimal, standalone `observation_envelope` row (no repo/worktree,
/// no payload) so FK-target tests have a real `observation_id` to point at.
/// `memory_evidence`/`candidate_evidence` only FK against the envelope, not
/// the payload, so this is sufficient fixture depth for this task's tests.
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

/// Create a `memory_entry` row and unwrap the outer (infrastructure) result,
/// returning the inner domain result.
#[allow(clippy::too_many_arguments)]
async fn create_memory(
    db: &StateDb,
    memory_id: &str,
    kind: MemoryKind,
    scope_kind: ScopeKind,
    scope_owner_id: &str,
    canonical_key: Option<&str>,
) -> Result<(), CreateMemoryEntryError> {
    let (id, owner, key) = (
        memory_id.to_string(),
        scope_owner_id.to_string(),
        canonical_key.map(str::to_string),
    );
    db.writer()
        .transaction(move |tx| {
            create_memory_entry(
                tx,
                &NewMemoryEntry {
                    memory_id: &id,
                    kind,
                    text: "some durable text",
                    canonical_key: key.as_deref(),
                    scope_kind,
                    scope_owner_id: &owner,
                    confidence: 0.5,
                    importance: 0.5,
                    valid_from_tree: None,
                    last_verified_tree: None,
                    supersedes_id: None,
                },
                1000,
            )
        })
        .await
        .expect("create memory tx (infrastructure)")
}

/// Convenience: a worktree-scoped memory entry with no canonical key, created
/// and expected to succeed.
async fn memory(db: &StateDb, seed: u8, kind: MemoryKind) -> String {
    let id = uuid(seed);
    let scope_owner = uuid(seed.wrapping_add(50));
    assert_eq!(
        create_memory(db, &id, kind, ScopeKind::Worktree, &scope_owner, None).await,
        Ok(())
    );
    id
}

/// Transition a `memory_entry` and unwrap the outer (infrastructure) result.
async fn transition_entry(
    db: &StateDb,
    memory_id: &str,
    to: MemoryState,
) -> Result<(), MemoryTransitionError> {
    let id = memory_id.to_string();
    db.writer()
        .transaction(move |tx| transition_memory_entry(tx, &id, to))
        .await
        .expect("transition tx (infrastructure)")
}

// ---------------------------------------------------------------------------
// memory_entry: happy paths per kind group
// ---------------------------------------------------------------------------

#[tokio::test]
async fn happy_path_task_and_question() {
    let (_home, db) = open_state();

    let task = memory(&db, 1, MemoryKind::Task).await;
    assert_eq!(
        transition_entry(&db, &task, MemoryState::Resolved).await,
        Ok(())
    );
    let read = db.open_read().expect("read conn");
    assert_eq!(
        memory_entry_state(&read, &task).expect("state"),
        Some((MemoryKind::Task, MemoryState::Resolved)),
    );
    drop(read);

    let question = memory(&db, 2, MemoryKind::Question).await;
    assert_eq!(
        transition_entry(&db, &question, MemoryState::Retracted).await,
        Ok(())
    );
    let read = db.open_read().expect("read conn");
    assert_eq!(
        memory_entry_state(&read, &question).expect("state"),
        Some((MemoryKind::Question, MemoryState::Retracted)),
    );
}

#[tokio::test]
async fn happy_path_hypothesis_all_three_edges() {
    let (_home, db) = open_state();

    let confirmed = memory(&db, 10, MemoryKind::Hypothesis).await;
    assert_eq!(
        transition_entry(&db, &confirmed, MemoryState::Confirmed).await,
        Ok(())
    );

    let rejected = memory(&db, 11, MemoryKind::Hypothesis).await;
    assert_eq!(
        transition_entry(&db, &rejected, MemoryState::Rejected).await,
        Ok(())
    );

    let superseded = memory(&db, 12, MemoryKind::Hypothesis).await;
    assert_eq!(
        transition_entry(&db, &superseded, MemoryState::Superseded).await,
        Ok(())
    );

    let read = db.open_read().expect("read conn");
    assert_eq!(
        memory_entry_state(&read, &confirmed).expect("state"),
        Some((MemoryKind::Hypothesis, MemoryState::Confirmed)),
    );
    assert_eq!(
        memory_entry_state(&read, &rejected).expect("state"),
        Some((MemoryKind::Hypothesis, MemoryState::Rejected)),
    );
    assert_eq!(
        memory_entry_state(&read, &superseded).expect("state"),
        Some((MemoryKind::Hypothesis, MemoryState::Superseded)),
    );
}

#[tokio::test]
async fn happy_path_fact_decision_convention_procedure() {
    let (_home, db) = open_state();

    let fact = memory(&db, 20, MemoryKind::Fact).await;
    assert_eq!(
        transition_entry(&db, &fact, MemoryState::Superseded).await,
        Ok(())
    );

    let decision = memory(&db, 21, MemoryKind::Decision).await;
    assert_eq!(
        transition_entry(&db, &decision, MemoryState::Retracted).await,
        Ok(())
    );

    let convention = memory(&db, 22, MemoryKind::Convention).await;
    assert_eq!(
        transition_entry(&db, &convention, MemoryState::Superseded).await,
        Ok(())
    );

    let procedure = memory(&db, 23, MemoryKind::Procedure).await;
    assert_eq!(
        transition_entry(&db, &procedure, MemoryState::Retracted).await,
        Ok(())
    );

    let read = db.open_read().expect("read conn");
    for (id, kind, expected) in [
        (&fact, MemoryKind::Fact, MemoryState::Superseded),
        (&decision, MemoryKind::Decision, MemoryState::Retracted),
        (&convention, MemoryKind::Convention, MemoryState::Superseded),
        (&procedure, MemoryKind::Procedure, MemoryState::Retracted),
    ] {
        assert_eq!(
            memory_entry_state(&read, id).expect("state"),
            Some((kind, expected)),
        );
    }
}

// ---------------------------------------------------------------------------
// memory_entry: illegal / unknown / terminal / hypothesis-vs-fact
// ---------------------------------------------------------------------------

#[tokio::test]
async fn illegal_memory_transition_is_typed_error_and_rolls_back() {
    let (_home, db) = open_state();
    let fact = memory(&db, 30, MemoryKind::Fact).await;

    // `confirmed` is not in `fact`'s machine at all (spec 04 §5) — legal only
    // for `hypothesis`.
    assert_eq!(
        transition_entry(&db, &fact, MemoryState::Confirmed).await,
        Err(MemoryTransitionError::Illegal(IllegalMemoryTransition {
            kind: MemoryKind::Fact,
            from: MemoryState::Active,
            to: MemoryState::Confirmed,
        })),
    );

    let read = db.open_read().expect("read conn");
    assert_eq!(
        memory_entry_state(&read, &fact).expect("state"),
        Some((MemoryKind::Fact, MemoryState::Active)),
        "state unchanged after the rejected transition",
    );
}

#[tokio::test]
async fn unknown_memory_transition_is_typed_error() {
    let (_home, db) = open_state();
    let ghost = uuid(40); // never created
    assert_eq!(
        transition_entry(&db, &ghost, MemoryState::Resolved).await,
        Err(MemoryTransitionError::UnknownMemory),
    );
}

/// The group card's explicit test: `hypothesis: active → confirmed` is legal
/// and `confirmed` is NOT terminal (spec 04 §5, 08 §6 — stays recall-eligible
/// as high-trust); the analogous `fact: active → confirmed` is illegal, and
/// promoting a fact instead moves through `superseded` (terminal, excluded
/// from default recall).
#[tokio::test]
async fn hypothesis_confirm_vs_fact_supersede() {
    let (_home, db) = open_state();

    let hyp = memory(&db, 50, MemoryKind::Hypothesis).await;
    assert_eq!(
        transition_entry(&db, &hyp, MemoryState::Confirmed).await,
        Ok(())
    );

    let fact = memory(&db, 51, MemoryKind::Fact).await;
    assert_eq!(
        transition_entry(&db, &fact, MemoryState::Confirmed).await,
        Err(MemoryTransitionError::Illegal(IllegalMemoryTransition {
            kind: MemoryKind::Fact,
            from: MemoryState::Active,
            to: MemoryState::Confirmed,
        })),
    );
    assert_eq!(
        transition_entry(&db, &fact, MemoryState::Superseded).await,
        Ok(())
    );

    let read = db.open_read().expect("read conn");
    let (_, hyp_state) = memory_entry_state(&read, &hyp).expect("state").unwrap();
    let (_, fact_state) = memory_entry_state(&read, &fact).expect("state").unwrap();
    assert!(
        !hyp_state.is_terminal(),
        "confirmed hypothesis not terminal"
    );
    assert!(fact_state.is_terminal(), "superseded fact is terminal");
}

/// D-020 regression: spec 04 §5's own prose narrates promotion acting on an
/// *already-confirmed* hypothesis ("a confirmed hypothesis stays... promotion
/// to fact happens only via explicit supersede... which transitions to
/// superseded") — `confirmed → superseded` must be legal, not just
/// `active → superseded`.
#[tokio::test]
async fn hypothesis_confirmed_can_be_superseded() {
    let (_home, db) = open_state();

    let hyp = memory(&db, 52, MemoryKind::Hypothesis).await;
    assert_eq!(
        transition_entry(&db, &hyp, MemoryState::Confirmed).await,
        Ok(())
    );
    assert_eq!(
        transition_entry(&db, &hyp, MemoryState::Superseded).await,
        Ok(()),
        "a confirmed hypothesis must be promotable via supersede (D-020)"
    );

    let read = db.open_read().expect("read conn");
    assert_eq!(
        memory_entry_state(&read, &hyp).expect("state"),
        Some((MemoryKind::Hypothesis, MemoryState::Superseded)),
    );
}

#[tokio::test]
async fn terminal_states_excluded_from_recall_by_default() {
    let (_home, db) = open_state();

    let resolved = memory(&db, 60, MemoryKind::Task).await;
    transition_entry(&db, &resolved, MemoryState::Resolved)
        .await
        .expect("resolve");
    let retracted = memory(&db, 61, MemoryKind::Question).await;
    transition_entry(&db, &retracted, MemoryState::Retracted)
        .await
        .expect("retract");
    let rejected = memory(&db, 62, MemoryKind::Hypothesis).await;
    transition_entry(&db, &rejected, MemoryState::Rejected)
        .await
        .expect("reject");
    let superseded = memory(&db, 63, MemoryKind::Decision).await;
    transition_entry(&db, &superseded, MemoryState::Superseded)
        .await
        .expect("supersede");
    let active = memory(&db, 64, MemoryKind::Task).await;
    let confirmed = memory(&db, 65, MemoryKind::Hypothesis).await;
    transition_entry(&db, &confirmed, MemoryState::Confirmed)
        .await
        .expect("confirm");

    let read = db.open_read().expect("read conn");
    for (id, expect_terminal) in [
        (&resolved, true),
        (&retracted, true),
        (&rejected, true),
        (&superseded, true),
        (&active, false),
        (&confirmed, false),
    ] {
        let (_, state) = memory_entry_state(&read, id).expect("state").unwrap();
        assert_eq!(
            state.is_terminal(),
            expect_terminal,
            "{id}: {state:?}.is_terminal()"
        );
    }
}

// ---------------------------------------------------------------------------
// memory_entry: recall_candidates_for_scope (T14-08)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn recall_candidates_for_scope_excludes_terminal_states() {
    let (_home, db) = open_state();
    let scope_owner = uuid(70);

    let active = uuid(71);
    create_memory(
        &db,
        &active,
        MemoryKind::Task,
        ScopeKind::Worktree,
        &scope_owner,
        None,
    )
    .await
    .expect("create active");

    let confirmed = uuid(72);
    create_memory(
        &db,
        &confirmed,
        MemoryKind::Hypothesis,
        ScopeKind::Worktree,
        &scope_owner,
        None,
    )
    .await
    .expect("create hypothesis");
    transition_entry(&db, &confirmed, MemoryState::Confirmed)
        .await
        .expect("confirm");

    let superseded = uuid(73);
    create_memory(
        &db,
        &superseded,
        MemoryKind::Decision,
        ScopeKind::Worktree,
        &scope_owner,
        None,
    )
    .await
    .expect("create decision");
    transition_entry(&db, &superseded, MemoryState::Superseded)
        .await
        .expect("supersede");

    let read = db.open_read().expect("read conn");
    let candidates = recall_candidates_for_scope(&read, ScopeKind::Worktree, &scope_owner)
        .expect("recall candidates");
    let ids: Vec<&str> = candidates.iter().map(|c| c.memory_id.as_str()).collect();

    assert!(ids.contains(&active.as_str()), "active must be eligible");
    assert!(
        ids.contains(&confirmed.as_str()),
        "confirmed hypothesis must be eligible"
    );
    assert!(
        !ids.contains(&superseded.as_str()),
        "superseded must be excluded"
    );
    assert_eq!(candidates.len(), 2);
}

#[tokio::test]
async fn recall_candidates_for_scope_isolates_by_scope() {
    let (_home, db) = open_state();
    let worktree_a = uuid(80);
    let worktree_b = uuid(81);

    let in_a = uuid(82);
    create_memory(
        &db,
        &in_a,
        MemoryKind::Fact,
        ScopeKind::Worktree,
        &worktree_a,
        None,
    )
    .await
    .expect("create in a");

    let in_b = uuid(83);
    create_memory(
        &db,
        &in_b,
        MemoryKind::Fact,
        ScopeKind::Worktree,
        &worktree_b,
        None,
    )
    .await
    .expect("create in b");

    let in_global = uuid(84);
    create_memory(
        &db,
        &in_global,
        MemoryKind::Fact,
        ScopeKind::Global,
        GLOBAL_SCOPE_OWNER_ID,
        None,
    )
    .await
    .expect("create global");

    let read = db.open_read().expect("read conn");

    let a = recall_candidates_for_scope(&read, ScopeKind::Worktree, &worktree_a)
        .expect("scope a")
        .into_iter()
        .map(|c| c.memory_id)
        .collect::<Vec<_>>();
    assert_eq!(a, vec![in_a.clone()], "worktree a sees only its own entry");

    let b = recall_candidates_for_scope(&read, ScopeKind::Worktree, &worktree_b)
        .expect("scope b")
        .into_iter()
        .map(|c| c.memory_id)
        .collect::<Vec<_>>();
    assert_eq!(b, vec![in_b], "worktree b sees only its own entry");

    let global = recall_candidates_for_scope(&read, ScopeKind::Global, GLOBAL_SCOPE_OWNER_ID)
        .expect("scope global")
        .into_iter()
        .map(|c| c.memory_id)
        .collect::<Vec<_>>();
    assert_eq!(
        global,
        vec![in_global],
        "global scope isolated from both worktrees"
    );
}

#[tokio::test]
async fn recall_candidates_for_scope_carries_confidence_and_created_at() {
    let (_home, db) = open_state();
    let scope_owner = uuid(90);
    let memory_id = uuid(91);
    let (id, owner) = (memory_id.clone(), scope_owner.clone());
    db.writer()
        .transaction(move |tx| {
            create_memory_entry(
                tx,
                &NewMemoryEntry {
                    memory_id: &id,
                    kind: MemoryKind::Fact,
                    text: "confidence and timestamp round trip",
                    canonical_key: None,
                    scope_kind: ScopeKind::Worktree,
                    scope_owner_id: &owner,
                    confidence: 0.73,
                    importance: 0.5,
                    valid_from_tree: None,
                    last_verified_tree: None,
                    supersedes_id: None,
                },
                4_242,
            )
        })
        .await
        .expect("create memory tx (infrastructure)")
        .expect("create memory (domain)");

    let read = db.open_read().expect("read conn");
    let candidates = recall_candidates_for_scope(&read, ScopeKind::Worktree, &scope_owner)
        .expect("recall candidates");
    assert_eq!(candidates.len(), 1);
    let candidate = &candidates[0];
    assert_eq!(candidate.memory_id, memory_id);
    assert_eq!(candidate.text, "confidence and timestamp round trip");
    assert!((candidate.confidence - 0.73).abs() < f64::EPSILON);
    assert_eq!(candidate.created_at, 4_242);
}

// ---------------------------------------------------------------------------
// memory_entry: scope/canonical uniqueness, including the global singleton
// ---------------------------------------------------------------------------

#[tokio::test]
async fn duplicate_canonical_key_same_scope_rejected() {
    let (_home, db) = open_state();
    let owner = uuid(70);

    assert_eq!(
        create_memory(
            &db,
            &uuid(71),
            MemoryKind::Fact,
            ScopeKind::Repository,
            &owner,
            Some("dup-key"),
        )
        .await,
        Ok(())
    );

    let (id2, owner2) = (uuid(72), owner.clone());
    let result = db
        .writer()
        .transaction(move |tx| {
            create_memory_entry(
                tx,
                &NewMemoryEntry {
                    memory_id: &id2,
                    kind: MemoryKind::Fact,
                    text: "second",
                    canonical_key: Some("dup-key"),
                    scope_kind: ScopeKind::Repository,
                    scope_owner_id: &owner2,
                    confidence: 0.5,
                    importance: 0.5,
                    valid_from_tree: None,
                    last_verified_tree: None,
                    supersedes_id: None,
                },
                1000,
            )
        })
        .await;
    assert!(
        matches!(result, Err(WriteError::Sqlite(_))),
        "duplicate canonical_key in the same scope must hit the UNIQUE index, got {result:?}",
    );
}

#[tokio::test]
async fn same_canonical_key_different_scope_allowed() {
    let (_home, db) = open_state();
    let (owner_a, owner_b) = (uuid(73), uuid(74));

    assert_eq!(
        create_memory(
            &db,
            &uuid(75),
            MemoryKind::Fact,
            ScopeKind::Repository,
            &owner_a,
            Some("shared-key"),
        )
        .await,
        Ok(())
    );
    assert_eq!(
        create_memory(
            &db,
            &uuid(76),
            MemoryKind::Fact,
            ScopeKind::Repository,
            &owner_b,
            Some("shared-key"),
        )
        .await,
        Ok(()),
        "a different scope_owner_id must not conflict",
    );
}

#[tokio::test]
async fn null_canonical_key_never_conflicts() {
    let (_home, db) = open_state();
    let owner = uuid(77);

    assert_eq!(
        create_memory(
            &db,
            &uuid(78),
            MemoryKind::Fact,
            ScopeKind::Repository,
            &owner,
            None
        )
        .await,
        Ok(())
    );
    assert_eq!(
        create_memory(
            &db,
            &uuid(79),
            MemoryKind::Fact,
            ScopeKind::Repository,
            &owner,
            None
        )
        .await,
        Ok(()),
        "two NULL canonical_key rows in the same scope must not conflict (partial index)",
    );
}

#[tokio::test]
async fn global_scope_requires_the_singleton_owner() {
    let (_home, db) = open_state();

    let wrong_owner = uuid(80);
    assert_eq!(
        create_memory(
            &db,
            &uuid(81),
            MemoryKind::Convention,
            ScopeKind::Global,
            &wrong_owner,
            None,
        )
        .await,
        Err(CreateMemoryEntryError::InvalidGlobalScopeOwner),
    );

    assert_eq!(
        create_memory(
            &db,
            &uuid(82),
            MemoryKind::Convention,
            ScopeKind::Global,
            GLOBAL_SCOPE_OWNER_ID,
            None,
        )
        .await,
        Ok(()),
        "the singleton owner id must be accepted",
    );

    // Nothing was written for the rejected attempt.
    let read = db.open_read().expect("read conn");
    assert_eq!(memory_entry_state(&read, &uuid(81)).expect("state"), None);
}

// ---------------------------------------------------------------------------
// memory_evidence / candidate_evidence: FK constraints
// ---------------------------------------------------------------------------

#[tokio::test]
async fn memory_evidence_unknown_memory_rejected() {
    let (_home, db) = open_state();
    let observation_id = seed_observation(&db, 90).await;
    let ghost_memory = uuid(91);

    let (m, o) = (ghost_memory, observation_id);
    let result = db
        .writer()
        .transaction(move |tx| {
            insert_memory_evidence(
                tx,
                &NewMemoryEvidence {
                    memory_id: &m,
                    observation_id: &o,
                    evidence_kind: local_rag_store::EvidenceKind::UserStatement,
                    session_id: "sess-1",
                    agent_id: None,
                    commit_hash: None,
                },
            )
        })
        .await;
    assert!(
        matches!(result, Err(WriteError::Sqlite(_))),
        "got {result:?}"
    );
}

#[tokio::test]
async fn memory_evidence_unknown_observation_rejected() {
    let (_home, db) = open_state();
    let memory_id = memory(&db, 92, MemoryKind::Fact).await;
    let ghost_observation = uuid(93);

    let (m, o) = (memory_id, ghost_observation);
    let result = db
        .writer()
        .transaction(move |tx| {
            insert_memory_evidence(
                tx,
                &NewMemoryEvidence {
                    memory_id: &m,
                    observation_id: &o,
                    evidence_kind: local_rag_store::EvidenceKind::UserStatement,
                    session_id: "sess-1",
                    agent_id: None,
                    commit_hash: None,
                },
            )
        })
        .await;
    assert!(
        matches!(result, Err(WriteError::Sqlite(_))),
        "got {result:?}"
    );
}

#[tokio::test]
async fn memory_evidence_valid_row_round_trips() {
    let (_home, db) = open_state();
    let memory_id = memory(&db, 94, MemoryKind::Fact).await;
    let observation_id = seed_observation(&db, 95).await;

    let (m, o) = (memory_id.clone(), observation_id.clone());
    db.writer()
        .transaction(move |tx| {
            insert_memory_evidence(
                tx,
                &NewMemoryEvidence {
                    memory_id: &m,
                    observation_id: &o,
                    evidence_kind: local_rag_store::EvidenceKind::ToolResult,
                    session_id: "sess-1",
                    agent_id: Some("agent-1"),
                    commit_hash: None,
                },
            )
        })
        .await
        .expect("insert evidence");

    let read = db.open_read().expect("read conn");
    assert_eq!(
        memory_evidence_for(&read, &memory_id).expect("evidence"),
        vec![observation_id],
    );
}

#[tokio::test]
async fn candidate_evidence_unknown_candidate_rejected() {
    let (_home, db) = open_state();
    let observation_id = seed_observation(&db, 96).await;
    let ghost_candidate = uuid(97);

    let (c, o) = (ghost_candidate, observation_id);
    let result = db
        .writer()
        .transaction(move |tx| insert_candidate_evidence(tx, &c, &o))
        .await;
    assert!(
        matches!(result, Err(WriteError::Sqlite(_))),
        "got {result:?}"
    );
}

#[tokio::test]
async fn candidate_evidence_unknown_observation_rejected() {
    let (_home, db) = open_state();
    let candidate_id = uuid(98);
    let (cid,) = (candidate_id.clone(),);
    db.writer()
        .transaction(move |tx| {
            create_candidate(
                tx,
                &NewCandidate {
                    candidate_id: &cid,
                    proposed_operation: "{}",
                    conflicts: None,
                },
                1000,
            )
        })
        .await
        .expect("create candidate");
    let ghost_observation = uuid(99);

    let (c, o) = (candidate_id, ghost_observation);
    let result = db
        .writer()
        .transaction(move |tx| insert_candidate_evidence(tx, &c, &o))
        .await;
    assert!(
        matches!(result, Err(WriteError::Sqlite(_))),
        "got {result:?}"
    );
}

// ---------------------------------------------------------------------------
// pending_memory_candidate: transitions
// ---------------------------------------------------------------------------

async fn candidate(db: &StateDb, seed: u8) -> String {
    let id = uuid(seed);
    let cid = id.clone();
    db.writer()
        .transaction(move |tx| {
            create_candidate(
                tx,
                &NewCandidate {
                    candidate_id: &cid,
                    proposed_operation: "{\"op\":\"create\"}",
                    conflicts: None,
                },
                1000,
            )
        })
        .await
        .expect("create candidate");
    id
}

async fn transition_cand(
    db: &StateDb,
    candidate_id: &str,
    to: CandidateState,
) -> Result<(), CandidateTransitionError> {
    let id = candidate_id.to_string();
    db.writer()
        .transaction(move |tx| transition_candidate(tx, &id, to))
        .await
        .expect("transition tx (infrastructure)")
}

#[tokio::test]
async fn candidate_happy_path_each_terminal() {
    let (_home, db) = open_state();

    let approved = candidate(&db, 100).await;
    assert_eq!(
        transition_cand(&db, &approved, CandidateState::Approved).await,
        Ok(())
    );
    let rejected = candidate(&db, 101).await;
    assert_eq!(
        transition_cand(&db, &rejected, CandidateState::Rejected).await,
        Ok(())
    );
    let expired = candidate(&db, 102).await;
    assert_eq!(
        transition_cand(&db, &expired, CandidateState::Expired).await,
        Ok(())
    );

    let read = db.open_read().expect("read conn");
    assert_eq!(
        candidate_state(&read, &approved).expect("state"),
        Some(CandidateState::Approved)
    );
    assert_eq!(
        candidate_state(&read, &rejected).expect("state"),
        Some(CandidateState::Rejected)
    );
    assert_eq!(
        candidate_state(&read, &expired).expect("state"),
        Some(CandidateState::Expired)
    );
}

#[tokio::test]
async fn candidate_illegal_transition_is_typed_error() {
    let (_home, db) = open_state();
    let id = candidate(&db, 103).await;
    transition_cand(&db, &id, CandidateState::Approved)
        .await
        .expect("approve");

    // Once approved, moving to rejected is illegal (terminal).
    assert_eq!(
        transition_cand(&db, &id, CandidateState::Rejected).await,
        Err(CandidateTransitionError::Illegal(
            IllegalCandidateTransition {
                from: CandidateState::Approved,
                to: CandidateState::Rejected,
            }
        )),
    );
}

#[tokio::test]
async fn candidate_unknown_transition_is_typed_error() {
    let (_home, db) = open_state();
    let ghost = uuid(104);
    assert_eq!(
        transition_cand(&db, &ghost, CandidateState::Approved).await,
        Err(CandidateTransitionError::UnknownCandidate),
    );
}

// ---------------------------------------------------------------------------
// consolidation_run: transitions
// ---------------------------------------------------------------------------

async fn run(db: &StateDb, seed: u8) -> String {
    let id = uuid(seed);
    let (rid,) = (id.clone(),);
    db.writer()
        .transaction(move |tx| {
            create_consolidation_run(
                tx,
                &NewConsolidationRun {
                    run_id: &rid,
                    session_id: "sess-1",
                    from_received_seq: 1,
                    to_received_seq: 10,
                    router_version: "v1",
                },
                1000,
            )
        })
        .await
        .expect("create run");
    id
}

async fn transition_r(db: &StateDb, run_id: &str, to: RunState) -> Result<(), RunTransitionError> {
    let id = run_id.to_string();
    db.writer()
        .transaction(move |tx| transition_run(tx, &id, to, 2000))
        .await
        .expect("transition tx (infrastructure)")
}

#[tokio::test]
async fn run_happy_path_pending_running_applied() {
    let (_home, db) = open_state();
    let id = run(&db, 110).await;

    assert_eq!(transition_r(&db, &id, RunState::Running).await, Ok(()));
    assert_eq!(transition_r(&db, &id, RunState::Applied).await, Ok(()));

    let read = db.open_read().expect("read conn");
    assert_eq!(
        consolidation_run_state(&read, &id).expect("state"),
        Some(RunState::Applied)
    );
}

#[tokio::test]
async fn run_failed_then_retried_to_running() {
    let (_home, db) = open_state();
    let id = run(&db, 111).await;

    assert_eq!(transition_r(&db, &id, RunState::Running).await, Ok(()));
    assert_eq!(transition_r(&db, &id, RunState::Failed).await, Ok(()));
    // As-built (spec 04 §4 "(retryable)"): a failed run re-enters running
    // under the same run_id.
    assert_eq!(transition_r(&db, &id, RunState::Running).await, Ok(()));

    let read = db.open_read().expect("read conn");
    assert_eq!(
        consolidation_run_state(&read, &id).expect("state"),
        Some(RunState::Running)
    );
}

#[tokio::test]
async fn run_illegal_transition_is_typed_error() {
    let (_home, db) = open_state();
    let id = run(&db, 112).await;

    // pending → applied skips the lease-acquisition edge.
    assert_eq!(
        transition_r(&db, &id, RunState::Applied).await,
        Err(RunTransitionError::Illegal(IllegalRunTransition {
            from: RunState::Pending,
            to: RunState::Applied,
        })),
    );
}

#[tokio::test]
async fn run_applied_is_terminal() {
    let (_home, db) = open_state();
    let id = run(&db, 113).await;
    transition_r(&db, &id, RunState::Running)
        .await
        .expect("run");
    transition_r(&db, &id, RunState::Applied)
        .await
        .expect("apply");

    for to in [RunState::Pending, RunState::Running, RunState::Failed] {
        assert_eq!(
            transition_r(&db, &id, to).await,
            Err(RunTransitionError::Illegal(IllegalRunTransition {
                from: RunState::Applied,
                to,
            })),
        );
    }
}

#[tokio::test]
async fn run_unknown_transition_is_typed_error() {
    let (_home, db) = open_state();
    let ghost = uuid(114);
    assert_eq!(
        transition_r(&db, &ghost, RunState::Running).await,
        Err(RunTransitionError::UnknownRun),
    );
}

#[tokio::test]
async fn processing_cursor_upsert_round_trips() {
    let (_home, db) = open_state();
    let session_id = "sess-cursor-1".to_string();

    let read = db.open_read().expect("read conn");
    assert_eq!(
        processing_cursor(&read, &session_id).expect("read"),
        None,
        "unknown session starts with no cursor",
    );
    drop(read);

    let s = session_id.clone();
    db.writer()
        .transaction(move |tx| upsert_processing_cursor(tx, &s, 42))
        .await
        .expect("upsert");
    let read = db.open_read().expect("read conn");
    assert_eq!(
        processing_cursor(&read, &session_id).expect("read"),
        Some(42)
    );
    drop(read);

    let s = session_id.clone();
    db.writer()
        .transaction(move |tx| upsert_processing_cursor(tx, &s, 99))
        .await
        .expect("upsert again");
    let read = db.open_read().expect("read conn");
    assert_eq!(
        processing_cursor(&read, &session_id).expect("read"),
        Some(99),
        "upsert overwrites the previous cursor value",
    );
}

// ---------------------------------------------------------------------------
// audit_event: uniqueness constraints and round-trip
// ---------------------------------------------------------------------------

#[tokio::test]
async fn audit_event_round_trips() {
    let (_home, db) = open_state();
    let memory_id = memory(&db, 120, MemoryKind::Fact).await;

    let m = memory_id.clone();
    db.writer()
        .transaction(move |tx| {
            insert_audit_event(
                tx,
                &NewAuditEvent {
                    entity_kind: "memory_entry",
                    entity_id: &m,
                    entity_version: 1,
                    op: "create",
                    actor: Actor::User,
                    idempotency_key: None,
                    payload: Some("{\"text\":\"...\"}"),
                },
                1000,
            )
        })
        .await
        .expect("insert audit event");

    let read = db.open_read().expect("read conn");
    let rows = read_audit_events_for_entity(&read, "memory_entry", &memory_id).expect("rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].entity_version, 1);
    assert_eq!(rows[0].op, "create");
    assert_eq!(rows[0].actor, Actor::User);
}

#[tokio::test]
async fn audit_event_unique_entity_version_conflict() {
    let (_home, db) = open_state();
    let memory_id = memory(&db, 121, MemoryKind::Fact).await;

    let m = memory_id.clone();
    db.writer()
        .transaction(move |tx| {
            insert_audit_event(
                tx,
                &NewAuditEvent {
                    entity_kind: "memory_entry",
                    entity_id: &m,
                    entity_version: 1,
                    op: "create",
                    actor: Actor::User,
                    idempotency_key: None,
                    payload: None,
                },
                1000,
            )
        })
        .await
        .expect("first insert");

    let m = memory_id.clone();
    let result = db
        .writer()
        .transaction(move |tx| {
            insert_audit_event(
                tx,
                &NewAuditEvent {
                    entity_kind: "memory_entry",
                    entity_id: &m,
                    entity_version: 1,
                    op: "reinforce",
                    actor: Actor::Router,
                    idempotency_key: None,
                    payload: None,
                },
                1001,
            )
        })
        .await;
    assert!(
        matches!(result, Err(WriteError::Sqlite(_))),
        "same (entity_kind, entity_id, entity_version) must conflict, got {result:?}",
    );
}

#[tokio::test]
async fn audit_event_unique_idempotency_key_conflict() {
    let (_home, db) = open_state();
    let memory_id = memory(&db, 122, MemoryKind::Fact).await;

    let m = memory_id.clone();
    db.writer()
        .transaction(move |tx| {
            insert_audit_event(
                tx,
                &NewAuditEvent {
                    entity_kind: "memory_entry",
                    entity_id: &m,
                    entity_version: 1,
                    op: "create",
                    actor: Actor::Router,
                    idempotency_key: Some("idem-1"),
                    payload: None,
                },
                1000,
            )
        })
        .await
        .expect("first insert");

    // Different entity_version (so the other UNIQUE is not what fires), same
    // idempotency_key.
    let m = memory_id.clone();
    let result = db
        .writer()
        .transaction(move |tx| {
            insert_audit_event(
                tx,
                &NewAuditEvent {
                    entity_kind: "memory_entry",
                    entity_id: &m,
                    entity_version: 2,
                    op: "reinforce",
                    actor: Actor::Router,
                    idempotency_key: Some("idem-1"),
                    payload: None,
                },
                1001,
            )
        })
        .await;
    assert!(
        matches!(result, Err(WriteError::Sqlite(_))),
        "same idempotency_key must conflict even across entity_versions, got {result:?}",
    );
}

// ---------------------------------------------------------------------------
// T14-07: recall primitives (active_entries_for_scope / memory_entry_summary)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn active_entries_for_scope_excludes_terminal_states_and_other_scopes() {
    let (_home, db) = open_state();
    let owner = uuid(150);
    let other_owner = uuid(151);

    let active_id = uuid(152);
    create_memory(
        &db,
        &active_id,
        MemoryKind::Fact,
        ScopeKind::Worktree,
        &owner,
        None,
    )
    .await
    .expect("create active");

    let resolved_id = uuid(153);
    create_memory(
        &db,
        &resolved_id,
        MemoryKind::Task,
        ScopeKind::Worktree,
        &owner,
        None,
    )
    .await
    .expect("create task");
    transition_entry(&db, &resolved_id, MemoryState::Resolved)
        .await
        .expect("resolve");

    let other_scope_id = uuid(154);
    create_memory(
        &db,
        &other_scope_id,
        MemoryKind::Fact,
        ScopeKind::Worktree,
        &other_owner,
        None,
    )
    .await
    .expect("create in a different scope");

    let read = db.open_read().expect("read conn");
    let found = active_entries_for_scope(&read, ScopeKind::Worktree, &owner, None).expect("query");
    assert_eq!(
        found
            .iter()
            .map(|e| e.memory_id.as_str())
            .collect::<Vec<_>>(),
        vec![active_id.as_str()],
        "only the active entry in this scope is offered as a recall candidate"
    );
    assert_eq!(found[0].kind, MemoryKind::Fact);
    assert_eq!(found[0].entry_version, 1);
}

#[tokio::test]
async fn active_entries_for_scope_filters_by_canonical_key_when_given() {
    let (_home, db) = open_state();
    let owner = uuid(155);

    let keyed_id = uuid(156);
    create_memory(
        &db,
        &keyed_id,
        MemoryKind::Decision,
        ScopeKind::Worktree,
        &owner,
        Some("storage-backend"),
    )
    .await
    .expect("create keyed");

    let unkeyed_id = uuid(157);
    create_memory(
        &db,
        &unkeyed_id,
        MemoryKind::Decision,
        ScopeKind::Worktree,
        &owner,
        None,
    )
    .await
    .expect("create unkeyed");

    let read = db.open_read().expect("read conn");
    let found =
        active_entries_for_scope(&read, ScopeKind::Worktree, &owner, Some("storage-backend"))
            .expect("query");
    assert_eq!(
        found
            .iter()
            .map(|e| e.memory_id.as_str())
            .collect::<Vec<_>>(),
        vec![keyed_id.as_str()]
    );
    assert_eq!(found[0].canonical_key.as_deref(), Some("storage-backend"));

    let unfiltered =
        active_entries_for_scope(&read, ScopeKind::Worktree, &owner, None).expect("query");
    assert_eq!(unfiltered.len(), 2, "no filter returns both");
}

#[tokio::test]
async fn memory_entry_summary_finds_terminal_entries_too() {
    let (_home, db) = open_state();
    let owner = uuid(158);
    let id = uuid(159);
    create_memory(
        &db,
        &id,
        MemoryKind::Question,
        ScopeKind::Worktree,
        &owner,
        None,
    )
    .await
    .expect("create question");
    transition_entry(&db, &id, MemoryState::Resolved)
        .await
        .expect("resolve");

    let read = db.open_read().expect("read conn");
    // Unlike active_entries_for_scope, a direct id lookup must still find a
    // terminal entry -- a caller that already knows the id needs to see
    // "this was already resolved", not an absence.
    let summary = memory_entry_summary(&read, &id)
        .expect("query")
        .expect("terminal entries are still found by id");
    assert_eq!(summary.state, MemoryState::Resolved);
    assert_eq!(summary.kind, MemoryKind::Question);

    assert_eq!(
        active_entries_for_scope(&read, ScopeKind::Worktree, &owner, None).expect("query"),
        Vec::new(),
        "the same entry is excluded from the scope-scan once terminal"
    );
}

#[tokio::test]
async fn memory_entry_summary_is_none_for_an_unknown_id() {
    let (_home, db) = open_state();
    let read = db.open_read().expect("read conn");
    assert_eq!(
        memory_entry_summary(&read, &uuid(160)).expect("query"),
        None
    );
}

#[tokio::test]
async fn canonical_key_owner_finds_the_owner_regardless_of_state() {
    let (_home, db) = open_state();
    let owner = uuid(161);
    let id = uuid(162);
    create_memory(
        &db,
        &id,
        MemoryKind::Fact,
        ScopeKind::Worktree,
        &owner,
        Some("storage-backend"),
    )
    .await
    .expect("create keyed");
    transition_entry(&db, &id, MemoryState::Retracted)
        .await
        .expect("retract");

    let read = db.open_read().expect("read conn");
    // Unlike active_entries_for_scope, this must still see a terminal row --
    // the real `memory_canonical` unique index has no state filter, so a
    // retracted row still blocks the key.
    assert_eq!(
        canonical_key_owner(&read, ScopeKind::Worktree, &owner, "storage-backend").expect("query"),
        Some(id)
    );
}

#[tokio::test]
async fn canonical_key_owner_is_none_when_unclaimed_or_in_a_different_scope() {
    let (_home, db) = open_state();
    let owner = uuid(163);
    let other_owner = uuid(164);
    let id = uuid(165);
    create_memory(
        &db,
        &id,
        MemoryKind::Fact,
        ScopeKind::Worktree,
        &owner,
        Some("storage-backend"),
    )
    .await
    .expect("create keyed");

    let read = db.open_read().expect("read conn");
    assert_eq!(
        canonical_key_owner(&read, ScopeKind::Worktree, &owner, "unclaimed-key").expect("query"),
        None
    );
    assert_eq!(
        canonical_key_owner(&read, ScopeKind::Worktree, &other_owner, "storage-backend")
            .expect("query"),
        None,
        "the same key text in a different scope is a different slot"
    );
}

// ---------------------------------------------------------------------------
// T15-04: list_memory_entries_for_scope / memory_entry_counts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_memory_entries_for_scope_includes_terminal_states_and_isolates_by_scope() {
    let (_home, db) = open_state();
    let owner = uuid(166);
    let other_owner = uuid(167);

    let active_id = uuid(168);
    create_memory(
        &db,
        &active_id,
        MemoryKind::Fact,
        ScopeKind::Worktree,
        &owner,
        None,
    )
    .await
    .expect("create active");

    let retracted_id = uuid(169);
    create_memory(
        &db,
        &retracted_id,
        MemoryKind::Fact,
        ScopeKind::Worktree,
        &owner,
        None,
    )
    .await
    .expect("create to retract");
    transition_entry(&db, &retracted_id, MemoryState::Retracted)
        .await
        .expect("retract");

    let other_scope_id = uuid(170);
    create_memory(
        &db,
        &other_scope_id,
        MemoryKind::Fact,
        ScopeKind::Worktree,
        &other_owner,
        None,
    )
    .await
    .expect("create in a different scope");

    let read = db.open_read().expect("read conn");
    let found = list_memory_entries_for_scope(&read, ScopeKind::Worktree, &owner, None, None)
        .expect("query");
    // Unlike active_entries_for_scope/recall_candidates_for_scope, a
    // review-tool listing must still surface the retracted entry (spec 04
    // §5: "remain queryable via review tools") -- and never a different
    // scope's rows.
    assert_eq!(
        found
            .iter()
            .map(|e| e.memory_id.as_str())
            .collect::<Vec<_>>(),
        vec![active_id.as_str(), retracted_id.as_str()],
        "both entries in this scope are listed, ordered by created_at then memory_id"
    );
    assert_eq!(found[1].state, MemoryState::Retracted);
}

#[tokio::test]
async fn list_memory_entries_for_scope_filters_by_kind_and_state() {
    let (_home, db) = open_state();
    let owner = uuid(171);

    let fact_id = uuid(172);
    create_memory(
        &db,
        &fact_id,
        MemoryKind::Fact,
        ScopeKind::Worktree,
        &owner,
        None,
    )
    .await
    .expect("create fact");

    let task_id = uuid(173);
    create_memory(
        &db,
        &task_id,
        MemoryKind::Task,
        ScopeKind::Worktree,
        &owner,
        None,
    )
    .await
    .expect("create task");
    transition_entry(&db, &task_id, MemoryState::Resolved)
        .await
        .expect("resolve");

    let read = db.open_read().expect("read conn");

    let by_kind = list_memory_entries_for_scope(
        &read,
        ScopeKind::Worktree,
        &owner,
        Some(MemoryKind::Fact),
        None,
    )
    .expect("query");
    assert_eq!(
        by_kind
            .iter()
            .map(|e| e.memory_id.as_str())
            .collect::<Vec<_>>(),
        vec![fact_id.as_str()]
    );

    let by_state = list_memory_entries_for_scope(
        &read,
        ScopeKind::Worktree,
        &owner,
        None,
        Some(MemoryState::Resolved),
    )
    .expect("query");
    assert_eq!(
        by_state
            .iter()
            .map(|e| e.memory_id.as_str())
            .collect::<Vec<_>>(),
        vec![task_id.as_str()]
    );

    let unfiltered = list_memory_entries_for_scope(&read, ScopeKind::Worktree, &owner, None, None)
        .expect("query");
    assert_eq!(unfiltered.len(), 2, "no filter returns both");
}

#[tokio::test]
async fn list_memory_entries_for_scope_carries_the_full_row_shape() {
    let (_home, db) = open_state();
    let owner = uuid(174);
    let id = uuid(175);
    create_memory(
        &db,
        &id,
        MemoryKind::Decision,
        ScopeKind::Worktree,
        &owner,
        Some("storage-backend"),
    )
    .await
    .expect("create keyed");

    let read = db.open_read().expect("read conn");
    let found = list_memory_entries_for_scope(&read, ScopeKind::Worktree, &owner, None, None)
        .expect("query");
    assert_eq!(found.len(), 1);
    let row = &found[0];
    assert_eq!(row.memory_id, id);
    assert_eq!(row.kind, MemoryKind::Decision);
    assert_eq!(row.state, MemoryState::Active);
    assert_eq!(row.text, "some durable text");
    assert_eq!(row.canonical_key.as_deref(), Some("storage-backend"));
    assert_eq!(row.scope_kind, ScopeKind::Worktree);
    assert_eq!(row.scope_owner_id, owner);
    assert_eq!(row.confidence, 0.5);
    assert_eq!(row.importance, 0.5);
    assert_eq!(row.valid_from_tree, None);
    assert_eq!(row.last_verified_tree, None);
    assert_eq!(row.supersedes_id, None);
    assert_eq!(row.entry_version, 1);
    assert_eq!(row.created_at, 1000);
    assert_eq!(row.updated_at, 1000);
}

#[tokio::test]
async fn memory_entry_counts_groups_by_kind_and_state_store_wide() {
    let (_home, db) = open_state();
    let owner = uuid(176);
    let other_owner = uuid(177);

    // Two active facts (possibly different scopes -- counts are store-wide,
    // not scope-filtered).
    create_memory(
        &db,
        &uuid(178),
        MemoryKind::Fact,
        ScopeKind::Worktree,
        &owner,
        None,
    )
    .await
    .expect("create fact 1");
    create_memory(
        &db,
        &uuid(179),
        MemoryKind::Fact,
        ScopeKind::Worktree,
        &other_owner,
        None,
    )
    .await
    .expect("create fact 2");

    // One resolved task.
    let task_id = uuid(180);
    create_memory(
        &db,
        &task_id,
        MemoryKind::Task,
        ScopeKind::Worktree,
        &owner,
        None,
    )
    .await
    .expect("create task");
    transition_entry(&db, &task_id, MemoryState::Resolved)
        .await
        .expect("resolve");

    let read = db.open_read().expect("read conn");
    let counts = memory_entry_counts(&read).expect("counts");
    assert_eq!(
        counts,
        vec![
            MemoryCountRow {
                kind: MemoryKind::Fact,
                state: MemoryState::Active,
                count: 2,
            },
            MemoryCountRow {
                kind: MemoryKind::Task,
                state: MemoryState::Resolved,
                count: 1,
            },
        ],
        "ordered by (kind, state); empty buckets are omitted, not zero-filled"
    );
}

#[tokio::test]
async fn memory_entry_counts_is_empty_for_an_empty_store() {
    let (_home, db) = open_state();
    let read = db.open_read().expect("read conn");
    assert_eq!(memory_entry_counts(&read).expect("counts"), Vec::new());
}
