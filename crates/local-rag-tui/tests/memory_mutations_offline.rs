//! T18-05 round-trip fixture tests: `execute_memory_action` against a real `state.sqlite`,
//! covering every action's success path plus every typed domain rejection the card names
//! (optimistic-conflict/illegal-transition/entry-terminal) surfacing without a panic — same
//! per-file-fixture convention as `tests/memory_offline.rs`, seed helpers duplicated from
//! `crates/local-rag/tests/support/mod.rs:390-567`.
//!
//! Every test is a plain synchronous `#[test]`, not `#[tokio::test]`: `execute_memory_action`
//! drives its own throwaway tokio runtime internally (`local_rag_tui::rt::block_on`, crate-
//! internal — this external test binary never sees it), and tokio forbids starting a runtime from
//! inside a runtime already driving the current thread. Seeding still needs `.await` (every store
//! write goes through `StateWriter::transaction`, an `async fn`), so this file carries its own
//! tiny `block_on` — the same throwaway-runtime-per-call shape `local_rag_tui::rt::block_on`
//! itself uses — to drive exactly the seed calls, never overlapping with `execute_memory_action`'s
//! own runtime because each `block_on` call builds, drives, and drops its runtime before the next
//! one starts; only sequential (never nested) runtimes ever touch this test thread.

use std::future::Future;

use local_rag_core::paths::StoreLayout;
use local_rag_store::{
    CandidateState, MemoryKind, MemoryState, NewMemoryEntry, ProposedOperation, ScopeKind, StateDb,
    create_memory_entry, memory_entry_by_id, propose_candidate, reject_candidate,
    transition_memory_entry,
};
use local_rag_test_support::TempHome;
use local_rag_tui::memory::{ListNav, MemoryAction, MemoryNav, execute_memory_action};

fn block_on<F: Future>(fut: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a current-thread tokio runtime")
        .block_on(fut)
}

fn open_layout() -> (TempHome, StoreLayout) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    (home, layout)
}

#[allow(clippy::too_many_arguments)]
async fn seed_memory_entry(
    layout: &StoreLayout,
    memory_id: &str,
    kind: MemoryKind,
    scope_kind: ScopeKind,
    scope_owner_id: &str,
    text: &str,
    importance: f64,
    now_ms: i64,
) {
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    let (id, owner, text) = (
        memory_id.to_string(),
        scope_owner_id.to_string(),
        text.to_string(),
    );
    db.writer()
        .transaction(move |tx| {
            create_memory_entry(
                tx,
                &NewMemoryEntry {
                    memory_id: &id,
                    kind,
                    text: &text,
                    canonical_key: None,
                    scope_kind,
                    scope_owner_id: &owner,
                    confidence: 0.5,
                    importance,
                    valid_from_tree: None,
                    last_verified_tree: None,
                    supersedes_id: None,
                },
                now_ms,
            )
        })
        .await
        .expect("seed memory entry tx (infrastructure)")
        .expect("seed memory entry (domain)");
}

async fn transition_seeded_memory_entry(layout: &StoreLayout, memory_id: &str, to: MemoryState) {
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    let id = memory_id.to_string();
    db.writer()
        .transaction(move |tx| transition_memory_entry(tx, &id, to))
        .await
        .expect("transition tx (infrastructure)")
        .expect("transition (domain)");
}

async fn seed_pending_candidate(
    layout: &StoreLayout,
    candidate_id: &str,
    target_memory_id: &str,
    scope_owner_id: &str,
    now_ms: i64,
) {
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    let (cid, target, owner) = (
        candidate_id.to_string(),
        target_memory_id.to_string(),
        scope_owner_id.to_string(),
    );
    db.writer()
        .transaction(move |tx| {
            let op = ProposedOperation::Create {
                memory_id: target,
                kind: "fact".to_string(),
                text: "candidate-proposed text".to_string(),
                canonical_key: None,
                scope_kind: "worktree".to_string(),
                scope_owner_id: owner,
                confidence: 0.5,
                importance: 0.5,
                valid_from_tree: None,
                last_verified_tree: None,
            };
            propose_candidate(tx, &cid, &op, &[], &[], now_ms)
        })
        .await
        .expect("propose candidate tx");
}

