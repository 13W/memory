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

use local_rag_core::hash::sha256_hex;
use local_rag_core::identity::uuidv7_from;
use local_rag_core::paths::StoreLayout;
use local_rag_store::memory::{
    Actor, CandidateState, CandidateTransitionError, CreateMemoryEntryError, GLOBAL_SCOPE_OWNER_ID,
    IllegalCandidateTransition, IllegalMemoryTransition, IllegalRunTransition, MemoryCountRow,
    MemoryKind, MemoryState, MemoryTransitionError, NewAuditEvent, NewCandidate,
    NewConsolidationRun, NewMemoryEntry, NewMemoryEvidence, RunCountRow, RunState,
    RunTransitionError, STUCK_RUN_ATTEMPT_THRESHOLD, STUCK_RUN_REASON_MAX_CHARS, ScopeKind,
    active_entries_for_scope, active_entry_with_text, candidate_state, canonical_key_owner,
    consolidation_run_counts, consolidation_run_state, create_candidate, create_consolidation_run,
    create_memory_entry, insert_audit_event, insert_candidate_evidence, insert_memory_evidence,
    list_memory_entries_for_scope, memory_entry_by_id, memory_entry_counts, memory_entry_state,
    memory_entry_summary, memory_evidence_for, observations_applied_since,
    oldest_open_run_created_at, processing_cursor, read_audit_events_for_entity,
    recall_candidates_for_scope, stuck_consolidation_runs, total_pending_backlog,
    transition_candidate, transition_memory_entry, transition_run, upsert_processing_cursor,
};
use local_rag_store::rusqlite::params;
use local_rag_store::{
    CURRENT_NORMALIZER_VERSION, FailureKind, LEASE_DURATION_MS, MAX_NORMALIZATION_ATTEMPTS,
    NormalizationCountRow, NormalizationStatus, NormalizationWrite, StuckRunRow, UpsertOutcome,
    entries_needing_normalization, normalization_counts, normalization_for, record_run_failure,
    retry_run, upsert_normalization,
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

/// `D-078`: the deduplication lookup. Same scope + exact text + non-terminal
/// is the whole predicate, and each of those three words is load-bearing —
/// asserted here rather than left to the caller, because the caller
/// (`local_rag_memory::guard`) uses the answer to *silently* turn a `create`
/// into a `reinforce`, and a lookup that over-matched would drop a claim the
/// author meant to keep.
#[tokio::test]
async fn active_entry_with_text_matches_only_an_exact_live_same_scope_entry() {
    let (_home, db) = open_state();
    let owner = uuid(190);
    let other_owner = uuid(191);
    const TEXT: &str = "some durable text";

    // Terminal: retracted, and therefore not a match — a claim restated after
    // a retraction is a new entry, not a reinforcement of the dead one.
    let retracted_id = uuid(192);
    create_memory(
        &db,
        &retracted_id,
        MemoryKind::Fact,
        ScopeKind::Worktree,
        &owner,
        None,
    )
    .await
    .expect("create");
    transition_entry(&db, &retracted_id, MemoryState::Retracted)
        .await
        .expect("retract");

    // Another scope: same sentence, different project, different fact.
    create_memory(
        &db,
        &uuid(193),
        MemoryKind::Fact,
        ScopeKind::Worktree,
        &other_owner,
        None,
    )
    .await
    .expect("create elsewhere");

    let read = db.open_read().expect("read conn");
    assert_eq!(
        active_entry_with_text(&read, ScopeKind::Worktree, &owner, TEXT).expect("query"),
        None,
        "a retracted entry and another scope's entry are both non-matches",
    );

    // Now a live one in this scope.
    let live_id = uuid(194);
    create_memory(
        &db,
        &live_id,
        MemoryKind::Fact,
        ScopeKind::Worktree,
        &owner,
        None,
    )
    .await
    .expect("create live");
    // …and a second one, so the tie-break has something to break. A store that
    // already accumulated duplicates (which is how this deviation was found)
    // must still give one stable answer.
    let newer_id = uuid(195);
    create_memory(
        &db,
        &newer_id,
        MemoryKind::Fact,
        ScopeKind::Worktree,
        &owner,
        None,
    )
    .await
    .expect("create a duplicate");

    let read = db.open_read().expect("read conn");
    let found = active_entry_with_text(&read, ScopeKind::Worktree, &owner, TEXT)
        .expect("query")
        .expect("the live entry matches");
    assert_eq!(
        found.memory_id, live_id,
        "the oldest wins, deterministically"
    );
    assert_eq!(found.entry_version, 1);

    assert_eq!(
        active_entry_with_text(&read, ScopeKind::Worktree, &owner, "some durable text!")
            .expect("query"),
        None,
        "one byte of difference is a different claim — the guard must not judge similarity",
    );
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

/// `memory_entry_by_id` (T16-02, `inspect memory <id>`/`export`/`purge
/// memory <id>`'s own read) returns every column and tracks a state/version
/// change, unlike the narrower [`memory_entry_summary`] projection.
#[tokio::test]
async fn memory_entry_by_id_reads_every_column_regardless_of_state_then_none_for_unknown_id() {
    let (_home, db) = open_state();
    let owner = uuid(161);
    let id = uuid(162);
    create_memory(
        &db,
        &id,
        MemoryKind::Fact,
        ScopeKind::Worktree,
        &owner,
        Some("canon-key"),
    )
    .await
    .expect("create fact");

    let read = db.open_read().expect("read conn");
    let row = memory_entry_by_id(&read, &id)
        .expect("query")
        .expect("row present");
    assert_eq!(row.memory_id, id);
    assert_eq!(row.kind, MemoryKind::Fact);
    assert_eq!(row.state, MemoryState::Active);
    assert_eq!(row.text, "some durable text");
    assert_eq!(row.canonical_key.as_deref(), Some("canon-key"));
    assert_eq!(row.scope_kind, ScopeKind::Worktree);
    assert_eq!(row.scope_owner_id, owner);
    assert_eq!(row.entry_version, 1);
    drop(read);

    transition_entry(&db, &id, MemoryState::Retracted)
        .await
        .expect("retract");
    let read = db.open_read().expect("read conn");
    assert_eq!(
        memory_entry_by_id(&read, &id)
            .expect("query")
            .expect("row present")
            .state,
        MemoryState::Retracted,
        "memory_entry_by_id finds a terminal-state row too, like memory_entry_summary"
    );

    assert_eq!(
        memory_entry_by_id(&read, &uuid(163)).expect("query"),
        None,
        "an unknown memory_id reads back as None, not an error"
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

#[tokio::test]
async fn consolidation_run_counts_groups_by_state_store_wide() {
    let (_home, db) = open_state();
    db.writer()
        .transaction(|tx| {
            create_consolidation_run(
                tx,
                &NewConsolidationRun {
                    run_id: "run-pending",
                    session_id: "sess-1",
                    from_received_seq: 1,
                    to_received_seq: 5,
                    router_version: "v1",
                },
                1000,
            )?;
            create_consolidation_run(
                tx,
                &NewConsolidationRun {
                    run_id: "run-applied",
                    session_id: "sess-1",
                    from_received_seq: 6,
                    to_received_seq: 10,
                    router_version: "v1",
                },
                1000,
            )?;
            transition_run(tx, "run-applied", RunState::Running, 1100)?.expect("legal");
            transition_run(tx, "run-applied", RunState::Applied, 1200)?.expect("legal");
            Ok(())
        })
        .await
        .expect("seed runs");

    let read = db.open_read().expect("read conn");
    assert_eq!(
        consolidation_run_counts(&read).expect("counts"),
        vec![
            RunCountRow {
                state: RunState::Applied,
                count: 1,
            },
            RunCountRow {
                state: RunState::Pending,
                count: 1,
            },
        ],
        "ordered by state text ('applied' < 'pending'); empty buckets omitted"
    );
}

#[tokio::test]
async fn consolidation_run_counts_is_empty_for_an_empty_store() {
    let (_home, db) = open_state();
    let read = db.open_read().expect("read conn");
    assert_eq!(consolidation_run_counts(&read).expect("counts"), Vec::new());
}

#[tokio::test]
async fn total_pending_backlog_sums_pending_backlog_across_every_session() {
    let (_home, db) = open_state();
    db.writer()
        .transaction(|tx| {
            // One global `received_seq` sequence, insertion order: sess-b
            // gets 1..=2, sess-a gets 3..=5 -- mirrors `sessions_with_
            // pending_backlog`'s own test fixture shape. D-052: the total
            // must be each session's own row count (2 + 3 = 5), not each
            // session's distance to its own last `received_seq` (2 + 5 = 7,
            // which double-counts sess-b's 2 rows inside sess-a's 0..=5
            // span -- `received_seq` is one sequence shared by both
            // sessions, not a per-session counter).
            for (oid, evt, sess) in [
                ("obs-b1", "evt-b1", "sess-b"),
                ("obs-b2", "evt-b2", "sess-b"),
                ("obs-a1", "evt-a1", "sess-a"),
                ("obs-a2", "evt-a2", "sess-a"),
                ("obs-a3", "evt-a3", "sess-a"),
            ] {
                tx.execute(
                    "INSERT INTO observation_envelope \
                       (observation_id, source_event_id, payload_hash, event_type, \
                        evidence_kind, trust, session_id) \
                     VALUES (?1, ?2, 'deadbeef', 'Stop', 'user_statement', 'normal', ?3)",
                    [oid, evt, sess],
                )?;
            }
            Ok(())
        })
        .await
        .expect("seed envelopes across two sessions");

    let read = db.open_read().expect("read conn");
    assert_eq!(
        total_pending_backlog(&read).expect("total backlog"),
        5,
        "no cursor for either session: 2 own rows (sess-b) + 3 own rows (sess-a)"
    );
    drop(read);

    db.writer()
        .transaction(|tx| upsert_processing_cursor(tx, "sess-b", 2))
        .await
        .expect("catch sess-b up");

    let read = db.open_read().expect("read conn");
    assert_eq!(
        total_pending_backlog(&read).expect("total backlog"),
        3,
        "sess-b caught up (backlog 0, drops out of the sum); sess-a's 3 own rows remain"
    );
}

#[tokio::test]
async fn total_pending_backlog_is_zero_for_an_empty_store() {
    let (_home, db) = open_state();
    let read = db.open_read().expect("read conn");
    assert_eq!(total_pending_backlog(&read).expect("total backlog"), 0);
}

#[tokio::test]
async fn observations_applied_since_sums_window_sizes_of_recently_applied_runs() {
    let (_home, db) = open_state();
    db.writer()
        .transaction(|tx| {
            // Applied before the cutoff -- must not count.
            create_consolidation_run(
                tx,
                &NewConsolidationRun {
                    run_id: "run-old",
                    session_id: "sess-1",
                    from_received_seq: 1,
                    to_received_seq: 10,
                    router_version: "v1",
                },
                500,
            )?;
            transition_run(tx, "run-old", RunState::Running, 600)?.expect("legal");
            transition_run(tx, "run-old", RunState::Applied, 700)?.expect("legal");
            // Applied after the cutoff -- must count (window size 5: 11..=15).
            create_consolidation_run(
                tx,
                &NewConsolidationRun {
                    run_id: "run-recent",
                    session_id: "sess-1",
                    from_received_seq: 11,
                    to_received_seq: 15,
                    router_version: "v1",
                },
                1500,
            )?;
            transition_run(tx, "run-recent", RunState::Running, 1600)?.expect("legal");
            transition_run(tx, "run-recent", RunState::Applied, 1700)?.expect("legal");
            // Still running after the cutoff -- must not count (not applied).
            create_consolidation_run(
                tx,
                &NewConsolidationRun {
                    run_id: "run-in-flight",
                    session_id: "sess-1",
                    from_received_seq: 16,
                    to_received_seq: 20,
                    router_version: "v1",
                },
                1800,
            )?;
            transition_run(tx, "run-in-flight", RunState::Running, 1900)?.expect("legal");
            Ok(())
        })
        .await
        .expect("seed runs");

    let read = db.open_read().expect("read conn");
    assert_eq!(
        observations_applied_since(&read, 1000).expect("applied since"),
        5,
        "only run-recent's window (11..=15, 5 observations) is applied after the cutoff"
    );
}

#[tokio::test]
async fn observations_applied_since_is_zero_for_an_empty_store() {
    let (_home, db) = open_state();
    let read = db.open_read().expect("read conn");
    assert_eq!(
        observations_applied_since(&read, 0).expect("applied since"),
        0
    );
}

#[tokio::test]
async fn oldest_open_run_created_at_ignores_applied_runs() {
    let (_home, db) = open_state();
    db.writer()
        .transaction(|tx| {
            create_consolidation_run(
                tx,
                &NewConsolidationRun {
                    run_id: "run-applied",
                    session_id: "sess-1",
                    from_received_seq: 1,
                    to_received_seq: 5,
                    router_version: "v1",
                },
                1000,
            )?;
            transition_run(tx, "run-applied", RunState::Running, 1100)?.expect("legal");
            transition_run(tx, "run-applied", RunState::Applied, 1200)?.expect("legal");
            // The oldest still-open run -- created before the newer one below.
            create_consolidation_run(
                tx,
                &NewConsolidationRun {
                    run_id: "run-open-old",
                    session_id: "sess-1",
                    from_received_seq: 6,
                    to_received_seq: 10,
                    router_version: "v1",
                },
                2000,
            )?;
            create_consolidation_run(
                tx,
                &NewConsolidationRun {
                    run_id: "run-open-new",
                    session_id: "sess-1",
                    from_received_seq: 11,
                    to_received_seq: 15,
                    router_version: "v1",
                },
                3000,
            )?;
            Ok(())
        })
        .await
        .expect("seed runs");

    let read = db.open_read().expect("read conn");
    assert_eq!(
        oldest_open_run_created_at(&read).expect("oldest open run"),
        Some(2000),
        "the applied run's created_at (1000) must not win despite being earlier"
    );
}

#[tokio::test]
async fn oldest_open_run_created_at_is_none_when_every_run_is_applied() {
    let (_home, db) = open_state();
    db.writer()
        .transaction(|tx| {
            create_consolidation_run(
                tx,
                &NewConsolidationRun {
                    run_id: "run-applied",
                    session_id: "sess-1",
                    from_received_seq: 1,
                    to_received_seq: 5,
                    router_version: "v1",
                },
                1000,
            )?;
            transition_run(tx, "run-applied", RunState::Running, 1100)?.expect("legal");
            transition_run(tx, "run-applied", RunState::Applied, 1200)?.expect("legal");
            Ok(())
        })
        .await
        .expect("seed run");

    let read = db.open_read().expect("read conn");
    assert_eq!(
        oldest_open_run_created_at(&read).expect("oldest open run"),
        None
    );
}

#[tokio::test]
async fn oldest_open_run_created_at_is_none_for_an_empty_store() {
    let (_home, db) = open_state();
    let read = db.open_read().expect("read conn");
    assert_eq!(
        oldest_open_run_created_at(&read).expect("oldest open run"),
        None
    );
}

// ---------------------------------------------------------------------------
// D-071: which consolidation runs a human has to look at
// ---------------------------------------------------------------------------

/// Insert `observation_envelope` rows covering `from_seq..=to_seq` for
/// `session_id` — [`stuck_consolidation_runs`]'s shrink-carve-out asks how
/// many observations a run's window actually spans.
async fn seed_window_envelopes(db: &StateDb, session_id: &str, from_seq: i64, to_seq: i64) {
    let session = session_id.to_string();
    db.writer()
        .transaction(move |tx| {
            for seq in from_seq..=to_seq {
                tx.execute(
                    "INSERT INTO observation_envelope \
                       (received_seq, observation_id, source_event_id, payload_hash, event_type, \
                        evidence_kind, trust, session_id) \
                     VALUES (?1, ?2, ?3, 'deadbeef', 'Stop', 'user_statement', 'normal', ?4)",
                    params![
                        seq,
                        format!("obs-{session}-{seq}"),
                        format!("evt-{session}-{seq}"),
                        session
                    ],
                )?;
            }
            Ok(())
        })
        .await
        .expect("seed window envelopes");
}

/// A `failed` run that really went through `attempts` recorded failures —
/// `create -> running -> failed`, then `retry_run -> failed` for each further
/// attempt, exactly the cycle the runner drives.
#[allow(clippy::too_many_arguments)]
async fn seed_failed_run(
    db: &StateDb,
    run_id: &str,
    session_id: &str,
    from_seq: i64,
    to_seq: i64,
    kind: FailureKind,
    context_overflow: bool,
    fingerprint: &str,
    reason: &str,
    attempts: i64,
    created_at: i64,
) {
    let (rid, sid) = (run_id.to_string(), session_id.to_string());
    let (fp, rsn) = (fingerprint.to_string(), reason.to_string());
    db.writer()
        .transaction(move |tx| {
            create_consolidation_run(
                tx,
                &NewConsolidationRun {
                    run_id: &rid,
                    session_id: &sid,
                    from_received_seq: from_seq,
                    to_received_seq: to_seq,
                    router_version: "v1",
                },
                created_at,
            )?;
            for attempt in 0..attempts {
                if attempt == 0 {
                    transition_run(tx, &rid, RunState::Running, created_at)?
                        .expect("pending -> running");
                } else {
                    retry_run(tx, &rid, LEASE_DURATION_MS, created_at)?.expect("failed -> running");
                }
                record_run_failure(
                    tx,
                    &rid,
                    kind,
                    &rsn,
                    context_overflow,
                    Some(&fp),
                    created_at,
                )?
                .expect("running -> failed");
            }
            Ok(())
        })
        .await
        .expect("seed failed run");
}

/// A healthy store has nothing to report: an `applied` run is never stuck, and
/// neither is a run that has failed fewer times than the threshold.
#[tokio::test]
async fn stuck_consolidation_runs_is_empty_for_a_healthy_store() {
    let (_home, db) = open_state();
    seed_window_envelopes(&db, "sess-1", 1, 4).await;
    db.writer()
        .transaction(|tx| {
            create_consolidation_run(
                tx,
                &NewConsolidationRun {
                    run_id: "run-applied",
                    session_id: "sess-1",
                    from_received_seq: 1,
                    to_received_seq: 2,
                    router_version: "v1",
                },
                1_000,
            )?;
            transition_run(tx, "run-applied", RunState::Running, 1_100)?.expect("legal");
            transition_run(tx, "run-applied", RunState::Applied, 1_200)?.expect("legal");
            Ok(())
        })
        .await
        .expect("seed applied run");
    seed_failed_run(
        &db,
        "run-young",
        "sess-1",
        3,
        4,
        FailureKind::Transient,
        false,
        "build-1",
        "no generation provider configured",
        STUCK_RUN_ATTEMPT_THRESHOLD - 1,
        1_000,
    )
    .await;

    let read = db.open_read().expect("read conn");
    assert_eq!(
        stuck_consolidation_runs(&read, "build-1", STUCK_RUN_ATTEMPT_THRESHOLD).expect("stuck"),
        Vec::new(),
        "one attempt short of the threshold is still just a retry"
    );
}

/// The threshold is a real boundary, and the row carries what a report needs
/// to name the run without opening `sqlite3`.
#[tokio::test]
async fn stuck_consolidation_runs_reports_a_run_at_the_attempt_threshold() {
    let (_home, db) = open_state();
    seed_window_envelopes(&db, "sess-1", 1, 3).await;
    seed_failed_run(
        &db,
        "run-looping",
        "sess-1",
        1,
        3,
        FailureKind::Transient,
        false,
        "build-1",
        "state transaction failed (rolled back): database is locked",
        STUCK_RUN_ATTEMPT_THRESHOLD,
        1_000,
    )
    .await;

    let read = db.open_read().expect("read conn");
    let stuck =
        stuck_consolidation_runs(&read, "build-1", STUCK_RUN_ATTEMPT_THRESHOLD).expect("stuck");
    assert_eq!(
        stuck,
        vec![StuckRunRow {
            run_id: "run-looping".to_string(),
            session_id: "sess-1".to_string(),
            attempt_count: STUCK_RUN_ATTEMPT_THRESHOLD,
            dead_lettered: false,
            last_failure_kind: Some("transient".to_string()),
            last_failure_reason: Some(
                "state transaction failed (rolled back): database is locked".to_string()
            ),
            from_received_seq: 1,
            to_received_seq: 3,
        }],
        "still retry-eligible, but visibly not converging"
    );
}

/// A dead-letter is reported from its very first attempt — nothing but a
/// rebuild will ever pick it up again — but only while the fingerprint
/// matches the running build.
#[tokio::test]
async fn stuck_consolidation_runs_reports_a_dead_letter_on_this_build_only() {
    let (_home, db) = open_state();
    seed_window_envelopes(&db, "sess-1", 1, 3).await;
    seed_failed_run(
        &db,
        "run-dead",
        "sess-1",
        1,
        3,
        FailureKind::Mechanical,
        false,
        "build-1",
        "state transaction failed (rolled back): UNIQUE constraint failed: \
         candidate_evidence.candidate_id, candidate_evidence.observation_id",
        1,
        1_000,
    )
    .await;

    let read = db.open_read().expect("read conn");
    let stuck =
        stuck_consolidation_runs(&read, "build-1", STUCK_RUN_ATTEMPT_THRESHOLD).expect("stuck");
    assert_eq!(
        stuck.len(),
        1,
        "a single attempt is enough once it is final"
    );
    assert_eq!(stuck[0].run_id, "run-dead");
    assert!(stuck[0].dead_lettered);
    assert_eq!(stuck[0].attempt_count, 1);

    assert_eq!(
        stuck_consolidation_runs(&read, "build-2", STUCK_RUN_ATTEMPT_THRESHOLD).expect("stuck"),
        Vec::new(),
        "on a different build the very same row is retry-eligible again, not stuck"
    );
}

/// D-058's shrink-and-retry resolves a context-overflow dead-letter by itself
/// as long as the window still has room to halve — reporting that would be a
/// false alarm. The floor case (a window already down to one observation) has
/// no room left and stays reported.
#[tokio::test]
async fn stuck_consolidation_runs_skips_the_overflow_run_shrink_and_retry_will_fix() {
    let (_home, db) = open_state();
    seed_window_envelopes(&db, "sess-wide", 1, 4).await;
    seed_window_envelopes(&db, "sess-floor", 5, 5).await;
    seed_failed_run(
        &db,
        "run-wide",
        "sess-wide",
        1,
        4,
        FailureKind::Mechanical,
        true,
        "build-1",
        "deterministic context overflow for this window, retrying will not help",
        1,
        1_000,
    )
    .await;
    seed_failed_run(
        &db,
        "run-floor",
        "sess-floor",
        5,
        5,
        FailureKind::Mechanical,
        true,
        "build-1",
        "deterministic context overflow for this window, retrying will not help",
        1,
        1_000,
    )
    .await;

    let read = db.open_read().expect("read conn");
    let stuck =
        stuck_consolidation_runs(&read, "build-1", STUCK_RUN_ATTEMPT_THRESHOLD).expect("stuck");
    assert_eq!(
        stuck.iter().map(|r| r.run_id.as_str()).collect::<Vec<_>>(),
        vec!["run-floor"],
        "the four-observation window will simply be halved on the next tick"
    );
}

/// A failure reason is arbitrary text; the reported row is meant to be
/// printed, so it is bounded — and bounded by characters, never by bytes.
#[tokio::test]
async fn stuck_consolidation_runs_truncates_a_long_failure_reason() {
    let (_home, db) = open_state();
    seed_window_envelopes(&db, "sess-1", 1, 2).await;
    let reason = "и".repeat(STUCK_RUN_REASON_MAX_CHARS + 50);
    seed_failed_run(
        &db,
        "run-verbose",
        "sess-1",
        1,
        2,
        FailureKind::Mechanical,
        false,
        "build-1",
        &reason,
        1,
        1_000,
    )
    .await;

    let read = db.open_read().expect("read conn");
    let stuck =
        stuck_consolidation_runs(&read, "build-1", STUCK_RUN_ATTEMPT_THRESHOLD).expect("stuck");
    let reported = stuck[0]
        .last_failure_reason
        .as_deref()
        .expect("a reason was recorded");
    assert_eq!(
        reported.chars().count(),
        STUCK_RUN_REASON_MAX_CHARS + 1,
        "truncated to the cap plus the ellipsis"
    );
    assert!(reported.ends_with('…'), "{reported}");
}

/// D-072, found in live verification: the shrink carve-out must not depend on
/// whether some *other* run of the same session happens to be executing right
/// now. Keyed off the session's latest non-`applied` run, a concurrently
/// `running` neighbour pushed the real dead-letter out of "latest" and back
/// into the report — `doctor` and `stats`, minutes apart on an unchanged
/// store, disagreed on how many runs were stuck.
///
/// The fixture is the live shape of session `4b92bfd5`: an old, permanently
/// dead window, the newest failed one that shrink-and-retry will still halve,
/// and a fresh run in flight.
#[tokio::test]
async fn stuck_consolidation_runs_does_not_flicker_while_another_run_is_in_flight() {
    let (_home, db) = open_state();
    seed_window_envelopes(&db, "sess-1", 1, 30).await;
    // Older overflow dead-letter: nothing will ever act on it again.
    seed_failed_run(
        &db,
        "run-abandoned",
        "sess-1",
        1,
        10,
        FailureKind::Mechanical,
        true,
        "build-1",
        "deterministic context overflow for this window, retrying will not help",
        1,
        1_000,
    )
    .await;
    // Newest failed run of the session: shrink-and-retry still has room.
    seed_failed_run(
        &db,
        "run-shrinkable",
        "sess-1",
        11,
        20,
        FailureKind::Mechanical,
        true,
        "build-1",
        "deterministic context overflow for this window, retrying will not help",
        1,
        2_000,
    )
    .await;

    let read = db.open_read().expect("read conn");
    let quiet: Vec<String> =
        stuck_consolidation_runs(&read, "build-1", STUCK_RUN_ATTEMPT_THRESHOLD)
            .expect("stuck")
            .into_iter()
            .map(|r| r.run_id)
            .collect();
    assert_eq!(
        quiet,
        vec!["run-abandoned".to_string()],
        "only the run nothing will ever retry"
    );
    drop(read);

    // A newer run for the same session starts: `running`, no failure yet.
    db.writer()
        .transaction(|tx| {
            create_consolidation_run(
                tx,
                &NewConsolidationRun {
                    run_id: "run-in-flight",
                    session_id: "sess-1",
                    from_received_seq: 21,
                    to_received_seq: 30,
                    router_version: "v1",
                },
                3_000,
            )?;
            transition_run(tx, "run-in-flight", RunState::Running, 3_000)?.expect("legal");
            Ok(())
        })
        .await
        .expect("seed in-flight run");

    let read = db.open_read().expect("read conn");
    let busy: Vec<String> = stuck_consolidation_runs(&read, "build-1", STUCK_RUN_ATTEMPT_THRESHOLD)
        .expect("stuck")
        .into_iter()
        .map(|r| r.run_id)
        .collect();
    assert_eq!(
        busy, quiet,
        "the same store, one transient run later, must give the same answer"
    );
}

// ---------------------------------------------------------------------------
// T21-01: the normalization queue
// ---------------------------------------------------------------------------

/// A `memory_entry` with a caller-chosen text and `created_at` — the queue's
/// order and its staleness check both read exactly those two.
async fn seed_entry_with_text(db: &StateDb, memory_id: &str, text: &str, created_at: i64) {
    let (id, text) = (memory_id.to_string(), text.to_string());
    db.writer()
        .transaction(move |tx| {
            create_memory_entry(
                tx,
                &NewMemoryEntry {
                    memory_id: &id,
                    kind: MemoryKind::Fact,
                    text: &text,
                    canonical_key: None,
                    scope_kind: ScopeKind::Global,
                    scope_owner_id: GLOBAL_SCOPE_OWNER_ID,
                    confidence: 0.5,
                    importance: 0.5,
                    valid_from_tree: None,
                    last_verified_tree: None,
                    supersedes_id: None,
                },
                created_at,
            )
        })
        .await
        .expect("create tx")
        .expect("create ok");
}

#[allow(clippy::too_many_arguments)]
async fn write_normalization(
    db: &StateDb,
    memory_id: &str,
    status: NormalizationStatus,
    entry_text: &str,
    source_text: Option<&str>,
    normalizer_version: i64,
    attempt_count: i64,
    next_attempt_at: Option<i64>,
    now_ms: i64,
) -> UpsertOutcome {
    let (id, sha) = (memory_id.to_string(), sha256_hex(entry_text.as_bytes()));
    let source = source_text.map(str::to_string);
    db.writer()
        .transaction(move |tx| {
            upsert_normalization(
                tx,
                &NormalizationWrite {
                    memory_id: &id,
                    status,
                    expected_text_sha256: &sha,
                    canon_text_sha256: &sha,
                    source_text: source.as_deref(),
                    source_language: Some("ru"),
                    normalizer_model_id: Some("test-model"),
                    prompt_version: Some(1),
                    normalizer_version,
                    attempt_count,
                    last_error: None,
                    next_attempt_at,
                },
                now_ms,
            )
        })
        .await
        .expect("upsert tx")
}

fn queued(db: &StateDb, now_ms: i64, limit: usize) -> Vec<String> {
    let read = db.open_read().expect("read conn");
    entries_needing_normalization(&read, CURRENT_NORMALIZER_VERSION, now_ms, limit)
        .expect("queue")
        .into_iter()
        .map(|p| p.memory_id)
        .collect()
}

/// An entry nobody has normalized is due; a terminal one never is, however
/// untranslated — recall does not return it, so translating it would spend
/// inference on text no reader will ever embed.
#[tokio::test]
async fn the_normalization_queue_offers_new_entries_and_never_terminal_ones() {
    let (_home, db) = open_state();
    seed_entry_with_text(&db, "m-live", "живой текст", 1_000).await;
    seed_entry_with_text(&db, "m-gone", "мёртвый текст", 1_001).await;
    assert_eq!(
        transition_entry(&db, "m-gone", MemoryState::Retracted).await,
        Ok(())
    );

    assert_eq!(queued(&db, 10_000, 10), vec!["m-live".to_string()]);
}

/// The staleness rule the whole table turns on: a `ready` row is current only
/// while the entry's text still hashes to what was normalized. `reinforce`
/// bumping `entry_version` is deliberately not part of that question.
#[tokio::test]
async fn a_ready_row_leaves_the_queue_until_the_text_moves_under_it() {
    let (_home, db) = open_state();
    seed_entry_with_text(&db, "m-1", "исходный текст", 1_000).await;

    assert_eq!(queued(&db, 10_000, 10), vec!["m-1".to_string()]);
    let outcome = write_normalization(
        &db,
        "m-1",
        NormalizationStatus::Translated,
        "исходный текст",
        Some("source text"),
        CURRENT_NORMALIZER_VERSION,
        1,
        None,
        2_000,
    )
    .await;
    assert_eq!(outcome, UpsertOutcome::Written);
    assert!(queued(&db, 10_000, 10).is_empty(), "normalized and current");

    db.writer()
        .transaction(|tx| {
            tx.execute(
                "UPDATE memory_entry SET text = 'переписанный текст' WHERE memory_id = 'm-1'",
                [],
            )
        })
        .await
        .expect("edit the text out from under the row");
    assert_eq!(
        queued(&db, 10_000, 10),
        vec!["m-1".to_string()],
        "a stale variant must come back to the queue"
    );
}

/// `skipped` is a real answer, not an absence: an already-English entry costs
/// zero inference and must not be offered again.
#[tokio::test]
async fn a_skipped_entry_stays_out_of_the_queue() {
    let (_home, db) = open_state();
    seed_entry_with_text(&db, "m-1", "already english", 1_000).await;
    write_normalization(
        &db,
        "m-1",
        NormalizationStatus::English,
        "already english",
        None,
        CURRENT_NORMALIZER_VERSION,
        0,
        None,
        2_000,
    )
    .await;
    assert!(queued(&db, 10_000, 10).is_empty());
}

/// A failure backs off on the clock and gives up after
/// `MAX_NORMALIZATION_ATTEMPTS` — the entry then simply keeps using its
/// original text, which is the pre-normalization behaviour.
#[tokio::test]
async fn a_failed_entry_waits_for_its_backoff_and_then_gives_up() {
    let (_home, db) = open_state();
    seed_entry_with_text(&db, "m-1", "исходный текст", 1_000).await;
    write_normalization(
        &db,
        "m-1",
        NormalizationStatus::Failed,
        "исходный текст",
        None,
        CURRENT_NORMALIZER_VERSION,
        1,
        Some(5_000),
        2_000,
    )
    .await;

    assert!(queued(&db, 4_999, 10).is_empty(), "before next_attempt_at");
    assert_eq!(
        queued(&db, 5_000, 10),
        vec!["m-1".to_string()],
        "at next_attempt_at it is due again"
    );

    write_normalization(
        &db,
        "m-1",
        NormalizationStatus::Failed,
        "исходный текст",
        None,
        CURRENT_NORMALIZER_VERSION,
        MAX_NORMALIZATION_ATTEMPTS,
        Some(5_000),
        6_000,
    )
    .await;
    assert!(
        queued(&db, 100_000, 10).is_empty(),
        "past the attempt cap it stops being offered, however long we wait"
    );
}

/// A newer normalizer re-normalizes everything — including rows that are
/// `ready` and current, and rows that already exhausted their attempts.
#[tokio::test]
async fn a_newer_normalizer_version_re_queues_every_row() {
    let (_home, db) = open_state();
    seed_entry_with_text(&db, "m-ready", "исходный текст", 1_000).await;
    seed_entry_with_text(&db, "m-spent", "другой текст", 1_001).await;
    write_normalization(
        &db,
        "m-ready",
        NormalizationStatus::Translated,
        "исходный текст",
        Some("source text"),
        CURRENT_NORMALIZER_VERSION - 1,
        1,
        None,
        2_000,
    )
    .await;
    write_normalization(
        &db,
        "m-spent",
        NormalizationStatus::Failed,
        "другой текст",
        None,
        CURRENT_NORMALIZER_VERSION - 1,
        MAX_NORMALIZATION_ATTEMPTS,
        None,
        2_000,
    )
    .await;

    assert_eq!(
        queued(&db, 10_000, 10),
        vec!["m-ready".to_string(), "m-spent".to_string()],
        "an older normalizer_version is due whatever the status says"
    );
}

/// Oldest first, and `limit` really bounds the batch — the worker translates a
/// bounded number of entries per tick.
#[tokio::test]
async fn the_queue_is_oldest_first_and_respects_its_limit() {
    let (_home, db) = open_state();
    seed_entry_with_text(&db, "m-3", "третий", 3_000).await;
    seed_entry_with_text(&db, "m-1", "первый", 1_000).await;
    seed_entry_with_text(&db, "m-2", "второй", 2_000).await;

    assert_eq!(
        queued(&db, 10_000, 10),
        vec!["m-1".to_string(), "m-2".to_string(), "m-3".to_string()],
    );
    assert_eq!(
        queued(&db, 10_000, 2),
        vec!["m-1".to_string(), "m-2".to_string()]
    );
}

/// The counts T21-08's `stats`/`doctor` will read, over a real store.
#[tokio::test]
async fn normalization_counts_group_by_status_over_a_real_store() {
    let (_home, db) = open_state();
    seed_entry_with_text(&db, "m-1", "исходный текст", 1_000).await;
    seed_entry_with_text(&db, "m-2", "already english", 1_001).await;
    write_normalization(
        &db,
        "m-1",
        NormalizationStatus::Translated,
        "исходный текст",
        Some("source text"),
        CURRENT_NORMALIZER_VERSION,
        1,
        None,
        2_000,
    )
    .await;
    write_normalization(
        &db,
        "m-2",
        NormalizationStatus::English,
        "already english",
        None,
        CURRENT_NORMALIZER_VERSION,
        0,
        None,
        2_000,
    )
    .await;

    let read = db.open_read().expect("read conn");
    assert_eq!(
        normalization_counts(&read).expect("counts"),
        // `ORDER BY status` is the reader's own contract, so the expected
        // order is alphabetical on the stored value: english < translated.
        vec![
            NormalizationCountRow {
                status: NormalizationStatus::English,
                count: 1,
            },
            NormalizationCountRow {
                status: NormalizationStatus::Translated,
                count: 1,
            },
        ],
    );
    assert_eq!(
        normalization_for(&read, "m-1")
            .expect("row")
            .expect("present")
            .source_text
            .as_deref(),
        Some("source text"),
    );
}

/// D-074: the delete path and the backfill path must agree about which cache
/// row belongs to an entry, or a purge removes nothing and reports success.
/// Both go through `subject_memory_entry`; this pins that they still do.
#[tokio::test]
async fn memory_subject_hash_agrees_with_the_backfills_own_subject_key() {
    let (_home, db) = open_state();
    let id = memory(&db, 90, MemoryKind::Fact).await;

    let read = db.open_read().expect("read conn");
    let hash = local_rag_store::memory_subject_hash(&read, &id)
        .expect("read subject hash")
        .expect("the entry exists");
    let keys = local_rag_store::memory_entry_subject_keys(&read, "rep-1").expect("subject keys");

    assert!(
        keys.iter().any(|k| k.subject_hash == hash
            && k.subject_kind == local_rag_store::SubjectKind::MemoryEntry),
        "the purge's hash must be one the backfill would have produced",
    );
    assert_eq!(
        local_rag_store::memory_subject_hash(&read, "no-such-entry").expect("read"),
        None,
        "an unknown id has no subject rather than an empty-text hash",
    );
}
