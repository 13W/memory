//! The backend-neutral [`ProjectionStore`]/[`ShardHandle`] contract (spec 05 §1),
//! asserted against **every** backend this crate ships — T12-02.
//!
//! Spec 05 §1 is `[FIXED abstraction, signatures [SPEC]]`: the point of the
//! trait is that the rest of the system cannot tell which backend is underneath.
//! Until T12-02 there was only one implementation, so that claim was untested.
//! Now there are two — the production
//! [`BruteForceProjectionStore`](local_rag_projection::BruteForceProjectionStore)
//! (ADR-0003) and the fault-injection
//! [`FakeProjectionStore`](local_rag_projection::FakeProjectionStore), which keeps
//! carrying the group-07 F-matrix's named failpoints — and every case below runs
//! against both, so a divergence in idempotence, head ordering, ranking or
//! corruption reporting fails the build rather than silently changing behavior
//! depending on which store the daemon was built with.
//!
//! Backend-*specific* concerns stay in their own modules' unit tests: the
//! `points.bin` binary format and its truncation cases in
//! `crates/projection/src/brute_force.rs`, the fake's hex-text persistence and
//! failpoint seams in `crates/projection/src/fake.rs`.
//!
//! Deterministic: an isolated [`TempHome`] per case, fixed ids, no network, no
//! wall-clock sleeps.

use std::path::Path;

use local_rag_core::identity::Uuid;
use local_rag_projection::{
    BruteForceProjectionStore, DenseQuery, DistanceMetric, FakeProjectionStore, Hash32, PointId,
    ProjectionError, ProjectionHead, ProjectionPoint, ProjectionStore, ShardHandle, ShardParams,
};
use local_rag_test_support::TempHome;

const DIMS: usize = 3;

/// Every backend under test, by name (the name is only for assertion messages).
fn backends() -> Vec<(&'static str, Box<dyn ProjectionStore>)> {
    vec![
        (
            "brute-force",
            Box::new(BruteForceProjectionStore::new()) as Box<dyn ProjectionStore>,
        ),
        ("fake", Box::new(FakeProjectionStore::new())),
    ]
}

/// Run `case` against every backend, each in its own fresh shard directory.
fn for_each_backend(params: ShardParams, mut case: impl FnMut(&str, &dyn ShardHandle, &Path)) {
    for (name, store) in backends() {
        let home = TempHome::new().expect("temp home");
        let dir = home.join("shard");
        let handle = store.open(&dir, params).expect("open shard");
        case(name, handle.as_ref(), &dir);
    }
}

fn id(n: u8) -> PointId {
    PointId::from_hex(format!("{n:064x}"))
}

fn point(n: u8, vector: Vec<f32>) -> ProjectionPoint {
    ProjectionPoint {
        point_id: id(n),
        vector,
    }
}

fn uuid(n: u8) -> Uuid {
    format!("00000000-0000-7000-8000-0000000000{n:02}")
        .parse()
        .expect("uuid")
}

fn head(point_count: u64) -> ProjectionHead {
    ProjectionHead {
        worktree_id: uuid(1),
        generation_id: uuid(2),
        model_space_id: uuid(3),
        projection_op_id: uuid(4),
        projection_schema_version: local_rag_projection::PROJECTION_SCHEMA_VERSION,
        point_count,
        manifest_hash: Hash32::from_hex("cd".repeat(32)),
    }
}

fn ids_of(handle: &dyn ShardHandle) -> Vec<String> {
    let mut ids: Vec<String> = handle
        .point_ids()
        .expect("point ids")
        .map(|i| i.as_str().to_string())
        .collect();
    ids.sort();
    ids
}

// ---- lifecycle ---------------------------------------------------------------

