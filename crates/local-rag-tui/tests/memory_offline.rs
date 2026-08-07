//! T18-04 fixture-store tests: `compute_memory_data` against a known store, exercising both the
//! entries and candidates paths, every filter, pagination, and entry-detail+evidence — mirrors
//! this crate's own `tests/repositories_offline.rs`/`tests/status_offline.rs` per-file-fixture
//! convention (seed helpers duplicated here from `crates/local-rag/tests/support/mod.rs:390-567`,
//! which are private to `local-rag`'s own test binary).

use std::path::Path;

use local_rag::daemon::gitroot;
use local_rag_core::paths::StoreLayout;
use local_rag_store::{
    CandidateState, EvidenceKind, MemoryKind, MemoryState, NewMemoryEntry, NewMemoryEvidence,
    ProposedOperation, ScopeKind, StateDb, create_repository, create_worktree,
    insert_memory_evidence, insert_projection_state, observe_repository_path,
    observe_worktree_path, propose_candidate, transition_memory_entry,
};
use local_rag_test_support::TempHome;
use local_rag_tui::memory::{
    ListNav, MemoryMode, MemoryNav, MemoryScreenData, compute_memory_data,
};

fn open_layout() -> (TempHome, StoreLayout) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    (home, layout)
}

/// Register one active `{repo, worktree}` at `path`, the same fixture shape
/// `tests/repositories_offline.rs::seed_active_repo_and_worktree` already establishes.
async fn seed_active_repo_and_worktree(
    layout: &StoreLayout,
    repo_id: &str,
    worktree_id: &str,
    path: &Path,
) {
    let facts = gitroot::probe(path).expect("probe the seeded path");
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    let (repo_id, worktree_id) = (repo_id.to_string(), worktree_id.to_string());
    db.writer()
        .transaction(move |tx| {
            create_repository(tx, &repo_id, facts.remote_fingerprint.as_deref(), 1_000)?;
            create_worktree(tx, &worktree_id, &repo_id, facts.kind, 1_000)?;
            observe_worktree_path(
                tx,
                &worktree_id,
                &facts.observed_canonical_path,
                &facts.display_path,
                &facts.path_fingerprint,
                1_000,
            )?;
            observe_repository_path(tx, &repo_id, &facts.observed_canonical_path, 1_000)?;
            insert_projection_state(tx, &worktree_id, 1_000)
        })
        .await
        .expect("seed active repo+worktree");
}

