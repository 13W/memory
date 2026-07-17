//! T02-04 acceptance tests for request-root resolution and re-attach (spec
//! 02 §3.3, 04 §7, 12 §7, 01 §5).
//!
//! All tests are deterministic: an isolated [`TempHome`], fixed `now_ms`
//! literals, and ids minted from [`uuidv7_from`] with fixed entropy (no
//! `SystemUuidV7`, so no wall clock or `/dev/urandom`), and no git/filesystem
//! access — the resolver operates purely on caller-supplied
//! [`WorktreeRootFacts`]. Writer operations run through
//! [`StateWriter::transaction`]; reads use [`StateDb::open_read`].

use local_rag_core::identity::domain::path_fingerprint;
use local_rag_core::identity::remote;
use local_rag_core::identity::uuidv7_from;
use local_rag_core::paths::StoreLayout;
use local_rag_store::StateDb;
use local_rag_store::registry::{
    AttachError, Candidate, IllegalWorktreeTransition, RequestRoot, Resolution, WorktreeKind,
    WorktreeRootFacts, WorktreeState, attach, create_repository, create_worktree, current_path,
    current_worktree_path, observe_repository_path, observe_worktree_path, path_history, resolve,
    transition_worktree_state, worktree_path_history, worktree_state,
};
use local_rag_store::rusqlite::Connection;
use local_rag_test_support::TempHome;

/// A temporary store with an ensured tree and an opened [`StateDb`] (production
/// migration set: registry v1 + worktree v2).
fn open_state() -> (TempHome, StateDb) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    (home, db)
}

/// A distinct, deterministic UUIDv7 string; `seed` varies the last entropy byte.
fn uuid(seed: u8) -> String {
    let mut rand = [0u8; 10];
    rand[9] = seed;
    uuidv7_from(1000, rand).to_string()
}

/// Build `WorktreeRootFacts` for `path` (display = path, computed fingerprint),
/// with no common-dir hint.
fn facts(path: &str, kind: WorktreeKind, remote_fp: Option<&str>) -> WorktreeRootFacts {
    WorktreeRootFacts {
        observed_canonical_path: path.to_string(),
        display_path: path.to_string(),
        path_fingerprint: path_fingerprint(path),
        kind,
        common_dir_fingerprint: None,
        remote_fingerprint: remote_fp.map(str::to_string),
    }
}

fn candidate(repo_id: &str, worktree_id: &str, kind: WorktreeKind) -> Candidate {
    Candidate {
        repo_id: repo_id.to_string(),
        worktree_id: worktree_id.to_string(),
        kind,
    }
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).expect("count")
}

/// Create a repository (optional remote fingerprint) in one transaction.
async fn create_repo(db: &StateDb, repo_id: &str, remote_fp: Option<&str>, now: i64) {
    let (id, fp) = (repo_id.to_string(), remote_fp.map(str::to_string));
    db.writer()
        .transaction(move |tx| create_repository(tx, &id, fp.as_deref(), now))
        .await
        .expect("create repo");
}

/// Register a worktree of `kind` at `path` (discovery): create the row, observe
/// its current path, and — for a root tree (main/non_git) — the repository path.
async fn register_worktree(
    db: &StateDb,
    repo_id: &str,
    worktree_id: &str,
    kind: WorktreeKind,
    path: &str,
    now: i64,
) {
    let (repo, wt, p) = (
        repo_id.to_string(),
        worktree_id.to_string(),
        path.to_string(),
    );
    let fp = path_fingerprint(path);
    db.writer()
        .transaction(move |tx| {
            create_worktree(tx, &wt, &repo, kind, now)?;
            observe_worktree_path(tx, &wt, &p, &p, &fp, now)?;
            if kind != WorktreeKind::Linked {
                observe_repository_path(tx, &repo, &p, now)?;
            }
            Ok(())
        })
        .await
        .expect("register worktree");
}

/// Observe a new current path for an existing worktree (worktree side only).
async fn observe_worktree_at(db: &StateDb, worktree_id: &str, path: &str, now: i64) {
    let (wt, p) = (worktree_id.to_string(), path.to_string());
    let fp = path_fingerprint(path);
    db.writer()
        .transaction(move |tx| observe_worktree_path(tx, &wt, &p, &p, &fp, now))
        .await
        .expect("observe worktree path");
}

