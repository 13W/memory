//! T05-05 acceptance tests for retry/failure handling (spec 04 §1 `building →
//! failed`; spec 06 §2). Compiled only under the `failpoints` feature: they arm the
//! generation builder's per-phase injection points to deterministically fail each
//! build phase, then assert the failure semantics the card requires:
//!
//! - a failure at **each** build phase marks the generation `failed` and leaves a
//!   previously-active generation untouched and routable;
//! - `failed`/`retiring` are never selected for routing (`active_generations`);
//! - a retry after a failure builds a fresh valid generation and — thanks to
//!   structural sharing — adds **no duplicate content** rows;
//! - the reconcile driver folds the outcome into observable failure state (counter,
//!   backoff deadline, `last_error`, failed generation id) and clears it on success.
//!
//! The named failpoints live in a **process-global** registry, so tests in this
//! binary hold [`SERIAL`] for their whole duration and reset the registry on entry;
//! other integration test files are separate processes and never share it.
//!
//! Determinism: an isolated store + tree, a fixed `now_ms`, and a seeded
//! [`SeqUuidV7`]. No wall clock, no network, no sleeps.
#![cfg(feature = "failpoints")]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::{Mutex, MutexGuard};

use local_rag_core::identity::path::CaseSensitivity;
use local_rag_core::identity::{Uuid, UuidSource, uuidv7_from};
use local_rag_core::paths::StoreLayout;
use local_rag_core::redaction::Scanner;
use local_rag_index::classify::ClassifierConfig;
use local_rag_index::reconcile::{
    BuildOutcome, ReconcileHandle, ScheduleConfig, TriggerKind, WorktreeMeta, WorktreeReconciler,
    build_generation, spawn_reconciler,
};
use local_rag_index::scan::{ScanManifest, ScanMode, StatCache, scan};
use local_rag_store::registry::{
    GenerationState, WorktreeKind, allocate_generation, create_repository, create_worktree,
    current_generation, generation_state, set_current_generation, transition_generation,
};
use local_rag_store::rusqlite::Connection;
use local_rag_store::{StateDb, active_generations};
use local_rag_test_support::TempHome;
use local_rag_test_support::failpoint::{Action, global};

/// Serializes tests that touch the process-global failpoint registry. An async
/// (tokio) mutex so the guard may be held across the reconcile `.await`s that a
/// failpoint test needs to keep the registry to itself for its whole duration.
static SERIAL: Mutex<()> = Mutex::const_new(());

async fn serial() -> MutexGuard<'static, ()> {
    let guard = SERIAL.lock().await;
    // Start from a clean slate: no leftover arming from a prior test, and no arming
    // visible to the builds this test runs.
    global().reset();
    guard
}

/// The three per-phase failpoints the builder hosts (spec 06 §2 phases).
const PHASES: [&str; 3] = [
    "reconcile.build.after_allocate",
    "reconcile.build.persist_file",
    "reconcile.build.before_finalize",
];

/// A seeded, deterministic [`UuidSource`] (mirrors `reconcile.rs`/`scheduler.rs`).
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
        uuidv7_from(1000 + n, [0xEF; 10])
    }
}

/// An isolated store, an on-disk worktree root, and its registered id.
struct Fixture {
    _home: TempHome,
    db: Arc<StateDb>,
    root: PathBuf,
    worktree_id: String,
}

async fn fixture() -> Fixture {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let db = Arc::new(StateDb::open(layout.state_db()).expect("open state.sqlite"));
    let root = home.join("wt");
    std::fs::create_dir_all(&root).expect("create root");

    let repo = "018f0000-0000-7000-8000-0000000000a1".to_string();
    let worktree_id = "018f0000-0000-7000-8000-0000000000b1".to_string();
    let (r, w) = (repo.clone(), worktree_id.clone());
    db.writer()
        .transaction(move |tx| {
            create_repository(tx, &r, None, 1000)?;
            create_worktree(tx, &w, &r, WorktreeKind::Main, 1000)
        })
        .await
        .expect("seed repo + worktree");

    Fixture {
        _home: home,
        db,
        root,
        worktree_id,
    }
}

fn write(root: &Path, rel: &str, contents: &[u8]) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create parents");
    }
    std::fs::write(path, contents).expect("write file");
}

fn scan_tree(root: &Path) -> ScanManifest {
    let mut cache = StatCache::new();
    scan(
        root,
        WorktreeKind::Main,
        CaseSensitivity::Sensitive,
        1 << 20,
        ScanMode::Strict,
        &[],
        &mut cache,
    )
    .expect("scan")
    .0
}

async fn build(fx: &Fixture, manifest: &ScanManifest, uuids: &SeqUuidV7) -> BuildOutcome {
    build_generation(
        &fx.db,
        &fx.worktree_id,
        &fx.root,
        manifest,
        &ClassifierConfig::new(1 << 20),
        &Scanner::new(),
        uuids,
        2000,
    )
    .await
    .expect("build_generation")
}