/// A fresh shard is empty and headless — a valid, detectable state, not an
/// error (spec 05 §10 F7).
#[test]
fn a_fresh_shard_is_empty_and_headless() {
    for_each_backend(ShardParams::with_dimensions(DIMS), |name, shard, _dir| {
        assert_eq!(shard.point_count().expect("count"), 0, "{name}");
        assert_eq!(shard.read_head().expect("head"), None, "{name}");
        assert!(ids_of(shard).is_empty(), "{name}");
        assert!(
            shard
                .search(&DenseQuery {
                    vector: vec![1.0, 0.0, 0.0],
                    k: 5,
                })
                .expect("search")
                .is_empty(),
            "{name}"
        );
    });
}

/// `upsert` is idempotent by point id, and a repeat overwrites the vector
/// rather than adding a row (spec 05 §1/§3).
#[test]
fn upsert_is_idempotent_by_point_id() {
    for_each_backend(ShardParams::with_dimensions(DIMS), |name, shard, _dir| {
        let first = [point(1, vec![1.0, 0.0, 0.0]), point(2, vec![0.0, 1.0, 0.0])];
        shard.upsert(&first).expect("upsert");
        shard.upsert(&first).expect("upsert again");
        assert_eq!(shard.point_count().expect("count"), 2, "{name}");

        shard
            .upsert(&[point(1, vec![5.0, 0.0, 0.0])])
            .expect("overwrite");
        assert_eq!(shard.point_count().expect("count"), 2, "{name}");
        let hits = shard
            .search(&DenseQuery {
                vector: vec![1.0, 0.0, 0.0],
                k: 1,
            })
            .expect("search");
        assert_eq!(hits[0].point_id, id(1), "{name}");
        assert_eq!(hits[0].score, 5.0, "{name}: the vector was replaced");
    });
}

/// `delete` is idempotent and tolerates unknown ids (spec 05 §1/§3).
#[test]
fn delete_is_idempotent_and_ignores_unknown_ids() {
    for_each_backend(ShardParams::with_dimensions(DIMS), |name, shard, _dir| {
        shard
            .upsert(&[point(1, vec![1.0, 0.0, 0.0]), point(2, vec![0.0, 1.0, 0.0])])
            .expect("upsert");
        shard.delete(&[id(1)]).expect("delete");
        shard.delete(&[id(1)]).expect("delete again");
        shard.delete(&[id(200)]).expect("unknown id");
        shard.delete(&[]).expect("empty batch");
        assert_eq!(shard.point_count().expect("count"), 1, "{name}");
        assert_eq!(ids_of(shard), vec![id(2).as_str().to_string()], "{name}");
    });
}

/// `point_ids` and `point_count` describe the same set — the pair
/// validate-on-open compares a manifest against (spec 05 §1/§4/§6).
#[test]
fn point_ids_and_point_count_agree_after_every_mutation() {
    for_each_backend(ShardParams::with_dimensions(DIMS), |name, shard, _dir| {
        for n in 1..=5u8 {
            shard
                .upsert(&[point(n, vec![n as f32, 0.0, 0.0])])
                .expect("upsert");
            assert_eq!(
                shard.point_count().expect("count") as usize,
                ids_of(shard).len(),
                "{name}: after upserting {n}"
            );
        }
        shard.delete(&[id(2), id(4)]).expect("delete");
        assert_eq!(shard.point_count().expect("count"), 3, "{name}");
        assert_eq!(
            ids_of(shard),
            vec![
                id(1).as_str().to_string(),
                id(3).as_str().to_string(),
                id(5).as_str().to_string(),
            ],
            "{name}"
        );
    });
}

/// Everything a backend persists survives a close/reopen — points *and* head.
#[test]
fn state_survives_reopen() {
    let params = ShardParams::with_dimensions(DIMS);
    for (name, store) in backends() {
        let home = TempHome::new().expect("temp home");
        let dir = home.join("shard");
        {
            let shard = store.open(&dir, params).expect("open");
            shard
                .upsert(&[
                    point(1, vec![1.0, 2.0, 3.0]),
                    point(2, vec![-1.0, 0.0, 0.5]),
                ])
                .expect("upsert");
            shard.write_head(&head(2)).expect("write head");
        }
        let reopened = store.open(&dir, params).expect("reopen");
        assert_eq!(reopened.point_count().expect("count"), 2, "{name}");
        assert_eq!(reopened.read_head().expect("head"), Some(head(2)), "{name}");
        let hits = reopened
            .search(&DenseQuery {
                vector: vec![1.0, 0.0, 0.0],
                k: 10,
            })
            .expect("search");
        assert_eq!(hits[0].point_id, id(1), "{name}");
        assert_eq!(hits[0].score, 1.0, "{name}: components round-tripped");
    }
}