/// Drive a worktree to `to`, asserting the transition is legal.
async fn set_state(db: &StateDb, worktree_id: &str, to: WorktreeState) {
    let wt = worktree_id.to_string();
    db.writer()
        .transaction(move |tx| transition_worktree_state(tx, &wt, to))
        .await
        .expect("transition tx")
        .expect("legal transition");
}

/// Run [`attach`] in one transaction and return the domain outcome.
async fn attach_at(
    db: &StateDb,
    repo_id: &str,
    worktree_id: &str,
    facts: WorktreeRootFacts,
    now: i64,
) -> Result<(), AttachError> {
    let (repo, wt) = (repo_id.to_string(), worktree_id.to_string());
    db.writer()
        .transaction(move |tx| attach(tx, &repo, &wt, &facts, now))
        .await
        .expect("attach tx")
}

/// Resolve `facts` (with an optional `repo_hint`).
fn resolve_root(db: &StateDb, facts: WorktreeRootFacts, hint: Option<&str>) -> Resolution {
    let read = db.open_read().expect("read conn");
    resolve(
        &read,
        &RequestRoot {
            worktree_root: Some(facts),
            repo_hint: hint.map(str::to_string),
        },
    )
    .expect("resolve")
}

/// A directory move (detach + `repo attach` at the new path) preserves both the
/// repository and worktree ids; the old path is retained as history.
#[tokio::test]
async fn directory_move_preserves_repo_and_worktree_ids() {
    let (_home, db) = open_state();
    let (repo, wt) = (uuid(1), uuid(101));
    create_repo(&db, &repo, None, 1000).await;
    register_worktree(&db, &repo, &wt, WorktreeKind::Main, "/old", 1000).await;

    assert_eq!(
        resolve_root(&db, facts("/old", WorktreeKind::Main, None), None),
        Resolution::Resolved {
            repo_id: repo.clone(),
            worktree_id: wt.clone()
        },
        "resolves at the original path",
    );

    // Move: detach, then re-attach at the new path.
    set_state(&db, &wt, WorktreeState::Detached).await;
    assert_eq!(
        attach_at(
            &db,
            &repo,
            &wt,
            facts("/new", WorktreeKind::Main, None),
            2000
        )
        .await,
        Ok(()),
    );

    assert_eq!(
        resolve_root(&db, facts("/new", WorktreeKind::Main, None), None),
        Resolution::Resolved {
            repo_id: repo.clone(),
            worktree_id: wt.clone()
        },
        "same identity, now at /new",
    );
    assert_eq!(
        resolve_root(&db, facts("/old", WorktreeKind::Main, None), None),
        Resolution::GlobalOnly,
        "the old path no longer resolves",
    );

    let read = db.open_read().expect("read");
    assert_eq!(
        count(&read, "SELECT count(*) FROM worktree"),
        1,
        "no new worktree"
    );
    assert_eq!(
        count(&read, "SELECT count(*) FROM repository"),
        1,
        "no new repo"
    );
    assert_eq!(
        worktree_state(&read, &wt).expect("state"),
        Some(WorktreeState::Active),
        "reattach drove it back to active",
    );
    let wt_hist = worktree_path_history(&read, &wt).expect("wt history");
    assert!(
        wt_hist
            .iter()
            .any(|p| p.observed_canonical_path == "/old" && !p.is_current),
        "/old retained on the worktree, no longer current",
    );
    assert!(
        wt_hist
            .iter()
            .any(|p| p.observed_canonical_path == "/new" && p.is_current),
    );
    let repo_hist = path_history(&read, &repo).expect("repo history");
    assert!(
        repo_hist
            .iter()
            .any(|p| p.observed_path == "/old" && !p.is_current)
    );
    assert_eq!(
        current_path(&read, &repo).expect("cur"),
        Some("/new".to_string())
    );
}

