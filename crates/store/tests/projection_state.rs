//! T07-02 acceptance tests for the two-axis projection state guards (spec 03
//! §2.2, machine in spec 04 §2): the DB-integration side — the guarded
//! [`write_projection_state`] transition driven through [`StateWriter::transaction`],
//! the two-axis invariants, and the one-axis-per-operation precondition.
//!
//! All tests are deterministic: an isolated [`TempHome`], fixed `now_ms` literals,
//! and ids minted from [`uuidv7_from`] with fixed entropy. Pure coverage (the
//! status truth-table, the invariant table, corrupt-enum reads) lives in the
//! `registry::projection_state` module unit tests; these exercise the DB
//! operations and the nested-result rollback semantics.

use local_rag_core::identity::uuidv7_from;
use local_rag_core::paths::StoreLayout;
use local_rag_store::registry::{
    DEFAULT_MODEL_SPACE_ID, IllegalProjectionTransition, ProjectionInvariantViolation,
    ProjectionStateChange, ProjectionStateError, ProjectionStateRow, ProjectionStatus,
    allocate_generation, create_repository, create_worktree, insert_projection_state,
    projection_state, write_projection_state,
};
use local_rag_store::rusqlite::params;
use local_rag_store::{StateDb, WorktreeKind};
use local_rag_test_support::TempHome;

/// A temporary store with an ensured tree and an opened [`StateDb`] (runs the
/// production migration set: registry v1, worktree v2, code v3, projection v4).
fn open_state() -> (TempHome, StateDb) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    (home, db)
}

/// A distinct, deterministic UUIDv7 string keyed by `seed`.
fn uuid(seed: u8) -> String {
    let mut rand = [0u8; 10];
    rand[9] = seed;
    uuidv7_from(1000, rand).to_string()
}

/// Create a repository and one `active` main worktree; returns `worktree_id`.
async fn worktree(db: &StateDb, seed: u8) -> String {
    let repo = uuid(seed);
    let wt = uuid(seed.wrapping_add(100));
    let (repo0, wt0) = (repo.clone(), wt.clone());
    db.writer()
        .transaction(move |tx| {
            create_repository(tx, &repo0, None, 1000)?;
            create_worktree(tx, &wt0, &repo0, WorktreeKind::Main, 1000)
        })
        .await
        .expect("create repo + worktree");
    wt
}

/// Allocate one generation (born `building`) for `worktree_id`; returns its id.
async fn allocate(db: &StateDb, worktree_id: &str, gen_seed: u8) -> String {
    let genr = uuid(gen_seed);
    let (w, g) = (worktree_id.to_string(), genr.clone());
    db.writer()
        .transaction(move |tx| allocate_generation(tx, &w, &g, 1000).map(|_| ()))
        .await
        .expect("allocate generation");
    genr
}

/// Insert an extra `active` model space (raw, for exercising the model axis).
async fn insert_model_space(db: &StateDb, id: &str) {
    let i = id.to_string();
    db.writer()
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO model_space (model_space_id, display_name, state, created_at, updated_at) \
                 VALUES (?1, ?2, 'active', 1000, 1000)",
                params![i, format!("space-{i}")],
            )
            .map(|_| ())
        })
        .await
        .expect("insert model space");
}

/// Initialize a `clean`, empty projection state row for `worktree_id`.
async fn init(db: &StateDb, worktree_id: &str) {
    let w = worktree_id.to_string();
    db.writer()
        .transaction(move |tx| insert_projection_state(tx, &w, 1000))
        .await
        .expect("init projection state");
}

/// Apply a guarded change, unwrapping the outer (infrastructure) result and
/// returning the inner domain result.
async fn write(
    db: &StateDb,
    worktree_id: &str,
    change: ProjectionStateChange,
) -> Result<(), ProjectionStateError> {
    let w = worktree_id.to_string();
    db.writer()
        .transaction(move |tx| write_projection_state(tx, &w, &change, 2000))
        .await
        .expect("write projection state tx (infrastructure)")
}

/// Read the projection state row (must exist).
fn read_state(db: &StateDb, worktree_id: &str) -> ProjectionStateRow {
    let read = db.open_read().expect("read conn");
    projection_state(&read, worktree_id)
        .expect("read projection state")
        .expect("row exists")
}

#[tokio::test]
async fn init_creates_clean_empty_row() {
    let (_home, db) = open_state();
    let wt = worktree(&db, 1).await;
    init(&db, &wt).await;

    let row = read_state(&db, &wt);
    assert_eq!(row.status, ProjectionStatus::Clean);
    assert_eq!(row.active_generation_id, None);
    assert_eq!(row.active_model_space_id, None);
    assert_eq!(row.projected_generation_id, None);
    assert_eq!(row.target_generation_id, None);
    assert_eq!(row.projection_op_id, None);
}