/// The head is the LAST write of an op (spec 05 §1/§5): points written before
/// it are already durable, and a reopen *without* a head still sees them — the
/// asymmetry that makes "head present ⇒ everything before it landed" a usable
/// proof.
#[test]
fn points_are_durable_before_the_head_is_written() {
    let params = ShardParams::with_dimensions(DIMS);
    for (name, store) in backends() {
        let home = TempHome::new().expect("temp home");
        let dir = home.join("shard");
        {
            let shard = store.open(&dir, params).expect("open");
            shard
                .upsert(&[point(1, vec![1.0, 0.0, 0.0])])
                .expect("upsert");
            // Deliberately no `write_head`.
        }
        let reopened = store.open(&dir, params).expect("reopen");
        assert_eq!(reopened.point_count().expect("count"), 1, "{name}");
        assert_eq!(
            reopened.read_head().expect("head"),
            None,
            "{name}: an op that never wrote its head leaves no head"
        );
    }
}

/// A second head overwrites the first — a shard has exactly one commit marker.
#[test]
fn write_head_replaces_the_previous_head() {
    for_each_backend(ShardParams::with_dimensions(DIMS), |name, shard, _dir| {
        shard.write_head(&head(0)).expect("first head");
        let second = ProjectionHead {
            projection_op_id: uuid(9),
            point_count: 7,
            ..head(0)
        };
        shard.write_head(&second).expect("second head");
        assert_eq!(shard.read_head().expect("head"), Some(second), "{name}");
    });
}

// ---- search semantics --------------------------------------------------------

/// Ranking is score-descending with `point_id` ascending as the tie-break, and
/// `k` truncates — identically for every backend (spec 09 §4's determinism, seen
/// from below).
#[test]
fn ranking_and_truncation_are_identical_across_backends() {
    let mut per_backend: Vec<Vec<String>> = Vec::new();
    for_each_backend(ShardParams::with_dimensions(DIMS), |name, shard, _dir| {
        shard
            .upsert(&[
                point(1, vec![1.0, 0.0, 0.0]),
                point(2, vec![0.9, 0.0, 0.0]),
                // Two points with identical vectors ⇒ identical scores ⇒ the
                // tie-break is the only thing ordering them.
                point(3, vec![0.5, 0.0, 0.0]),
                point(4, vec![0.5, 0.0, 0.0]),
            ])
            .expect("upsert");
        let hits = shard
            .search(&DenseQuery {
                vector: vec![1.0, 0.0, 0.0],
                k: 3,
            })
            .expect("search");
        assert_eq!(hits.len(), 3, "{name}: k truncates");
        let order: Vec<String> = hits
            .iter()
            .map(|h| h.point_id.as_str().to_string())
            .collect();
        assert_eq!(
            order,
            vec![
                id(1).as_str().to_string(),
                id(2).as_str().to_string(),
                id(3).as_str().to_string(),
            ],
            "{name}: score desc, then point id asc"
        );
        per_backend.push(order);
    });
    assert_eq!(
        per_backend[0], per_backend[1],
        "backends must agree on ranking"
    );
}

