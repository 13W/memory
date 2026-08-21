//! T05-04 acceptance tests for the reconcile scheduler/driver (spec 06 §1–2).
//!
//! The fine-grained debounce/coalescing/mode-selection timing is covered by the
//! pure Layer-1 unit tests in `reconcile::schedule`; these integration tests assert
//! the coarse, end-to-end driver behavior against a **real** store + tree:
//!
//! - the driver actually runs a reconcile (via graceful-shutdown flush);
//! - a burst of concurrent triggers coalesces into exactly one next generation;
//! - the scan mode threads through (`Fast` trusts the stat cache, `Strict` re-hashes
//!   — the watcher-overflow / periodic / startup path);
//! - cancelling an in-flight reconcile leaves any active generation valid;
//! - registry composition (`load_worktree_meta`, `nested_prune_roots`).
//!
//! Determinism: an isolated store + tree, a seeded [`SeqUuidV7`] (`test-support` is
//! deliberately dependency-free, so the `UuidSource` double lives here), and a
//! debounce interval large enough that only the shutdown flush drives reconciles —
//! no wall-clock sleeps, no network.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use local_rag_core::identity::path::CaseSensitivity;
use local_rag_core::identity::{Uuid, UuidSource, uuidv7_from};
use local_rag_core::paths::StoreLayout;
use local_rag_core::redaction::Scanner;
use local_rag_index::classify::ClassifierConfig;
use local_rag_index::reconcile::{
    FixedWallClock, LastBuilt, ReconcileHandle, ScheduleConfig, TriggerKind, WallClock,
    WorktreeMeta, WorktreeReconciler, load_worktree_meta, nested_prune_roots, reconcile_once,
    spawn_reconciler,
};
use local_rag_index::scan::{ScanMode, StatCache};
use local_rag_store::registry::{
    GenerationState, WorktreeKind, allocate_generation, create_repository, create_worktree,
    current_generation, generation_state, observe_worktree_path, set_current_generation,
    transition_generation,
};
use local_rag_store::rusqlite::Connection;
use local_rag_store::{StateDb, active_generations};
use local_rag_test_support::TempHome;

/// A seeded, deterministic `UuidSource` (mirrors the one in `reconcile.rs`).
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
        uuidv7_from(1000 + n, [0xCD; 10])
    }
}

/// An isolated store, an on-disk worktree root, and its registered ids.
struct Fixture {
    _home: TempHome,
    db: Arc<StateDb>,
    root: PathBuf,
    repo_id: String,
    worktree_id: String,
}

async fn fixture() -> Fixture {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let db = Arc::new(StateDb::open(layout.state_db()).expect("open state.sqlite"));
    let root = home.join("wt");
    std::fs::create_dir_all(&root).expect("create root");

    let repo_id = "018f0000-0000-7000-8000-0000000000a1".to_string();
    let worktree_id = "018f0000-0000-7000-8000-0000000000b1".to_string();
    let (r, w) = (repo_id.clone(), worktree_id.clone());
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
        repo_id,
        worktree_id,
    }
}

/// Write `contents` to `root/rel`, creating parents.
fn write(root: &Path, rel: &str, contents: &[u8]) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create parents");
    }
    std::fs::write(path, contents).expect("write file");
}

fn count(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .expect("count")
}

/// A `WorktreeMeta` pointing at the fixture's root (git main tree, no nested roots).
fn meta(fx: &Fixture) -> WorktreeMeta {
    WorktreeMeta {
        worktree_id: fx.worktree_id.clone(),
        root: fx.root.clone(),
        kind: WorktreeKind::Main,
        case: CaseSensitivity::Sensitive,
        prune_roots: Vec::new(),
    }
}

/// The fixed wall-clock reading every driver test injects (D-062): a plausible
/// Unix-millisecond value (2026-08-06), far above anything the loop's monotonic
/// clock could produce, so an assertion on it distinguishes the two scales.
const TEST_WALL_MS: i64 = 1_786_000_000_000;

