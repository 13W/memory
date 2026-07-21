//! T06-03 integration tests for the orphan shard-directory sweep (spec 05 §8): the
//! DB-facing `run_orphan_shard_sweep` over real `worktree` rows and real
//! `projection/<worktree_id>` directories.
//!
//! Scope note (deviation D-004): T06-03 is split — only the orphan sweep is
//! implemented here (its foundations, the per-worktree shard layout and the
//! `worktree` registry, exist today). Quarantine rotation, the timed grace-destroy
//! of a `removing` worktree's shard, and spool GC are deferred to their owning cards
//! (T07-04, group 07/09 shard lifecycle, T13-05). These tests therefore prove
//! "orphan = no worktree row", not state-based deletion: a `detached`/`removing`
//! worktree still has a row, so its shard is retained.
//!
//! Deterministic: an isolated [`TempHome`], ids from [`uuidv7_from`] with fixed
//! entropy, no wall clock.

use std::fs;

use local_rag_core::identity::uuidv7_from;
use local_rag_core::paths::StoreLayout;
use local_rag_store::registry::{
    WorktreeKind, WorktreeState, create_repository, create_worktree, transition_worktree_state,
};
use local_rag_store::{StateDb, run_orphan_shard_sweep};
use local_rag_test_support::TempHome;

/// A temporary store: ensured tree + opened [`StateDb`], returning the layout too
/// (the sweep needs `projection_dir`).
fn open_state() -> (TempHome, StoreLayout, StateDb) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    (home, layout, db)
}

/// A distinct, deterministic UUIDv7 keyed by `seed` (no clock/entropy).
fn uuid(seed: u8) -> String {
    let mut rand = [0u8; 10];
    rand[9] = seed;
    uuidv7_from(1000, rand).to_string()
}

/// Create a repository once; returns its id.
async fn repository(db: &StateDb, seed: u8) -> String {
    let repo = uuid(seed);
    let r = repo.clone();
    db.writer()
        .transaction(move |tx| create_repository(tx, &r, None, 1000))
        .await
        .expect("create repository");
    repo
}

/// Create an `active` main worktree under `repo_id`, transitioned to `state`;
/// returns its id.
async fn worktree_in(db: &StateDb, repo_id: &str, seed: u8, state: WorktreeState) -> String {
    let wt = uuid(seed);
    let (r, w) = (repo_id.to_string(), wt.clone());
    db.writer()
        .transaction(move |tx| create_worktree(tx, &w, &r, WorktreeKind::Main, 1000))
        .await
        .expect("create worktree");
    if state != WorktreeState::Active {
        let w = wt.clone();
        db.writer()
            .transaction(move |tx| transition_worktree_state(tx, &w, state))
            .await
            .expect("transition tx")
            .expect("legal transition");
    }
    wt
}

/// Create a shard directory (with a file inside, so it is non-empty).
fn make_shard(layout: &StoreLayout, name: &str) {
    let dir = layout.projection_shard(name);
    fs::create_dir_all(&dir).expect("mkdir shard");
    fs::write(dir.join("segment.bin"), b"x").expect("write shard file");
}

/// An orphan shard (a directory with no worktree row) is swept, while the shards of
/// live worktrees — including `detached` and `removing` ones (they still have a row)
/// — are retained.
#[tokio::test]
async fn orphan_shard_swept_worktree_backed_retained() {
    let (_home, layout, db) = open_state();
    let repo = repository(&db, 1).await;

    let wt_active = worktree_in(&db, &repo, 10, WorktreeState::Active).await;
    let wt_detached = worktree_in(&db, &repo, 11, WorktreeState::Detached).await;
    let wt_removing = worktree_in(&db, &repo, 12, WorktreeState::Removing).await;

    // A shard per live worktree, plus one orphan directory owned by no worktree.
    make_shard(&layout, &wt_active);
    make_shard(&layout, &wt_detached);
    make_shard(&layout, &wt_removing);
    make_shard(&layout, "orphan-shard");

    let report = run_orphan_shard_sweep(&db, &layout, false).expect("sweep");

    assert_eq!(report.removed, vec!["orphan-shard".to_string()]);
    assert_eq!(report.retained, 3, "three worktree-backed shards retained");
    assert!(
        !layout.projection_shard("orphan-shard").exists(),
        "orphan removed"
    );
    // Every worktree-backed shard survives, regardless of state.
    for wt in [&wt_active, &wt_detached, &wt_removing] {
        assert!(
            layout.projection_shard(wt).is_dir(),
            "worktree-backed shard {wt} retained"
        );
    }
}

/// A dry run reports the orphan it would remove but deletes nothing.
#[tokio::test]
async fn dry_run_reports_without_removing() {
    let (_home, layout, db) = open_state();
    let repo = repository(&db, 1).await;
    let wt = worktree_in(&db, &repo, 10, WorktreeState::Active).await;
    make_shard(&layout, &wt);
    make_shard(&layout, "orphan-shard");

    let report = run_orphan_shard_sweep(&db, &layout, true).expect("dry run");

    assert!(report.dry_run);
    assert_eq!(report.removed, vec!["orphan-shard".to_string()]);
    assert!(
        layout.projection_shard("orphan-shard").is_dir(),
        "dry run must not delete"
    );
}

/// Re-running the sweep is idempotent: the first removes the orphan, the second is a
/// no-op, and the live shard is untouched throughout.
#[tokio::test]
async fn repeated_sweep_is_idempotent() {
    let (_home, layout, db) = open_state();
    let repo = repository(&db, 1).await;
    let wt = worktree_in(&db, &repo, 10, WorktreeState::Active).await;
    make_shard(&layout, &wt);
    make_shard(&layout, "orphan-shard");

    let first = run_orphan_shard_sweep(&db, &layout, false).expect("first");
    assert_eq!(first.removed, vec!["orphan-shard".to_string()]);

    let second = run_orphan_shard_sweep(&db, &layout, false).expect("second");
    assert!(second.is_empty(), "second sweep is a no-op: {second:?}");
    assert!(layout.projection_shard(&wt).is_dir(), "live shard retained");
}

/// A freshly-ensured store with no shard directories sweeps to an empty report.
#[tokio::test]
async fn empty_projection_is_a_noop() {
    let (_home, layout, db) = open_state();
    let _repo = repository(&db, 1).await;
    let report = run_orphan_shard_sweep(&db, &layout, false).expect("sweep");
    assert!(report.is_empty());
    assert_eq!(report.retained, 0);
}
