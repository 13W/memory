//! D-066 acceptance tests for the generation retention sweep's two new callers.
//!
//! The store already proves the sweep itself (`crates/store/tests/retention*.rs`).
//! What is new here is the **composition**: the pin set
//! `local_rag::gc::sweep_external_pins` builds from `worktree_projection_state`,
//! and the guarantee that grows out of it — a generation that table still names
//! is never swept, whatever its state.
//!
//! That guarantee is the reason the sweep is safe to run automatically at all.
//! `worktree_projection_state` foreign-keys `generation` through three columns,
//! and `foreign_keys=ON` means sweeping a generation one of them names would
//! fail the batch transaction outright. Before D-066 nothing built these pins,
//! because nothing called the sweep.
//!
//! Deterministic: isolated [`TempHome`], fixed `now_ms` literals, ids minted
//! from [`uuidv7_from`] with fixed entropy — no wall clock, no sleeps.

use local_rag::gc::{plan_generation_sweep, run_generation_sweep, sweep_external_pins};
use local_rag_core::identity::uuidv7_from;
use local_rag_core::paths::StoreLayout;
use local_rag_store::registry::{
    GenerationState, WorktreeKind, allocate_generation, create_repository, create_worktree,
    insert_projection_state, transition_generation,
};
use local_rag_store::rusqlite::params;
use local_rag_store::{RetentionParams, StateDb};
use local_rag_test_support::TempHome;

const NOW_MS: i64 = 2_000_000;

/// Retention that pins nothing on its own: no last-`K`, zero-width window. Every
/// `retiring`/`failed` generation is a candidate, so anything that survives a
/// sweep under these params survived because of an **external** pin — which is
/// exactly what these tests are about.
fn no_retention() -> RetentionParams {
    RetentionParams {
        keep_last_k: 0,
        window_ms: 0,
    }
}

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

/// A repository with one `active` main worktree and an initialized (clean,
/// all-NULL) projection state row.
async fn worktree(db: &StateDb, seed: u8) -> String {
    let repo = uuid(seed);
    let wt = uuid(seed.wrapping_add(100));
    let (repo0, wt0, wt1) = (repo.clone(), wt.clone(), wt.clone());
    db.writer()
        .transaction(move |tx| {
            create_repository(tx, &repo0, None, 1000)?;
            create_worktree(tx, &wt0, &repo0, WorktreeKind::Main, 1000)?;
            insert_projection_state(tx, &wt1, 1000)
        })
        .await
        .expect("seed repo + worktree + projection state");
    wt
}

/// Allocate a generation and drive it to `retiring` — a sweep candidate.
async fn retiring_generation(db: &StateDb, worktree_id: &str, seed: u8) -> String {
    let genr = uuid(seed);
    let (w, g) = (worktree_id.to_string(), genr.clone());
    db.writer()
        .transaction(move |tx| {
            allocate_generation(tx, &w, &g, 1000)?;
            for to in [
                GenerationState::ProjectionReady,
                GenerationState::Active,
                GenerationState::Retiring,
            ] {
                transition_generation(tx, &g, to)?.expect("legal transition");
            }
            Ok(())
        })
        .await
        .expect("allocate and retire generation");
    genr
}

/// Point `projected_generation_id` at `generation_id` (raw: the guarded writer
/// enforces one-axis-per-operation, which cannot express "projected still names
/// a generation the registry already retired" — the very state this guards).
async fn set_projected(db: &StateDb, worktree_id: &str, generation_id: &str) {
    let (w, g) = (worktree_id.to_string(), generation_id.to_string());
    db.writer()
        .transaction(move |tx| {
            tx.execute(
                "UPDATE worktree_projection_state SET projected_generation_id = ?2 \
                 WHERE worktree_id = ?1",
                params![w, g],
            )
            .map(|_| ())
        })
        .await
        .expect("set projected generation");
}