fn count(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .expect("count")
}

/// Arm one failpoint with an injected error return (declaring it first, since the
/// site auto-registers only when hit).
fn arm(name: &str) {
    let fp = global();
    fp.register(name);
    fp.arm(name, Action::Error).expect("arm declared failpoint");
}

/// Seed an `active` generation via the legal state machine (a test precondition, not
/// production activation — mirrors `scheduler.rs`).
async fn seed_active_generation(db: &StateDb, worktree_id: &str, generation_id: &str) {
    let (wt, g) = (worktree_id.to_string(), generation_id.to_string());
    db.writer()
        .transaction(move |tx| allocate_generation(tx, &wt, &g, 500))
        .await
        .expect("allocate");
    for state in [GenerationState::ProjectionReady, GenerationState::Active] {
        let g = generation_id.to_string();
        db.writer()
            .transaction(move |tx| transition_generation(tx, &g, state))
            .await
            .expect("write")
            .expect("legal transition");
    }
    let (wt, g) = (worktree_id.to_string(), generation_id.to_string());
    db.writer()
        .transaction(move |tx| set_current_generation(tx, &wt, &g))
        .await
        .expect("set current");
}

/// A failure injected at **every** build phase marks the just-built generation
/// `failed`, while a previously-active generation stays `active`, current, and the
/// only routable one (`active_generations` never yields `failed`/`retiring`).
#[tokio::test]
async fn failpoint_at_each_build_phase_marks_failed_and_leaves_active() {
    for phase in PHASES {
        let _serial = serial().await;
        let fx = fixture().await;
        write(&fx.root, "a.rs", b"fn a() {}\n");
        let uuids = SeqUuidV7::new();

        // A previously-active generation A the failing build must not disturb.
        let gen_a = uuids.next_uuid().to_string();
        seed_active_generation(&fx.db, &fx.worktree_id, &gen_a).await;

        arm(phase);
        let manifest = scan_tree(&fx.root);
        let err = build_generation(
            &fx.db,
            &fx.worktree_id,
            &fx.root,
            &manifest,
            &ClassifierConfig::new(1 << 20),
            &Scanner::new(),
            &uuids,
            2000,
        )
        .await
        .expect_err(&format!("armed failpoint {phase} must fail the build"));
        global().disarm(phase).expect("disarm");

        let read = fx.db.open_read().expect("read");
        assert_eq!(
            generation_state(&read, &err.generation_id).expect("state"),
            Some(GenerationState::Failed),
            "{phase}: the failed build's generation is `failed`",
        );
        assert_eq!(
            generation_state(&read, &gen_a).expect("state"),
            Some(GenerationState::Active),
            "{phase}: the previously-active generation is untouched",
        );
        assert_eq!(
            active_generations(&read, &fx.worktree_id).expect("active"),
            vec![gen_a.clone()],
            "{phase}: only A is routable; the failed generation is never selected",
        );
        assert_eq!(
            current_generation(&read, &fx.worktree_id).expect("current"),
            Some(gen_a.clone()),
            "{phase}: the current pointer still points at A",
        );
    }
}

/// A failure at the finalize phase persists all content under the failed generation;
/// a retry then builds a fresh valid generation that **reuses** that content via
/// structural sharing, so no content-shared row is duplicated (spec 06 §2 `[FIXED]`).
#[tokio::test]
async fn retry_after_failure_builds_valid_generation_without_duplicates() {
    let _serial = serial().await;
    let fx = fixture().await;
    write(&fx.root, "a.rs", b"fn a() {}\n");
    write(&fx.root, "b.rs", b"fn b() {}\n");
    let uuids = SeqUuidV7::new();

    // Fail at finalize: every file_revision/content_blob/parsed_unit is persisted,
    // but the generation is marked `failed` instead of reaching projection_ready.
    arm("reconcile.build.before_finalize");
    let manifest = scan_tree(&fx.root);
    let err = build_generation(
        &fx.db,
        &fx.worktree_id,
        &fx.root,
        &manifest,
        &ClassifierConfig::new(1 << 20),
        &Scanner::new(),
        &uuids,
        2000,
    )
    .await
    .expect_err("finalize failpoint must fail the build");
    global()
        .disarm("reconcile.build.before_finalize")
        .expect("disarm");

    let read = fx.db.open_read().expect("read");
    assert_eq!(
        generation_state(&read, &err.generation_id).expect("state"),
        Some(GenerationState::Failed),
    );
    let (rev1, blob1, unit1) = (
        count(&read, "file_revision"),
        count(&read, "content_blob"),
        count(&read, "parsed_unit"),
    );
    assert!(
        rev1 >= 2 && unit1 >= 2,
        "content was persisted before finalize"
    );
    drop(read);

    // Retry (failpoint disarmed): a fresh generation reusing all content.
    let gen2 = build(&fx, &manifest, &uuids).await;
    assert_eq!(
        gen2.revisions_reused, 2,
        "the retry reuses both revisions (structural sharing), parses nothing new",
    );

    let read = fx.db.open_read().expect("read");
    assert_eq!(
        generation_state(&read, &gen2.generation_id).expect("state"),
        Some(GenerationState::ProjectionReady),
        "the retry produces a valid projection_ready generation",
    );
    assert_eq!(
        count(&read, "file_revision"),
        rev1,
        "no duplicate revisions"
    );
    assert_eq!(count(&read, "content_blob"), blob1, "no duplicate blobs");
    assert_eq!(count(&read, "parsed_unit"), unit1, "no duplicate units");
    assert!(
        active_generations(&read, &fx.worktree_id)
            .expect("active")
            .is_empty(),
        "neither the failed nor the retried generation is routed (no activation)",
    );
}

