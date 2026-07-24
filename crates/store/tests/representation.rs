//! T11-01 acceptance tests for the representation/model-space registry (spec
//! 03 §2.2, machine in spec 04 §3, 10 §2/§3):
//!
//! - six-field uniqueness / duplicate serialization converges (real DB, via
//!   [`register_representation`], across a genuine transaction);
//! - the seeded default model space (T07-02, `SCHEMA_V4`) reads back as
//!   `Active` through the new reader;
//! - incomplete coverage cannot reach `projection_ready`, and only `Active` is
//!   ever `eligible_as_target` — "retiring cannot become target".
//!
//! Pure state-machine/coverage-shape coverage (the full `check_transition`
//! matrix, `recompute_coverage`'s per-required-kind shape, corrupt-enum reads)
//! lives in the `registry::representation` module's unit tests; these exercise
//! the DB operations, schema constraints, and the migration.
//!
//! Deterministic: an isolated [`TempHome`], fixed `now_ms` literals, no clock or
//! network.

use std::collections::BTreeMap;

use local_rag_core::identity::uuidv7_from;
use local_rag_core::paths::StoreLayout;
use local_rag_store::registry::{
    Coverage, CoverageEntry, DistanceMetric, ModelSpaceState, ModelSpaceTransitionError,
    RepresentationKey, RepresentationKind, create_model_space, eligible_as_target,
    model_space_state, recompute_coverage, register_representation, representation_key,
    set_model_space_representation, transition_model_space, write_model_space_coverage,
};
use local_rag_store::{DEFAULT_MODEL_SPACE_ID, StateDb};
use local_rag_test_support::TempHome;

/// A temporary store with an ensured tree and an opened [`StateDb`] (runs the
/// production migration set, through v6/T11-01).
fn open_state() -> (TempHome, StateDb) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    (home, db)
}

/// A distinct, deterministic UUIDv7 string keyed by `seed`, never touching the
/// clock or entropy source.
fn uuid(seed: u8) -> String {
    let mut rand = [0u8; 10];
    rand[9] = seed;
    uuidv7_from(1000, rand).to_string()
}

fn key(model_id: &str) -> RepresentationKey {
    RepresentationKey {
        kind: RepresentationKind::CodeRaw,
        representation_version: 1,
        normalization_version: 1,
        model_id: model_id.to_string(),
        dimensions: 768,
        distance_metric: DistanceMetric::Dot,
    }
}

/// Two distinct six-field keys register two distinct rows; the same key
/// registered twice converges on the first id — no second row (spec 03 §2.2's
/// `UNIQUE` constraint plus [`register_representation`]'s idempotent upsert).
#[tokio::test]
async fn six_field_uniqueness_and_duplicate_convergence() {
    let (_home, db) = open_state();

    let (id_a, id_b, id_a_retry) = (uuid(1), uuid(2), uuid(3));
    let (a0, b0, retry0) = (id_a.clone(), id_b.clone(), id_a_retry.clone());
    let (returned_a, returned_b, returned_retry) = db
        .writer()
        .transaction(move |tx| {
            let a = register_representation(tx, &a0, &key("model-a"), 1000)?;
            let b = register_representation(tx, &b0, &key("model-b"), 1000)?;
            // Same six-field key as `a`, different candidate id and timestamp:
            // must converge on `a`'s id, not create a third row.
            let retry = register_representation(tx, &retry0, &key("model-a"), 2000)?;
            Ok((a, b, retry))
        })
        .await
        .expect("register three");

    assert_eq!(returned_a, id_a);
    assert_eq!(returned_b, id_b);
    assert_eq!(
        returned_retry, id_a,
        "duplicate serialization converges on the first-registered id"
    );
    assert_ne!(id_a, id_b, "distinct keys get distinct ids");

    let read = db.open_read().expect("read conn");
    assert_eq!(
        representation_key(&read, &id_a).expect("read a"),
        Some(key("model-a")),
    );
    assert_eq!(
        representation_key(&read, &id_b).expect("read b"),
        Some(key("model-b")),
    );
    // The discarded candidate id from the duplicate attempt was never inserted.
    assert_eq!(
        representation_key(&read, &id_a_retry).expect("read retry candidate"),
        None,
        "the duplicate registration's own candidate id is not a row",
    );

    let count: i64 = read
        .query_row("SELECT COUNT(*) FROM representation", [], |r| r.get(0))
        .expect("count");
    assert_eq!(count, 2, "exactly two rows, no duplicate for model-a");
}