#[tokio::test]
async fn updating_requires_a_target_and_leaves_state_unchanged_on_rejection() {
    let (_home, db) = open_state();
    let wt = worktree(&db, 2).await;
    init(&db, &wt).await;

    // Move to `updating` with no target tuple → rejected.
    let change = ProjectionStateChange {
        status_to: Some(ProjectionStatus::Updating),
        projection_op_id: Some(uuid(20)),
        ..Default::default()
    };
    assert_eq!(
        write(&db, &wt, change).await,
        Err(ProjectionStateError::Invariant(
            ProjectionInvariantViolation::TargetMissingWhenUpdating
        )),
    );
    // The no-op transaction committed nothing: still clean.
    assert_eq!(read_state(&db, &wt).status, ProjectionStatus::Clean);
}

#[tokio::test]
async fn clean_requires_no_target_and_active_equals_projected() {
    let (_home, db) = open_state();
    let wt = worktree(&db, 3).await;
    let gen_id = allocate(&db, &wt, 30).await;
    init(&db, &wt).await;

    // Clean with a lingering target is rejected.
    let with_target = ProjectionStateChange {
        status_to: Some(ProjectionStatus::Clean),
        target_generation_id: Some(gen_id.clone()),
        ..Default::default()
    };
    assert_eq!(
        write(&db, &wt, with_target).await,
        Err(ProjectionStateError::Invariant(
            ProjectionInvariantViolation::TargetSetWhenClean
        )),
    );

    // Clean with active != projected is rejected.
    let skewed = ProjectionStateChange {
        status_to: Some(ProjectionStatus::Clean),
        active_generation_id: Some(gen_id.clone()),
        projected_generation_id: None,
        ..Default::default()
    };
    assert_eq!(
        write(&db, &wt, skewed).await,
        Err(ProjectionStateError::Invariant(
            ProjectionInvariantViolation::ActiveNotProjectedWhenClean
        )),
    );
}

#[tokio::test]
async fn simultaneous_generation_and_model_target_is_rejected() {
    let (_home, db) = open_state();
    let wt = worktree(&db, 4).await;
    let gen_a = allocate(&db, &wt, 40).await;
    init(&db, &wt).await;

    // First projection: establish active=(gen_a, default) via write-ahead + commit.
    let op1 = uuid(41);
    write(
        &db,
        &wt,
        ProjectionStateChange {
            status_to: Some(ProjectionStatus::Updating),
            target_generation_id: Some(gen_a.clone()),
            target_model_space_id: Some(DEFAULT_MODEL_SPACE_ID.to_string()),
            projection_op_id: Some(op1.clone()),
            ..Default::default()
        },
    )
    .await
    .expect("write-ahead ok");
    write(
        &db,
        &wt,
        ProjectionStateChange {
            status_to: Some(ProjectionStatus::Clean),
            active_generation_id: Some(gen_a.clone()),
            active_model_space_id: Some(DEFAULT_MODEL_SPACE_ID.to_string()),
            projected_generation_id: Some(gen_a.clone()),
            projected_model_space_id: Some(DEFAULT_MODEL_SPACE_ID.to_string()),
            projection_op_id: Some(op1),
            ..Default::default()
        },
    )
    .await
    .expect("commit ok");

    // Now attempt a switch that moves BOTH axes at once (gen_a→gen_b, default→ms_b).
    let ms_b = uuid(42);
    insert_model_space(&db, &ms_b).await;
    let gen_b = allocate(&db, &wt, 43).await;
    let both = ProjectionStateChange {
        status_to: Some(ProjectionStatus::Updating),
        active_generation_id: Some(gen_a.clone()),
        active_model_space_id: Some(DEFAULT_MODEL_SPACE_ID.to_string()),
        projected_generation_id: Some(gen_a.clone()),
        projected_model_space_id: Some(DEFAULT_MODEL_SPACE_ID.to_string()),
        target_generation_id: Some(gen_b),
        target_model_space_id: Some(ms_b),
        projection_op_id: Some(uuid(44)),
        ..Default::default()
    };
    assert_eq!(
        write(&db, &wt, both).await,
        Err(ProjectionStateError::Invariant(
            ProjectionInvariantViolation::BothAxesMovedAtOnce
        )),
    );

    // Rejected: the committed clean state stands.
    let row = read_state(&db, &wt);
    assert_eq!(row.status, ProjectionStatus::Clean);
    assert_eq!(row.active_generation_id.as_deref(), Some(gen_a.as_str()));
    assert_eq!(
        row.active_model_space_id.as_deref(),
        Some(DEFAULT_MODEL_SPACE_ID)
    );
}

