//! The daemon-managed indexing supervisor (spec 02 §1 `[FIXED]`, §4.1 step 4
//! "start workers", §4.3's idle-gate, §3.3 `[FIXED]`: no ambient current
//! project) — T20-06.
//!
//! [`spawn_supervisor`] starts one [`spawn_worktree_task`] (T20-05) per
//! `enabled` row of the `managed_worktree` registry (T20-01) — a **list of
//! background work**, never an ambient "current project": request routing is
//! unchanged by this module (spec 02 §3.3's own as-built note). The registry
//! table is durable; a running supervisor only ever caches a live projection
//! of it, so registrations made while the daemon was down are picked up the
//! next time it starts, and registrations made by another process (`local-rag
//! project add`, T20-08) while this daemon is up are picked up by
//! [`SupervisorHandle::reload`] (an explicit `admin/projects_reload`, T20-07,
//! or this module's own slow backstop poll) — "notify is a hint, the table is
//! truth," spec 06 §1's own discipline for the reconcile watcher, applied
//! here to the registry itself.
//!
//! Deliberately never started in [`crate::daemon::mode::DaemonMode::
//! MigrationOnly`] (see `lifecycle.rs`'s `state_db.as_ref()` gate, the same
//! one the two startup resume passes and the consolidation-trigger worker
//! already use) — there is no usable `state.sqlite` to read the registry
//! from, or to project into.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use local_rag_core::config::DataPolicy;
use local_rag_core::identity::{Uuid, UuidSource};
use local_rag_core::paths::StoreLayout;
use local_rag_index::classify::ClassifierConfig;
use local_rag_store::{
    CacheDb, ManagedWorktree, RetentionParams, StateDb, WorktreeLockRegistry, managed_worktrees,
};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use super::worktree_task::{WorktreeTaskHandle, WorktreeTaskParams, spawn_worktree_task};
use crate::daemon::embedder_provider::LazyEmbedderProvider;
use crate::daemon::jobs::JobRegistry;

/// How many worktree tasks the supervisor starts concurrently — during its
/// own cold start and during any [`SupervisorHandle::reload`] that must bring
/// up several rows at once — a chosen, not derived, `[SPEC]` constant, the
/// same footing as `daemon::probe::LIVENESS_PROBE_TIMEOUT_MS`: `N`
/// newly-registered projects must not all begin their initial `Startup`
/// reconcile scan in the same instant. This bounds how many worktree tasks
/// *begin* starting up (thread spawn, registry/meta read, watcher install, the
/// `Startup` trigger send) at once; it is not a global admission-control
/// limiter on how many background reconcile scans may still be *in flight*
/// past that point — at v0's scale (the worktrees one daemon process has been
/// explicitly told to manage) that stronger guarantee is not worth the extra
/// coupling of every worktree task reporting scan-completion back to this
/// supervisor.
const MAX_CONCURRENT_STARTUP_RECONCILES: usize = 2;

/// Everything [`spawn_supervisor`] needs, shared by every worktree task it
/// starts — the daemon-wide handles [`WorktreeTaskParams`] itself needs
/// (minus the per-worktree `worktree_id`, filled in per row) plus the
/// supervisor's own backstop cadence.
pub struct SupervisorParams {
    pub state: Arc<StateDb>,
    pub cache: Arc<CacheDb>,
    pub layout: StoreLayout,
    pub uuids: Arc<dyn UuidSource + Send + Sync>,
    /// The daemon's single `L2` lock registry (T20-04) — shared with
    /// `SearchEngine` and every worktree task this supervisor starts.
    pub locks: Arc<WorktreeLockRegistry>,
    /// The daemon's single ONNX-session owner (T20-03) — shared with every
    /// worktree task this supervisor starts.
    pub embedder_provider: Arc<LazyEmbedderProvider>,
    pub jobs: JobRegistry,
    pub model_space_id: Uuid,
    pub retention: RetentionParams,
    pub data_policy: DataPolicy,
    pub classifier: ClassifierConfig,
    /// How often the backstop poll re-reads `managed_worktree` and
    /// reconciles the live task set against it, in case a notification was
    /// missed. A field, not a hardcoded constant, so tests can drive it
    /// directly — the same rationale `DaemonHandle`'s own
    /// `consolidation_poll_interval` already documents.
    pub backstop_poll_interval: Duration,
}

