//! `T23-08` / ADR-0014 Decision 2 acceptance tests for
//! `local_rag_store::memory::fold_all_pending_duplicates` — the multi-group
//! driver over `fold_pending_duplicates` (whose own per-group classification
//! is exhaustively tested in `tests/memory_review.rs`; this file tests only
//! the driver's own job: sequencing, dry-run, and the distinct-claim
//! invariant). Mirrors `tests/housekeeping.rs`'s own candidate-sweep tests in
//! shape, per this crate's per-file-fixture convention.
//!
//! # A note on what is *not* tested here
//!
//! The driver's `Err(_) => retained` branch (a group's read-pass-selected
//! survivor no longer `pending` by the time that group's own write
//! transaction runs) exists as defense in depth, but this file does not
//! attempt to trigger it: `fold_all_pending_duplicates`'s read pass
//! (`pending_candidate_groups`) is immediately followed by per-group writes
//! that each re-derive their own group from a fresh `SELECT` inside their
//! own transaction, so within one single-process, single-writer-queue call
//! there is no point at which an outside actor could observe the stale read
//! and race it — any mutation that happens *before* this function is called
//! is simply reflected in its own fresh read, which is ordinary correctness,
//! not a race. A genuine cross-process race would need actual concurrent
//! writers and non-deterministic timing to reproduce, which this project's
//! own testing rules (`CLAUDE.md`: deterministic, no flaky timing) rule out.
//! `fold_pending_duplicates`'s own reaction to a *pre-existing* non-pending
//! survivor is exhaustively covered by `memory_review.rs`'s
//! `folding_a_non_pending_candidate_is_typed_not_pending`.

use local_rag_core::identity::uuidv7_from;
use local_rag_core::paths::StoreLayout;
use local_rag_store::memory::{
    AUDIT_ENTITY_CANDIDATE, ProposedOperation, approve_candidate, pending_candidate_groups,
    reject_candidate,
};
use local_rag_store::rusqlite::{Connection, params};
use local_rag_store::{
    CandidateState, MemoryKind, NewCandidate, ScopeKind, StateDb, create_candidate,
    fold_all_pending_duplicates, read_audit_events_for_entity,
};
use local_rag_test_support::TempHome;

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

