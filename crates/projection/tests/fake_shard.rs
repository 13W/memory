//! Behavioural tests for the persistent fake shard over the public
//! [`ShardHandle`] trait (spec 05 §1/§3). These need no fault injection, so they
//! run under default features; the failpoint-driven crash tests live in
//! `fake_faults.rs`.

use local_rag_projection::{
    DenseQuery, FakeProjectionStore, FakeShard, PointId, ProjectionError, ProjectionPoint,
    ProjectionStore, ShardParams, head,
};
use local_rag_test_support::TempHome;

const DIMS: usize = 3;

fn wt() -> local_rag_core::identity::Uuid {
    "01234567-89ab-7122-b344-5566778899aa".parse().unwrap()
}
fn gen_id() -> local_rag_core::identity::Uuid {
    "0000000a-0000-7000-8000-00000000000b".parse().unwrap()
}
fn ms() -> local_rag_core::identity::Uuid {
    "0000000c-0000-7000-8000-00000000000d".parse().unwrap()
}
fn op() -> local_rag_core::identity::Uuid {
    "0000000e-0000-7000-8000-00000000000f".parse().unwrap()
}

/// A shard directory under an isolated temp home, plus the store to open it.
fn shard_dir() -> (TempHome, std::path::PathBuf) {
    let home = TempHome::new().expect("temp home");
    let dir = home.join("projection").join("wt-1");
    (home, dir)
}

fn point(id: &str, vector: [f32; DIMS]) -> ProjectionPoint {
    ProjectionPoint {
        point_id: PointId::from_hex(id),
        vector: vector.to_vec(),
    }
}

fn open(dir: &std::path::Path) -> Box<dyn local_rag_projection::ShardHandle> {
    FakeProjectionStore::new()
        .open(dir, ShardParams { dimensions: DIMS })
        .expect("open shard")
}

fn ids_of(shard: &dyn local_rag_projection::ShardHandle) -> Vec<String> {
    shard
        .point_ids()
        .expect("point ids")
        .map(|id| id.as_str().to_string())
        .collect()
}

#[test]
fn fresh_shard_is_empty_with_no_head() {
    let (_home, dir) = shard_dir();
    let shard = open(&dir);
    assert_eq!(shard.point_count().expect("count"), 0);
    assert!(shard.read_head().expect("head").is_none());
    assert!(ids_of(shard.as_ref()).is_empty());
}

#[test]
fn reopen_preserves_points_and_head() {
    let (_home, dir) = shard_dir();
    {
        let shard = open(&dir);
        shard
            .upsert(&[point("0a", [1.0, 0.0, 0.0]), point("0b", [0.0, 1.0, 0.0])])
            .expect("upsert");
        let ids = [PointId::from_hex("0a"), PointId::from_hex("0b")];
        let h = head(wt(), gen_id(), ms(), op(), &ids);
        shard.write_head(&h).expect("write head");
    } // handle dropped — models a restart

    // A brand-new store/handle re-reads the persisted files.
    let shard = open(&dir);
    assert_eq!(shard.point_count().expect("count"), 2);
    assert_eq!(ids_of(shard.as_ref()), ["0a", "0b"]);
    let reopened = shard.read_head().expect("head").expect("head present");
    assert_eq!(reopened.point_count, 2);
    assert_eq!(reopened.worktree_id, wt());
    assert_eq!(reopened.generation_id, gen_id());
    assert_eq!(reopened.projection_op_id, op());
    // The manifest survives byte-for-byte.
    assert_eq!(
        reopened.manifest_hash,
        head(
            wt(),
            gen_id(),
            ms(),
            op(),
            &[PointId::from_hex("0a"), PointId::from_hex("0b")]
        )
        .manifest_hash,
    );
}

#[test]
fn upsert_is_idempotent_by_point_id() {
    let (_home, dir) = shard_dir();
    let shard = open(&dir);
    shard
        .upsert(&[point("0a", [1.0, 0.0, 0.0])])
        .expect("first");
    // Same id, different vector → overwrite, still one point.
    shard
        .upsert(&[point("0a", [0.0, 0.0, 9.0])])
        .expect("second");
    shard
        .upsert(&[point("0a", [0.0, 0.0, 9.0])])
        .expect("third");
    assert_eq!(shard.point_count().expect("count"), 1);

    // The overwrite persisted the latest vector.
    drop(shard);
    let shard = open(&dir);
    let hits = shard
        .search(&DenseQuery {
            vector: vec![0.0, 0.0, 1.0],
            k: 1,
        })
        .expect("search");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].point_id.as_str(), "0a");
    assert!((hits[0].score - 9.0).abs() < 1e-6);
}

