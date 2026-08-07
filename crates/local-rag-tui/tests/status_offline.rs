//! T18-02 fixture-store tests: the Status screen's `probe_daemon`/`read_durable_counts` on a
//! known store, exercising both "daemon state" branches independently of "durable counts" —
//! mirrors `crates/local-rag/tests/cli_stats.rs`'s own `open_layout`/`seed_entry`/`seed_candidate`/
//! `seed_active_repo_and_worktree` helpers (duplicated here per this workspace's established
//! per-file-fixture convention — those functions are private to `local-rag`'s own test binary).

use std::time::Duration;

use local_rag::daemon::{StoreLockInfo, gitroot};
use local_rag_core::paths::StoreLayout;
use local_rag_store::{
    GLOBAL_SCOPE_OWNER_ID, MemoryKind, NewMemoryEntry, ProposedOperation, ScopeKind, StateDb,
    create_memory_entry, create_repository, create_worktree, insert_projection_state,
    observe_repository_path, observe_worktree_path, propose_candidate,
};
use local_rag_test_support::TempHome;
use local_rag_tui::status::{DaemonStatus, DurableCounts, probe_daemon, read_durable_counts};

const PROBE_TIMEOUT: Duration = Duration::from_millis(200);

fn open_layout() -> (TempHome, StoreLayout) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    (home, layout)
}