fn create_op(memory_id: &str, scope_owner_id: &str, text: &str) -> ProposedOperation {
    ProposedOperation::Create {
        memory_id: memory_id.to_string(),
        kind: MemoryKind::Fact.as_str().to_string(),
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

/// Seed one exact-duplicate group directly via `create_candidate`, bypassing
/// `propose_candidate`'s own dedup check (`T23-07`) the same way the
/// pre-`T23-07` backlog itself arose. `ids[0]` gets the oldest `created_at`.
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

fn candidate_state_of(conn: &Connection, candidate_id: &str) -> CandidateState {
    conn.query_row(
        "SELECT review_state FROM pending_memory_candidate WHERE candidate_id = ?1",
        params![candidate_id],
        |r| r.get::<_, String>(0),
    )
    .map(|raw| CandidateState::from_db(&raw).expect("valid review_state"))
    .expect("candidate exists")
}

fn audit_row_count(conn: &Connection, candidate_id: &str) -> usize {
    read_audit_events_for_entity(conn, AUDIT_ENTITY_CANDIDATE, candidate_id)
        .expect("read audit rows")
        .len()
}

fn distinct_claim_count(conn: &Connection) -> usize {
    pending_candidate_groups(conn).expect("groups").len()
}

#[tokio::test]
async fn dry_run_reports_the_same_groups_and_writes_nothing() {
    let (_home, db) = open_state();
    let owner = uuid(1);
    duplicate_group(
        &db,
        &["g1-a", "g1-b", "g1-c"],
        &create_op(&uuid(2), &owner, "claim one"),
        1_000,
    )
    .await;
    duplicate_group(
        &db,
        &["g2-a", "g2-b"],
        &create_op(&uuid(3), &owner, "claim two"),
        2_000,
    )
    .await;

    let report = fold_all_pending_duplicates(&db, 5_000, true)
        .await
        .expect("dry run");
    assert!(report.dry_run);
    assert_eq!(report.groups_examined, 2);
    assert_eq!(report.groups_folded, 2);
    assert_eq!(report.folded.len(), 3, "{:?}", report.folded);
    assert_eq!(report.distinct_claims_before, 2);
    assert_eq!(
        report.distinct_claims_after, 2,
        "a dry run does not change the invariant it reports"
    );

    let read = db.open_read().expect("read conn");
    for id in ["g1-a", "g1-b", "g1-c", "g2-a", "g2-b"] {
        assert_eq!(
            candidate_state_of(&read, id),
            CandidateState::Pending,
            "{id}: dry run must not transition anything"
        );
        assert_eq!(
            audit_row_count(&read, id),
            0,
            "{id}: dry run writes no audit row"
        );
    }
}

#[tokio::test]
async fn dedup_run_after_a_run_is_a_no_op() {
    let (_home, db) = open_state();
    let owner = uuid(10);
    duplicate_group(
        &db,
        &["a", "b", "c"],
        &create_op(&uuid(11), &owner, "claim"),
        1_000,
    )
    .await;

    let first = fold_all_pending_duplicates(&db, 5_000, false)
        .await
        .expect("first run");
    assert_eq!(first.folded, vec!["b".to_string(), "c".to_string()]);
    assert_eq!(first.distinct_claims_after, 1);

    let second = fold_all_pending_duplicates(&db, 6_000, false)
        .await
        .expect("second run");
    assert!(
        second.folded.is_empty(),
        "second run is a no-op: {second:?}"
    );
    assert_eq!(
        second.groups_examined, 0,
        "the one distinct claim is now a group of one — nothing left to examine"
    );
    assert_eq!(second.groups_folded, 0);
    assert_eq!(second.distinct_claims_before, 1);
    assert_eq!(second.distinct_claims_after, 1);

    let read = db.open_read().expect("read conn");
    assert_eq!(candidate_state_of(&read, "a"), CandidateState::Pending);
    assert_eq!(
        audit_row_count(&read, "b"),
        1,
        "the first run's audit row is not duplicated"
    );
}

/// A twin already rejected by hand before the run is simply absent from the
/// group `pending_candidate_groups` computes (its `review_state` is no
/// longer `pending`) — the run folds only what remains, without error.
#[tokio::test]
async fn dedup_resumes_after_a_partial_group_collapse() {
    let (_home, db) = open_state();
    let owner = uuid(20);
    duplicate_group(
        &db,
        &["survivor", "twin-a", "twin-b"],
        &create_op(&uuid(21), &owner, "claim"),
        1_000,
    )
    .await;

    let id = "twin-a".to_string();
    db.writer()
        .transaction(move |tx| reject_candidate(tx, &id))
        .await
        .expect("reject tx (infra)")
        .expect("reject twin-a by hand first");

    let report = fold_all_pending_duplicates(&db, 5_000, false)
        .await
        .expect("resume run");
    assert_eq!(
        report.folded,
        vec!["twin-b".to_string()],
        "twin-a is already gone; only twin-b is folded"
    );

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
    assert_eq!(
        audit_row_count(&read, "twin-a"),
        0,
        "an operator's own reject writes no fold audit row"
    );
    assert_eq!(audit_row_count(&read, "twin-b"), 1);
}

/// The card's own acceptance, made executable: the queue is reducible
/// without losing a distinct proposal.
#[tokio::test]
async fn the_distinct_claim_count_is_unchanged_by_a_full_run() {
    let (_home, db) = open_state();
    let owner = uuid(30);
    duplicate_group(
        &db,
        &["g1-a", "g1-b", "g1-c", "g1-d"],
        &create_op(&uuid(31), &owner, "claim one"),
        1_000,
    )
    .await;
    duplicate_group(
        &db,
        &["g2-a", "g2-b"],
        &create_op(&uuid(32), &owner, "claim two"),
        2_000,
    )
    .await;
    duplicate_group(
        &db,
        &["g3-a"],
        &create_op(&uuid(33), &owner, "claim three"),
        3_000,
    )
    .await;

    let read = db.open_read().expect("read conn");
    let before = distinct_claim_count(&read);
    drop(read);
    assert_eq!(before, 3);

    let report = fold_all_pending_duplicates(&db, 5_000, false)
        .await
        .expect("run");
    assert_eq!(report.distinct_claims_before, 3);
    assert_eq!(report.distinct_claims_after, 3);

    let read = db.open_read().expect("read conn");
    let after = distinct_claim_count(&read);
    assert_eq!(after, before, "the invariant the card asks for");
    // 4 + 2 + 1 = 7 rows total; the run folded 3 + 1 = 4 of them, leaving
    // exactly one pending survivor per distinct claim.
    assert_eq!(report.folded.len(), 4, "{:?}", report.folded);
}

#[tokio::test]
async fn folds_every_group_in_one_run_leaving_singletons_and_non_pending_untouched() {
    let (_home, db) = open_state();
    let owner = uuid(40);
    duplicate_group(
        &db,
        &["dup-a", "dup-b"],
        &create_op(&uuid(41), &owner, "duplicated claim"),
        1_000,
    )
    .await;
    duplicate_group(
        &db,
        &["lonely"],
        &create_op(&uuid(42), &owner, "lone claim"),
        1_000,
    )
    .await;

    // A candidate sharing `dup-a`'s exact claim, but already materialized —
    // must never be touched by a bulk fold (folding never approves).
    duplicate_group(
        &db,
        &["already-approved"],
        &create_op(&uuid(43), &owner, "an already-decided claim"),
        1_000,
    )
    .await;
    let id = "already-approved".to_string();
    db.writer()
        .transaction(move |tx| approve_candidate(tx, &id, 1_500))
        .await
        .expect("approve tx (infra)")
        .expect("approve it out of band");

    let report = fold_all_pending_duplicates(&db, 5_000, false)
        .await
        .expect("run");
    assert_eq!(report.folded, vec!["dup-b".to_string()]);
    assert_eq!(
        report.groups_examined, 1,
        "only the duplicated claim is a group at all"
    );

    let read = db.open_read().expect("read conn");
    assert_eq!(candidate_state_of(&read, "dup-a"), CandidateState::Pending);
    assert_eq!(candidate_state_of(&read, "dup-b"), CandidateState::Rejected);
    assert_eq!(
        candidate_state_of(&read, "lonely"),
        CandidateState::Pending,
        "a group of one is never folded"
    );
    assert_eq!(
        candidate_state_of(&read, "already-approved"),
        CandidateState::Approved,
        "a bulk fold never touches an already-materialized candidate"
    );
}