impl SupervisorParams {
    fn task_params(&self, worktree_id: Uuid) -> WorktreeTaskParams {
        WorktreeTaskParams {
            state: Arc::clone(&self.state),
            cache: Arc::clone(&self.cache),
            layout: self.layout.clone(),
            uuids: Arc::clone(&self.uuids),
            locks: Arc::clone(&self.locks),
            embedder_provider: Arc::clone(&self.embedder_provider),
            jobs: self.jobs.clone(),
            worktree_id,
            model_space_id: self.model_space_id,
            retention: self.retention,
            data_policy: self.data_policy,
            classifier: self.classifier,
        }
    }
}

/// How many tasks a single [`SupervisorHandle::reload`] (or the cold start
/// inside [`spawn_supervisor`]) started/stopped — lets a caller (a test, or a
/// future `admin/projects_reload`, T20-07) observe that a reload applied
/// exactly the expected delta, not a guess derived from timing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReloadOutcome {
    pub started: usize,
    pub stopped: usize,
}

enum Command {
    Reload(oneshot::Sender<ReloadOutcome>),
    Shutdown(oneshot::Sender<()>),
}

/// A running indexing supervisor. [`SupervisorHandle::reload`] and
/// [`SupervisorHandle::shutdown`] are the only two things a caller can do to
/// it — the live task set itself is owned exclusively by the actor task
/// [`spawn_supervisor`] spawns, never shared behind a lock, so there is no
/// way for a caller to observe or mutate it except through those two calls.
#[derive(Debug)]
pub struct SupervisorHandle {
    commands: mpsc::Sender<Command>,
    join: JoinHandle<()>,
}

impl SupervisorHandle {
    /// Re-read `managed_worktree` and bring the live task set to match it:
    /// start any newly-enabled/added row, stop any disabled/removed one.
    /// Idempotent — a `reload()` with nothing changed returns
    /// `ReloadOutcome::default()`.
    pub async fn reload(&self) -> ReloadOutcome {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self.commands.send(Command::Reload(reply_tx)).await.is_err() {
            // The actor already exited (should only happen post-`shutdown`,
            // which consumes `self` — this branch exists so a stray call
            // through a shared reference some future caller holds fails soft
            // rather than panicking).
            return ReloadOutcome::default();
        }
        reply_rx.await.unwrap_or_default()
    }

    /// Stop every running worktree task and the supervisor's own backstop
    /// loop, then wait for both to fully exit. Leaves no dangling tasks and
    /// no orphaned `building` generation — each stopped
    /// [`WorktreeTaskHandle::stop`] already flushes its own last successful
    /// generation before returning (T20-05's own contract).
    pub async fn shutdown(self) {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .commands
            .send(Command::Shutdown(reply_tx))
            .await
            .is_ok()
        {
            let _ = reply_rx.await;
        }
        let _ = self.join.await;
    }
}

/// Start the indexing supervisor: spawns its actor task (a plain
/// `tokio::spawn`, not a dedicated OS thread — the supervisor itself holds no
/// `!Send` future, only [`WorktreeTaskHandle`]s, which are `Send`) and
/// returns immediately, mirroring `lifecycle.rs`'s own
/// `spawn_consolidation_trigger`: non-blocking relative to daemon readiness
/// (spec 02 §4.1 step 4 binds the socket and marks ready without waiting on
/// this). The actor's own cold start (reading the registry and bringing up
/// every `enabled` row) proceeds in the background from here.
pub fn spawn_supervisor(params: SupervisorParams) -> SupervisorHandle {
    let (commands_tx, commands_rx) = mpsc::channel(4);
    let join = tokio::spawn(run_supervisor(params, commands_rx));
    SupervisorHandle {
        commands: commands_tx,
        join,
    }
}