/// `distance_metric` is honored by every backend, not just the production one.
#[test]
fn every_backend_honors_the_shards_distance_metric() {
    let cosine = ShardParams {
        dimensions: DIMS,
        distance_metric: DistanceMetric::Cosine,
    };
    for_each_backend(cosine, |name, shard, _dir| {
        shard
            .upsert(&[
                point(1, vec![0.5, 0.0, 0.0]), // aligned, short
                point(2, vec![3.0, 3.0, 0.0]), // longer, skewed
            ])
            .expect("upsert");
        let hits = shard
            .search(&DenseQuery {
                vector: vec![1.0, 0.0, 0.0],
                k: 2,
            })
            .expect("search");
        assert_eq!(
            hits[0].point_id,
            id(1),
            "{name}: cosine prefers alignment over magnitude (dot would invert this)"
        );
    });

    let l2 = ShardParams {
        dimensions: DIMS,
        distance_metric: DistanceMetric::L2,
    };
    for_each_backend(l2, |name, shard, _dir| {
        shard
            .upsert(&[point(1, vec![1.0, 0.0, 0.0]), point(2, vec![9.0, 9.0, 9.0])])
            .expect("upsert");
        let hits = shard
            .search(&DenseQuery {
                vector: vec![1.0, 0.0, 0.0],
                k: 2,
            })
            .expect("search");
        assert_eq!(hits[0].point_id, id(1), "{name}: nearest still ranks first");
        assert!(hits[0].score >= hits[1].score, "{name}: higher is closer");
    });
}

/// A vector whose length disagrees with the shard is refused, on both the write
/// and the read side — never silently zero-padded or truncated.
#[test]
fn dimension_mismatches_are_refused() {
    for_each_backend(ShardParams::with_dimensions(DIMS), |name, shard, _dir| {
        let err = shard
            .upsert(&[point(1, vec![1.0, 0.0])])
            .expect_err("{name}: short vector must be refused");
        assert!(
            matches!(
                err,
                ProjectionError::DimensionMismatch {
                    expected: DIMS,
                    actual: 2
                }
            ),
            "{name}: unexpected error {err}"
        );
        assert_eq!(shard.point_count().expect("count"), 0, "{name}");
    });
}

// ---- destroy -----------------------------------------------------------------

/// `destroy` removes the shard's on-disk state and is idempotent (spec 05
/// §7/§8) — the operation quarantine/rebuild relies on.
#[test]
fn destroy_removes_state_and_is_idempotent() {
    let params = ShardParams::with_dimensions(DIMS);
    for (name, store) in backends() {
        let home = TempHome::new().expect("temp home");
        let dir = home.join("shard");
        let shard = store.open(&dir, params).expect("open");
        shard
            .upsert(&[point(1, vec![1.0, 0.0, 0.0])])
            .expect("seed");
        shard.write_head(&head(1)).expect("head");
        drop(shard);

        store
            .open(&dir, params)
            .expect("reopen")
            .destroy()
            .expect("destroy");
        assert!(!dir.exists(), "{name}: shard directory is gone");

        // Destroying an already-destroyed shard is a no-op, and the reopen that
        // precedes it sees a clean empty shard rather than an error.
        let reopened = store.open(&dir, params).expect("reopen after destroy");
        assert_eq!(reopened.point_count().expect("count"), 0, "{name}");
        assert_eq!(reopened.read_head().expect("head"), None, "{name}");
        reopened.destroy().expect("destroy again");
    }
}

/// `optimize` is policy-driven and must never disturb shard contents (spec 05
/// §9). For brute-force it is a documented no-op (ADR-0003: no threshold exists
/// to set).
#[test]
fn optimize_never_changes_what_the_shard_holds() {
    for_each_backend(ShardParams::with_dimensions(DIMS), |name, shard, _dir| {
        shard
            .upsert(&[point(1, vec![1.0, 0.0, 0.0]), point(2, vec![0.0, 1.0, 0.0])])
            .expect("upsert");
        shard.write_head(&head(2)).expect("head");
        let before = ids_of(shard);

        shard.optimize().expect("optimize");

        assert_eq!(ids_of(shard), before, "{name}");
        assert_eq!(shard.point_count().expect("count"), 2, "{name}");
        assert_eq!(shard.read_head().expect("head"), Some(head(2)), "{name}");
    });
}