/// T07-02's seeded default model space reads back as `Active` through the new
/// reader (spec 04 §3: "the default space MUST be `active`"), and is the only
/// state `eligible_as_target` accepts.
#[tokio::test]
async fn default_model_space_is_active_and_eligible_as_target() {
    let (_home, db) = open_state();
    let read = db.open_read().expect("read conn");

    let state = model_space_state(&read, DEFAULT_MODEL_SPACE_ID)
        .expect("read default model space state")
        .expect("default model space row exists (T07-02 seed)");
    assert_eq!(state, ModelSpaceState::Active);
    assert!(eligible_as_target(state));
}

/// A model space with an under-covered required kind cannot reach
/// `projection_ready` — "incomplete cannot projection_ready/target" — and stays
/// ineligible as a switch target throughout.
#[tokio::test]
async fn incomplete_coverage_blocks_projection_ready_and_target_eligibility() {
    let (_home, db) = open_state();

    let model_space_id = uuid(10);
    let representation_id = uuid(11);
    let (msid, repid, msid2) = (
        model_space_id.clone(),
        representation_id.clone(),
        model_space_id.clone(),
    );

    // Build a model space requiring `code_raw`, with partial coverage
    // (ready < expected).
    db.writer()
        .transaction(move |tx| {
            create_model_space(tx, &msid, "incomplete-space", 1000)?;
            register_representation(tx, &repid, &key("model-a"), 1000)?;
            set_model_space_representation(
                tx,
                &msid,
                RepresentationKind::CodeRaw,
                &repid,
                true,
                1000,
            )?;
            let mut counts = BTreeMap::new();
            counts.insert(
                RepresentationKind::CodeRaw,
                CoverageEntry {
                    expected: 10,
                    ready: 3,
                    failed: 0,
                },
            );
            let coverage = recompute_coverage(&[RepresentationKind::CodeRaw], &counts);
            write_model_space_coverage(tx, &msid, &coverage, 1000)
        })
        .await
        .expect("build incomplete model space");

    // `building → projection_ready` is rejected: the required kind is
    // under-covered.
    let required = [RepresentationKind::CodeRaw];
    let result = db
        .writer()
        .transaction(move |tx| {
            transition_model_space(
                tx,
                &msid2,
                ModelSpaceState::ProjectionReady,
                &required,
                2000,
            )
        })
        .await
        .expect("transition tx (infrastructure)");
    assert_eq!(result, Err(ModelSpaceTransitionError::IncompleteCoverage));

    // State is unchanged (no mutation on rejection) and therefore still not
    // eligible as a switch target.
    let read = db.open_read().expect("read conn");
    let state = model_space_state(&read, &model_space_id)
        .expect("read state")
        .expect("row exists");
    assert_eq!(
        state,
        ModelSpaceState::Building,
        "unchanged after rejection"
    );
    assert!(!eligible_as_target(state));
    drop(read);

    // Completing coverage and retrying succeeds through to `active`, at which
    // point — and ONLY at which point — it becomes target-eligible.
    let model_space_id2 = model_space_id.clone();
    db.writer()
        .transaction(move |tx| {
            let mut counts = BTreeMap::new();
            counts.insert(
                RepresentationKind::CodeRaw,
                CoverageEntry {
                    expected: 10,
                    ready: 10,
                    failed: 0,
                },
            );
            let coverage = recompute_coverage(&[RepresentationKind::CodeRaw], &counts);
            write_model_space_coverage(tx, &model_space_id2, &coverage, 3000)?;
            let ready = transition_model_space(
                tx,
                &model_space_id2,
                ModelSpaceState::ProjectionReady,
                &[RepresentationKind::CodeRaw],
                3000,
            )?;
            assert_eq!(ready, Ok(()), "full coverage now allows projection_ready");
            transition_model_space(
                tx,
                &model_space_id2,
                ModelSpaceState::Active,
                &[RepresentationKind::CodeRaw],
                3000,
            )
        })
        .await
        .expect("transition tx (infrastructure)")
        .expect("projection_ready -> active");

    let read = db.open_read().expect("read conn");
    let state = model_space_state(&read, &model_space_id)
        .expect("read state")
        .expect("row exists");
    assert_eq!(state, ModelSpaceState::Active);
    assert!(eligible_as_target(state), "active is target-eligible");
}

