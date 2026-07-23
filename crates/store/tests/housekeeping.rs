//! Integration tests for the two DB-facing shard-directory sweeps (spec 05 §8)
//! over real `worktree` rows and real `projection/<worktree_id>` directories:
//!
//! - **T06-03** — `run_orphan_shard_sweep`: "orphan" means *no worktree row at
//!   all*, so a `detached`/`removing` worktree's shard is retained here;
//! - **D-007** — `run_expired_shard_sweep`: the timed grace-destroy of a
//!   `detached`/`removing` worktree's shard, which deviation D-004 originally
//!   deferred out of T06-03 and gate G09 found had run out of owning cards.
//!   The two sweeps are complementary, and the tests below assert that
//!   explicitly (an orphan sweep never destroys a row-backed shard however old;
//!   an expired sweep never destroys an `active` one however old).
//!
//! Deterministic: an isolated [`TempHome`], ids from [`uuidv7_from`] with fixed
//! entropy, and an explicit `now_ms` passed to every clock-sensitive call — no
//! wall clock and no sleeps anywhere.

use std::fs;

use local_rag_core::identity::uuidv7_from;
use local_rag_core::paths::StoreLayout;
use local_rag_store::registry::{
    WorktreeKind, WorktreeState, create_repository, create_worktree, transition_worktree_state,
};
use local_rag_store::{
    SHARD_DESTROY_GRACE_MS, StateDb, run_expired_shard_sweep, run_orphan_shard_sweep,
};
use local_rag_test_support::TempHome;

/// The fixed clock reading every fixture row is created at.
const CREATED_AT: i64 = 1000;

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

/// Create an `active` main worktree under `repo_id`, transitioned to `state` at
/// [`CREATED_AT`]; returns its id.
async fn worktree_in(db: &StateDb, repo_id: &str, seed: u8, state: WorktreeState) -> String {
    worktree_in_at(db, repo_id, seed, state, CREATED_AT).await
}

