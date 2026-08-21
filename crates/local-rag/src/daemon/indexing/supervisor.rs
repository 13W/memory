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
use local_rag_index::reconcile::TriggerKind;
use local_rag_store::{
    CacheDb, ManagedWorktree, RetentionParams, StateDb, WorktreeLockRegistry, managed_worktrees,
};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use super::worktree_task::{
    WorktreeTaskHandle, WorktreeTaskParams, WorktreeTaskStatus, spawn_worktree_task,
};
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct ReloadOutcome {
    pub started: usize,
    pub stopped: usize,
}

/// One `managed_worktree` row (T20-01, durable) joined with its worktree
/// task's live status (T20-05), if one is currently running — `admin/
/// projects_list`'s (T20-07) own shape. `task` is `None` for a `disabled`
/// row, or (transiently) for an `enabled` row whose task has not finished
/// starting up yet; it is never fabricated to look like a healthy task that
/// does not exist.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ProjectStatus {
    pub worktree_id: String,
    pub enabled: bool,
    pub registered_at: i64,
    pub updated_at: i64,
    pub task: Option<WorktreeTaskStatus>,
}

/// Why [`SupervisorClient::reconcile_now`] (`admin/reconcile_now`, T20-07)
/// could not inject a trigger — today, the only reason: `worktree_id` names
/// no row with a task currently running (never registered, registered but
/// `enabled = 0`, or an `enabled` row whose task has not started yet). The
/// registry and the live task set are deliberately not distinguished any
/// further here — from a caller's perspective "reconcile a specific worktree
/// right now" either works or it does not, and `dispatch.rs` maps this one
/// variant to a single JSON-RPC `-32602`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileNowError {
    NotManaged,
}

impl std::fmt::Display for ReconcileNowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReconcileNowError::NotManaged => {
                write!(f, "worktree is not managed (or has no task running)")
            }
        }
    }
}

impl std::error::Error for ReconcileNowError {}

enum Command {
    Reload(oneshot::Sender<ReloadOutcome>),
    ListProjects(oneshot::Sender<Vec<ProjectStatus>>),
    TriggerNow(String, oneshot::Sender<Result<(), ReconcileNowError>>),
    Shutdown(oneshot::Sender<()>),
}

/// A cheap, `Clone`-able handle over the supervisor's command channel —
/// everything a caller can do to a running supervisor *except*
/// [`SupervisorHandle::shutdown`], which stays exclusive to the single owner
/// (`DaemonHandle`, T20-06). Derived via [`SupervisorHandle::client`] and
/// shared with the MCP surface (T20-07's `admin/*` verbs): `McpHandler` is
/// constructed and starts serving connections independently of
/// `DaemonHandle`'s own lifetime, so it cannot hold a borrowed
/// `&SupervisorHandle` — only this owned, freely-cloneable handle. The live
/// task set itself is owned exclusively by the actor task
/// [`spawn_supervisor`] spawns, never shared behind a lock — every method
/// here is a round-trip through the command channel, never a direct read.
#[derive(Debug, Clone)]
pub struct SupervisorClient {
    commands: mpsc::Sender<Command>,
}

impl SupervisorClient {
    /// Re-read `managed_worktree` and bring the live task set to match it:
    /// start any newly-enabled/added row, stop any disabled/removed one.
    /// Idempotent — a `reload()` with nothing changed returns
    /// `ReloadOutcome::default()`.
    pub async fn reload(&self) -> ReloadOutcome {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self.commands.send(Command::Reload(reply_tx)).await.is_err() {
            // The actor already exited (`shutdown` consumes the owning
            // `SupervisorHandle` — this branch exists so a stray call
            // through a clone taken beforehand fails soft rather than
            // panicking).
            return ReloadOutcome::default();
        }
        reply_rx.await.unwrap_or_default()
    }

    /// Every enrolled worktree — durable fields (T20-01) joined with live
    /// task status (T20-05), if a task is currently running for it —
    /// `admin/projects_list`'s (T20-07) own data source.
    pub async fn list_projects(&self) -> Vec<ProjectStatus> {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .commands
            .send(Command::ListProjects(reply_tx))
            .await
            .is_err()
        {
            return Vec::new();
        }
        reply_rx.await.unwrap_or_default()
    }

    /// Inject [`local_rag_index::reconcile::TriggerKind::Manual`] into
    /// `worktree_id`'s task directly — `admin/reconcile_now`'s (T20-07) own
    /// mechanism. `Err(ReconcileNowError::NotManaged)` when no task is
    /// currently running for that id (never registered, disabled, or not yet
    /// started).
    pub async fn reconcile_now(&self, worktree_id: &str) -> Result<(), ReconcileNowError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .commands
            .send(Command::TriggerNow(worktree_id.to_string(), reply_tx))
            .await
            .is_err()
        {
            return Err(ReconcileNowError::NotManaged);
        }
        reply_rx.await.unwrap_or(Err(ReconcileNowError::NotManaged))
    }
}

/// A running indexing supervisor — the single owner `DaemonHandle` (T20-06)
/// holds. [`Self::client`] derives the shareable [`SupervisorClient`] every
/// other caller uses; [`Self::shutdown`] is exclusive to this handle.
#[derive(Debug)]
pub struct SupervisorHandle {
    client: SupervisorClient,
    join: JoinHandle<()>,
}

impl SupervisorHandle {
    /// See [`SupervisorClient::reload`].
    pub async fn reload(&self) -> ReloadOutcome {
        self.client.reload().await
    }

    /// See [`SupervisorClient::list_projects`].
    pub async fn list_projects(&self) -> Vec<ProjectStatus> {
        self.client.list_projects().await
    }