/// The wall-clock seam every driver test wires in place of the system clock, so
/// durable `_at` columns stay byte-deterministic (D-062).
fn test_clock() -> Arc<dyn WallClock> {
    Arc::new(FixedWallClock(TEST_WALL_MS))
}

/// A schedule config whose debounce never elapses within a test, so reconciles are
/// driven only by the graceful-shutdown flush (deterministic regardless of CI speed).
fn flush_only_schedule() -> ScheduleConfig {
    ScheduleConfig {
        debounce_ms: 60 * 60 * 1000,
        periodic_ms: i64::MAX / 4,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn driver_runs_a_reconcile_on_shutdown_flush() {
    let fx = fixture().await;
    write(&fx.root, "a.rs", b"fn a() {}\n");
    let uuids: Arc<dyn UuidSource + Send + Sync> = Arc::new(SeqUuidV7::new());

    let reconciler = WorktreeReconciler::new(
        fx.db.clone(),
        meta(&fx),
        ClassifierConfig::new(1 << 20),
        Scanner::new(),
        uuids,
        test_clock(),
        flush_only_schedule(),
    );
    let ReconcileHandle { sender, join, .. } = spawn_reconciler(reconciler, 8);
    sender.send(TriggerKind::FsChange).await.expect("send");
    drop(sender); // graceful shutdown → flush the scheduled reconcile
    join.await.expect("join");

    let read = fx.db.open_read().expect("read");
    assert_eq!(count(&read, "generation"), 1, "one reconcile ran");
    let gid: String = read
        .query_row("SELECT generation_id FROM generation", [], |r| r.get(0))
        .expect("gen id");
    assert_eq!(
        generation_state(&read, &gid).expect("state"),
        Some(GenerationState::ProjectionReady),
        "the reconcile stops at projection_ready",
    );
    assert!(
        active_generations(&read, &fx.worktree_id)
            .expect("active")
            .is_empty(),
        "the driver never activates (group 07)",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_triggers_make_one_next_generation() {
    let fx = fixture().await;
    write(&fx.root, "a.rs", b"fn a() {}\n");
    write(&fx.root, "b.rs", b"fn b() {}\n");
    let uuids: Arc<dyn UuidSource + Send + Sync> = Arc::new(SeqUuidV7::new());

    let reconciler = WorktreeReconciler::new(
        fx.db.clone(),
        meta(&fx),
        ClassifierConfig::new(1 << 20),
        Scanner::new(),
        uuids,
        test_clock(),
        flush_only_schedule(),
    );
    let ReconcileHandle { sender, join, .. } = spawn_reconciler(reconciler, 64);
    // A burst of triggers while nothing is in flight: they coalesce into one pending
    // request, flushed as a single reconcile on shutdown.
    for _ in 0..8 {
        sender.send(TriggerKind::FsChange).await.expect("send");
    }
    drop(sender);
    join.await.expect("join");

    let read = fx.db.open_read().expect("read");
    assert_eq!(
        count(&read, "generation"),
        1,
        "a burst of triggers coalesces into exactly one next generation",
    );
}

/// D-062 regression: the driver must persist **wall-clock** Unix milliseconds
/// (spec 03 `03-data-model.md:10`), not the debouncer's monotonic loop time.
///
/// Before the fix `WorktreeReconciler::run` handed the same `origin.elapsed()`
/// millisecond to both the debouncer and `reconcile_once`, so every `_at` column
/// the daemon/`watch` path wrote held milliseconds-since-loop-start — a freshly
/// started loop stamped rows with values in the single digits, which is why the
/// reporter's live store showed `generation.created_at` as `1970-01-01`. The two
/// clocks are asserted apart here: a monotonic reading inside one test could
/// never reach [`TEST_WALL_MS`].
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn driver_stamps_rows_with_wall_clock_not_loop_time() {
    let fx = fixture().await;
    write(&fx.root, "a.rs", b"fn a() {}\n");
    let uuids: Arc<dyn UuidSource + Send + Sync> = Arc::new(SeqUuidV7::new());

    let reconciler = WorktreeReconciler::new(
        fx.db.clone(),
        meta(&fx),
        ClassifierConfig::new(1 << 20),
        Scanner::new(),
        uuids,
        test_clock(),
        flush_only_schedule(),
    );
    let ReconcileHandle { sender, join, .. } = spawn_reconciler(reconciler, 8);
    sender.send(TriggerKind::FsChange).await.expect("send");
    drop(sender); // graceful shutdown → flush the scheduled reconcile
    join.await.expect("join");

    let read = fx.db.open_read().expect("read");
    // The generation itself...
    let created_at: i64 = read
        .query_row("SELECT created_at FROM generation", [], |r| r.get(0))
        .expect("generation created_at");
    assert_eq!(
        created_at, TEST_WALL_MS,
        "generation.created_at is the injected wall clock, not the loop's elapsed millis",
    );
    // ...and every row the same reconcile wrote underneath it (`build.rs` threads
    // one `now_ms` into `create_or_reuse_file_revision`/`persist_parse_output`).
    for table in ["content_blob", "file_revision"] {
        let (min, max): (i64, i64) = read
            .query_row(
                &format!("SELECT MIN(created_at), MAX(created_at) FROM {table}"),
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("row timestamps");
        assert_eq!(
            (min, max),
            (TEST_WALL_MS, TEST_WALL_MS),
            "{table}.created_at is wall-clock too",
        );
    }
}

/// D-089: a reconcile whose scan reproduces the manifest that built the last
/// generation must not mint another one.
///
/// Before this, every reconcile minted unconditionally — `build_generation`'s
/// first statement is `uuids.next_uuid()` and the row is committed before a single
/// file is examined. The watcher schedules a reconcile for *any* path event with
/// no filtering at all, while the scan is gitignore-aware, so on the owner's store
/// every write into `target/` bought a generation: 114 an hour, and generations
/// #5415..#5422 were byte-identical in membership, 479 files each. Each one is a
/// permanent cost, because 06 §5 pins `building`/`projection_ready` roots
/// unconditionally and the embedding backfill walks every pin root every cycle.
#[tokio::test]
async fn an_unchanged_tree_does_not_mint_another_generation() {
    let fx = fixture().await;
    write(&fx.root, "a.rs", b"fn a() {}\n");
    let uuids = SeqUuidV7::new();
    let m = meta(&fx);
    let cfg = ClassifierConfig::new(1 << 20);
    let scanner = Scanner::new();
    let mut cache = StatCache::new();

    let first = reconcile_once(
        &fx.db,
        &m,
        ScanMode::Strict,
        &mut cache,
        &cfg,
        &scanner,
        &uuids,
        1000,
        None,
    )
    .await
    .expect("first reconcile");
    let built = first.expect_built().generation_id.clone();
    let last = LastBuilt {
        manifest: first.manifest,
        generation_id: built.clone(),
    };
    {
        let read = fx.db.open_read().expect("read");
        assert_eq!(count(&read, "generation"), 1);
    }

    // Same tree, same manifest: no new generation, and the cycle still names the
    // generation that does describe this tree.
    let again = reconcile_once(
        &fx.db,
        &m,
        ScanMode::Strict,
        &mut cache,
        &cfg,
        &scanner,
        &uuids,
        2000,
        Some(&last),
    )
    .await
    .expect("second reconcile");
    assert!(
        again.outcome.built().is_none(),
        "an unchanged tree must not mint a generation"
    );
    assert_eq!(again.outcome.generation_id(), built);
    {
        let read = fx.db.open_read().expect("read");
        assert_eq!(
            count(&read, "generation"),
            1,
            "the generation table must not have grown"
        );
    }

    // A real edit is still indexed — the skip is a skip, not a stop.
    write(&fx.root, "a.rs", b"fn a() { let x = 1; }\n");
    let after_edit = reconcile_once(
        &fx.db,
        &m,
        ScanMode::Strict,
        &mut cache,
        &cfg,
        &scanner,
        &uuids,
        3000,
        Some(&last),
    )
    .await
    .expect("third reconcile");
    assert_ne!(
        after_edit.expect_built().generation_id,
        built,
        "a changed tree must mint"
    );
    {
        let read = fx.db.open_read().expect("read");
        assert_eq!(count(&read, "generation"), 2);
    }
}

/// The skip is decided **only** by the `last_built` the caller supplies, so a
/// caller that supplies none — `local-rag index`, the benches — always builds.
/// That keeps an explicit user command explicit.
#[tokio::test]
async fn a_caller_without_a_remembered_manifest_always_builds() {
    let fx = fixture().await;
    write(&fx.root, "a.rs", b"fn a() {}\n");
    let uuids = SeqUuidV7::new();
    let m = meta(&fx);
    let cfg = ClassifierConfig::new(1 << 20);
    let scanner = Scanner::new();
    let mut cache = StatCache::new();

    for now_ms in [1000, 2000, 3000] {
        let report = reconcile_once(
            &fx.db,
            &m,
            ScanMode::Strict,
            &mut cache,
            &cfg,
            &scanner,
            &uuids,
            now_ms,
            None,
        )
        .await
        .expect("reconcile");
        report.expect_built();
    }
    let read = fx.db.open_read().expect("read");
    assert_eq!(count(&read, "generation"), 3);
}

#[tokio::test]
async fn reconcile_mode_fast_uses_cache_strict_rehashes() {
    let fx = fixture().await;
    write(&fx.root, "a.rs", b"fn a() {}\n");
    write(&fx.root, "b.rs", b"fn b() {}\n");
    let uuids = SeqUuidV7::new();
    let m = meta(&fx);
    let cfg = ClassifierConfig::new(1 << 20);
    let scanner = Scanner::new();
    let mut cache = StatCache::new();

    // Cold Fast scan: nothing cached → everything hashed.
    let r1 = reconcile_once(
        &fx.db,
        &m,
        ScanMode::Fast,
        &mut cache,
        &cfg,
        &scanner,
        &uuids,
        1000,
        None,
    )
    .await
    .expect("cold fast");
    assert_eq!(r1.mode, ScanMode::Fast);
    assert_eq!(r1.scan.reused, 0, "a cold cache hashes every candidate");
    assert!(r1.scan.hashed >= 2);

    // Warm Fast scan: unchanged files reuse their cached hash.
    let r2 = reconcile_once(
        &fx.db,
        &m,
        ScanMode::Fast,
        &mut cache,
        &cfg,
        &scanner,
        &uuids,
        2000,
        None,
    )
    .await
    .expect("warm fast");
    assert!(r2.scan.reused >= 2, "a warm cache is trusted in fast mode");
    assert_eq!(r2.scan.hashed, 0);

    // Strict scan ignores the warm cache and re-hashes all (the watcher-overflow /
    // periodic / startup path, spec 06 §1 `[FIXED]`).
    let r3 = reconcile_once(
        &fx.db,
        &m,
        ScanMode::Strict,
        &mut cache,
        &cfg,
        &scanner,
        &uuids,
        3000,
        None,
    )
    .await
    .expect("strict");
    assert_eq!(r3.mode, ScanMode::Strict);
    assert_eq!(r3.scan.reused, 0, "strict re-hashes every candidate");
    assert!(r3.scan.hashed >= 2);
}

/// Seed an `active` generation via the legal state machine (a test precondition, not
/// production activation — G05 forbids simulating activation in *product* code).
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

#[tokio::test]
async fn cancellation_leaves_active_generation_valid() {
    let fx = fixture().await;
    write(&fx.root, "a.rs", b"fn a() {}\n");
    let uuids = SeqUuidV7::new();
    let m = meta(&fx);
    let cfg = ClassifierConfig::new(1 << 20);
    let scanner = Scanner::new();

    let gen_a = uuids.next_uuid().to_string();
    seed_active_generation(&fx.db, &fx.worktree_id, &gen_a).await;
    {
        let read = fx.db.open_read().expect("read");
        assert_eq!(
            active_generations(&read, &fx.worktree_id).expect("active"),
            vec![gen_a.clone()],
        );
        assert_eq!(
            current_generation(&read, &fx.worktree_id).expect("current"),
            Some(gen_a.clone()),
        );
    }

    // Cancel a reconcile by dropping its future at the first await (the biased,
    // already-ready branch wins). Any generation the writer committed before the
    // drop is an abandoned `building`/`projection_ready` row set, never activated.
    let mut cache = StatCache::new();
    tokio::select! {
        biased;
        _ = std::future::ready(()) => {}
        _ = reconcile_once(&fx.db, &m, ScanMode::Strict, &mut cache, &cfg, &scanner, &uuids, 1000, None) => {
            panic!("the reconcile future should have been dropped");
        }
    }

    let read = fx.db.open_read().expect("read");
    assert_eq!(
        active_generations(&read, &fx.worktree_id).expect("active"),
        vec![gen_a.clone()],
        "the active generation is untouched by a cancelled reconcile",
    );
    assert_eq!(
        current_generation(&read, &fx.worktree_id).expect("current"),
        Some(gen_a.clone()),
    );
    assert_eq!(
        generation_state(&read, &gen_a).expect("state"),
        Some(GenerationState::Active),
    );
    drop(read);

    // A completed reconcile also never clobbers the active generation: it stops at
    // projection_ready and leaves the current pointer alone.
    let mut cache2 = StatCache::new();
    let report = reconcile_once(
        &fx.db,
        &m,
        ScanMode::Strict,
        &mut cache2,
        &cfg,
        &scanner,
        &uuids,
        2000,
        None,
    )
    .await
    .expect("completed reconcile");

    let read = fx.db.open_read().expect("read");
    assert_eq!(
        active_generations(&read, &fx.worktree_id).expect("active"),
        vec![gen_a.clone()],
        "the new generation is projection_ready, not active",
    );
    assert_eq!(
        generation_state(&read, &report.expect_built().generation_id).expect("state"),
        Some(GenerationState::ProjectionReady),
    );
    assert_eq!(
        current_generation(&read, &fx.worktree_id).expect("current"),
        Some(gen_a),
        "the current-generation pointer still points at A",
    );
}

#[tokio::test]
async fn load_worktree_meta_reads_root_kind_and_prune_roots() {
    let fx = fixture().await;
    let (w, p) = (
        fx.worktree_id.clone(),
        fx.root.to_string_lossy().into_owned(),
    );
    fx.db
        .writer()
        .transaction(move |tx| observe_worktree_path(tx, &w, &p, &p, "fp-main", 1000))
        .await
        .expect("observe path");

    let loaded = load_worktree_meta(&fx.db, &fx.worktree_id, CaseSensitivity::Sensitive)
        .expect("meta")
        .expect("worktree exists");
    assert_eq!(loaded.root, fx.root);
    assert_eq!(loaded.kind, WorktreeKind::Main);
    assert!(loaded.is_git());
    assert!(loaded.prune_roots.is_empty(), "no nested worktrees yet");

    assert!(
        load_worktree_meta(
            &fx.db,
            "018f0000-0000-7000-8000-0000000000ff",
            CaseSensitivity::Sensitive,
        )
        .expect("meta")
        .is_none(),
        "an unknown worktree yields None",
    );
}

#[tokio::test]
async fn nested_prune_roots_lists_same_repo_sibling_under_root() {
    let fx = fixture().await;
    // A second worktree of the same repo, checked out nested under fx.root.
    let sibling = "018f0000-0000-7000-8000-0000000000b2".to_string();
    let nested = fx.root.join(".worktrees/wt2");
    let (r, s, p) = (
        fx.repo_id.clone(),
        sibling.clone(),
        nested.to_string_lossy().into_owned(),
    );
    fx.db
        .writer()
        .transaction(move |tx| {
            create_worktree(tx, &s, &r, WorktreeKind::Linked, 1000)?;
            observe_worktree_path(tx, &s, &p, &p, "fp-wt2", 1000)
        })
        .await
        .expect("seed sibling");

    let roots = nested_prune_roots(&fx.db, &fx.worktree_id, &fx.root).expect("prune roots");
    assert_eq!(
        roots,
        vec![".worktrees/wt2".to_string()],
        "the sibling worktree's subtree is a prune root (worktree-relative)",
    );
}