/// Insert one `active` `memory_entry` row directly (mirrors `crates/local-rag/tests/support/
/// mod.rs::seed_memory_entry`).
#[allow(clippy::too_many_arguments)]
async fn seed_memory_entry(
    layout: &StoreLayout,
    memory_id: &str,
    kind: MemoryKind,
    scope_kind: ScopeKind,
    scope_owner_id: &str,
    text: &str,
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
            local_rag_store::create_memory_entry(
                tx,
                &NewMemoryEntry {
                    memory_id: &id,
                    kind,
                    text: &text,
                    canonical_key: None,
                    scope_kind,
                    scope_owner_id: &owner,
                    confidence: 0.5,
                    importance: 0.5,
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

async fn seed_observation(layout: &StoreLayout, observation_id: &str) {
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    let oid = observation_id.to_string();
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
}

async fn seed_memory_evidence(layout: &StoreLayout, memory_id: &str, observation_id: &str) {
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    let (mid, oid) = (memory_id.to_string(), observation_id.to_string());
    db.writer()
        .transaction(move |tx| {
            insert_memory_evidence(
                tx,
                &NewMemoryEvidence {
                    memory_id: &mid,
                    observation_id: &oid,
                    evidence_kind: EvidenceKind::UserStatement,
                    session_id: "sess-1",
                    agent_id: None,
                    commit_hash: None,
                },
            )
        })
        .await
        .expect("seed memory evidence");
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

fn entries_nav() -> MemoryNav {
    MemoryNav::List(ListNav::default())
}

fn candidates_nav() -> MemoryNav {
    MemoryNav::List(ListNav {
        mode: MemoryMode::Candidates,
        ..ListNav::default()
    })
}

// ---- 1: unresolved cwd sees only global scope ----

#[tokio::test]
async fn unresolved_cwd_sees_only_global_scope_entries() {
    let (home, layout) = open_layout();
    let repo_path = home.join("repo-a");
    std::fs::create_dir_all(&repo_path).expect("create repo dir");
    seed_active_repo_and_worktree(&layout, "repo-a", "wt-a", &repo_path).await;

    seed_memory_entry(
        &layout,
        "mem-global",
        MemoryKind::Fact,
        ScopeKind::Global,
        local_rag_store::GLOBAL_SCOPE_OWNER_ID,
        "a global fact",
        1_000,
    )
    .await;
    seed_memory_entry(
        &layout,
        "mem-repo",
        MemoryKind::Fact,
        ScopeKind::Repository,
        "repo-a",
        "a repo fact",
        1_000,
    )
    .await;

    let unresolved_cwd = home.join("not-a-worktree");
    std::fs::create_dir_all(&unresolved_cwd).expect("create unresolved cwd");
    let data = compute_memory_data(&layout, &unresolved_cwd, &entries_nav());
    match data {
        MemoryScreenData::EntryList {
            scope_label, rows, ..
        } => {
            assert_eq!(scope_label, "global");
            assert_eq!(rows.len(), 1, "{rows:?}");
            assert_eq!(rows[0].memory_id, "mem-global");
        }
        other => panic!("expected EntryList, got {other:?}"),
    }
}

// ---- 2: resolved repo+worktree unions all three scopes, sorted ----

#[tokio::test]
async fn resolved_worktree_unions_all_three_scopes_sorted_by_created_at() {
    let (home, layout) = open_layout();
    let repo_path = home.join("repo-b");
    std::fs::create_dir_all(&repo_path).expect("create repo dir");
    seed_active_repo_and_worktree(&layout, "repo-b", "wt-b", &repo_path).await;

    seed_memory_entry(
        &layout,
        "mem-3",
        MemoryKind::Fact,
        ScopeKind::Worktree,
        "wt-b",
        "third",
        3_000,
    )
    .await;
    seed_memory_entry(
        &layout,
        "mem-1",
        MemoryKind::Fact,
        ScopeKind::Global,
        local_rag_store::GLOBAL_SCOPE_OWNER_ID,
        "first",
        1_000,
    )
    .await;
    seed_memory_entry(
        &layout,
        "mem-2",
        MemoryKind::Fact,
        ScopeKind::Repository,
        "repo-b",
        "second",
        2_000,
    )
    .await;

    let data = compute_memory_data(&layout, &repo_path, &entries_nav());
    match data {
        MemoryScreenData::EntryList {
            scope_label,
            rows,
            total,
            ..
        } => {
            assert_eq!(scope_label, "repo:repo-b");
            assert_eq!(total, 3);
            let ids: Vec<&str> = rows.iter().map(|r| r.memory_id.as_str()).collect();
            assert_eq!(ids, ["mem-1", "mem-2", "mem-3"], "{ids:?}");
        }
        other => panic!("expected EntryList, got {other:?}"),
    }
}

// ---- 3: kind filter narrows ----

#[tokio::test]
async fn kind_filter_narrows_to_matching_rows_only() {
    let (home, layout) = open_layout();
    let repo_path = home.join("repo-c");
    std::fs::create_dir_all(&repo_path).expect("create repo dir");
    seed_active_repo_and_worktree(&layout, "repo-c", "wt-c", &repo_path).await;

    seed_memory_entry(
        &layout,
        "mem-fact",
        MemoryKind::Fact,
        ScopeKind::Global,
        local_rag_store::GLOBAL_SCOPE_OWNER_ID,
        "a fact",
        1_000,
    )
    .await;
    seed_memory_entry(
        &layout,
        "mem-decision",
        MemoryKind::Decision,
        ScopeKind::Global,
        local_rag_store::GLOBAL_SCOPE_OWNER_ID,
        "a decision",
        2_000,
    )
    .await;

    let nav = MemoryNav::List(ListNav {
        kind_filter: Some(MemoryKind::Decision),
        ..ListNav::default()
    });
    let data = compute_memory_data(&layout, &repo_path, &nav);
    match data {
        MemoryScreenData::EntryList { rows, total, .. } => {
            assert_eq!(total, 1, "{rows:?}");
            assert_eq!(rows[0].memory_id, "mem-decision");
        }
        other => panic!("expected EntryList, got {other:?}"),
    }
}

// ---- 4: state filter (unfiltered) includes terminal states, unlike recall ----

#[tokio::test]
async fn unfiltered_entry_list_includes_terminal_states_unlike_recall() {
    let (home, layout) = open_layout();
    let repo_path = home.join("repo-d");
    std::fs::create_dir_all(&repo_path).expect("create repo dir");
    seed_active_repo_and_worktree(&layout, "repo-d", "wt-d", &repo_path).await;

    seed_memory_entry(
        &layout,
        "mem-active",
        MemoryKind::Fact,
        ScopeKind::Global,
        local_rag_store::GLOBAL_SCOPE_OWNER_ID,
        "active fact",
        1_000,
    )
    .await;
    seed_memory_entry(
        &layout,
        "mem-retracted",
        MemoryKind::Fact,
        ScopeKind::Global,
        local_rag_store::GLOBAL_SCOPE_OWNER_ID,
        "retracted fact",
        2_000,
    )
    .await;
    transition_seeded_memory_entry(&layout, "mem-retracted", MemoryState::Retracted).await;

    let data = compute_memory_data(&layout, &repo_path, &entries_nav());
    match data {
        MemoryScreenData::EntryList { rows, total, .. } => {
            assert_eq!(total, 2, "{rows:?}");
            let retracted = rows
                .iter()
                .find(|r| r.memory_id == "mem-retracted")
                .expect("retracted row present");
            assert_eq!(retracted.state, MemoryState::Retracted);
        }
        other => panic!("expected EntryList, got {other:?}"),
    }

    let nav = MemoryNav::List(ListNav {
        entry_state_filter: Some(MemoryState::Retracted),
        ..ListNav::default()
    });
    let filtered = compute_memory_data(&layout, &repo_path, &nav);
    match filtered {
        MemoryScreenData::EntryList { rows, total, .. } => {
            assert_eq!(total, 1, "{rows:?}");
            assert_eq!(rows[0].memory_id, "mem-retracted");
        }
        other => panic!("expected EntryList, got {other:?}"),
    }
}

// ---- 5: scope filter narrows to one scope ----

#[tokio::test]
async fn scope_filter_narrows_to_one_scope() {
    let (home, layout) = open_layout();
    let repo_path = home.join("repo-e");
    std::fs::create_dir_all(&repo_path).expect("create repo dir");
    seed_active_repo_and_worktree(&layout, "repo-e", "wt-e", &repo_path).await;

    seed_memory_entry(
        &layout,
        "mem-global",
        MemoryKind::Fact,
        ScopeKind::Global,
        local_rag_store::GLOBAL_SCOPE_OWNER_ID,
        "global",
        1_000,
    )
    .await;
    seed_memory_entry(
        &layout,
        "mem-worktree",
        MemoryKind::Fact,
        ScopeKind::Worktree,
        "wt-e",
        "worktree",
        2_000,
    )
    .await;

    let nav = MemoryNav::List(ListNav {
        scope_filter: Some(ScopeKind::Worktree),
        ..ListNav::default()
    });
    let data = compute_memory_data(&layout, &repo_path, &nav);
    match data {
        MemoryScreenData::EntryList { rows, total, .. } => {
            assert_eq!(total, 1, "{rows:?}");
            assert_eq!(rows[0].memory_id, "mem-worktree");
        }
        other => panic!("expected EntryList, got {other:?}"),
    }
}

// ---- 6: entry pagination, has_more ----

#[tokio::test]
async fn entry_pagination_reports_has_more_across_two_pages() {
    let (home, layout) = open_layout();
    let repo_path = home.join("repo-f");
    std::fs::create_dir_all(&repo_path).expect("create repo dir");
    seed_active_repo_and_worktree(&layout, "repo-f", "wt-f", &repo_path).await;

    for i in 0..12 {
        seed_memory_entry(
            &layout,
            &format!("mem-{i:02}"),
            MemoryKind::Fact,
            ScopeKind::Global,
            local_rag_store::GLOBAL_SCOPE_OWNER_ID,
            "text",
            1_000 + i,
        )
        .await;
    }

    let first_page = compute_memory_data(&layout, &repo_path, &entries_nav());
    match first_page {
        MemoryScreenData::EntryList {
            rows,
            total,
            has_more,
            ..
        } => {
            assert_eq!(total, 12);
            assert_eq!(rows.len(), 10, "{rows:?}");
            assert!(has_more);
            assert_eq!(rows[0].memory_id, "mem-00");
        }
        other => panic!("expected EntryList, got {other:?}"),
    }

    let second_page = compute_memory_data(
        &layout,
        &repo_path,
        &MemoryNav::List(ListNav {
            offset: 10,
            ..ListNav::default()
        }),
    );
    match second_page {
        MemoryScreenData::EntryList { rows, has_more, .. } => {
            assert_eq!(rows.len(), 2, "{rows:?}");
            assert!(!has_more);
            assert_eq!(rows[0].memory_id, "mem-10");
        }
        other => panic!("expected EntryList, got {other:?}"),
    }
}

// ---- 7: candidates mode + state filter ----

#[tokio::test]
async fn candidates_mode_with_state_filter() {
    let (_home, layout) = open_layout();
    StateDb::open(layout.state_db()).expect("bootstrap state.sqlite");

    seed_pending_candidate(
        &layout,
        "cand-pending",
        "mem-target-1",
        "some-worktree",
        1_000,
    )
    .await;
    seed_pending_candidate(
        &layout,
        "cand-other",
        "mem-target-2",
        "some-worktree",
        2_000,
    )
    .await;

    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    db.writer()
        .transaction(|tx| local_rag_store::reject_candidate(tx, "cand-other"))
        .await
        .expect("reject tx (infrastructure)")
        .expect("reject (domain)");

    let data = compute_memory_data(&layout, Path::new("/nonexistent"), &candidates_nav());
    match data {
        MemoryScreenData::CandidateList { rows, .. } => {
            assert_eq!(rows.len(), 2, "{rows:?}");
        }
        other => panic!("expected CandidateList, got {other:?}"),
    }

    let nav = MemoryNav::List(ListNav {
        mode: MemoryMode::Candidates,
        candidate_state_filter: Some(CandidateState::Pending),
        ..ListNav::default()
    });
    let filtered = compute_memory_data(&layout, Path::new("/nonexistent"), &nav);
    match filtered {
        MemoryScreenData::CandidateList { rows, .. } => {
            assert_eq!(rows.len(), 1, "{rows:?}");
            assert_eq!(rows[0].candidate_id, "cand-pending");
        }
        other => panic!("expected CandidateList, got {other:?}"),
    }
}

// ---- 8: candidate pagination (over-fetch-by-one) ----

#[tokio::test]
async fn candidate_pagination_reports_has_more() {
    let (_home, layout) = open_layout();
    StateDb::open(layout.state_db()).expect("bootstrap state.sqlite");

    for i in 0..11 {
        seed_pending_candidate(
            &layout,
            &format!("cand-{i:02}"),
            &format!("mem-target-{i}"),
            "some-worktree",
            1_000 + i,
        )
        .await;
    }

    let data = compute_memory_data(&layout, Path::new("/nonexistent"), &candidates_nav());
    match data {
        MemoryScreenData::CandidateList { rows, has_more, .. } => {
            assert_eq!(rows.len(), 10, "{rows:?}");
            assert!(has_more);
        }
        other => panic!("expected CandidateList, got {other:?}"),
    }
}

// ---- 9: entry detail with evidence, ascending by id ----

#[tokio::test]
async fn entry_detail_shows_full_fields_and_evidence() {
    let (_home, layout) = open_layout();
    StateDb::open(layout.state_db()).expect("bootstrap state.sqlite");

    seed_memory_entry(
        &layout,
        "mem-detail",
        MemoryKind::Decision,
        ScopeKind::Global,
        local_rag_store::GLOBAL_SCOPE_OWNER_ID,
        "a decision worth remembering",
        1_000,
    )
    .await;
    seed_observation(&layout, "obs-2").await;
    seed_observation(&layout, "obs-1").await;
    seed_memory_evidence(&layout, "mem-detail", "obs-2").await;
    seed_memory_evidence(&layout, "mem-detail", "obs-1").await;

    let nav = MemoryNav::EntryDetail {
        memory_id: "mem-detail".to_string(),
        list: ListNav::default(),
    };
    let data = compute_memory_data(&layout, Path::new("/nonexistent"), &nav);
    match data {
        MemoryScreenData::EntryDetail {
            entry,
            evidence_ids,
        } => {
            assert_eq!(entry.memory_id, "mem-detail");
            assert_eq!(entry.kind, MemoryKind::Decision);
            assert_eq!(entry.text, "a decision worth remembering");
            let mut ids = evidence_ids.clone();
            ids.sort();
            assert_eq!(ids, vec!["obs-1".to_string(), "obs-2".to_string()]);
        }
        other => panic!("expected EntryDetail, got {other:?}"),
    }
}

// ---- 10: entry with no evidence ----

#[tokio::test]
async fn entry_detail_with_no_evidence_is_an_empty_vec_not_unavailable() {
    let (_home, layout) = open_layout();
    StateDb::open(layout.state_db()).expect("bootstrap state.sqlite");

    seed_memory_entry(
        &layout,
        "mem-lonely",
        MemoryKind::Fact,
        ScopeKind::Global,
        local_rag_store::GLOBAL_SCOPE_OWNER_ID,
        "no evidence here",
        1_000,
    )
    .await;

    let nav = MemoryNav::EntryDetail {
        memory_id: "mem-lonely".to_string(),
        list: ListNav::default(),
    };
    let data = compute_memory_data(&layout, Path::new("/nonexistent"), &nav);
    match data {
        MemoryScreenData::EntryDetail { evidence_ids, .. } => {
            assert!(evidence_ids.is_empty(), "{evidence_ids:?}");
        }
        other => panic!("expected EntryDetail, got {other:?}"),
    }
}

// ---- 11: vanished entry is Unavailable, not a panic ----

#[tokio::test]
async fn entry_detail_for_a_vanished_entry_is_unavailable() {
    let (_home, layout) = open_layout();
    StateDb::open(layout.state_db()).expect("bootstrap state.sqlite");

    let nav = MemoryNav::EntryDetail {
        memory_id: "mem-ghost".to_string(),
        list: ListNav::default(),
    };
    let data = compute_memory_data(&layout, Path::new("/nonexistent"), &nav);
    match data {
        MemoryScreenData::Unavailable { reason } => {
            assert!(reason.contains("mem-ghost"), "{reason}");
        }
        other => panic!("expected Unavailable, got {other:?}"),
    }
}

// ---- 12: uninitialized store is Unavailable ----

#[test]
fn memory_is_unavailable_before_the_store_is_ever_initialized() {
    let (_home, layout) = open_layout();
    // `layout.ensure()` only creates the directory tree — no `state.sqlite` exists yet.
    let data = compute_memory_data(&layout, Path::new("/nonexistent"), &entries_nav());
    match data {
        MemoryScreenData::Unavailable { reason } => {
            assert!(reason.contains("not yet initialized"), "{reason}");
        }
        other => panic!("expected Unavailable, got {other:?}"),
    }
}