fn generation_exists(db: &StateDb, generation_id: &str) -> bool {
    let conn = db.open_read().expect("read conn");
    conn.query_row(
        "SELECT COUNT(*) FROM generation WHERE generation_id = ?1",
        params![generation_id],
        |r| r.get::<_, i64>(0),
    )
    .expect("count generation")
        > 0
}

#[tokio::test]
async fn the_pin_set_carries_every_generation_projection_state_names() {
    let (_home, db) = open_state();
    let wt = worktree(&db, 1).await;
    let pinned = retiring_generation(&db, &wt, 10).await;

    assert!(
        sweep_external_pins(&db)
            .expect("pins")
            .referenced_generations
            .is_empty(),
        "an all-NULL projection state pins nothing"
    );

    set_projected(&db, &wt, &pinned).await;

    let pins = sweep_external_pins(&db).expect("pins");
    assert!(
        pins.referenced_generations.contains(&pinned),
        "a generation `projected_generation_id` names must appear in the pin set"
    );
    assert!(
        pins.leases.is_empty() && pins.referenced_file_revisions.is_empty(),
        "the other two ExternalPins fields have no source as built"
    );
}

/// The safety property D-066 exists for: `worktree_projection_state` still names
/// this `retiring` generation, so the sweep must leave it alone even though the
/// retention window and last-`K` pin nothing.
#[tokio::test]
async fn a_generation_projection_state_still_names_is_never_swept() {
    let (_home, db) = open_state();
    let wt = worktree(&db, 2).await;
    let pinned = retiring_generation(&db, &wt, 20).await;
    let unpinned = retiring_generation(&db, &wt, 21).await;
    set_projected(&db, &wt, &pinned).await;

    let report = run_generation_sweep(&db, &no_retention(), NOW_MS)
        .await
        .expect("sweep");

    assert!(
        generation_exists(&db, &pinned),
        "sweeping a generation projection state references would violate its \
         foreign key and roll the batch back"
    );
    assert!(
        !generation_exists(&db, &unpinned),
        "the unpinned sibling proves the sweep really did run"
    );
    assert_eq!(report.generations, 1, "exactly one generation swept");
}

/// The dry run reports the same candidate set and changes nothing — the lever
/// `local-rag gc --dry-run` gives before a first sweep on a store with a
/// backlog.
#[tokio::test]
async fn the_dry_run_reports_the_same_candidates_and_deletes_nothing() {
    let (_home, db) = open_state();
    let wt = worktree(&db, 3).await;
    let pinned = retiring_generation(&db, &wt, 30).await;
    let unpinned = retiring_generation(&db, &wt, 31).await;
    set_projected(&db, &wt, &pinned).await;

    let plan = plan_generation_sweep(&db, &no_retention(), NOW_MS)
        .await
        .expect("plan");

    assert_eq!(
        plan.candidate_generations,
        vec![unpinned.clone()],
        "the plan names the unpinned generation only"
    );
    assert_eq!(plan.would_delete.generations, 1);
    assert!(
        generation_exists(&db, &pinned) && generation_exists(&db, &unpinned),
        "a dry run must not delete anything"
    );

    // ...and the real sweep then matches what the plan promised.
    let report = run_generation_sweep(&db, &no_retention(), NOW_MS)
        .await
        .expect("sweep");
    assert_eq!(report.generations, plan.would_delete.generations);
}

/// Re-running is a no-op, which is what makes "sweep on every daemon start"
/// safe: a start that follows an already-collected store does nothing.
#[tokio::test]
async fn a_second_sweep_is_a_no_op() {
    let (_home, db) = open_state();
    let wt = worktree(&db, 4).await;
    retiring_generation(&db, &wt, 40).await;

    let first = run_generation_sweep(&db, &no_retention(), NOW_MS)
        .await
        .expect("first sweep");
    let second = run_generation_sweep(&db, &no_retention(), NOW_MS)
        .await
        .expect("second sweep");

    assert_eq!(first.generations, 1);
    assert_eq!(second.total(), 0, "nothing left to collect");
}
