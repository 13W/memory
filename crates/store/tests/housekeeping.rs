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
use std::sync::atomic::{AtomicU64, Ordering};

use local_rag_core::identity::{Uuid, UuidSource, uuidv7_from};
use local_rag_core::paths::StoreLayout;
use local_rag_core::spool::{FramePayload, encode_frame, encode_segment_header};
use local_rag_store::registry::{
    RequestRoot, WorktreeKind, WorktreeState, allocate_generation, create_repository,
    create_worktree, insert_projection_state, transition_worktree_state,
};
use local_rag_store::{
    CANDIDATE_EXPIRY_MS, CandidateState, NewCandidate, SHARD_DESTROY_GRACE_MS,
    SPOOL_SESSION_ABSENCE_MS, StateDb, candidate_state, create_candidate, import_session_tail,
    run_candidate_expiry_sweep, run_expired_shard_sweep, run_orphan_shard_sweep,
    run_spool_session_sweep, run_unreferenced_space_sweep,
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

// ---- D-011: unreferenced per-model-space shard directories (spec 05 §8, 10 §4) ----

/// Insert an extra model space row in the given state (raw — the registry's own
/// guarded constructors are exercised in `tests/representation.rs`).
async fn insert_model_space(db: &StateDb, id: &str, state: &str) {
    let (i, s) = (id.to_string(), state.to_string());
    db.writer()
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO model_space (model_space_id, display_name, state, created_at, \
                 updated_at) VALUES (?1, ?2, ?3, 1000, 1000)",
                local_rag_store::rusqlite::params![i, format!("space-{i}"), s],
            )
            .map(|_| ())
        })
        .await
        .expect("insert model space");
}

/// Allocate one generation (born `building`) for `worktree_id`; returns its id.
async fn allocate(db: &StateDb, worktree_id: &str, seed: u8) -> String {
    let genr = uuid(seed);
    let (w, g) = (worktree_id.to_string(), genr.clone());
    db.writer()
        .transaction(move |tx| allocate_generation(tx, &w, &g, CREATED_AT).map(|_| ()))
        .await
        .expect("allocate generation");
    genr
}

/// Point a worktree's projection state at `(generation, model_space)` on all
/// three tuples — the shape a completed switch leaves behind.
async fn project_onto(db: &StateDb, worktree_id: &str, generation_id: &str, model_space_id: &str) {
    let w = worktree_id.to_string();
    db.writer()
        .transaction(move |tx| insert_projection_state(tx, &w, CREATED_AT))
        .await
        .expect("init projection state");

    let (w, g, m) = (
        worktree_id.to_string(),
        generation_id.to_string(),
        model_space_id.to_string(),
    );
    db.writer()
        .transaction(move |tx| {
            tx.execute(
                "UPDATE worktree_projection_state \
                 SET active_generation_id = ?2, active_model_space_id = ?3, \
                     projected_generation_id = ?2, projected_model_space_id = ?3 \
                 WHERE worktree_id = ?1",
                local_rag_store::rusqlite::params![w, g, m],
            )
            .map(|_| ())
        })
        .await
        .expect("point projection state");
}

/// Create `projection/<wt>/<space>` with a file inside.
fn make_space_shard(layout: &StoreLayout, wt: &str, space: &str) {
    let dir = layout.projection_shard_space(wt, space);
    fs::create_dir_all(&dir).expect("mkdir space shard");
    fs::write(dir.join("segment.bin"), b"x").expect("write shard file");
}

/// The requirement in one test: a live worktree that migrated off model space A
/// (spec 10 §4 steps 4–6) has A's shard directory reclaimed, while the space it
/// actually runs keeps its own — and the two older sweeps see nothing to do,
/// which is precisely why this third one exists.
#[tokio::test]
async fn a_migrated_away_model_space_shard_is_reclaimed() {
    let (_home, layout, db) = open_state();
    let repo = repository(&db, 1).await;
    let wt = worktree_in(&db, &repo, 10, WorktreeState::Active).await;

    let space_a = uuid(50);
    let space_b = uuid(51);
    insert_model_space(&db, &space_a, "retiring").await;
    insert_model_space(&db, &space_b, "active").await;
    let generation = allocate(&db, &wt, 60).await;
    project_onto(&db, &wt, &generation, &space_b).await;

    make_space_shard(&layout, &wt, &space_a);
    make_space_shard(&layout, &wt, &space_b);

    // Neither existing sweep can see this garbage: the worktree is alive
    // (not an orphan) and `active` (never expired).
    let orphans = run_orphan_shard_sweep(&db, &layout, false).expect("orphan sweep");
    assert!(orphans.is_empty(), "root is worktree-backed: {orphans:?}");
    let expired = run_expired_shard_sweep(&db, &layout, i64::MAX, 0, false).expect("expired sweep");
    assert!(expired.is_empty(), "worktree is active: {expired:?}");
    assert!(layout.projection_shard_space(&wt, &space_a).is_dir());

    let report = run_unreferenced_space_sweep(&db, &layout, false).expect("space sweep");

    assert_eq!(report.removed, vec![format!("{wt}/{space_a}")]);
    assert!(
        !layout.projection_shard_space(&wt, &space_a).exists(),
        "the space the worktree migrated off is reclaimed"
    );
    assert!(
        layout.projection_shard_space(&wt, &space_b).is_dir(),
        "the space it runs is untouched"
    );
    assert!(
        layout.projection_shard(&wt).is_dir(),
        "the worktree's shard root itself survives (spec 05 §8: keyed by worktree_id)"
    );
}