#[tokio::test]
async fn illegal_status_transition_is_typed_error() {
    let (_home, db) = open_state();
    let wt = worktree(&db, 5).await;
    init(&db, &wt).await;

    // clean → rebuilding is illegal (must pass through dirty).
    let change = ProjectionStateChange {
        status_to: Some(ProjectionStatus::Rebuilding),
        ..Default::default()
    };
    assert_eq!(
        write(&db, &wt, change).await,
        Err(ProjectionStateError::Illegal(IllegalProjectionTransition {
            from: ProjectionStatus::Clean,
            to: ProjectionStatus::Rebuilding,
        })),
    );
    assert_eq!(read_state(&db, &wt).status, ProjectionStatus::Clean);
}

#[tokio::test]
async fn unknown_worktree_is_typed_error() {
    let (_home, db) = open_state();
    let ghost = uuid(6); // never given a projection_state row
    let change = ProjectionStateChange {
        status_to: Some(ProjectionStatus::Updating),
        ..Default::default()
    };
    assert_eq!(
        write(&db, &ghost, change).await,
        Err(ProjectionStateError::UnknownWorktree),
    );
}

#[tokio::test]
async fn happy_path_one_axis_switch_walks_clean_updating_clean() {
    let (_home, db) = open_state();
    let wt = worktree(&db, 7).await;
    let gen_a = allocate(&db, &wt, 70).await;
    init(&db, &wt).await;

    // Establish the generation axis (model stays the default).
    let op1 = uuid(71);
    write(
        &db,
        &wt,
        ProjectionStateChange {
            status_to: Some(ProjectionStatus::Updating),
            target_generation_id: Some(gen_a.clone()),
            target_model_space_id: Some(DEFAULT_MODEL_SPACE_ID.to_string()),
            projection_op_id: Some(op1.clone()),
            ..Default::default()
        },
    )
    .await
    .expect("write-ahead");
    assert_eq!(read_state(&db, &wt).status, ProjectionStatus::Updating);

    write(
        &db,
        &wt,
        ProjectionStateChange {
            status_to: Some(ProjectionStatus::Clean),
            active_generation_id: Some(gen_a.clone()),
            active_model_space_id: Some(DEFAULT_MODEL_SPACE_ID.to_string()),
            projected_generation_id: Some(gen_a.clone()),
            projected_model_space_id: Some(DEFAULT_MODEL_SPACE_ID.to_string()),
            projection_op_id: Some(op1),
            ..Default::default()
        },
    )
    .await
    .expect("commit");

    // Now switch only the model axis (generation stays gen_a).
    let ms_b = uuid(72);
    insert_model_space(&db, &ms_b).await;
    let op2 = uuid(73);
    write(
        &db,
        &wt,
        ProjectionStateChange {
            status_to: Some(ProjectionStatus::Updating),
            active_generation_id: Some(gen_a.clone()),
            active_model_space_id: Some(DEFAULT_MODEL_SPACE_ID.to_string()),
            projected_generation_id: Some(gen_a.clone()),
            projected_model_space_id: Some(DEFAULT_MODEL_SPACE_ID.to_string()),
            target_generation_id: Some(gen_a.clone()),
            target_model_space_id: Some(ms_b.clone()),
            projection_op_id: Some(op2.clone()),
            ..Default::default()
        },
    )
    .await
    .expect("model-axis write-ahead");
    write(
        &db,
        &wt,
        ProjectionStateChange {
            status_to: Some(ProjectionStatus::Clean),
            active_generation_id: Some(gen_a.clone()),
            active_model_space_id: Some(ms_b.clone()),
            projected_generation_id: Some(gen_a.clone()),
            projected_model_space_id: Some(ms_b.clone()),
            projection_op_id: Some(op2),
            ..Default::default()
        },
    )
    .await
    .expect("model-axis commit");

    let row = read_state(&db, &wt);
    assert_eq!(row.status, ProjectionStatus::Clean);
    assert_eq!(row.active_generation_id.as_deref(), Some(gen_a.as_str()));
    assert_eq!(row.active_model_space_id.as_deref(), Some(ms_b.as_str()));
    assert_eq!(row.target_generation_id, None);
    assert_eq!(row.target_model_space_id, None);
}