async fn reject_seeded_candidate(layout: &StoreLayout, candidate_id: &str) {
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    let cid = candidate_id.to_string();
    db.writer()
        .transaction(move |tx| reject_candidate(tx, &cid))
        .await
        .expect("reject tx (infrastructure)")
        .expect("reject (domain)");
}

fn ok_action_result(nav: MemoryNav) -> String {
    match nav {
        MemoryNav::ActionResult {
            message,
            is_error: false,
            ..
        } => message,
        MemoryNav::ActionResult {
            message,
            is_error: true,
            ..
        } => panic!("expected success, got error: {message}"),
        other => panic!("expected ActionResult, got {other:?}"),
    }
}

fn error_action_result(nav: MemoryNav) -> String {
    match nav {
        MemoryNav::ActionResult {
            message,
            is_error: true,
            ..
        } => message,
        MemoryNav::ActionResult {
            message,
            is_error: false,
            ..
        } => panic!("expected an error, got success: {message}"),
        other => panic!("expected ActionResult, got {other:?}"),
    }
}

// ---- 1: approve materializes the proposed create ----

#[test]
fn approve_materializes_the_proposed_create() {
    let (_home, layout) = open_layout();
    StateDb::open(layout.state_db()).expect("bootstrap state.sqlite");
    block_on(seed_pending_candidate(
        &layout, "cand-1", "mem-new", "wt-a", 1_000,
    ));

    let message = ok_action_result(execute_memory_action(
        &layout,
        MemoryAction::Approve {
            candidate_id: "cand-1".to_string(),
            list: ListNav::default(),
        },
    ));
    assert!(message.contains("approved cand-1"), "{message}");

    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    let conn = db.open_read().expect("read connection");
    let entry = memory_entry_by_id(&conn, "mem-new")
        .expect("read entry")
        .expect("entry materialized");
    assert_eq!(entry.kind, MemoryKind::Fact);
    assert_eq!(entry.state, MemoryState::Active);
}

// ---- 2: reject moves a pending candidate to rejected ----

#[test]
fn reject_moves_pending_candidate_to_rejected() {
    let (_home, layout) = open_layout();
    StateDb::open(layout.state_db()).expect("bootstrap state.sqlite");
    block_on(seed_pending_candidate(
        &layout, "cand-1", "mem-new", "wt-a", 1_000,
    ));

    let message = ok_action_result(execute_memory_action(
        &layout,
        MemoryAction::Reject {
            candidate_id: "cand-1".to_string(),
            list: ListNav::default(),
        },
    ));
    assert!(message.contains("rejected cand-1"), "{message}");

    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    let conn = db.open_read().expect("read connection");
    let rows = local_rag_store::list_candidates(&conn, Some(CandidateState::Rejected), 10, 0)
        .expect("list candidates");
    assert!(rows.iter().any(|r| r.candidate_id == "cand-1"), "{rows:?}");
}

// ---- 3: edit updates text/importance and bumps entry_version ----

#[test]
fn edit_updates_text_and_importance_and_bumps_version() {
    let (_home, layout) = open_layout();
    StateDb::open(layout.state_db()).expect("bootstrap state.sqlite");
    block_on(seed_memory_entry(
        &layout,
        "mem-1",
        MemoryKind::Fact,
        ScopeKind::Global,
        local_rag_store::GLOBAL_SCOPE_OWNER_ID,
        "original text",
        0.5,
        1_000,
    ));

    let message = ok_action_result(execute_memory_action(
        &layout,
        MemoryAction::Edit {
            memory_id: "mem-1".to_string(),
            expected_version: 1,
            text: "updated text".to_string(),
            importance: 0.9,
            list: ListNav::default(),
        },
    ));
    assert!(message.contains("edited mem-1"), "{message}");

    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    let conn = db.open_read().expect("read connection");
    let entry = memory_entry_by_id(&conn, "mem-1")
        .expect("read entry")
        .expect("entry exists");
    assert_eq!(entry.text, "updated text");
    assert_eq!(entry.importance, 0.9);
    assert_eq!(entry.entry_version, 2);
}