/// A path recreated after its worktree moved away does not inherit the moved
/// worktree's identity: the still-active moved worktree is not a reattach
/// candidate, so the freed path resolves to a freshly registered identity.
#[tokio::test]
async fn recreated_path_does_not_steal_identity() {
    let (_home, db) = open_state();
    let (repo, wt) = (uuid(2), uuid(102));
    create_repo(&db, &repo, None, 1000).await;
    register_worktree(&db, &repo, &wt, WorktreeKind::Main, "/p", 1000).await;

    // The worktree moves to /p2 (stays active); /p becomes history.
    observe_worktree_at(&db, &wt, "/p2", 2000).await;
    assert_eq!(
        resolve_root(&db, facts("/p", WorktreeKind::Main, None), None),
        Resolution::GlobalOnly,
        "the fingerprint match to the still-active worktree does not steal identity",
    );

    // A fresh repo+worktree is registered at /p (caller mints new ids).
    let (repo2, wt2) = (uuid(3), uuid(103));
    create_repo(&db, &repo2, None, 3000).await;
    register_worktree(&db, &repo2, &wt2, WorktreeKind::Main, "/p", 3000).await;

    assert_ne!(wt2, wt);
    assert_ne!(repo2, repo);
    assert_eq!(
        resolve_root(&db, facts("/p", WorktreeKind::Main, None), None),
        Resolution::Resolved {
            repo_id: repo2.clone(),
            worktree_id: wt2.clone()
        },
        "/p resolves to the new identity",
    );

    let read = db.open_read().expect("read");
    assert_eq!(
        worktree_state(&read, &wt).expect("state"),
        Some(WorktreeState::Active),
        "old worktree untouched",
    );
    assert_eq!(
        current_worktree_path(&read, &wt).expect("cur"),
        Some("/p2".to_string()),
    );
}

/// Two detached linked worktrees of one repository cannot be resolved
/// automatically — not even with a repo-level hint; an explicit attach is
/// required. The common-dir fingerprint is modelled by the advisory remote hint.
#[tokio::test]
async fn linked_ambiguity_requires_id() {
    let (_home, db) = open_state();
    let repo = uuid(4);
    let fp = remote::fingerprint("git@github.com:org/repo.git");
    let (main_wt, wl1, wl2) = (uuid(104), uuid(105), uuid(106));
    create_repo(&db, &repo, Some(&fp), 1000).await;
    register_worktree(&db, &repo, &main_wt, WorktreeKind::Main, "/main", 1000).await;
    register_worktree(&db, &repo, &wl1, WorktreeKind::Linked, "/a", 1000).await;
    register_worktree(&db, &repo, &wl2, WorktreeKind::Linked, "/b", 1000).await;
    set_state(&db, &wl1, WorktreeState::Detached).await;
    set_state(&db, &wl2, WorktreeState::Detached).await;

    let f = facts("/moved", WorktreeKind::Linked, Some(&fp));
    // wl1 = uuid(105) < wl2 = uuid(106): candidates come back sorted by id.
    let mut expected = vec![
        candidate(&repo, &wl1, WorktreeKind::Linked),
        candidate(&repo, &wl2, WorktreeKind::Linked),
    ];
    expected.sort_by(|a, b| a.worktree_id.cmp(&b.worktree_id));
    assert_eq!(
        resolve_root(&db, f.clone(), None),
        Resolution::Ambiguous {
            candidates: expected.clone()
        },
        "two detached linked worktrees with no ID ⇒ ambiguous",
    );
    assert!(
        matches!(
            resolve_root(&db, f.clone(), Some(&repo)),
            Resolution::Ambiguous { .. }
        ),
        "a repo-level hint cannot pick between two linked worktrees of the repo",
    );

    // Explicit attach binds one; afterwards it resolves by current path.
    assert_eq!(attach_at(&db, &repo, &wl1, f.clone(), 2000).await, Ok(()));
    assert_eq!(
        resolve_root(&db, facts("/moved", WorktreeKind::Linked, None), None),
        Resolution::Resolved {
            repo_id: repo.clone(),
            worktree_id: wl1.clone()
        },
    );
    let read = db.open_read().expect("read");
    assert_eq!(
        worktree_state(&read, &wl1).expect("state"),
        Some(WorktreeState::Active),
    );
}

/// A root that matches no current path (and no request root at all) resolves to
/// global scope only — never an error.
#[tokio::test]
async fn unknown_root_resolves_global_only() {
    let (_home, db) = open_state();
    let (repo, wt) = (uuid(5), uuid(107));
    create_repo(&db, &repo, None, 1000).await;
    register_worktree(&db, &repo, &wt, WorktreeKind::Main, "/some/where", 1000).await;

    assert_eq!(
        resolve_root(&db, facts("/never/seen", WorktreeKind::Main, None), None),
        Resolution::GlobalOnly,
    );

    let read = db.open_read().expect("read");
    assert_eq!(
        resolve(&read, &RequestRoot::default()).expect("resolve none"),
        Resolution::GlobalOnly,
        "no worktree_root at all ⇒ global only",
    );
}

