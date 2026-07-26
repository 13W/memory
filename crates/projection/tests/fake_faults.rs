//! Fault-injection tests for the fake shard (spec 05 §1/§5/§10).
//!
//! These prove the "head is last operation" ordering invariant — the F3
//! detection signal — and exercise the `inspect`/`corrupt` controls the later
//! validate-on-open and F1–F12 work (T07-04/T07-05) build on. The whole file is
//! gated on the `failpoints` feature; run with
//! `cargo test -p local-rag-projection --features failpoints`.
#![cfg(feature = "failpoints")]

use std::sync::{Mutex, MutexGuard, OnceLock};

use local_rag_core::identity::Uuid;
use local_rag_projection::{
    Corruption, FakeShard, PointId, ProjectionError, ProjectionPoint, ShardHandle, ShardParams,
    head,
};
use local_rag_test_support::{Action, TempHome, failpoint::global};

const DIMS: usize = 3;
const WRITE_HEAD_FP: &str = "projection.fake.write_head";

/// Serialize access to the process-global failpoint registry so parallel tests
/// never see each other's armings.
///
/// **Every test in this file must hold this guard**, not only the ones that
/// arm/disarm a failpoint themselves (D-005): the registry is process-global,
/// so while `head_is_last_operation_error_leaves_no_head` has
/// `projection.fake.write_head` armed with `Action::Error`, any *other* test on
/// another thread that calls `write_head` as ordinary setup — never expecting a
/// failpoint — is hit by the same injected error and fails spuriously.
fn serial() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

fn wt() -> Uuid {
    "01234567-89ab-7122-b344-5566778899aa".parse().unwrap()
}
fn gen_id() -> Uuid {
    "0000000a-0000-7000-8000-00000000000b".parse().unwrap()
}
fn ms() -> Uuid {
    "0000000c-0000-7000-8000-00000000000d".parse().unwrap()
}
fn op() -> Uuid {
    "0000000e-0000-7000-8000-00000000000f".parse().unwrap()
}

fn point(id: &str, vector: [f32; DIMS]) -> ProjectionPoint {
    ProjectionPoint {
        point_id: PointId::from_hex(id),
        vector: vector.to_vec(),
    }
}

fn params() -> ShardParams {
    ShardParams::with_dimensions(DIMS)
}

/// Points-then-head is the shape every op takes.
fn two_points() -> [ProjectionPoint; 2] {
    [point("0a", [1.0, 0.0, 0.0]), point("0b", [0.0, 1.0, 0.0])]
}

fn head_for(ids: &[&str]) -> local_rag_projection::ProjectionHead {
    let ids: Vec<PointId> = ids.iter().map(|s| PointId::from_hex(*s)).collect();
    head(wt(), gen_id(), ms(), op(), &ids)
}

#[test]
fn head_is_last_operation_error_leaves_no_head() {
    let _serial = serial();
    let home = TempHome::new().expect("temp home");
    let dir = home.join("projection").join("wt-1");

    let shard = FakeShard::open(&dir, params()).expect("open");
    shard.upsert(&two_points()).expect("upsert");

    // Arm the seam that fires at the *start* of write_head, before the head
    // lands. write_head must fail and leave the points persisted but no head.
    global().register(WRITE_HEAD_FP);
    global().arm(WRITE_HEAD_FP, Action::Error).expect("arm");
    let err = shard
        .write_head(&head_for(&["0a", "0b"]))
        .expect_err("injected");
    assert!(matches!(err, ProjectionError::Backend(_)), "got {err:?}");
    global().disarm(WRITE_HEAD_FP).expect("disarm");

    // Points were written (they precede write_head); the head never was.
    drop(shard);
    let reopened = FakeShard::open(&dir, params()).expect("reopen");
    assert_eq!(reopened.point_count().expect("count"), 2, "points survived");
    assert!(
        reopened.read_head().expect("head").is_none(),
        "head must be absent — its write is strictly last (F3)"
    );

    // Retrying the op cleanly now writes the head → converges.
    reopened
        .write_head(&head_for(&["0a", "0b"]))
        .expect("retry head");
    let inspection = reopened.inspect();
    assert_eq!(inspection.point_count, 2);
    assert_eq!(
        inspection.head.expect("head present").projection_op_id,
        op()
    );
}

/// A genuine process crash (SIGABRT) fired at the write_head seam, after the
/// points are persisted, followed by a reopen in a fresh process. Mirrors the
/// store's `resumable_hard_kill_via_sigabrt` pattern.
#[cfg(unix)]
#[test]
fn head_is_last_operation_survives_sigabrt() {
    use std::os::unix::process::ExitStatusExt;
    use std::process::Command;

    use local_rag_test_support::run_capturing;

    const CHILD_ENV: &str = "LOCAL_RAG_T0701_SIGABRT_DIR";

    // Child mode: upsert points, then abort inside write_head.
    if let Ok(dir) = std::env::var(CHILD_ENV) {
        let dir = std::path::PathBuf::from(dir);
        let shard = FakeShard::open(&dir, params()).expect("child open");
        shard.upsert(&two_points()).expect("child upsert");

        global().register(WRITE_HEAD_FP);
        global()
            .arm(WRITE_HEAD_FP, Action::Abort)
            .expect("child arm abort");

        // Expected to abort inside write_head, before the head is persisted.
        let _ = shard.write_head(&head_for(&["0a", "0b"]));
        // Reaching here means the seam did not fire — fail loudly, not a signal.
        std::process::exit(97);
    }

    // Parent mode.
    let _serial = serial();
    let home = TempHome::new().expect("temp home");
    let dir = home.join("projection").join("wt-1");
    std::fs::create_dir_all(&dir).expect("mkdir");

    let mut cmd = Command::new(std::env::current_exe().expect("current exe"));
    cmd.arg("head_is_last_operation_survives_sigabrt")
        .arg("--exact")
        .arg("--nocapture")
        .env(CHILD_ENV, &dir);
    let outcome = run_capturing(cmd, "t07_01-sigabrt").expect("spawn child");

    assert_eq!(
        outcome.status.signal(),
        Some(6),
        "child must die with SIGABRT; status={:?} bundle={:?}\nstderr:\n{}",
        outcome.status,
        outcome.bundle,
        outcome.stderr_lossy()
    );

    // Fresh process reopen: points survived the hard kill; the head did not.
    let reopened = FakeShard::open(&dir, params()).expect("reopen");
    assert_eq!(reopened.point_count().expect("count"), 2, "points survived");
    assert!(
        reopened.read_head().expect("head").is_none(),
        "head absent after crash before write_head (F3)"
    );
}