// ---- 4: retract transitions an active Fact entry to retracted ----

#[test]
fn retract_transitions_active_fact_entry_to_retracted() {
    let (_home, layout) = open_layout();
    StateDb::open(layout.state_db()).expect("bootstrap state.sqlite");
    block_on(seed_memory_entry(
        &layout,
        "mem-1",
        MemoryKind::Fact,
        ScopeKind::Global,
        local_rag_store::GLOBAL_SCOPE_OWNER_ID,
        "text",
        0.5,
        1_000,
    ));

    let message = ok_action_result(execute_memory_action(
        &layout,
        MemoryAction::Retract {
            memory_id: "mem-1".to_string(),
            expected_version: 1,
            list: ListNav::default(),
        },
    ));
    assert!(message.contains("retracted mem-1"), "{message}");

    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    let conn = db.open_read().expect("read connection");
    let entry = memory_entry_by_id(&conn, "mem-1")
        .expect("read entry")
        .expect("entry exists");
    assert_eq!(entry.state, MemoryState::Retracted);
}

// ---- 5: merge supersedes the loser and points supersedes_id at the survivor ----

#[test]
fn merge_supersedes_the_loser_pointing_at_the_survivor() {
    let (_home, layout) = open_layout();
    StateDb::open(layout.state_db()).expect("bootstrap state.sqlite");
    block_on(seed_memory_entry(
        &layout,
        "mem-survivor",
        MemoryKind::Fact,
        ScopeKind::Global,
        local_rag_store::GLOBAL_SCOPE_OWNER_ID,
        "survivor text",
        0.5,
        1_000,
    ));
    block_on(seed_memory_entry(
        &layout,
        "mem-loser",
        MemoryKind::Fact,
        ScopeKind::Global,
        local_rag_store::GLOBAL_SCOPE_OWNER_ID,
        "loser text",
        0.5,
        1_000,
    ));

    let message = ok_action_result(execute_memory_action(
        &layout,
        MemoryAction::Merge {
            survivor_id: "mem-survivor".to_string(),
            survivor_expected_version: 1,
            losers: vec![("mem-loser".to_string(), 1)],
            list: ListNav::default(),
        },
    ));
    assert!(
        message.contains("merged 1 loser(s) into mem-survivor"),
        "{message}"
    );

    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    let conn = db.open_read().expect("read connection");
    let loser = memory_entry_by_id(&conn, "mem-loser")
        .expect("read loser")
        .expect("loser exists");
    assert_eq!(loser.state, MemoryState::Superseded);
    assert_eq!(loser.supersedes_id.as_deref(), Some("mem-survivor"));
}

// ---- 6: OptimisticConflict surfaces without panicking ----

#[test]
fn optimistic_conflict_surfaces_without_panicking() {
    let (_home, layout) = open_layout();
    StateDb::open(layout.state_db()).expect("bootstrap state.sqlite");
    block_on(seed_memory_entry(
        &layout,
        "mem-1",
        MemoryKind::Fact,
        ScopeKind::Global,
        local_rag_store::GLOBAL_SCOPE_OWNER_ID,
        "text",
        0.5,
        1_000,
    ));

    let message = error_action_result(execute_memory_action(
        &layout,
        MemoryAction::Edit {
            memory_id: "mem-1".to_string(),
            expected_version: 999,
            text: "new text".to_string(),
            importance: 0.5,
            list: ListNav::default(),
        },
    ));
    assert!(message.contains("optimistic conflict"), "{message}");
}