async fn seed_entry(state: &StateDb, memory_id: &str, kind: MemoryKind, text: &str, now_ms: i64) {
    let (id, text) = (memory_id.to_string(), text.to_string());
    state
        .writer()
        .transaction(move |tx| {
            create_memory_entry(
                tx,
                &NewMemoryEntry {
                    memory_id: &id,
                    kind,
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
                now_ms,
            )
        })
        .await
        .expect("seed entry tx")
        .expect("seed entry domain");
}

async fn seed_candidate(state: &StateDb, candidate_id: &str, now_ms: i64) {
    let cid = candidate_id.to_string();
    state
        .writer()
        .transaction(move |tx| {
            let op = ProposedOperation::Create {
                memory_id: "mem-target".to_string(),
                kind: "fact".to_string(),
                text: "candidate-proposed text".to_string(),
                canonical_key: None,
                scope_kind: "global".to_string(),
                scope_owner_id: GLOBAL_SCOPE_OWNER_ID.to_string(),
                confidence: 0.5,
                importance: 0.5,
                valid_from_tree: None,
                last_verified_tree: None,
            };
            propose_candidate(tx, &cid, &op, &[], &[], now_ms)
        })
        .await
        .expect("seed candidate tx");
}

async fn seed_active_repo_and_worktree(
    layout: &StoreLayout,
    repo_id: &str,
    worktree_id: &str,
    path: &std::path::Path,
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

#[test]
fn status_reflects_absent_lock_file() {
    let (_home, layout) = open_layout();
    assert_eq!(
        probe_daemon(&layout, PROBE_TIMEOUT),
        DaemonStatus::NotRunning
    );
}

#[test]
fn status_reflects_corrupt_lock_file() {
    let (_home, layout) = open_layout();
    std::fs::write(layout.store_lock(), b"not json").expect("write corrupt lock file");
    assert_eq!(
        probe_daemon(&layout, PROBE_TIMEOUT),
        DaemonStatus::NotRunning
    );
}

#[test]
fn status_reflects_starting_when_not_ready_and_pid_alive() {
    let (_home, layout) = open_layout();
    let info = StoreLockInfo {
        instance_uuid: "inst-starting".to_string(),
        pid: std::process::id(),
        daemon_version: "0.0.0".to_string(),
        started_at: 1_000,
        ready: false,
        ready_at: None,
        socket_path: None,
    };
    std::fs::write(
        layout.store_lock(),
        serde_json::to_vec(&info).expect("serialize lock info"),
    )
    .expect("write starting lock file");

    assert_eq!(
        probe_daemon(&layout, PROBE_TIMEOUT),
        DaemonStatus::Starting {
            pid: std::process::id()
        }
    );
}

#[test]
fn status_reflects_not_running_when_ready_but_pid_dead() {
    let (_home, layout) = open_layout();
    let mut child = std::process::Command::new("true")
        .spawn()
        .expect("spawn a trivial child");
    let dead_pid = child.id();
    child.wait().expect("reap the child");

    let info = StoreLockInfo {
        instance_uuid: "inst-dead".to_string(),
        pid: dead_pid,
        daemon_version: "0.0.0".to_string(),
        started_at: 1_000,
        ready: true,
        ready_at: Some(1_100),
        socket_path: Some(layout.socket_path().display().to_string()),
    };
    std::fs::write(
        layout.store_lock(),
        serde_json::to_vec(&info).expect("serialize lock info"),
    )
    .expect("write ready-but-dead lock file");

    assert_eq!(
        probe_daemon(&layout, PROBE_TIMEOUT),
        DaemonStatus::NotRunning
    );
}

#[test]
fn status_reflects_not_running_when_ready_pid_alive_but_socket_unreachable() {
    let (_home, layout) = open_layout();
    let info = StoreLockInfo {
        instance_uuid: "inst-unreachable".to_string(),
        pid: std::process::id(),
        daemon_version: "0.0.0".to_string(),
        started_at: 1_000,
        ready: true,
        ready_at: Some(1_100),
        socket_path: Some(layout.socket_path().display().to_string()),
    };
    std::fs::write(
        layout.store_lock(),
        serde_json::to_vec(&info).expect("serialize lock info"),
    )
    .expect("write ready-but-unreachable lock file");

    // No real listener was ever bound at `layout.socket_path()` — `fetch_welcome` must fail
    // closed, not hang past `PROBE_TIMEOUT`.
    assert_eq!(
        probe_daemon(&layout, PROBE_TIMEOUT),
        DaemonStatus::NotRunning
    );
}

#[tokio::test]
async fn durable_counts_are_independent_of_daemon_state() {
    let (home, layout) = open_layout();
    {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        seed_entry(&state, "mem-a", MemoryKind::Fact, "a fact", 1_000).await;
        seed_entry(&state, "mem-b", MemoryKind::Task, "a task", 2_000).await;
        seed_candidate(&state, "cand-a", 1_000).await;
    }
    seed_active_repo_and_worktree(&layout, "repo-1", "wt-1", home.path()).await;

    // No `store.lock` at all — daemon state is `NotRunning` — but durable counts still read the
    // seeded data, proving the two halves of `StatusScreenData` are computed independently.
    let durable = read_durable_counts(&layout, home.path());
    match durable {
        DurableCounts::Available {
            entries_by_kind_state,
            pending_candidates_by_state,
            worktree,
        } => {
            assert_eq!(entries_by_kind_state.len(), 2, "{entries_by_kind_state:?}");
            assert_eq!(
                pending_candidates_by_state.len(),
                1,
                "{pending_candidates_by_state:?}"
            );
            let worktree = worktree.expect("worktree resolves against the seeded repo");
            assert_eq!(worktree.repo_id, "repo-1");
            assert_eq!(worktree.worktree_id, "wt-1");
            assert!(worktree.projection.is_some());
        }
        other => panic!("expected Available, got {other:?}"),
    }
}

#[test]
fn durable_counts_on_an_empty_store_are_available_with_no_worktree() {
    let (home, layout) = open_layout();
    // `StateDb::open` bootstraps a fresh, fully-migrated store — `diagnose_versions` must see it
    // as `Applied` with an empty `pending` list, not block on it.
    StateDb::open(layout.state_db()).expect("bootstrap an empty state.sqlite");

    let durable = read_durable_counts(&layout, home.path());
    match durable {
        DurableCounts::Available {
            entries_by_kind_state,
            pending_candidates_by_state,
            worktree,
        } => {
            assert!(entries_by_kind_state.is_empty());
            assert!(pending_candidates_by_state.is_empty());
            assert!(worktree.is_none(), "{worktree:?}");
        }
        other => panic!("expected Available, got {other:?}"),
    }
}

#[test]
fn durable_counts_are_unavailable_before_the_store_is_ever_initialized() {
    let (home, layout) = open_layout();
    // `layout.ensure()` only creates the directory tree — no `state.sqlite` exists yet.
    let durable = read_durable_counts(&layout, home.path());
    match durable {
        DurableCounts::Unavailable { reason } => {
            assert!(reason.contains("not yet initialized"), "{reason}");
        }
        other => panic!("expected Unavailable, got {other:?}"),
    }
}