/// A migration in flight is safe without any lock: spec 05 §5 commits the
/// write-ahead (`target_model_space_id`) before the backend is touched, so both
/// buffers are referenced for the whole switch.
#[tokio::test]
async fn a_switch_in_flight_is_never_swept() {
    let (_home, layout, db) = open_state();
    let repo = repository(&db, 1).await;
    let wt = worktree_in(&db, &repo, 10, WorktreeState::Active).await;

    let space_a = uuid(50);
    let space_b = uuid(51);
    insert_model_space(&db, &space_a, "active").await;
    insert_model_space(&db, &space_b, "active").await;
    let generation = allocate(&db, &wt, 60).await;
    project_onto(&db, &wt, &generation, &space_a).await;

    // The write-ahead: target set, backend not yet touched.
    let (w, g, m) = (wt.clone(), generation.clone(), space_b.clone());
    db.writer()
        .transaction(move |tx| {
            tx.execute(
                "UPDATE worktree_projection_state \
                 SET target_generation_id = ?2, target_model_space_id = ?3, status = 'updating' \
                 WHERE worktree_id = ?1",
                local_rag_store::rusqlite::params![w, g, m],
            )
            .map(|_| ())
        })
        .await
        .expect("write-ahead");

    make_space_shard(&layout, &wt, &space_a);
    make_space_shard(&layout, &wt, &space_b);

    let report = run_unreferenced_space_sweep(&db, &layout, false).expect("space sweep");

    assert!(report.is_empty(), "both buffers referenced: {report:?}");
    assert!(layout.projection_shard_space(&wt, &space_a).is_dir());
    assert!(layout.projection_shard_space(&wt, &space_b).is_dir());
}

/// Dry run, then a real run, then a repeat: the sweep is reportable and
/// idempotent like its two siblings.
#[tokio::test]
async fn space_sweep_dry_run_then_idempotent_real_run() {
    let (_home, layout, db) = open_state();
    let repo = repository(&db, 1).await;
    let wt = worktree_in(&db, &repo, 10, WorktreeState::Active).await;

    let space_a = uuid(50);
    let space_b = uuid(51);
    insert_model_space(&db, &space_a, "retiring").await;
    insert_model_space(&db, &space_b, "active").await;
    let generation = allocate(&db, &wt, 60).await;
    project_onto(&db, &wt, &generation, &space_b).await;
    make_space_shard(&layout, &wt, &space_a);

    let dry = run_unreferenced_space_sweep(&db, &layout, true).expect("dry run");
    assert!(dry.dry_run);
    assert_eq!(dry.removed, vec![format!("{wt}/{space_a}")]);
    assert!(layout.projection_shard_space(&wt, &space_a).is_dir());

    let first = run_unreferenced_space_sweep(&db, &layout, false).expect("first");
    assert_eq!(first.removed, vec![format!("{wt}/{space_a}")]);

    let second = run_unreferenced_space_sweep(&db, &layout, false).expect("second");
    assert!(second.is_empty(), "second run is a no-op: {second:?}");
}