// ---- 7: IllegalTransition surfaces without panicking (hypothesis has no retracted state) ----

#[test]
fn illegal_transition_surfaces_without_panicking() {
    let (_home, layout) = open_layout();
    StateDb::open(layout.state_db()).expect("bootstrap state.sqlite");
    block_on(seed_memory_entry(
        &layout,
        "mem-1",
        MemoryKind::Hypothesis,
        ScopeKind::Global,
        local_rag_store::GLOBAL_SCOPE_OWNER_ID,
        "a hypothesis",
        0.5,
        1_000,
    ));

    let message = error_action_result(execute_memory_action(
        &layout,
        MemoryAction::Retract {
            memory_id: "mem-1".to_string(),
            expected_version: 1,
            list: ListNav::default(),
        },
    ));
    assert!(message.contains("illegal memory transition"), "{message}");
}

// ---- 8: EntryTerminal surfaces without panicking ----

#[test]
fn entry_terminal_surfaces_without_panicking() {
    let (_home, layout) = open_layout();
    StateDb::open(layout.state_db()).expect("bootstrap state.sqlite");
    block_on(seed_memory_entry(
        &layout,
        "mem-1",
        MemoryKind::Fact,
        ScopeKind::Global,
        local_rag_store::GLOBAL_SCOPE_OWNER_ID,
        "text",
        0.5,
        1_000,
    ));
    block_on(transition_seeded_memory_entry(
        &layout,
        "mem-1",
        MemoryState::Retracted,
    ));

    let message = error_action_result(execute_memory_action(
        &layout,
        MemoryAction::Edit {
            memory_id: "mem-1".to_string(),
            // `transition_memory_entry` only writes `state` — it does not bump `entry_version`
            // (spec 04 §5's own as-built note), so the seeded entry is still v1 after retracting.
            expected_version: 1,
            text: "new text".to_string(),
            importance: 0.5,
            list: ListNav::default(),
        },
    ));
    assert_eq!(message, "entry is in a terminal state and cannot be edited");
}

// ---- 9: approving an already-rejected candidate surfaces IllegalTransition without panicking ----

#[test]
fn approve_on_a_rejected_candidate_surfaces_illegal_transition_without_panicking() {
    let (_home, layout) = open_layout();
    StateDb::open(layout.state_db()).expect("bootstrap state.sqlite");
    block_on(seed_pending_candidate(
        &layout, "cand-1", "mem-new", "wt-a", 1_000,
    ));
    block_on(reject_seeded_candidate(&layout, "cand-1"));

    // Only `approve → approve` short-circuits to `AlreadyApproved` (spec 04 §6's own carve-out);
    // `rejected → approved` is an ordinary illegal-transition rejection, `ReviewError::
    // IllegalTransition`, not `NotPending` (that variant is `edit_memory_candidate`'s own, not in
    // T18-05's action set — candidates here are only ever approved/rejected, never edited).
    let message = error_action_result(execute_memory_action(
        &layout,
        MemoryAction::Approve {
            candidate_id: "cand-1".to_string(),
            list: ListNav::default(),
        },
    ));
    assert!(
        message.contains("illegal candidate transition"),
        "{message}"
    );
}

// ---- 10: write path refuses before the store is ever initialized ----

#[test]
fn write_path_refuses_before_the_store_is_ever_initialized() {
    let (_home, layout) = open_layout();
    // `layout.ensure()` only creates the directory tree — no `state.sqlite` exists yet.
    let message = error_action_result(execute_memory_action(
        &layout,
        MemoryAction::Retract {
            memory_id: "mem-1".to_string(),
            expected_version: 1,
            list: ListNav::default(),
        },
    ));
    assert!(message.contains("not yet initialized"), "{message}");
}