/// A non-git directory resolves by path and, on move, syncs the repository path
/// (its stored kind is not linked).
#[tokio::test]
async fn non_git_happy_path() {
    let (_home, db) = open_state();
    let (repo, wt) = (uuid(6), uuid(108));
    create_repo(&db, &repo, None, 1000).await;
    register_worktree(&db, &repo, &wt, WorktreeKind::NonGit, "/dir", 1000).await;

    assert_eq!(
        resolve_root(&db, facts("/dir", WorktreeKind::NonGit, None), None),
        Resolution::Resolved {
            repo_id: repo.clone(),
            worktree_id: wt.clone()
        },
    );

    set_state(&db, &wt, WorktreeState::Detached).await;
    assert_eq!(
        attach_at(
            &db,
            &repo,
            &wt,
            facts("/dir2", WorktreeKind::NonGit, None),
            2000
        )
        .await,
        Ok(()),
    );
    assert_eq!(
        resolve_root(&db, facts("/dir2", WorktreeKind::NonGit, None), None),
        Resolution::Resolved {
            repo_id: repo.clone(),
            worktree_id: wt.clone()
        },
    );
    let read = db.open_read().expect("read");
    assert_eq!(
        current_path(&read, &repo).expect("cur"),
        Some("/dir2".to_string()),
        "non_git syncs repository_path (stored kind != linked)",
    );
}

/// Attaching a worktree id that does not exist is a domain error and writes
/// nothing.
#[tokio::test]
async fn attach_unknown_worktree() {
    let (_home, db) = open_state();
    let (repo, ghost) = (uuid(7), uuid(207));
    create_repo(&db, &repo, None, 1000).await;

    assert_eq!(
        attach_at(
            &db,
            &repo,
            &ghost,
            facts("/x", WorktreeKind::Main, None),
            1000
        )
        .await,
        Err(AttachError::UnknownWorktree),
    );
    let read = db.open_read().expect("read");
    assert_eq!(count(&read, "SELECT count(*) FROM worktree"), 0);
    assert_eq!(count(&read, "SELECT count(*) FROM worktree_path"), 0);
}

/// Attaching with the wrong `repo_id` is a domain error and mutates nothing.
#[tokio::test]
async fn attach_repo_mismatch() {
    let (_home, db) = open_state();
    let (repo_a, repo_b, wt) = (uuid(8), uuid(9), uuid(109));
    create_repo(&db, &repo_a, None, 1000).await;
    create_repo(&db, &repo_b, None, 1000).await;
    register_worktree(&db, &repo_a, &wt, WorktreeKind::Main, "/a", 1000).await;

    assert_eq!(
        attach_at(
            &db,
            &repo_b,
            &wt,
            facts("/moved", WorktreeKind::Main, None),
            2000
        )
        .await,
        Err(AttachError::RepoMismatch {
            expected_repo: repo_b.clone(),
            actual_repo: repo_a.clone(),
        }),
    );
    let read = db.open_read().expect("read");
    assert_eq!(
        current_worktree_path(&read, &wt).expect("cur"),
        Some("/a".to_string()),
        "no path mutation on mismatch",
    );
}

/// A `removing` worktree is terminal and cannot be reattached; state and paths
/// are untouched.
#[tokio::test]
async fn attach_removing_is_not_reattachable() {
    let (_home, db) = open_state();
    let (repo, wt) = (uuid(10), uuid(110));
    create_repo(&db, &repo, None, 1000).await;
    register_worktree(&db, &repo, &wt, WorktreeKind::Main, "/a", 1000).await;
    set_state(&db, &wt, WorktreeState::Removing).await;

    assert_eq!(
        attach_at(
            &db,
            &repo,
            &wt,
            facts("/moved", WorktreeKind::Main, None),
            2000
        )
        .await,
        Err(AttachError::NotReattachable(IllegalWorktreeTransition {
            from: WorktreeState::Removing,
            to: WorktreeState::Active,
        })),
    );
    let read = db.open_read().expect("read");
    assert_eq!(
        worktree_state(&read, &wt).expect("state"),
        Some(WorktreeState::Removing),
        "state unchanged",
    );
    assert_eq!(
        current_worktree_path(&read, &wt).expect("cur"),
        Some("/a".to_string()),
    );
    assert!(
        !worktree_path_history(&read, &wt)
            .expect("hist")
            .iter()
            .any(|p| p.observed_canonical_path == "/moved"),
        "no new path observed",
    );
}