/// `retiring` never becomes a switch target again — the state machine has no
/// edge back out of it, and `eligible_as_target` is `false` throughout.
#[tokio::test]
async fn retiring_cannot_become_target() {
    let (_home, db) = open_state();

    let model_space_id = uuid(20);
    let representation_id = uuid(21);
    let (msid, repid) = (model_space_id.clone(), representation_id.clone());

    db.writer()
        .transaction(move |tx| {
            create_model_space(tx, &msid, "retiring-space", 1000)?;
            register_representation(tx, &repid, &key("model-b"), 1000)?;
            set_model_space_representation(
                tx,
                &msid,
                RepresentationKind::CodeRaw,
                &repid,
                true,
                1000,
            )?;
            let mut counts = BTreeMap::new();
            counts.insert(
                RepresentationKind::CodeRaw,
                CoverageEntry {
                    expected: 1,
                    ready: 1,
                    failed: 0,
                },
            );
            let coverage = recompute_coverage(&[RepresentationKind::CodeRaw], &counts);
            write_model_space_coverage(tx, &msid, &coverage, 1000)
        })
        .await
        .expect("build model space");

    let required = [RepresentationKind::CodeRaw];
    for to in [ModelSpaceState::ProjectionReady, ModelSpaceState::Active] {
        let (msid, required) = (model_space_id.clone(), required);
        let result = db
            .writer()
            .transaction(move |tx| transition_model_space(tx, &msid, to, &required, 2000))
            .await
            .expect("transition tx (infrastructure)");
        assert_eq!(result, Ok(()), "→ {to:?}");
    }

    // Retire it.
    let msid = model_space_id.clone();
    let result = db
        .writer()
        .transaction(move |tx| {
            transition_model_space(tx, &msid, ModelSpaceState::Retiring, &required, 3000)
        })
        .await
        .expect("transition tx (infrastructure)");
    assert_eq!(result, Ok(()));

    let read = db.open_read().expect("read conn");
    let state = model_space_state(&read, &model_space_id)
        .expect("read state")
        .expect("row exists");
    assert_eq!(state, ModelSpaceState::Retiring);
    assert!(
        !eligible_as_target(state),
        "retiring is never target-eligible"
    );
    drop(read);

    // No transition out of `retiring` is legal, including back to `active`.
    for to in [
        ModelSpaceState::Building,
        ModelSpaceState::ProjectionReady,
        ModelSpaceState::Active,
    ] {
        let msid = model_space_id.clone();
        let result = db
            .writer()
            .transaction(move |tx| transition_model_space(tx, &msid, to, &[], 4000))
            .await
            .expect("transition tx (infrastructure)");
        assert!(
            matches!(result, Err(ModelSpaceTransitionError::Illegal(_))),
            "retiring → {to:?} must be illegal, got {result:?}",
        );
    }
}

/// `Coverage` round-trips through the real `model_space.coverage` TEXT column.
#[tokio::test]
async fn coverage_round_trips_through_the_real_column() {
    let (_home, db) = open_state();

    let model_space_id = uuid(30);
    let msid = model_space_id.clone();
    db.writer()
        .transaction(move |tx| {
            create_model_space(tx, &msid, "coverage-space", 1000)?;
            let mut counts = BTreeMap::new();
            counts.insert(
                RepresentationKind::Memory,
                CoverageEntry {
                    expected: 2,
                    ready: 1,
                    failed: 1,
                },
            );
            let coverage = recompute_coverage(&[RepresentationKind::Memory], &counts);
            write_model_space_coverage(tx, &msid, &coverage, 1000)
        })
        .await
        .expect("write coverage");

    let read = db.open_read().expect("read conn");
    let raw: String = read
        .query_row(
            "SELECT coverage FROM model_space WHERE model_space_id = ?1",
            [&model_space_id],
            |r| r.get(0),
        )
        .expect("read raw coverage column");
    let parsed = Coverage::from_json(&raw).expect("parse coverage JSON");
    assert_eq!(
        parsed.get(RepresentationKind::Memory),
        Some(CoverageEntry {
            expected: 2,
            ready: 1,
            failed: 1,
        }),
    );
    assert!(!parsed.fully_covered(&[RepresentationKind::Memory]));
}
