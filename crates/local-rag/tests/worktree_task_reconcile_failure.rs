//! T20-05 acceptance test: a failed reconcile does not stop the per-worktree
//! task's cycle, and the next trigger succeeds normally.
//!
//! Needs a real, deterministic reconcile failure — `local_rag_index::
//! reconcile::build`'s own named failpoint (`reconcile.build.after_allocate`,
//! "fail immediately after allocation, before any per-file work") gives
//! exactly that, the same mechanism `crates/models/tests/install_faults.rs`
//! already uses for its own crash-point tests. Global failpoint state is
//! process-wide, so this lives in its own file/binary — isolated from every
//! other `local-rag` test, none of which touch this failpoint.

#![cfg(feature = "failpoints")]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use local_rag::daemon::{
    JobRegistry, LazyEmbedderProvider, ProviderProbe, WorktreeTaskParams, spawn_worktree_task,
};
use local_rag::indexing::register_new_worktree;
use local_rag_core::config::DataPolicy;
use local_rag_core::identity::domain::path_fingerprint;
use local_rag_core::identity::{Uuid, UuidSource, uuidv7_from};
use local_rag_core::paths::StoreLayout;
use local_rag_embed::{Embedder, HashingEmbedder};
use local_rag_index::classify::ClassifierConfig;
use local_rag_store::{
    CacheDb, DEFAULT_MODEL_SPACE_ID, RepresentationKind, RetentionParams, StateDb, WorktreeKind,
    WorktreeLockRegistry, WorktreeRootFacts, register_representation,
    set_model_space_representation,
};
use local_rag_test_support::{Action, TempHome, failpoint::global};

const FAILPOINT: &str = "reconcile.build.after_allocate";

/// Arm on construction, guaranteed to disarm on drop (even on panic) — a
/// leaked arming would break every later run of this failpoint elsewhere in
/// the workspace, mirroring `crates/models/tests/install_faults.rs::Armed`.
struct Armed;

impl Armed {
    fn new() -> Self {
        global().register(FAILPOINT);
        global().arm(FAILPOINT, Action::Error).expect("arm");
        Armed
    }

    fn disarm(&self) {
        let _ = global().disarm(FAILPOINT);
    }
}

impl Drop for Armed {
    fn drop(&mut self) {
        let _ = global().disarm(FAILPOINT);
    }
}

struct SeqUuids {
    counter: AtomicU64,
}

impl SeqUuids {
    fn new() -> Self {
        SeqUuids {
            counter: AtomicU64::new(0),
        }
    }
}

impl UuidSource for SeqUuids {
    fn next_uuid(&self) -> Uuid {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        uuidv7_from(9_600_000 + n, [0x63; 10])
    }
}

fn facts_for(root: &std::path::Path) -> WorktreeRootFacts {
    let path = root.display().to_string();
    WorktreeRootFacts {
        observed_canonical_path: path.clone(),
        display_path: path.clone(),
        path_fingerprint: path_fingerprint(&path),
        kind: WorktreeKind::NonGit,
        common_dir_fingerprint: None,
        remote_fingerprint: None,
    }
}

async fn register_representations(state: &StateDb, now_ms: i64) {
    let code_key = HashingEmbedder::new(RepresentationKind::CodeRaw).key();
    let memory_key = HashingEmbedder::new(RepresentationKind::Memory).key();
    state
        .writer()
        .transaction(move |tx| {
            let code_id = register_representation(tx, "test-code-raw", &code_key, now_ms)?;
            set_model_space_representation(
                tx,
                DEFAULT_MODEL_SPACE_ID,
                RepresentationKind::CodeRaw,
                &code_id,
                true,
                now_ms,
            )?;
            let memory_id = register_representation(tx, "test-memory", &memory_key, now_ms)?;
            set_model_space_representation(
                tx,
                DEFAULT_MODEL_SPACE_ID,
                RepresentationKind::Memory,
                &memory_id,
                true,
                now_ms,
            )
        })
        .await
        .expect("register representations");
}

async fn wait_for(deadline: Duration, mut check: impl FnMut() -> bool) {
    tokio::time::timeout(deadline, async {
        while !check() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("condition did not become true within the bound");
}

#[tokio::test]
async fn a_failed_reconcile_does_not_stop_the_cycle_and_the_next_trigger_succeeds() {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let root = home.join("repo");
    std::fs::create_dir_all(&root).expect("create worktree root");
    std::fs::write(root.join("main.rs"), "fn one() {}").expect("seed file");

    let state = Arc::new(StateDb::open(layout.state_db()).expect("open state.sqlite"));
    let cache =
        Arc::new(CacheDb::open(layout.cache_db(), "test-instance").expect("open cache.sqlite"));
    let now_ms = 1_000;
    register_representations(&state, now_ms).await;

    let uuids: Arc<dyn UuidSource + Send + Sync> = Arc::new(SeqUuids::new());
    let repo_id = uuids.next_uuid();
    let worktree_id = uuids.next_uuid();
    let facts = facts_for(&root);
    register_new_worktree(&state, repo_id, worktree_id, &facts, now_ms)
        .await
        .expect("register worktree");

    let embedder_provider = Arc::new(LazyEmbedderProvider::with_probes(
        || ProviderProbe::Ready(Arc::new(HashingEmbedder::new(RepresentationKind::CodeRaw))),
        || ProviderProbe::Ready(Arc::new(HashingEmbedder::new(RepresentationKind::Memory))),
    ));
    let jobs = JobRegistry::new();

    let params = WorktreeTaskParams {
        state: state.clone(),
        cache,
        layout,
        uuids,
        locks: Arc::new(WorktreeLockRegistry::new()),
        embedder_provider,
        jobs,
        worktree_id,
        model_space_id: DEFAULT_MODEL_SPACE_ID.parse().expect("valid UUID"),
        retention: RetentionParams {
            keep_last_k: 2,
            window_ms: 7 * 24 * 60 * 60 * 1000,
        },
        data_policy: DataPolicy::LocalOnly,
        classifier: ClassifierConfig::new(1024 * 1024),
    };

    let armed = Armed::new();
    let handle = spawn_worktree_task(params).await.expect("start task");

    // The cold-start `Startup` trigger's reconcile fails deterministically —
    // `consecutive_failures` is monotonic until the next success, so a
    // bounded poll is race-free here (unlike a transient job-guard window).
    wait_for(Duration::from_secs(10), || {
        handle.status().consecutive_failures > 0
    })
    .await;
    let failed_status = handle.status();
    assert!(failed_status.last_generation_id.is_none());
    assert!(failed_status.last_error.is_some());

    armed.disarm();
    std::fs::write(root.join("main.rs"), "fn one() {}\nfn two() {}").expect("modify file");

    wait_for(Duration::from_secs(10), || {
        handle.status().last_generation_id.is_some()
    })
    .await;
    let recovered = handle.status();
    assert_eq!(recovered.consecutive_failures, 0);
    assert!(recovered.last_error.is_none());

    handle.stop().await;
}
