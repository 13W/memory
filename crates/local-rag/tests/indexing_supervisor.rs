//! T20-06 acceptance tests for the daemon-managed indexing supervisor —
//! in-process `DaemonHandle` scenarios (no external subprocess needed; see
//! `tests/serve_subprocess.rs`'s own two-worktree real-process scenario for
//! the one acceptance criterion that specifically calls for a genuine second
//! OS process).
//!
//! Every worktree here is a real (non-git) temp directory with one seed
//! file, registered directly against `state.sqlite` *before*
//! `DaemonHandle::start` ever opens it — the same pre-seed-then-start
//! ordering `tests/support/mod.rs::seed_indexed_worktree` and
//! `daemon::indexing::worktree_task`'s own `Fixture` already establish, and
//! the same reason: the daemon's own `ensure_store_instance_uuid`/migration
//! path must be the thing that first opens the store for real.

mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use local_rag::daemon::{DaemonHandle, LazyEmbedderProvider, ProviderProbe};
use local_rag::indexing::register_new_worktree;
use local_rag_core::identity::domain::path_fingerprint;
use local_rag_core::identity::{Uuid, UuidSource, uuidv7_from};
use local_rag_embed::{Embedder, HashingEmbedder};
use local_rag_store::{
    DEFAULT_MODEL_SPACE_ID, RepresentationKind, StateDb, WorktreeKind, WorktreeRootFacts,
    generation_state, projection_state, register_managed_worktree, register_representation,
    set_managed_enabled, set_model_space_representation,
};
use local_rag_test_support::TempHome;
use support::{open_layout, start_options};

/// Every test in this file spins up one or two full `DaemonHandle`s (real
/// sockets, real dedicated OS threads per worktree task, real `state.sqlite`
/// writer threads). Libtest runs `#[tokio::test]` functions within one binary
/// concurrently by default (one OS thread per test) — four of these heavy
/// scenarios competing for the same CPU cores at once self-induces enough
/// contention to blow past any reasonable bound, independent of the actual
/// product code's speed. Serializing this file's own tests (never a lock any
/// production code touches) keeps each test's timing representative of *its*
/// work, not of how many siblings happened to be scheduled alongside it —
/// the same reasoning `cli_service.rs`'s own subprocess-heavy tests already
/// apply via `--test-threads` in CI, made explicit here instead of relying on
/// an external flag.
static SEQUENTIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn serialize_heavy_daemon_tests() -> tokio::sync::MutexGuard<'static, ()> {
    SEQUENTIAL.lock().await
}

/// A deterministic, non-random UUID source — mirrors
/// `daemon::indexing::worktree_task`'s own test fixture `SeqUuids`, with a
/// distinct numeric range so a developer reading a failure from either file
/// never has to wonder which fixture produced a given id.
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
        uuidv7_from(9_800_000 + n, [0x56; 10])
    }
}