/// The three sweeps do not overlap: each takes exactly its own class of garbage
/// in one pass over the same store.
#[tokio::test]
async fn the_three_sweeps_partition_the_work() {
    let (_home, layout, db) = open_state();
    let repo = repository(&db, 1).await;

    let changed_at = 10_000i64;
    let wt_live = worktree_in_at(&db, &repo, 10, WorktreeState::Active, changed_at).await;
    let wt_removing = worktree_in_at(&db, &repo, 11, WorktreeState::Removing, changed_at).await;

    let space_a = uuid(50);
    let space_b = uuid(51);
    insert_model_space(&db, &space_a, "retiring").await;
    insert_model_space(&db, &space_b, "active").await;
    let generation = allocate(&db, &wt_live, 60).await;
    project_onto(&db, &wt_live, &generation, &space_b).await;

    make_space_shard(&layout, &wt_live, &space_a); // unreferenced space
    make_space_shard(&layout, &wt_live, &space_b); // live
    make_shard(&layout, &wt_removing); // past its grace period
    make_shard(&layout, "orphan-shard"); // no worktree row

    let spaces = run_unreferenced_space_sweep(&db, &layout, false).expect("space sweep");
    assert_eq!(spaces.removed, vec![format!("{wt_live}/{space_a}")]);

    let now = changed_at + SHARD_DESTROY_GRACE_MS;
    let expired = run_expired_shard_sweep(&db, &layout, now, SHARD_DESTROY_GRACE_MS, false)
        .expect("expired sweep");
    assert_eq!(expired.removed, vec![wt_removing.clone()]);

    let orphans = run_orphan_shard_sweep(&db, &layout, false).expect("orphan sweep");
    assert_eq!(orphans.removed, vec!["orphan-shard".to_string()]);

    // Only the live pair survives.
    assert!(layout.projection_shard_space(&wt_live, &space_b).is_dir());
    assert!(!layout.projection_shard(&wt_removing).exists());
    assert!(!layout.projection_shard("orphan-shard").exists());
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

// ---- T13-05: spool session GC (spec 07 §6) ----

struct SeqUuidV7 {
    counter: AtomicU64,
}

impl SeqUuidV7 {
    fn new() -> Self {
        Self {
            counter: AtomicU64::new(0),
        }
    }
}

impl UuidSource for SeqUuidV7 {
    fn next_uuid(&self) -> Uuid {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        uuidv7_from(1000 + n, [0xAB; 10])
    }
}

fn spool_fixture(session_id: &str, source_event_id: &str, captured_at: i64) -> FramePayload {
    FramePayload {
        format_version: 1,
        source_event_id: source_event_id.to_string(),
        dedup_key: None,
        event_type: "Stop".to_string(),
        captured_at,
        session_id: session_id.to_string(),
        agent_id: None,
        turn_id: None,
        batch_id: None,
        worktree_root: None,
        commit: None,
        evidence_kind: "model_claim".to_string(),
        trust: "low".to_string(),
        paths: vec![],
        redaction_version: None,
        payload: None,
        short_evidence_excerpt: None,
    }
}

fn write_spool_segment(layout: &StoreLayout, session_id: &str, seq: u32, frames: &[FramePayload]) {
    let session_dir = layout.spool_session(session_id);
    fs::create_dir_all(&session_dir).expect("session dir");
    let mut bytes = encode_segment_header().to_vec();
    for f in frames {
        bytes.extend_from_slice(&encode_frame(f).expect("under the frame cap"));
    }
    fs::write(session_dir.join(format!("{seq:06}.seg")), bytes).expect("write segment");
}

fn cursor_row_exists(db: &StateDb, session_id: &str) -> bool {
    let read = db.open_read().expect("read conn");
    let n: i64 = read
        .query_row(
            "SELECT count(*) FROM spool_import_cursor WHERE session_id = ?1",
            [session_id],
            |r| r.get(0),
        )
        .unwrap();
    n > 0
}

/// A session whose spool data is fully imported (cursor caught up, no further
/// segment) and absent (no new import) for the full budget is GC'd: its
/// directory and its `spool_import_cursor` row both disappear.
#[tokio::test]
async fn fully_committed_absent_session_is_removed() {
    let (_home, layout, db) = open_state();
    let uuids = SeqUuidV7::new();
    let request_root = RequestRoot::default();
    let session = "sess-old";

    write_spool_segment(
        &layout,
        session,
        1,
        &[spool_fixture(session, "st:sess-old:1", 1_000)],
    );
    import_session_tail(&db, &layout, session, &request_root, &uuids, 1_000, 72)
        .await
        .expect("import");

    let now = 1_000 + SPOOL_SESSION_ABSENCE_MS;
    let report = run_spool_session_sweep(&db, &layout, now, SPOOL_SESSION_ABSENCE_MS, false)
        .await
        .expect("sweep");

    assert_eq!(report.removed, vec![session.to_string()]);
    assert!(!layout.spool_session(session).exists());
    assert!(
        !cursor_row_exists(&db, session),
        "the orphaned cursor row is cleaned up too"
    );
}

/// One millisecond short of the absence budget, nothing is swept.
#[tokio::test]
async fn a_session_just_short_of_the_absence_budget_is_retained() {
    let (_home, layout, db) = open_state();
    let uuids = SeqUuidV7::new();
    let request_root = RequestRoot::default();
    let session = "sess-recent";

    write_spool_segment(
        &layout,
        session,
        1,
        &[spool_fixture(session, "st:sess-recent:1", 1_000)],
    );
    import_session_tail(&db, &layout, session, &request_root, &uuids, 1_000, 72)
        .await
        .expect("import");

    let just_before = 1_000 + SPOOL_SESSION_ABSENCE_MS - 1;
    let report =
        run_spool_session_sweep(&db, &layout, just_before, SPOOL_SESSION_ABSENCE_MS, false)
            .await
            .expect("sweep");

    assert!(report.is_empty(), "not due yet: {report:?}");
    assert!(layout.spool_session(session).is_dir());
    assert!(cursor_row_exists(&db, session));
}

/// A session with un-imported bytes past the cursor's `committed_offset` (the
/// hook wrote more after the last import pass) is never swept, however long
/// it has been since that last import — spec 07 §6's "fully committed" half.
#[tokio::test]
async fn uncommitted_segment_is_retained_even_when_absent() {
    let (_home, layout, db) = open_state();
    let uuids = SeqUuidV7::new();
    let request_root = RequestRoot::default();
    let session = "sess-uncommitted";

    write_spool_segment(
        &layout,
        session,
        1,
        &[spool_fixture(session, "st:sess-uncommitted:1", 1_000)],
    );
    import_session_tail(&db, &layout, session, &request_root, &uuids, 1_000, 72)
        .await
        .expect("first import catches up");

    // The hook appends a second frame directly (no re-import happens), so the
    // cursor now legitimately lags behind the file on disk.
    let second_frame = encode_frame(&spool_fixture(session, "st:sess-uncommitted:2", 2_000))
        .expect("under the frame cap");
    let seg_path = layout.spool_session(session).join("000001.seg");
    let mut bytes = fs::read(&seg_path).unwrap();
    bytes.extend_from_slice(&second_frame);
    fs::write(&seg_path, bytes).unwrap();

    let now = 1_000 + SPOOL_SESSION_ABSENCE_MS;
    let report = run_spool_session_sweep(&db, &layout, now, SPOOL_SESSION_ABSENCE_MS, false)
        .await
        .expect("sweep");

    assert!(
        report.is_empty(),
        "uncommitted bytes must retain the session: {report:?}"
    );
    assert!(layout.spool_session(session).is_dir());
    assert!(cursor_row_exists(&db, session));
}

/// A dry run reports what it would remove but deletes nothing; a repeated
/// real run afterward is idempotent.
#[tokio::test]
async fn spool_session_sweep_dry_run_then_idempotent_real_run() {
    let (_home, layout, db) = open_state();
    let uuids = SeqUuidV7::new();
    let request_root = RequestRoot::default();
    let session = "sess-old";

    write_spool_segment(
        &layout,
        session,
        1,
        &[spool_fixture(session, "st:sess-old:1", 1_000)],
    );
    import_session_tail(&db, &layout, session, &request_root, &uuids, 1_000, 72)
        .await
        .expect("import");
    let now = 1_000 + SPOOL_SESSION_ABSENCE_MS;

    let dry = run_spool_session_sweep(&db, &layout, now, SPOOL_SESSION_ABSENCE_MS, true)
        .await
        .expect("dry run");
    assert!(dry.dry_run);
    assert_eq!(dry.removed, vec![session.to_string()]);
    assert!(
        layout.spool_session(session).is_dir(),
        "dry run must not delete"
    );
    assert!(cursor_row_exists(&db, session));

    let first = run_spool_session_sweep(&db, &layout, now, SPOOL_SESSION_ABSENCE_MS, false)
        .await
        .expect("first real run");
    assert_eq!(first.removed, vec![session.to_string()]);

    let second = run_spool_session_sweep(&db, &layout, now, SPOOL_SESSION_ABSENCE_MS, false)
        .await
        .expect("second real run");
    assert!(second.is_empty(), "second sweep is a no-op: {second:?}");
}

/// A session with no cursor row at all (never imported) is never a candidate,
/// regardless of how old its directory looks on disk.
#[tokio::test]
async fn a_session_never_imported_is_never_a_candidate() {
    let (_home, layout, db) = open_state();
    let session = "sess-never-imported";
    write_spool_segment(
        &layout,
        session,
        1,
        &[spool_fixture(session, "st:sess-never-imported:1", 1_000)],
    );

    let report = run_spool_session_sweep(
        &db,
        &layout,
        1_000 + SPOOL_SESSION_ABSENCE_MS,
        SPOOL_SESSION_ABSENCE_MS,
        false,
    )
    .await
    .expect("sweep");

    assert!(report.is_empty());
    assert!(layout.spool_session(session).is_dir());
}

/// Seed a `pending_memory_candidate` row directly, born `pending` at
/// `created_at` — the candidate-expiry sweep's own input.
async fn pending_candidate(db: &StateDb, candidate_id: &str, created_at: i64) {
    let id = candidate_id.to_string();
    db.writer()
        .transaction(move |tx| {
            create_candidate(
                tx,
                &NewCandidate {
                    candidate_id: &id,
                    proposed_operation: "{\"op\":\"resolve\",\"memory_id\":\"m\",\"expected_version\":1}",
                    conflicts: None,
                },
                created_at,
            )
        })
        .await
        .expect("seed pending candidate")
}

/// A `pending` candidate past the 30-day expiry budget (spec 04 §6) is
/// transitioned to `expired`.
#[tokio::test]
async fn candidate_past_expiry_budget_is_expired() {
    let (_home, _layout, db) = open_state();
    pending_candidate(&db, "cand-old", 1_000).await;

    let now = 1_000 + CANDIDATE_EXPIRY_MS;
    let report = run_candidate_expiry_sweep(&db, now, CANDIDATE_EXPIRY_MS, false)
        .await
        .expect("sweep");

    assert_eq!(report.expired, vec!["cand-old".to_string()]);
    assert_eq!(report.retained, 0);
    let read = db.open_read().expect("read conn");
    assert_eq!(
        candidate_state(&read, "cand-old").expect("state"),
        Some(CandidateState::Expired),
    );
}

/// One millisecond short of the expiry budget, the candidate is retained.
#[tokio::test]
async fn candidate_just_short_of_expiry_budget_is_retained() {
    let (_home, _layout, db) = open_state();
    pending_candidate(&db, "cand-recent", 1_000).await;

    let just_before = 1_000 + CANDIDATE_EXPIRY_MS - 1;
    let report = run_candidate_expiry_sweep(&db, just_before, CANDIDATE_EXPIRY_MS, false)
        .await
        .expect("sweep");

    assert!(report.is_empty(), "not due yet: {report:?}");
    assert_eq!(report.retained, 1);
    let read = db.open_read().expect("read conn");
    assert_eq!(
        candidate_state(&read, "cand-recent").expect("state"),
        Some(CandidateState::Pending),
    );
}

/// A dry run reports what it would expire but transitions nothing; a
/// repeated real run afterward is idempotent.
#[tokio::test]
async fn candidate_expiry_sweep_dry_run_then_idempotent_real_run() {
    let (_home, _layout, db) = open_state();
    pending_candidate(&db, "cand-old", 1_000).await;
    let now = 1_000 + CANDIDATE_EXPIRY_MS;

    let dry = run_candidate_expiry_sweep(&db, now, CANDIDATE_EXPIRY_MS, true)
        .await
        .expect("dry run");
    assert!(dry.dry_run);
    assert_eq!(dry.expired, vec!["cand-old".to_string()]);
    let read = db.open_read().expect("read conn");
    assert_eq!(
        candidate_state(&read, "cand-old").expect("state"),
        Some(CandidateState::Pending),
        "dry run must not transition",
    );
    drop(read);

    let first = run_candidate_expiry_sweep(&db, now, CANDIDATE_EXPIRY_MS, false)
        .await
        .expect("first real run");
    assert_eq!(first.expired, vec!["cand-old".to_string()]);

    let second = run_candidate_expiry_sweep(&db, now, CANDIDATE_EXPIRY_MS, false)
        .await
        .expect("second real run");
    assert!(second.is_empty(), "second sweep is a no-op: {second:?}");
}

/// A candidate already moved out of `pending` (approved here) is never a
/// sweep candidate, however old its `created_at`.
#[tokio::test]
async fn non_pending_candidate_is_never_swept() {
    let (_home, _layout, db) = open_state();
    pending_candidate(&db, "cand-approved", 1_000).await;
    db.writer()
        .transaction(|tx| {
            local_rag_store::transition_candidate(tx, "cand-approved", CandidateState::Approved)
        })
        .await
        .expect("transition tx")
        .expect("legal transition");

    let now = 1_000 + CANDIDATE_EXPIRY_MS;
    let report = run_candidate_expiry_sweep(&db, now, CANDIDATE_EXPIRY_MS, false)
        .await
        .expect("sweep");

    assert!(report.is_empty());
    let read = db.open_read().expect("read conn");
    assert_eq!(
        candidate_state(&read, "cand-approved").expect("state"),
        Some(CandidateState::Approved),
    );
}
