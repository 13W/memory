//! T18-03 fixture-store tests: `compute_repositories_data` on a known store, exercising all three
//! drill-down levels independently — mirrors `crates/local-rag/tests/cli_repo.rs`'s own
//! `open_layout`/seed-helpers (duplicated here per this workspace's established per-file-fixture
//! convention — those functions are private to `local-rag`'s own test binary) and this crate's own
//! `tests/status_offline.rs`.

use std::path::Path;

use local_rag::daemon::gitroot;
use local_rag_core::paths::StoreLayout;
use local_rag_store::{
    StateDb, WorktreeState, create_repository, create_worktree, insert_projection_state,
    observe_repository_path, observe_worktree_path, transition_worktree_state,
};
use local_rag_test_support::TempHome;
use local_rag_tui::repositories::{
    RepositoriesNav, RepositoriesScreenData, compute_repositories_data,
};

fn open_layout() -> (TempHome, StoreLayout) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    (home, layout)
}

/// Register one active `{repo, worktree}` at `path` (a fresh worktree, current at `path`, in a
/// brand-new repository).
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

/// One more active worktree at `path`, under an already-existing `repo_id`.
async fn seed_sibling_worktree(
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
            create_worktree(tx, &worktree_id, &repo_id, facts.kind, 1_000)?;
            observe_worktree_path(
                tx,
                &worktree_id,
                &facts.observed_canonical_path,
                &facts.display_path,
                &facts.path_fingerprint,
                1_000,
            )?;
            insert_projection_state(tx, &worktree_id, 1_000)
        })
        .await
        .expect("seed sibling worktree");
}

async fn detach(layout: &StoreLayout, worktree_id: &str) {
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    let worktree_id = worktree_id.to_string();
    db.writer()
        .transaction(move |tx| {
            transition_worktree_state(tx, &worktree_id, WorktreeState::Detached, 2_000)
        })
        .await
        .expect("transition worktree state")
        .expect("legal transition");
}

/// A second path observation for an already-seeded worktree — grows its path history without
/// changing its repository/identity.
async fn move_worktree_path(layout: &StoreLayout, worktree_id: &str, path: &Path) {
    let facts = gitroot::probe(path).expect("probe the seeded path");
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    let worktree_id = worktree_id.to_string();
    db.writer()
        .transaction(move |tx| {
            observe_worktree_path(
                tx,
                &worktree_id,
                &facts.observed_canonical_path,
                &facts.display_path,
                &facts.path_fingerprint,
                2_000,
            )
        })
        .await
        .expect("move worktree path");
}

#[tokio::test]
async fn repos_level_shows_worktree_counts_including_a_detached_worktree() {
    let (home, layout) = open_layout();
    let path_a1 = home.join("repo-a-main");
    let path_a2 = home.join("repo-a-linked");
    let path_b1 = home.join("repo-b-main");
    std::fs::create_dir_all(&path_a1).expect("create path_a1");
    std::fs::create_dir_all(&path_a2).expect("create path_a2");
    std::fs::create_dir_all(&path_b1).expect("create path_b1");

    seed_active_repo_and_worktree(&layout, "repo-a", "wt-a1", &path_a1).await;
    seed_sibling_worktree(&layout, "repo-a", "wt-a2", &path_a2).await;
    seed_active_repo_and_worktree(&layout, "repo-b", "wt-b1", &path_b1).await;
    detach(&layout, "wt-a2").await;

    let data = compute_repositories_data(&layout, &RepositoriesNav::Repos { selected: 0 });
    match data {
        RepositoriesScreenData::Repos { rows, .. } => {
            assert_eq!(rows.len(), 2, "{rows:?}");
            let a = rows
                .iter()
                .find(|r| r.repo_id == "repo-a")
                .expect("repo-a present");
            assert_eq!(a.worktree_count, 2);
            let b = rows
                .iter()
                .find(|r| r.repo_id == "repo-b")
                .expect("repo-b present");
            assert_eq!(b.worktree_count, 1);
        }
        other => panic!("expected Repos, got {other:?}"),
    }

    let data = compute_repositories_data(
        &layout,
        &RepositoriesNav::Worktrees {
            repo_id: "repo-a".to_string(),
            selected: 0,
        },
    );
    match data {
        RepositoriesScreenData::Worktrees { worktrees, .. } => {
            assert_eq!(worktrees.len(), 2, "{worktrees:?}");
            let detached = worktrees
                .iter()
                .find(|w| w.worktree_id == "wt-a2")
                .expect("wt-a2 present");
            assert_eq!(detached.state.as_str(), "detached");
            let active = worktrees
                .iter()
                .find(|w| w.worktree_id == "wt-a1")
                .expect("wt-a1 present");
            assert_eq!(active.state.as_str(), "active");
        }
        other => panic!("expected Worktrees, got {other:?}"),
    }
}