fn facts_for(root: &Path) -> WorktreeRootFacts {
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

/// Register the two representations (`code_raw`, `memory`) the default model
/// space needs before any worktree can reach `ProjectionReady` — mirrors
/// `daemon::indexing::worktree_task`'s own test fixture exactly (same
/// `HashingEmbedder`-derived keys, so the `Ready` embedder provider these
/// tests hand `DaemonHandle::start` produces vectors under the same keys
/// this seeds).
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

/// Create a real (non-git) worktree directory under `home`, register it, and
/// enroll it in `managed_worktree` — `enabled` as given.
async fn seed_managed_worktree(
    home: &TempHome,
    state: &StateDb,
    uuids: &SeqUuids,
    dir_name: &str,
    enabled: bool,
    now_ms: i64,
) -> (Uuid, PathBuf) {
    let root = home.join(dir_name);
    std::fs::create_dir_all(&root).expect("create worktree root");
    std::fs::write(root.join("main.rs"), "fn parse_config() {}\n").expect("seed file");

    let repo_id = uuids.next_uuid();
    let worktree_id = uuids.next_uuid();
    let facts = facts_for(&root);
    register_new_worktree(state, repo_id, worktree_id, &facts, now_ms)
        .await
        .expect("register worktree");

    let worktree_id_str = worktree_id.to_string();
    state
        .writer()
        .transaction(move |tx| {
            register_managed_worktree(tx, &worktree_id_str, now_ms)?;
            if !enabled {
                set_managed_enabled(tx, &worktree_id_str, false, now_ms)?;
            }
            Ok(())
        })
        .await
        .expect("enroll managed worktree");

    (worktree_id, root)
}

/// A `Ready`-from-the-start embedder provider — no ONNX, no installed model,
/// same `HashingEmbedder` fixture `daemon::indexing::worktree_task`'s own
/// tests already use.
fn ready_embedder_provider() -> Arc<LazyEmbedderProvider> {
    Arc::new(LazyEmbedderProvider::with_probes(
        || ProviderProbe::Ready(Arc::new(HashingEmbedder::new(RepresentationKind::CodeRaw))),
        || ProviderProbe::Ready(Arc::new(HashingEmbedder::new(RepresentationKind::Memory))),
    ))
}

/// Bounded, event-driven wait — mirrors
/// `daemon::indexing::worktree_task`'s own tests' `wait_for`: a real but tiny
/// poll interval (this crate carries no `tokio` `test-util`, so there is no
/// paused virtual clock), bounded convergence, never a fixed sleep standing
/// in for "enough time must have passed."
async fn wait_for(deadline: Duration, mut check: impl FnMut() -> bool) {
    tokio::time::timeout(deadline, async {
        while !check() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("condition did not become true within the bound");
}

/// The worktree's current `active_generation_id`, if it has ever reached one.
fn active_generation(state: &StateDb, worktree_id: &Uuid) -> Option<String> {
    let conn = state.open_read().expect("open a read connection");
    projection_state(&conn, &worktree_id.to_string())
        .expect("read projection state")
        .and_then(|row| row.active_generation_id)
}

#[tokio::test]
async fn a_managed_worktree_is_indexed_at_startup_and_survives_a_daemon_restart() {
    let _guard = serialize_heavy_daemon_tests().await;
    let (home, layout) = open_layout();
    let uuids = SeqUuids::new();
    let now_ms = 1_000;

    let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
    register_representations(&state, now_ms).await;
    let (worktree_id, _root) =
        seed_managed_worktree(&home, &state, &uuids, "repo", true, now_ms).await;
    drop(state);

    let mut opts = start_options(layout.clone());
    opts.embedder_provider = ready_embedder_provider();
    // D-093: this is the one test that restarts a daemon *in the same
    // process*, so it is the one test that cannot rely on the production
    // shutdown budget. Since D-090 a drain that does not finish inside it
    // reports `WorkersDrained::No` and deliberately keeps the store lock until
    // the process exits — correct for a real daemon, unreachable here, and the
    // restart below would then refuse with `Lock(Locked { .. })` naming this
    // very daemon. Under `nextest`'s process-per-test load the default 3 s is
    // not enough for an indexing cycle to stop, which made this a test about
    // thread scheduling rather than about cold-start recovery.
    opts.indexing_shutdown_budget = Duration::from_secs(60);
    let handle = DaemonHandle::start(opts).await.expect("start");

    let state = StateDb::open(layout.state_db()).expect("reopen state.sqlite for polling");
    wait_for(Duration::from_secs(45), || {
        active_generation(&state, &worktree_id).is_some()
    })
    .await;
    drop(state);
    handle.shutdown().await;

    // Restart against the same store, with nothing re-registered: the row
    // written before the *first* start is still there, so the supervisor's
    // own cold start must bring the task back up unattended.
    let mut opts2 = start_options(layout.clone());
    opts2.embedder_provider = ready_embedder_provider();
    let handle2 = DaemonHandle::start(opts2).await.expect("restart");

    let state = StateDb::open(layout.state_db()).expect("reopen state.sqlite for polling");
    let before = active_generation(&state, &worktree_id);
    // Touch the seed file repeatedly (not just once) so the restarted task
    // has fresh work to do: a *cold-start* reconcile that happens to lose a
    // one-off race with this process's own just-shut-down daemon's not-yet-
    // reclaimed resources (`DaemonHandle::shutdown`'s `handshake_join.abort()`
    // is deliberately non-blocking — its own doc comment notes production
    // only ever reclaims that task's state via a full `Runtime` drop between
    // daemon lifecycles, something this same-process, same-runtime restart
    // never does) is expected to recover on its own on the *next* trigger
    // (spec 06 §1: "no retry is scheduled... the next successful reconcile
    // trigger tries again on its own", `worktree_task.rs::project_one`'s own
    // doc) — so this loop keeps supplying one.
    let mut attempt = 0u32;
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            attempt += 1;
            std::fs::write(
                home.join("repo").join("main.rs"),
                format!("fn parse_config() {{}}\nfn two_{attempt}() {{}}\n"),
            )
            .expect("modify seed file");
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            while tokio::time::Instant::now() < deadline {
                let current = active_generation(&state, &worktree_id);
                if current.is_some() && current != before {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    })
    .await
    .expect("the restarted task never produced a new generation");

    handle2.shutdown().await;
}

#[tokio::test]
async fn a_disabled_managed_worktree_is_never_started() {
    let _guard = serialize_heavy_daemon_tests().await;
    let (home, layout) = open_layout();
    let uuids = SeqUuids::new();
    let now_ms = 1_000;

    let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
    register_representations(&state, now_ms).await;
    let (enabled_id, _) = seed_managed_worktree(&home, &state, &uuids, "on", true, now_ms).await;
    let (disabled_id, _) = seed_managed_worktree(&home, &state, &uuids, "off", false, now_ms).await;
    drop(state);

    let mut opts = start_options(layout.clone());
    opts.embedder_provider = ready_embedder_provider();
    let handle = DaemonHandle::start(opts).await.expect("start");

    let state = StateDb::open(layout.state_db()).expect("reopen state.sqlite for polling");
    wait_for(Duration::from_secs(45), || {
        active_generation(&state, &enabled_id).is_some()
    })
    .await;
    // By the time the *enabled* sibling — started in the very same staggered
    // batch (`MAX_CONCURRENT_STARTUP_RECONCILES`) — has completed a full
    // reconcile/embed/activate/materialize cycle, a mistakenly-started task
    // for the disabled row would have had at least as much real wall-clock
    // opportunity to do the same.
    assert!(
        active_generation(&state, &disabled_id).is_none(),
        "a disabled managed_worktree row must never be started"
    );

    handle.shutdown().await;
}

#[tokio::test]
async fn reload_starts_and_stops_exactly_the_delta() {
    let _guard = serialize_heavy_daemon_tests().await;
    let (home, layout) = open_layout();
    let uuids = SeqUuids::new();
    let now_ms = 1_000;

    let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
    register_representations(&state, now_ms).await;
    let (first_id, first_root) =
        seed_managed_worktree(&home, &state, &uuids, "first", true, now_ms).await;
    drop(state);

    let mut opts = start_options(layout.clone());
    opts.embedder_provider = ready_embedder_provider();
    let handle = DaemonHandle::start(opts).await.expect("start");

    let state = StateDb::open(layout.state_db()).expect("reopen state.sqlite for polling");
    wait_for(Duration::from_secs(45), || {
        active_generation(&state, &first_id).is_some()
    })
    .await;

    // Register a second worktree and disable the first — both directly
    // against the table, exactly as a live `local-rag project add`/`disable`
    // (T20-08) would, with no daemon restart in between.
    let second_id = {
        let root = home.join("second");
        std::fs::create_dir_all(&root).expect("create worktree root");
        std::fs::write(root.join("main.rs"), "fn other() {}\n").expect("seed file");
        let repo_id = uuids.next_uuid();
        let worktree_id = uuids.next_uuid();
        let facts = facts_for(&root);
        register_new_worktree(&state, repo_id, worktree_id, &facts, now_ms)
            .await
            .expect("register second worktree");
        let id_str = worktree_id.to_string();
        state
            .writer()
            .transaction(move |tx| register_managed_worktree(tx, &id_str, now_ms))
            .await
            .expect("enroll second worktree");
        worktree_id
    };
    let first_id_str = first_id.to_string();
    state
        .writer()
        .transaction(move |tx| set_managed_enabled(tx, &first_id_str, false, now_ms))
        .await
        .expect("disable first worktree");

    let outcome = handle
        .indexing_supervisor()
        .expect("supervisor runs outside MigrationOnly")
        .reload()
        .await;
    assert_eq!(outcome.started, 1, "exactly the newly-enrolled row starts");
    assert_eq!(outcome.stopped, 1, "exactly the newly-disabled row stops");

    // The started delta really runs...
    wait_for(Duration::from_secs(45), || {
        active_generation(&state, &second_id).is_some()
    })
    .await;

    // ...and the stopped delta really stops: touching the first worktree's
    // file after `reload()` must produce no further generation.
    let before = active_generation(&state, &first_id);
    std::fs::write(
        first_root.join("main.rs"),
        "fn parse_config() {}\nfn three() {}\n",
    )
    .expect("modify first worktree's file");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        active_generation(&state, &first_id),
        before,
        "a disabled-then-reloaded worktree must no longer react to file changes"
    );

    handle.shutdown().await;
}

#[tokio::test]
async fn shutdown_leaves_no_dangling_task_and_no_orphaned_building_generation() {
    let _guard = serialize_heavy_daemon_tests().await;
    let (home, layout) = open_layout();
    let uuids = SeqUuids::new();
    let now_ms = 1_000;

    let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
    register_representations(&state, now_ms).await;
    let (id_a, _) = seed_managed_worktree(&home, &state, &uuids, "a", true, now_ms).await;
    let (id_b, _) = seed_managed_worktree(&home, &state, &uuids, "b", true, now_ms).await;
    drop(state);

    let mut opts = start_options(layout.clone());
    opts.embedder_provider = ready_embedder_provider();
    let handle = DaemonHandle::start(opts).await.expect("start");
    let jobs = handle.jobs.clone();

    let state = StateDb::open(layout.state_db()).expect("reopen state.sqlite for polling");
    wait_for(Duration::from_secs(45), || {
        active_generation(&state, &id_a).is_some() && active_generation(&state, &id_b).is_some()
    })
    .await;

    handle.shutdown().await;

    assert_eq!(jobs.len(), 0, "no job guard survives shutdown");

    // Every worktree's active generation must have reached `ProjectionReady`
    // — never left stuck `Building` by a task that was torn down mid-cycle.
    let conn = state.open_read().expect("open a read connection");
    for id in [&id_a, &id_b] {
        let active = projection_state(&conn, &id.to_string())
            .expect("read projection state")
            .and_then(|row| row.active_generation_id)
            .expect("an active generation exists");
        let generation = generation_state(&conn, &active)
            .expect("read generation state")
            .expect("the active generation row exists");
        assert_eq!(
            generation,
            local_rag_store::GenerationState::Active,
            "shutdown must not leave an orphaned `Building` generation"
        );
    }
}

/// `D-093`: the supervisor must read its shutdown budget from its params, not
/// from the constant.
///
/// Structural, in the shape `D-054`'s own guard already established for
/// `lifecycle.rs`, because the failure mode is silent: an injected budget that
/// something ignores looks exactly like an injected budget that works, until a
/// loaded machine disagrees. The constant stays — it is the production
/// default — but only as a definition and as prose; no code line may reach for
/// it directly.
#[test]
fn the_supervisor_takes_its_shutdown_budget_from_its_params() {
    let source = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/daemon/indexing/supervisor.rs"
    ))
    .expect("read supervisor.rs");

    let direct: Vec<&str> = source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .filter(|line| {
            !line
                .trim_start()
                .starts_with("pub const SHUTDOWN_JOIN_BUDGET")
        })
        .filter(|line| line.contains("SHUTDOWN_JOIN_BUDGET"))
        .collect();

    assert!(
        direct.is_empty(),
        "the supervisor must take its shutdown budget from `SupervisorParams` (D-093), but these \
         lines read the constant directly: {direct:?}"
    );
}