/// Re-attaching the same path twice is idempotent: one current path row per side,
/// `first_seen_at` preserved and `last_seen_at` bumped, state active.
#[tokio::test]
async fn attach_is_idempotent_under_retry() {
    let (_home, db) = open_state();
    let (repo, wt) = (uuid(11), uuid(111));
    create_repo(&db, &repo, None, 1000).await;
    register_worktree(&db, &repo, &wt, WorktreeKind::Main, "/a", 1000).await;
    set_state(&db, &wt, WorktreeState::Detached).await;

    assert_eq!(
        attach_at(&db, &repo, &wt, facts("/b", WorktreeKind::Main, None), 2000).await,
        Ok(()),
    );
    assert_eq!(
        attach_at(&db, &repo, &wt, facts("/b", WorktreeKind::Main, None), 3000).await,
        Ok(()),
    );

    let read = db.open_read().expect("read");
    let wt_hist = worktree_path_history(&read, &wt).expect("wt hist");
    assert_eq!(
        wt_hist
            .iter()
            .filter(|p| p.observed_canonical_path == "/b")
            .count(),
        1,
        "no duplicate /b row on retry",
    );
    let b = wt_hist
        .iter()
        .find(|p| p.observed_canonical_path == "/b")
        .expect("b row");
    assert!(b.is_current);
    assert_eq!(b.first_seen_at, 2000, "first_seen preserved");
    assert_eq!(b.last_seen_at, 3000, "last_seen bumped");
    assert_eq!(
        count(
            &read,
            "SELECT count(*) FROM worktree_path WHERE is_current = 1",
        ),
        1,
        "exactly one current path",
    );
    assert_eq!(
        worktree_state(&read, &wt).expect("state"),
        Some(WorktreeState::Active),
    );
}

/// A lone detached candidate never auto-resolves without a signal, but a matching
/// repo hint (the `repo attach <repo_id>` main-worktree path) selects it.
#[tokio::test]
async fn repo_hint_selects_single_detached_main() {
    let (_home, db) = open_state();
    let repo = uuid(12);
    let fp = remote::fingerprint("git@github.com:org/solo.git");
    let wt = uuid(112);
    create_repo(&db, &repo, Some(&fp), 1000).await;
    register_worktree(&db, &repo, &wt, WorktreeKind::Main, "/home", 1000).await;
    set_state(&db, &wt, WorktreeState::Detached).await;

    let f = facts("/moved", WorktreeKind::Main, Some(&fp));
    assert_eq!(
        resolve_root(&db, f.clone(), None),
        Resolution::Ambiguous {
            candidates: vec![candidate(&repo, &wt, WorktreeKind::Main)]
        },
        "a lone detached candidate does not auto-resolve without a hint",
    );
    assert_eq!(
        resolve_root(&db, f.clone(), Some(&repo)),
        Resolution::Resolved {
            repo_id: repo.clone(),
            worktree_id: wt.clone()
        },
        "the repo hint selects the single detached main worktree",
    );
}

/// A common-dir fingerprint is advisory and is never a registry lookup key: with
/// no exact/path-fingerprint/remote match it resolves to global scope only.
#[tokio::test]
async fn common_dir_fingerprint_alone_never_resolves() {
    let (_home, db) = open_state();
    let (repo, wt) = (uuid(13), uuid(113));
    create_repo(&db, &repo, None, 1000).await;
    register_worktree(&db, &repo, &wt, WorktreeKind::Main, "/here", 1000).await;
    set_state(&db, &wt, WorktreeState::Detached).await;

    let mut f = facts("/elsewhere", WorktreeKind::Main, None);
    f.common_dir_fingerprint = Some("some-common-dir-fingerprint".to_string());
    assert_eq!(resolve_root(&db, f, None), Resolution::GlobalOnly);
}