#[tokio::test]
async fn worktree_detail_shows_full_path_history_with_exactly_one_current() {
    let (home, layout) = open_layout();
    let path1 = home.join("repo-c-v1");
    let path2 = home.join("repo-c-v2");
    std::fs::create_dir_all(&path1).expect("create path1");
    std::fs::create_dir_all(&path2).expect("create path2");

    seed_active_repo_and_worktree(&layout, "repo-c", "wt-c1", &path1).await;
    move_worktree_path(&layout, "wt-c1", &path2).await;

    let data = compute_repositories_data(
        &layout,
        &RepositoriesNav::WorktreeDetail {
            repo_id: "repo-c".to_string(),
            worktree_id: "wt-c1".to_string(),
        },
    );
    match data {
        RepositoriesScreenData::WorktreeDetail {
            summary,
            current_path,
            history,
        } => {
            assert_eq!(summary.worktree_id, "wt-c1");
            assert!(current_path.is_some());
            assert_eq!(history.len(), 2, "{history:?}");
            assert_eq!(
                history.iter().filter(|h| h.is_current).count(),
                1,
                "{history:?}"
            );
        }
        other => panic!("expected WorktreeDetail, got {other:?}"),
    }
}

#[test]
fn repos_level_on_an_empty_store_is_available_with_no_rows() {
    let (_home, layout) = open_layout();
    StateDb::open(layout.state_db()).expect("bootstrap an empty state.sqlite");

    let data = compute_repositories_data(&layout, &RepositoriesNav::Repos { selected: 0 });
    match data {
        RepositoriesScreenData::Repos { rows, selected } => {
            assert!(rows.is_empty());
            assert_eq!(selected, 0);
        }
        other => panic!("expected Repos, got {other:?}"),
    }
}

#[test]
fn repositories_are_unavailable_before_the_store_is_ever_initialized() {
    let (_home, layout) = open_layout();
    // `layout.ensure()` only creates the directory tree — no `state.sqlite` exists yet.
    let data = compute_repositories_data(&layout, &RepositoriesNav::Repos { selected: 0 });
    match data {
        RepositoriesScreenData::Unavailable { reason } => {
            assert!(reason.contains("not yet initialized"), "{reason}");
        }
        other => panic!("expected Unavailable, got {other:?}"),
    }
}

#[test]
fn worktree_detail_for_a_nonexistent_worktree_is_unavailable_not_a_panic() {
    let (_home, layout) = open_layout();
    StateDb::open(layout.state_db()).expect("bootstrap an empty state.sqlite");

    let data = compute_repositories_data(
        &layout,
        &RepositoriesNav::WorktreeDetail {
            repo_id: "ghost-repo".to_string(),
            worktree_id: "ghost-wt".to_string(),
        },
    );
    match data {
        RepositoriesScreenData::Unavailable { reason } => {
            assert!(reason.contains("ghost-wt"), "{reason}");
        }
        other => panic!("expected Unavailable, got {other:?}"),
    }
}