    /// See [`SupervisorClient::reconcile_now`].
    pub async fn reconcile_now(&self, worktree_id: &str) -> Result<(), ReconcileNowError> {
        self.client.reconcile_now(worktree_id).await
    }

    /// A cheap `Clone`-able handle sharing every operation above except
    /// [`Self::shutdown`] — for callers that must outlive or out-scope this
    /// `SupervisorHandle`'s own borrow lifetime (T20-07's `McpHandler`).
    pub fn client(&self) -> SupervisorClient {
        self.client.clone()
    }

    /// Stop every running worktree task and the supervisor's own backstop
    /// loop, then wait for both to fully exit. Leaves no dangling tasks and
    /// no orphaned `building` generation — each stopped
    /// [`WorktreeTaskHandle::stop`] already flushes its own last successful
    /// generation before returning (T20-05's own contract).
    pub async fn shutdown(self) {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .client
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
        client: SupervisorClient {
            commands: commands_tx,
        },
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
                    Some(Command::ListProjects(reply)) => {
                        let _ = reply.send(list_projects(&params, &tasks));
                    }
                    Some(Command::TriggerNow(worktree_id, reply)) => {
                        let result = match tasks.get(&worktree_id) {
                            Some(handle) => {
                                let _ = handle.trigger(TriggerKind::Manual).await;
                                Ok(())
                            }
                            None => Err(ReconcileNowError::NotManaged),
                        };
                        let _ = reply.send(result);
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

/// Every enrolled worktree — durable fields joined with live task status, if
/// a task is currently running for it — [`SupervisorClient::list_projects`]'s
/// own implementation. A read failure degrades to an empty list (logged),
/// the same fail-soft the rest of this module already applies to
/// `read_managed_worktrees`.
fn list_projects(
    params: &SupervisorParams,
    tasks: &HashMap<String, WorktreeTaskHandle>,
) -> Vec<ProjectStatus> {
    let rows = match read_managed_worktrees(&params.state) {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!(
                "local-rag: indexing supervisor could not read managed_worktree for projects_list: {e}"
            );
            return Vec::new();
        }
    };
    rows.into_iter()
        .map(|row| ProjectStatus {
            task: tasks.get(&row.worktree_id).map(WorktreeTaskHandle::status),
            worktree_id: row.worktree_id,
            enabled: row.enabled,
            registered_at: row.registered_at,
            updated_at: row.updated_at,
        })
        .collect()
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

/// How long shutdown waits for the indexing tasks after cancelling them.
///
/// Comfortably inside `cli::service::STOP_TIMEOUT` (10 s), because the drain
/// this bounds is only the first of shutdown's steps — the checkpoint, the
/// cache close and the lock release all still have to happen before
/// `local-rag stop` can report success.
pub(crate) const SHUTDOWN_JOIN_BUDGET: Duration = Duration::from_secs(3);

/// Cancel every task in `tasks`, then wait — bounded — for their threads
/// (`D-077`).
///
/// **Cancel, not ask.** Spec 02 §4.3 is explicit that SIGTERM "cancel[s]
/// reconciles at the next safe point (state tx boundaries)", and
/// [`WorktreeTaskHandle::stop`] alone does not cancel anything: it signals,
/// then *joins the OS thread*, so it waits out however long the in-flight
/// `project_generation` still has to run. On a large store mid-indexing that
/// is minutes, and `local-rag stop` gives up after ten seconds while the
/// daemon keeps running — measured on the owner's 24 GB store, where the only
/// way out was `kill -9`.
///
/// [`WorktreeTaskHandle::abort`] is the preemptive half, and the composition
/// below is the one its own doc prescribes ("a caller that also wants to wait
/// for the background thread to fully exit afterward may still call `stop`").
/// It had no production caller before this; the one place it was exercised was
/// `worktree_task`'s own `cancelling_the_task_mid_cycle_leaves_the_store_valid
/// _and_reusable`, which is also the proof that cancelling here cannot corrupt
/// the store.
///
/// **Why the wait is still bounded.** Cancellation lands on an `.await`, and
/// the embed loop's are its `flush` calls — genuine `cache.sqlite` transaction
/// boundaries, exactly what the spec asks for. But `run_backfill` also has a
/// long *synchronous* stretch before the first of them (`blob_index` →
/// `occurrences_for_fts`, called straight from an `async fn`), and no cancel
/// can preempt that. So the budget exists for the case where the thread is
/// inside it. Abandoning the join there is safe by the spec's own next
/// sentence — "Kill at any point is safe by construction (05, 07)" — and by
/// what the stretch actually is: a read. The thread finishes it, unwinds at
/// its next `.await`, and exits; the process simply does not wait.
///
/// Deregistration is deliberately *not* routed through here: `reconcile`'s
/// own `handle.stop().await` stays graceful, because a worktree leaving the
/// registry is not a reason to throw away the projection it is halfway
/// through.
pub(crate) async fn stop_all(tasks: HashMap<String, WorktreeTaskHandle>) {
    let mut stopping = Vec::with_capacity(tasks.len());
    for (_, handle) in tasks {
        handle.abort();
        stopping.push(tokio::spawn(handle.stop()));
    }
    let joined = async {
        for join in stopping {
            let _ = join.await;
        }
    };
    if tokio::time::timeout(SHUTDOWN_JOIN_BUDGET, joined)
        .await
        .is_err()
    {
        tracing::warn!(
            "local-rag: an indexing task did not exit within {SHUTDOWN_JOIN_BUDGET:?} of being \
             cancelled — it is inside a synchronous scan and will exit on its own; shutdown \
             continues without it"
        );
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