#[test]
fn delete_is_idempotent() {
    let (_home, dir) = shard_dir();
    let shard = open(&dir);
    shard
        .upsert(&[point("0a", [1.0, 0.0, 0.0]), point("0b", [0.0, 1.0, 0.0])])
        .expect("upsert");
    shard.delete(&[PointId::from_hex("0a")]).expect("delete");
    // Deleting an absent id (and a re-delete) are both no-ops.
    shard
        .delete(&[PointId::from_hex("0a"), PointId::from_hex("zz")])
        .expect("redelete");
    assert_eq!(shard.point_count().expect("count"), 1);
    assert_eq!(ids_of(shard.as_ref()), ["0b"]);
}

#[test]
fn manifest_is_independent_of_upsert_order() {
    // Insert the same set in two different orders into two shards; the head
    // manifest computed from each shard's point set is identical.
    let manifest_for = |order: &[ProjectionPoint]| {
        let (_home, dir) = shard_dir();
        let shard = open(&dir);
        for p in order {
            shard.upsert(std::slice::from_ref(p)).expect("upsert");
        }
        let ids: Vec<PointId> = shard.point_ids().expect("ids").collect();
        head(wt(), gen_id(), ms(), op(), &ids).manifest_hash
    };

    let ascending = [
        point("0a", [1.0, 0.0, 0.0]),
        point("0b", [0.0, 1.0, 0.0]),
        point("0c", [0.0, 0.0, 1.0]),
    ];
    let descending = [
        point("0c", [0.0, 0.0, 1.0]),
        point("0b", [0.0, 1.0, 0.0]),
        point("0a", [1.0, 0.0, 0.0]),
    ];
    assert_eq!(manifest_for(&ascending), manifest_for(&descending));
}

#[test]
fn search_is_deterministic_top_k() {
    let (_home, dir) = shard_dir();
    let shard = open(&dir);
    shard
        .upsert(&[
            point("0a", [1.0, 0.0, 0.0]),
            point("0b", [0.9, 0.0, 0.0]),
            point("0c", [0.0, 1.0, 0.0]),
        ])
        .expect("upsert");
    let hits = shard
        .search(&DenseQuery {
            vector: vec![1.0, 0.0, 0.0],
            k: 2,
        })
        .expect("search");
    assert_eq!(
        hits.iter().map(|h| h.point_id.as_str()).collect::<Vec<_>>(),
        ["0a", "0b"],
        "top-2 ordered by descending score"
    );
}

#[test]
fn upsert_rejects_wrong_dimensionality() {
    let (_home, dir) = shard_dir();
    let shard = open(&dir);
    let err = shard
        .upsert(&[ProjectionPoint {
            point_id: PointId::from_hex("0a"),
            vector: vec![1.0, 2.0], // 2 dims, shard expects 3
        }])
        .expect_err("dimension mismatch");
    assert!(matches!(
        err,
        ProjectionError::DimensionMismatch {
            expected: 3,
            actual: 2
        }
    ));
    // Nothing was persisted.
    assert_eq!(shard.point_count().expect("count"), 0);
}

#[test]
fn destroy_removes_the_shard_directory() {
    let (_home, dir) = shard_dir();
    let shard = open(&dir);
    shard
        .upsert(&[point("0a", [1.0, 0.0, 0.0])])
        .expect("upsert");
    assert!(dir.exists());
    shard.destroy().expect("destroy");
    assert!(!dir.exists());
}

#[test]
fn open_on_corrupt_points_file_errors() {
    let (_home, dir) = shard_dir();
    std::fs::create_dir_all(&dir).expect("mkdir");
    // A points line with a non-hex vector → Corrupt at open (F12-style). Use the
    // concrete `FakeShard` here because `Box<dyn ShardHandle>` is not `Debug`.
    std::fs::write(dir.join("points"), "0a\tnothex\n").expect("write");
    let err = FakeShard::open(&dir, ShardParams { dimensions: DIMS }).expect_err("corrupt");
    assert!(matches!(err, ProjectionError::Corrupt(_)), "got {err:?}");
}