async fn run_supervisor(params: SupervisorParams, mut commands: mpsc::Receiver<Command>) {
    let mut tasks: HashMap<String, WorktreeTaskHandle> = HashMap::new();
    reconcile(&params, &mut tasks).await;

    let mut backstop = tokio::time::interval(params.backstop_poll_interval);
    backstop.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // `interval` fires its first tick immediately — consume it here so the
    // real cadence starts counting from *after* the cold start above, not
    // from before it.
    backstop.tick().await;

    loop {
        tokio::select! {
            cmd = commands.recv() => {
                match cmd {
                    Some(Command::Reload(reply)) => {
                        let outcome = reconcile(&params, &mut tasks).await;
                        let _ = reply.send(outcome);
                    }
                    Some(Command::Shutdown(reply)) => {
                        stop_all(tasks).await;
                        let _ = reply.send(());
                        return;
                    }
                    None => {
                        // Every `SupervisorHandle` was dropped without an
                        // explicit `shutdown()` — still leave no dangling
                        // tasks behind rather than leaking them.
                        stop_all(tasks).await;
                        return;
                    }
                }
            }
            _ = backstop.tick() => {
                reconcile(&params, &mut tasks).await;
            }
        }
    }
}

/// Bring `tasks` (the live set) to match `managed_worktree`'s `enabled` rows:
/// stop anything no longer wanted, start anything newly wanted (staggered in
/// [`MAX_CONCURRENT_STARTUP_RECONCILES`]-sized batches). Used both for the
/// supervisor's own cold start and for every later reload/backstop tick — the
/// same operation applied to whatever the current live set happens to be.
async fn reconcile(
    params: &SupervisorParams,
    tasks: &mut HashMap<String, WorktreeTaskHandle>,
) -> ReloadOutcome {
    let rows = match read_managed_worktrees(&params.state) {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!("local-rag: indexing supervisor could not read managed_worktree: {e}");
            return ReloadOutcome::default();
        }
    };

    let wanted: HashMap<String, ManagedWorktree> = rows
        .into_iter()
        .filter(|row| row.enabled)
        .map(|row| (row.worktree_id.clone(), row))
        .collect();

    let stale: Vec<String> = tasks
        .keys()
        .filter(|id| !wanted.contains_key(id.as_str()))
        .cloned()
        .collect();
    let mut stopped = 0usize;
    for id in stale {
        if let Some(handle) = tasks.remove(&id) {
            handle.stop().await;
            stopped += 1;
        }
    }

    let missing: Vec<String> = wanted
        .keys()
        .filter(|id| !tasks.contains_key(id.as_str()))
        .cloned()
        .collect();
    let mut started = 0usize;
    for chunk in missing.chunks(MAX_CONCURRENT_STARTUP_RECONCILES) {
        let mut starting = Vec::with_capacity(chunk.len());
        for id in chunk {
            let Ok(worktree_id) = id.parse::<Uuid>() else {
                tracing::error!(
                    "local-rag: indexing supervisor: managed_worktree row {id:?} is not a valid worktree id"
                );
                continue;
            };
            let task_params = params.task_params(worktree_id);
            starting.push((id.clone(), tokio::spawn(spawn_worktree_task(task_params))));
        }
        for (id, join) in starting {
            match join.await {
                Ok(Ok(handle)) => {
                    tasks.insert(id, handle);
                    started += 1;
                }
                Ok(Err(e)) => {
                    tracing::error!(
                        "local-rag: indexing supervisor could not start task for worktree {id}: {e}"
                    );
                }
                Err(e) => {
                    tracing::error!(
                        "local-rag: indexing supervisor's start call for worktree {id} panicked: {e}"
                    );
                }
            }
        }
    }

    ReloadOutcome { started, stopped }
}

/// Stop every task in `tasks` concurrently and wait for all of them —
/// `WorktreeTaskHandle::stop` already flushes each task's own last successful
/// generation before returning (T20-05), so this leaves no dangling tasks and
/// no orphaned `building` generation behind.
async fn stop_all(tasks: HashMap<String, WorktreeTaskHandle>) {
    let mut stopping = Vec::with_capacity(tasks.len());
    for (_, handle) in tasks {
        stopping.push(tokio::spawn(handle.stop()));
    }
    for join in stopping {
        let _ = join.await;
    }
}

/// A synchronous registry read — the same "quick blocking `open_read` call
/// made directly from async code, no `spawn_blocking`" pattern
/// `DaemonHandle::idle_inputs` already accepts for
/// `store_has_pending_spool_bytes`.
fn read_managed_worktrees(state: &StateDb) -> Result<Vec<ManagedWorktree>, String> {
    let conn = state.open_read().map_err(|e| e.to_string())?;
    managed_worktrees(&conn).map_err(|e| e.to_string())
}