/// [`worktree_in`], with an explicit clock reading for the transition — i.e.
/// control over the `state_changed_at` the grace period is measured from
/// (D-007).
async fn worktree_in_at(
    db: &StateDb,
    repo_id: &str,
    seed: u8,
    state: WorktreeState,
    changed_at: i64,
) -> String {
    let wt = uuid(seed);
    let (r, w) = (repo_id.to_string(), wt.clone());
    db.writer()
        .transaction(move |tx| create_worktree(tx, &w, &r, WorktreeKind::Main, CREATED_AT))
        .await
        .expect("create worktree");
    if state != WorktreeState::Active {
        let w = wt.clone();
        db.writer()
            .transaction(move |tx| transition_worktree_state(tx, &w, state, changed_at))
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

// ---- D-007: grace-period shard destruction (spec 05 §8) ----

/// The core case: after the `[SPEC: 7 days]` grace period, a `removing` and a
/// `detached` worktree's shards are destroyed, while an `active` worktree's is
/// retained no matter how long it has sat there.
#[tokio::test]
async fn expired_detached_and_removing_shards_are_destroyed_active_retained() {
    let (_home, layout, db) = open_state();
    let repo = repository(&db, 1).await;

    let changed_at = 10_000i64;
    let wt_active = worktree_in_at(&db, &repo, 10, WorktreeState::Active, changed_at).await;
    let wt_detached = worktree_in_at(&db, &repo, 11, WorktreeState::Detached, changed_at).await;
    let wt_removing = worktree_in_at(&db, &repo, 12, WorktreeState::Removing, changed_at).await;
    for wt in [&wt_active, &wt_detached, &wt_removing] {
        make_shard(&layout, wt);
    }

    let now = changed_at + SHARD_DESTROY_GRACE_MS;
    let report = run_expired_shard_sweep(&db, &layout, now, SHARD_DESTROY_GRACE_MS, false)
        .expect("expired sweep");

    let mut removed = report.removed.clone();
    removed.sort();
    let mut expected = vec![wt_detached.clone(), wt_removing.clone()];
    expected.sort();
    assert_eq!(removed, expected);
    assert!(!layout.projection_shard(&wt_detached).exists());
    assert!(!layout.projection_shard(&wt_removing).exists());
    assert!(
        layout.projection_shard(&wt_active).is_dir(),
        "an active worktree's shard is never destroyed by the grace sweep"
    );
}

/// One millisecond before the deadline nothing is destroyed — the sweep is
/// genuinely time-gated, not "destroy any non-active worktree's shard".
#[tokio::test]
async fn shard_is_retained_until_the_grace_period_elapses() {
    let (_home, layout, db) = open_state();
    let repo = repository(&db, 1).await;

    let changed_at = 10_000i64;
    let wt = worktree_in_at(&db, &repo, 10, WorktreeState::Removing, changed_at).await;
    make_shard(&layout, &wt);

    let just_before = changed_at + SHARD_DESTROY_GRACE_MS - 1;
    let report = run_expired_shard_sweep(&db, &layout, just_before, SHARD_DESTROY_GRACE_MS, false)
        .expect("expired sweep");

    assert!(report.is_empty(), "not due yet: {report:?}");
    assert!(layout.projection_shard(&wt).is_dir(), "shard retained");

    // ...and it *is* destroyed one millisecond later.
    let at_deadline = just_before + 1;
    let report = run_expired_shard_sweep(&db, &layout, at_deadline, SHARD_DESTROY_GRACE_MS, false)
        .expect("expired sweep");
    assert_eq!(report.removed, vec![wt.clone()]);
    assert!(!layout.projection_shard(&wt).exists());
}

/// Reattaching a `detached` worktree (`repo attach`, spec 04 §7) restamps
/// `state_changed_at`, so its shard survives a sweep that would otherwise have
/// destroyed it. This is the behavior that makes a stale clock safe: coming back
/// resets the budget.
#[tokio::test]
async fn reattaching_resets_the_grace_budget() {
    let (_home, layout, db) = open_state();
    let repo = repository(&db, 1).await;

    let detached_at = 10_000i64;
    let wt = worktree_in_at(&db, &repo, 10, WorktreeState::Detached, detached_at).await;
    make_shard(&layout, &wt);

    // Reattach just before the deadline.
    let reattached_at = detached_at + SHARD_DESTROY_GRACE_MS - 1;
    let w = wt.clone();
    db.writer()
        .transaction(move |tx| {
            transition_worktree_state(tx, &w, WorktreeState::Active, reattached_at)
        })
        .await
        .expect("transition tx")
        .expect("detached -> active is legal");

    // At the original deadline the shard is no longer due at all.
    let report = run_expired_shard_sweep(
        &db,
        &layout,
        detached_at + SHARD_DESTROY_GRACE_MS,
        SHARD_DESTROY_GRACE_MS,
        false,
    )
    .expect("expired sweep");
    assert!(
        report.is_empty(),
        "reattached worktree is not due: {report:?}"
    );
    assert!(layout.projection_shard(&wt).is_dir());
}

/// A self-transition (`removing → removing`, e.g. a crash/retry re-issuing the
/// same request) must not push the deadline forward — otherwise a retry loop
/// could keep a doomed shard alive indefinitely.
#[tokio::test]
async fn a_self_transition_does_not_extend_the_grace_budget() {
    let (_home, layout, db) = open_state();
    let repo = repository(&db, 1).await;

    let changed_at = 10_000i64;
    let wt = worktree_in_at(&db, &repo, 10, WorktreeState::Removing, changed_at).await;
    make_shard(&layout, &wt);

    // Re-request the state it is already in, much later.
    let w = wt.clone();
    let much_later = changed_at + SHARD_DESTROY_GRACE_MS * 10;
    db.writer()
        .transaction(move |tx| {
            transition_worktree_state(tx, &w, WorktreeState::Removing, much_later)
        })
        .await
        .expect("transition tx")
        .expect("self-transition is an idempotent no-op");

    // The original deadline still governs.
    let report = run_expired_shard_sweep(
        &db,
        &layout,
        changed_at + SHARD_DESTROY_GRACE_MS,
        SHARD_DESTROY_GRACE_MS,
        false,
    )
    .expect("expired sweep");
    assert_eq!(
        report.removed,
        vec![wt.clone()],
        "the no-op must not have restamped the clock"
    );
}

/// A dry run reports what it would destroy and deletes nothing; a real run then
/// destroys it, and a repeat run is a no-op (idempotence, including the case of
/// a `removing` row whose shard is already gone).
#[tokio::test]
async fn expired_sweep_dry_run_then_idempotent_real_run() {
    let (_home, layout, db) = open_state();
    let repo = repository(&db, 1).await;

    let changed_at = 10_000i64;
    let wt = worktree_in_at(&db, &repo, 10, WorktreeState::Removing, changed_at).await;
    make_shard(&layout, &wt);
    let now = changed_at + SHARD_DESTROY_GRACE_MS;

    let dry =
        run_expired_shard_sweep(&db, &layout, now, SHARD_DESTROY_GRACE_MS, true).expect("dry run");
    assert!(dry.dry_run);
    assert_eq!(dry.removed, vec![wt.clone()]);
    assert!(
        layout.projection_shard(&wt).is_dir(),
        "dry run must not delete"
    );

    let first = run_expired_shard_sweep(&db, &layout, now, SHARD_DESTROY_GRACE_MS, false)
        .expect("first real run");
    assert_eq!(first.removed, vec![wt.clone()]);
    assert!(!layout.projection_shard(&wt).exists());

    let second = run_expired_shard_sweep(&db, &layout, now, SHARD_DESTROY_GRACE_MS, false)
        .expect("second real run");
    assert!(
        second.is_empty(),
        "the row still says `removing`, but its shard is gone: {second:?}"
    );
}

/// The two sweeps are complementary and non-overlapping: the orphan sweep never
/// destroys a row-backed shard however old, and the expired sweep never
/// destroys an orphan (which has no row and therefore no clock) — that one
/// belongs to `run_orphan_shard_sweep`.
#[tokio::test]
async fn the_two_sweeps_do_not_overlap() {
    let (_home, layout, db) = open_state();
    let repo = repository(&db, 1).await;

    let changed_at = 10_000i64;
    let wt = worktree_in_at(&db, &repo, 10, WorktreeState::Removing, changed_at).await;
    make_shard(&layout, &wt);
    make_shard(&layout, "orphan-shard");
    let now = changed_at + SHARD_DESTROY_GRACE_MS;

    // The expired sweep takes the row-backed, past-deadline shard only.
    let expired = run_expired_shard_sweep(&db, &layout, now, SHARD_DESTROY_GRACE_MS, false)
        .expect("expired sweep");
    assert_eq!(expired.removed, vec![wt.clone()]);
    assert!(
        layout.projection_shard("orphan-shard").is_dir(),
        "the orphan is not the grace sweep's business"
    );

    // The orphan sweep then takes the orphan.
    let orphans = run_orphan_shard_sweep(&db, &layout, false).expect("orphan sweep");
    assert_eq!(orphans.removed, vec!["orphan-shard".to_string()]);
}