#[test]
fn inspect_reports_loaded_state() {
    let _serial = serial();
    let home = TempHome::new().expect("temp home");
    let dir = home.join("projection").join("wt-1");
    let shard = FakeShard::open(&dir, params()).expect("open");
    shard.upsert(&two_points()).expect("upsert");
    shard.write_head(&head_for(&["0a", "0b"])).expect("head");

    let inspection = shard.inspect();
    assert_eq!(
        inspection
            .point_ids
            .iter()
            .map(PointId::as_str)
            .collect::<Vec<_>>(),
        ["0a", "0b"]
    );
    assert_eq!(inspection.point_count, 2);
    assert_eq!(inspection.head.expect("head").point_count, 2);
}

#[test]
fn corrupt_drop_point_is_visible_after_reopen() {
    let _serial = serial();
    let home = TempHome::new().expect("temp home");
    let dir = home.join("projection").join("wt-1");
    let shard = FakeShard::open(&dir, params()).expect("open");
    shard.upsert(&two_points()).expect("upsert");
    shard.write_head(&head_for(&["0a", "0b"])).expect("head");

    // Drop a point out of band; the in-memory handle is unchanged.
    shard
        .corrupt(Corruption::DropPoint(PointId::from_hex("0a")))
        .expect("corrupt");
    assert_eq!(shard.point_count().expect("count"), 2, "memory unchanged");

    // A fresh open sees the divergence: 1 persisted point, head still claims 2.
    let reopened = FakeShard::open(&dir, params()).expect("reopen");
    let inspection = reopened.inspect();
    assert_eq!(inspection.point_count, 1);
    assert_eq!(inspection.point_ids, [PointId::from_hex("0b")]);
    assert_eq!(
        inspection.head.expect("head").point_count,
        2,
        "head count now disagrees with the point set (detected in T07-04)"
    );
}

#[test]
fn corrupt_remove_head_is_visible_after_reopen() {
    let _serial = serial();
    let home = TempHome::new().expect("temp home");
    let dir = home.join("projection").join("wt-1");
    let shard = FakeShard::open(&dir, params()).expect("open");
    shard.upsert(&two_points()).expect("upsert");
    shard.write_head(&head_for(&["0a", "0b"])).expect("head");

    shard.corrupt(Corruption::RemoveHead).expect("corrupt");

    let reopened = FakeShard::open(&dir, params()).expect("reopen");
    assert!(
        reopened.read_head().expect("head").is_none(),
        "head gone (F7)"
    );
    assert_eq!(reopened.point_count().expect("count"), 2, "points intact");
}

#[test]
fn corrupt_swap_point_yields_equal_count_different_set() {
    let _serial = serial();
    let home = TempHome::new().expect("temp home");
    let dir = home.join("projection").join("wt-1");
    let shard = FakeShard::open(&dir, params()).expect("open");
    shard.upsert(&two_points()).expect("upsert");
    shard.write_head(&head_for(&["0a", "0b"])).expect("head");

    // Replace 0a with 0c: same count, different id set (F8).
    shard
        .corrupt(Corruption::SwapPoint {
            remove: PointId::from_hex("0a"),
            insert: point("0c", [0.0, 0.0, 1.0]),
        })
        .expect("corrupt");

    let reopened = FakeShard::open(&dir, params()).expect("reopen");
    let inspection = reopened.inspect();
    assert_eq!(inspection.point_count, 2, "count unchanged");
    assert_eq!(
        inspection.point_ids,
        [PointId::from_hex("0b"), PointId::from_hex("0c")],
        "id set changed"
    );
}

#[test]
fn corrupt_overwrite_head_makes_shard_unopenable() {
    let _serial = serial();
    let home = TempHome::new().expect("temp home");
    let dir = home.join("projection").join("wt-1");
    let shard = FakeShard::open(&dir, params()).expect("open");
    shard.upsert(&two_points()).expect("upsert");
    shard.write_head(&head_for(&["0a", "0b"])).expect("head");

    shard
        .corrupt(Corruption::OverwriteHead(
            b"garbage without an equals".to_vec(),
        ))
        .expect("corrupt");

    let err = FakeShard::open(&dir, params()).expect_err("unopenable");
    assert!(
        matches!(err, ProjectionError::Corrupt(_)),
        "got {err:?} (F12)"
    );
}