/// A schedule whose debounce never elapses within a test, so the only reconcile is
/// the graceful-shutdown flush (deterministic regardless of CI speed).
fn flush_only_schedule() -> ScheduleConfig {
    ScheduleConfig {
        debounce_ms: 60 * 60 * 1000,
        periodic_ms: i64::MAX / 4,
    }
}

fn meta(fx: &Fixture) -> WorktreeMeta {
    WorktreeMeta {
        worktree_id: fx.worktree_id.clone(),
        root: fx.root.clone(),
        kind: WorktreeKind::Main,
        case: CaseSensitivity::Sensitive,
        prune_roots: Vec::new(),
    }
}

/// A failing reconcile is folded into the driver's observable failure state: the
/// counter increments, the backoff deadline is armed, and the failed generation's id
/// + `last_error` are recorded — without ever routing the failed generation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn driver_records_failure_and_backs_off() {
    let _serial = serial().await;
    let fx = fixture().await;
    write(&fx.root, "a.rs", b"fn a() {}\n");
    let uuids: Arc<dyn UuidSource + Send + Sync> = Arc::new(SeqUuidV7::new());

    arm("reconcile.build.persist_file");
    let reconciler = WorktreeReconciler::new(
        fx.db.clone(),
        meta(&fx),
        ClassifierConfig::new(1 << 20),
        Scanner::new(),
        uuids,
        flush_only_schedule(),
    );
    let ReconcileHandle {
        sender,
        join,
        failures,
    } = spawn_reconciler(reconciler, 8);
    sender.send(TriggerKind::FsChange).await.expect("send");
    drop(sender); // flush → one failing reconcile
    join.await.expect("join");
    global()
        .disarm("reconcile.build.persist_file")
        .expect("disarm");

    let failure = failures
        .borrow()
        .clone()
        .expect("a failed reconcile is observable");
    assert_eq!(failure.consecutive_failures, 1, "one failure recorded");
    assert!(failure.backoff_until_ms > 0, "the backoff floor is armed");
    assert!(
        failure.generation_id.is_some(),
        "the build allocated a generation before failing",
    );
    assert!(
        failure.last_error.contains("failpoint"),
        "last_error carries the cause, got {:?}",
        failure.last_error,
    );

    // The failed generation is `failed` and never routed.
    let read = fx.db.open_read().expect("read");
    let gid = failure.generation_id.unwrap();
    assert_eq!(
        generation_state(&read, &gid).expect("state"),
        Some(GenerationState::Failed),
    );
    assert!(
        active_generations(&read, &fx.worktree_id)
            .expect("active")
            .is_empty(),
        "a failed reconcile activates nothing",
    );
}

/// A healthy reconcile publishes `None`: the driver builds a valid generation and the
/// failure observability stays clear.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn driver_reports_success_as_no_failure() {
    let _serial = serial().await;
    let fx = fixture().await;
    write(&fx.root, "a.rs", b"fn a() {}\n");
    let uuids: Arc<dyn UuidSource + Send + Sync> = Arc::new(SeqUuidV7::new());

    // No failpoint armed (serial()/reset() guarantees a clean registry).
    let reconciler = WorktreeReconciler::new(
        fx.db.clone(),
        meta(&fx),
        ClassifierConfig::new(1 << 20),
        Scanner::new(),
        uuids,
        flush_only_schedule(),
    );
    let ReconcileHandle {
        sender,
        join,
        failures,
    } = spawn_reconciler(reconciler, 8);
    sender.send(TriggerKind::FsChange).await.expect("send");
    drop(sender);
    join.await.expect("join");

    assert!(
        failures.borrow().is_none(),
        "a successful reconcile records no failure",
    );
    let read = fx.db.open_read().expect("read");
    assert_eq!(
        count(&read, "generation"),
        1,
        "one reconcile ran to completion"
    );
}
