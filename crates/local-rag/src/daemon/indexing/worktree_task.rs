//! One worktree's continuous background indexing task (spec 06 §1 `[FIXED]`:
//! "watcher = hint, reconcile = truth") — T20-05.
//!
//! [`spawn_worktree_task`] composes the same primitives `cli::watch`'s
//! `run_watch_loop` already does (`local_rag_index::reconcile::
//! {spawn_reconciler, spawn_watcher}`, forced `TriggerKind::Startup`, a
//! `tokio::select!` over `successes`/`failures`/shutdown), but as a
//! standalone daemon background task instead of a foreground CLI loop, and
//! with one addition `cli::watch` never needed: every successful
//! `project_generation` call runs inside `local_rag::indexing::write_locked`
//! (T20-04) — `L2.write` held for that call only, **not** around
//! `reconcile_once` itself, which runs on its own debounced schedule inside
//! the separately-spawned `WorktreeReconciler` task and is never called
//! directly from here (spec 02 §5's write path is realized structurally for
//! the reconcile step by that task's own single-owner design, per its
//! module doc; `L2.write`'s practical value here is giving a concurrent
//! `SearchEngine::search_code` call the `BUSY_RETRY` spec 02 §6 promises
//! while a generation is being projected).
//!
//! # Why a dedicated OS thread, not a plain `tokio::spawn`
//!
//! `project_generation`'s embedding step (`local_rag_embed::run_backfill`)
//! deliberately keeps one `state.sqlite` read connection open across the
//! whole backfill pass — `state_read` is read at the start and used again
//! for `write_coverage` at the end, so every subject sees one consistent
//! snapshot. `rusqlite::Connection` is `!Sync`, so a `&Connection` held
//! across an `.await` makes the enclosing future `!Send`; that is a correct,
//! load-bearing property of `run_backfill`, not a bug to route around by
//! editing `crates/embed`. `tokio::spawn` requires `F: Send`, so this task's
//! loop cannot be handed to it directly.
//!
//! The fix is the same one `local-rag-tui`'s `admin_client.rs` already uses
//! for its own long-lived, independently-cancellable background work: run
//! everything on one dedicated OS thread with its own single-threaded Tokio
//! runtime (`Builder::new_current_thread`), where nothing ever needs to move
//! across threads, so `!Send` futures are legal. The loop itself still runs
//! as a real, independently cancellable task — `tokio::task::LocalSet` +
//! `spawn_local` give a `!Send`-friendly task with the same
//! `JoinHandle`/`AbortHandle` semantics `tokio::spawn` would, which is what
//! makes `WorktreeTaskHandle::abort` a genuine preemptive cancel (unlike
//! `std::thread`, which cannot be preempted from outside, or
//! `spawn_blocking`, whose task keeps running to completion regardless of
//! `abort()`).
//!
//! `IndexCtx` is assembled by hand from already-open daemon state (never via
//! `local_rag::indexing::finish_index_ctx`, which opens its own ONNX
//! sessions — correct for a one-shot CLI process, explicitly wrong for a
//! daemon that must keep to at most two sessions per process, T20-03):
//! `embedder`/`memory_embedder` are read fresh from the shared
//! `LazyEmbedderProvider` on every tick, so a model installed after the
//! daemon started is picked up without a restart (D-037), same as the query
//! side.
//!
//! This module deliberately does not wire itself into `daemon::lifecycle`/
//! `DaemonHandle` — that composition (N tasks, started from the
//! `managed_worktree` registry, `reload()`, shutdown) is T20-06's scope.

use std::sync::{Arc, Mutex};

use local_rag_core::config::DataPolicy;
use local_rag_core::identity::{Uuid, UuidSource};
use local_rag_core::paths::StoreLayout;
use local_rag_core::redaction::Scanner;
use local_rag_index::classify::ClassifierConfig;
use local_rag_index::reconcile::{
    MetaError, ReconcileHandle, ScheduleConfig, SystemWallClock, TriggerKind, WorktreeReconciler,
    load_worktree_meta, spawn_reconciler, spawn_watcher,
};
use local_rag_store::{
    CacheDb, IndexingOutcome, RetentionParams, StateDb, WorktreeLockRegistry, write_indexing_status,
};
use tokio::sync::{mpsc, oneshot};
use tokio::task::AbortHandle;

use crate::daemon::embedder_provider::LazyEmbedderProvider;
use crate::daemon::jobs::{JobKind, JobRegistry};
use crate::indexing::{IndexCtx, project_generation, write_locked};

/// Everything one worktree's task needs — connection handles shared with the
/// rest of the daemon, plus the indexing defaults a future caller (T20-06)
/// derives from `config.toml`/`X-001`'s fixed retention values.
pub struct WorktreeTaskParams {
    pub state: Arc<StateDb>,
    pub cache: Arc<CacheDb>,
    pub layout: StoreLayout,
    pub uuids: Arc<dyn UuidSource + Send + Sync>,
    /// The daemon's single `L2` lock registry (T20-04) — shared with
    /// `SearchEngine`, not constructed here.
    pub locks: Arc<WorktreeLockRegistry>,
    /// The daemon's single ONNX-session owner (T20-03) — `embedder`/
    /// `memory_embedder` are read from this on every tick, never opened here.
    pub embedder_provider: Arc<LazyEmbedderProvider>,
    pub jobs: JobRegistry,
    pub worktree_id: Uuid,
    pub model_space_id: Uuid,
    pub retention: RetentionParams,
    pub data_policy: DataPolicy,
    pub classifier: ClassifierConfig,
}

/// In-memory status of a running (or just-stopped) worktree task — read by
/// the admin surface (T20-07's `admin/projects_list`), not persisted
/// anywhere.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct WorktreeTaskStatus {
    /// The most recently *projected* (embedded/activated/materialized)
    /// generation — distinct from the reconciler's own `successes` watch,
    /// which only means "built," not yet "served."
    pub last_generation_id: Option<String>,
    /// When [`Self::last_generation_id`] last changed, Unix milliseconds.
    ///
    /// This is the millisecond the cycle **started** (the same reading
    /// [`Self::in_progress_since`] got), not the one it finished at. X-006's
    /// durable mirror `worktree_indexing_status.last_success_at` records the
    /// finish instead, which is the more useful of the two for "how stale is
    /// this index"; the two therefore differ by one cycle's duration. Left as
    /// it is deliberately — restamping this field is a T20-05 behavior change,
    /// not X-006's business.
    pub last_success_ms: Option<i64>,
    /// Consecutive reconcile/project failures since the last success.
    pub consecutive_failures: u32,
    /// The most recent failure's human-readable cause, if any.
    pub last_error: Option<String>,
    /// When the *current* embed/activate/materialize cycle started, Unix
    /// milliseconds — `None` whenever the task is idle, waiting on its next
    /// trigger (D-049's forward note): without this, a future progress
    /// indicator (`local-rag stats`, already given consolidation's own
    /// elapsed/ETA the same way by D-049) has no way to compute either for
    /// indexing. Set at [`JobGuard`](crate::daemon::jobs::JobGuard)
    /// acquisition in [`project_one`], cleared back to `None` once that call
    /// returns — success or failure alike.
    pub in_progress_since: Option<i64>,
}

/// Why [`spawn_worktree_task`] could not start.
#[derive(Debug)]
pub enum WorktreeTaskStartError {
    /// Loading the worktree's registry metadata failed.
    Meta(MetaError),
    /// The worktree row (or its current path) no longer resolves.
    WorktreeVanished,
    /// The filesystem watcher could not be started (`notify`'s error,
    /// captured as text — this crate does not depend on `notify` directly,
    /// the same trade-off `cli::watch.rs` already makes for the identical
    /// error).
    Watcher(String),
    /// This task's dedicated single-threaded Tokio runtime could not be
    /// built (resource exhaustion) — vanishingly rare, kept typed rather
    /// than panicking the background OS thread over it.
    Runtime(String),
    /// The background OS thread ended (panicked) before it could report
    /// either outcome above.
    ThreadPanicked,
}

impl std::fmt::Display for WorktreeTaskStartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorktreeTaskStartError::Meta(e) => write!(f, "could not load worktree metadata: {e}"),
            WorktreeTaskStartError::WorktreeVanished => {
                write!(f, "the worktree vanished from the registry")
            }
            WorktreeTaskStartError::Watcher(e) => {
                write!(f, "could not start the filesystem watcher: {e}")
            }
            WorktreeTaskStartError::Runtime(e) => {
                write!(f, "could not build the worktree task's runtime: {e}")
            }
            WorktreeTaskStartError::ThreadPanicked => {
                write!(
                    f,
                    "the worktree task's background thread panicked during startup"
                )
            }
        }
    }
}

impl std::error::Error for WorktreeTaskStartError {}

/// What the background OS thread reports back once it has finished starting
/// up (whether or not startup succeeded).
struct StartedTask {
    status: Arc<Mutex<WorktreeTaskStatus>>,
    abort_handle: AbortHandle,
    trigger: mpsc::Sender<TriggerKind>,
}

/// A running worktree task: read [`WorktreeTaskHandle::status`] at any time,
/// [`WorktreeTaskHandle::stop`] it gracefully, [`WorktreeTaskHandle::abort`]
/// it immediately, or [`WorktreeTaskHandle::trigger`] an out-of-band reconcile
/// (T20-07's `admin/reconcile_now`).
#[derive(Debug)]
pub struct WorktreeTaskHandle {
    status: Arc<Mutex<WorktreeTaskStatus>>,
    stop: oneshot::Sender<()>,
    abort_handle: AbortHandle,
    thread: std::thread::JoinHandle<()>,
    /// A clone of the reconciler's own trigger sender (T20-07) — the
    /// original is moved into the loop's `spawn_local` closure (used there
    /// only to `drop` it on shutdown, signaling the reconciler to flush and
    /// exit); this clone is `mpsc`, so cloning it costs nothing structurally
    /// and does not change when the reconciler task itself sees every sender
    /// dropped.
    trigger: mpsc::Sender<TriggerKind>,
}

impl WorktreeTaskHandle {
    /// A snapshot of the task's current status.
    pub fn status(&self) -> WorktreeTaskStatus {
        self.status
            .lock()
            .expect("worktree task status mutex poisoned")
            .clone()
    }

    /// Send `kind` to this worktree's reconciler directly — T20-07's
    /// `admin/reconcile_now` injects [`TriggerKind::Manual`] this way.
    /// Fire-and-forget: returns once the trigger is *enqueued*, not once the
    /// resulting reconcile (if any) has finished — the same "next trigger
    /// tries again on its own" contract [`project_one`]'s own doc already
    /// establishes for every other trigger source. `Err` means the
    /// reconciler task has already ended (the worktree task is mid-shutdown);
    /// harmless to ignore, same as every other `sender.send(..)` call site in
    /// this module.
    pub async fn trigger(
        &self,
        kind: TriggerKind,
    ) -> Result<(), mpsc::error::SendError<TriggerKind>> {
        self.trigger.send(kind).await
    }

    /// Signal the task to stop, then wait for its background thread to
    /// finish — including its own final flush of any generation the
    /// reconciler published after the loop last observed the channel
    /// (mirrors `cli::watch.rs`'s own shutdown-time flush).
    ///
    /// Drops [`Self::trigger`] explicitly, **before** waiting on the thread:
    /// `WorktreeReconciler::run` only returns once *every* clone of its
    /// trigger sender is dropped (`crates/index/src/reconcile/driver.rs`'s
    /// own `ReconcileHandle` doc — "drop all senders to shut the task
    /// down"), and the dedicated thread's own shutdown branch already
    /// `drop`s its copy and awaits that exact return. Leaving this handle's
    /// clone alive across the `spawn_blocking` `.await` below — the field
    /// would otherwise sit unused-but-live in this async fn's own state
    /// machine — would make that a deadlock every `stop()` call would hit,
    /// not a rare race: the thread can never observe "every sender dropped"
    /// while this one is still held here.
    pub async fn stop(self) {
        let _ = self.stop.send(());
        drop(self.trigger);
        let _ = tokio::task::spawn_blocking(move || self.thread.join()).await;
    }

    /// Cancel the task's loop immediately, mid-cycle if one is in flight —
    /// unlike [`Self::stop`], this does not wait for an in-progress
    /// `project_generation` call to finish. Safe to call any number of
    /// times; a caller that also wants to wait for the background thread to
    /// fully exit afterward may still call [`Self::stop`] (harmless once the
    /// loop is already gone).
    pub fn abort(&self) {
        self.abort_handle.abort();
    }
}

/// A callback fired synchronously, right after `project_one` takes its
/// `JobGuard` — [`spawn_worktree_task`]'s production callers get a no-op;
/// tests use [`spawn_worktree_task_instrumented`] to observe that instant
/// deterministically (`JobRegistry::len()` is already incremented by the
/// time this fires), instead of racing a real, possibly sub-millisecond
/// window with a poll loop.
pub(crate) type JobStartedHook = Arc<dyn Fn() + Send + Sync>;

/// Start one worktree's continuous reconcile/project cycle on its own
/// dedicated background thread (see the module doc for why). Returns once
/// the cold-start `TriggerKind::Startup` trigger has been *sent* (not once
/// it has finished projecting) — the returned handle's status starts empty
/// and fills in as the background task makes progress.
pub async fn spawn_worktree_task(
    params: WorktreeTaskParams,
) -> Result<WorktreeTaskHandle, WorktreeTaskStartError> {
    spawn_worktree_task_instrumented(params, Arc::new(|| {})).await
}

/// [`spawn_worktree_task`], plus a [`JobStartedHook`] — the seam
/// `#[cfg(test)]` uses to synchronize with the task's own `JobGuard` window
/// exactly, mirroring `local_rag_search::SearchEngine::
/// search_code_instrumented`'s identical "same logic, plus an observer only
/// tests use" shape.
pub(crate) async fn spawn_worktree_task_instrumented(
    params: WorktreeTaskParams,
    on_job_started: JobStartedHook,
) -> Result<WorktreeTaskHandle, WorktreeTaskStartError> {
    let (start_tx, start_rx) = oneshot::channel::<Result<StartedTask, WorktreeTaskStartError>>();
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let worktree_id = params.worktree_id;

    let thread = std::thread::Builder::new()
        .name(format!("worktree-task-{worktree_id}"))
        .spawn(move || run_on_dedicated_thread(params, stop_rx, start_tx, on_job_started))
        .expect("spawn the worktree task's background OS thread");

    match start_rx.await {
        Ok(Ok(started)) => Ok(WorktreeTaskHandle {
            status: started.status,
            stop: stop_tx,
            abort_handle: started.abort_handle,
            thread,
            trigger: started.trigger,
        }),
        Ok(Err(e)) => {
            let _ = tokio::task::spawn_blocking(move || thread.join()).await;
            Err(e)
        }
        Err(_recv_error) => {
            let _ = tokio::task::spawn_blocking(move || thread.join()).await;
            Err(WorktreeTaskStartError::ThreadPanicked)
        }
    }
}

/// The body of the dedicated OS thread: build a single-threaded Tokio
/// runtime, do the (`!Send`-free) setup, then hand the loop itself to a
/// `LocalSet`-spawned task so it stays independently abortable, and finally
/// block on that task until it returns (gracefully or via [`AbortHandle::abort`]).
fn run_on_dedicated_thread(
    params: WorktreeTaskParams,
    stop_rx: oneshot::Receiver<()>,
    start_tx: oneshot::Sender<Result<StartedTask, WorktreeTaskStartError>>,
    on_job_started: JobStartedHook,
) {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let _ = start_tx.send(Err(WorktreeTaskStartError::Runtime(e.to_string())));
            return;
        }
    };

    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async move {
        let case = crate::daemon::gitroot::case_sensitivity();
        let meta = match load_worktree_meta(&params.state, &params.worktree_id.to_string(), case)
        {
            Ok(Some(meta)) => meta,
            Ok(None) => {
                let _ = start_tx.send(Err(WorktreeTaskStartError::WorktreeVanished));
                return;
            }
            Err(e) => {
                let _ = start_tx.send(Err(WorktreeTaskStartError::Meta(e)));
                return;
            }
        };

        let reconciler = WorktreeReconciler::new(
            params.state.clone(),
            meta.clone(),
            params.classifier,
            Scanner::new(),
            params.uuids.clone(),
            Arc::new(SystemWallClock),
            ScheduleConfig::default(),
        );
        let ReconcileHandle {
            sender,
            join: reconciler_join,
            failures,
            successes,
        } = spawn_reconciler(reconciler, 8);

        let watcher = match spawn_watcher(&meta.root, meta.is_git(), sender.clone()) {
            Ok(w) => w,
            Err(e) => {
                drop(sender);
                let _ = reconciler_join.await;
                let _ = start_tx.send(Err(WorktreeTaskStartError::Watcher(e.to_string())));
                return;
            }
        };

        // Cold start: reconcile once immediately, mirroring `cli::watch.rs`'s
        // own forced first trigger — never wait for the first real change or
        // the periodic 6h backstop.
        let _ = sender.send(TriggerKind::Startup).await;

        // T20-07: a clone kept *outside* the loop's own `spawn_local`
        // closure below, for `WorktreeTaskHandle::trigger` (`admin/
        // reconcile_now`) to send into directly. Cloned before `sender`
        // itself moves into that closure, where it is used only to `drop`
        // (never to send) — see `WorktreeTaskHandle::stop`'s own doc for why
        // this clone must not outlive that `drop`.
        let trigger_sender = sender.clone();

        let status = Arc::new(Mutex::new(WorktreeTaskStatus::default()));
        let status_for_loop = Arc::clone(&status);

        let inner = tokio::task::spawn_local(async move {
            let mut successes = successes;
            let mut failures = failures;
            let mut stop = stop_rx;
            loop {
                tokio::select! {
                    _ = &mut stop => {
                        drop(watcher);
                        drop(sender);
                        let _ = reconciler_join.await;
                        // Flush: the shutdown-time reconcile
                        // `WorktreeReconciler::run`'s own doc promises ("any
                        // scheduled reconcile is flushed before returning")
                        // may have published one more success after the
                        // last time this loop observed the channel.
                        let final_generation = successes.borrow().clone();
                        let last = status_for_loop
                            .lock()
                            .expect("worktree task status mutex poisoned")
                            .last_generation_id
                            .clone();
                        if final_generation.is_some() && final_generation != last {
                            project_one(&params, &status_for_loop, final_generation, &on_job_started).await;
                        }
                        return;
                    }
                    changed = successes.changed() => {
                        if changed.is_err() {
                            // The reconciler task ended on its own (should
                            // only happen once every sender is dropped, i.e.
                            // shutdown); fall through to the next iteration,
                            // which observes `stop`.
                            continue;
                        }
                        let generation_id = successes.borrow().clone();
                        let last = status_for_loop
                            .lock()
                            .expect("worktree task status mutex poisoned")
                            .last_generation_id
                            .clone();
                        if generation_id != last {
                            project_one(&params, &status_for_loop, generation_id, &on_job_started).await;
                        }
                    }
                    changed = failures.changed() => {
                        if changed.is_ok()
                            && let Some(f) = failures.borrow().clone()
                        {
                            let mut s = status_for_loop
                                .lock()
                                .expect("worktree task status mutex poisoned");
                            s.consecutive_failures = f.consecutive_failures;
                            s.last_error = Some(f.last_error);
                        }
                    }
                }
            }
        });

        let abort_handle = inner.abort_handle();
        let _ = start_tx.send(Ok(StartedTask {
            status,
            abort_handle,
            trigger: trigger_sender,
        }));
        let _ = inner.await;
    });
}

/// Project `generation_id` (embed → activate → materialize) under `L2.write`,
/// recording the outcome on `status` — never panics, never propagates an
/// error to the caller: a transient failure here must not stop the task from
/// reacting to the *next* trigger (spec 02 §6: nothing degrades silently, but
/// nothing here is fatal either).
///
/// [`JobGuard`](crate::daemon::jobs::JobGuard) is taken immediately before
/// [`write_locked`] and dropped at the end of this function — D-024's
/// discipline: held only for the active span, never while the outer loop
/// merely waits on its next `select!`.
async fn project_one(
    params: &WorktreeTaskParams,
    status: &Arc<Mutex<WorktreeTaskStatus>>,
    generation_id: Option<String>,
    on_job_started: &JobStartedHook,
) {
    let Some(generation_id) = generation_id else {
        return;
    };
    let Ok(gid) = generation_id.parse::<Uuid>() else {
        let mut s = status.lock().expect("worktree task status mutex poisoned");
        s.consecutive_failures += 1;
        s.last_error = Some(format!(
            "internal error: generation id {generation_id} is not a UUID"
        ));
        return;
    };

    // Re-probed every tick, not cached at task-start: a model installed
    // after this daemon started must be picked up without a restart (D-037),
    // the same contract the query side already gets from
    // `LazyEmbedderProvider`. Neither leg has a meaningful placeholder value
    // (`Embedder::key()` is not fallible), so a not-yet-ready model skips
    // this tick's projection entirely rather than writing under a fabricated
    // representation key — no `JobGuard` is taken for a skip, and no retry
    // is scheduled here: the next successful reconcile trigger (a real
    // change, or the periodic 6h backstop, spec 06 §1 `[FIXED]`) tries
    // again on its own.
    let (Some(embedder), Some(memory_embedder)) = (
        params.embedder_provider.code(),
        params.embedder_provider.memory(),
    ) else {
        let mut s = status.lock().expect("worktree task status mutex poisoned");
        s.last_error = Some(
            "embedding model not installed/opened yet; will retry on the next trigger".to_string(),
        );
        return;
    };

    let ctx = IndexCtx {
        state: params.state.clone(),
        cache: params.cache.clone(),
        layout: params.layout.clone(),
        uuids: params.uuids.clone(),
        embedder,
        memory_embedder,
        model_space_id: params.model_space_id,
        retention: params.retention,
        data_policy: params.data_policy,
        classifier: params.classifier,
    };

    let _job = params.jobs.begin(JobKind::Reconcile);
    on_job_started();
    let now_ms = system_now_ms();
    {
        let mut s = status.lock().expect("worktree task status mutex poisoned");
        s.in_progress_since = Some(now_ms);
    }
    let worktree_id_str = params.worktree_id.to_string();
    tracing::info!(
        worktree_id = %worktree_id_str,
        generation_id = %generation_id,
        "indexing cycle started"
    );
    let result = write_locked(
        &params.locks,
        &worktree_id_str,
        project_generation(&ctx, params.worktree_id, gid, now_ms),
    )
    .await;

    // Fold the outcome into the in-memory status first. The guard is confined
    // to this block on purpose: `std::sync::MutexGuard` is not `Send`, so it
    // must not be alive across the durable write's `.await` below.
    let failure = {
        let mut s = status.lock().expect("worktree task status mutex poisoned");
        s.in_progress_since = None;
        match &result {
            Ok(_) => {
                s.last_generation_id = Some(generation_id.clone());
                s.last_success_ms = Some(now_ms);
                s.consecutive_failures = 0;
                s.last_error = None;
                None
            }
            Err(e) => {
                s.consecutive_failures += 1;
                s.last_error = Some(e.to_string());
                Some((s.consecutive_failures, e.to_string()))
            }
        }
    };

    let finished_ms = system_now_ms();
    let duration_ms = finished_ms.saturating_sub(now_ms);
    match (&result, &failure) {
        (Ok(outcome), _) => tracing::info!(
            worktree_id = %worktree_id_str,
            generation_id = %generation_id,
            embedded = outcome.backfill.embedded,
            reused = outcome.backfill.reused,
            embed_failed = outcome.backfill.failed,
            occurrences = outcome.fts.occurrence_count,
            duration_ms,
            "indexing cycle finished"
        ),
        (Err(_), Some((consecutive_failures, reason))) => tracing::warn!(
            worktree_id = %worktree_id_str,
            generation_id = %generation_id,
            consecutive_failures,
            duration_ms,
            reason = %reason,
            "indexing cycle failed"
        ),
        // A failed `result` always produced a `failure` record in the block
        // above; keeping the arm total avoids an `unreachable!` in a path whose
        // whole job is to never panic the background task.
        (Err(_), None) => {}
    }

    // X-006: mirror the outcome durably, so it survives the idle shutdown that
    // erases the in-memory status every quiet 15 minutes. Deliberately outside
    // `write_locked` — one short global-writer transaction, L2 already released.
    // A write failure here is never fatal: the generation is projected either
    // way, so it is logged and dropped like the observability channels above.
    let (wt, gen_owned, err_owned) = (
        worktree_id_str.clone(),
        result.is_ok().then(|| generation_id.clone()),
        failure.as_ref().map(|(_, reason)| reason.clone()),
    );
    let consecutive_failures = failure.as_ref().map_or(0, |(n, _)| *n);
    if let Err(e) = params
        .state
        .writer()
        .transaction(move |tx| {
            write_indexing_status(
                tx,
                &wt,
                IndexingOutcome {
                    attempt_at: now_ms,
                    success: gen_owned.as_deref(),
                    consecutive_failures,
                    last_error: err_owned.as_deref(),
                },
                finished_ms,
            )
        })
        .await
    {
        tracing::warn!(
            worktree_id = %worktree_id_str,
            error = %e,
            "could not persist the indexing status (the index itself is unaffected)"
        );
    }
    // `_job` drops here — before control returns to the outer `select!`.
}

/// The current wall-clock time as Unix milliseconds — mirrors
/// `lifecycle::system_now_ms`/`main.rs::system_now_ms`/
/// `daemon::consolidation_trigger::system_now_ms`'s own established
/// convention: each call site carries its own trivial copy rather than a
/// shared helper.
fn system_now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before Unix epoch")
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use local_rag_core::identity::domain::path_fingerprint;
    use local_rag_core::identity::uuidv7_from;
    use local_rag_embed::{Embedder, HashingEmbedder};
    use local_rag_store::{
        DEFAULT_MODEL_SPACE_ID, RepresentationKind, WorktreeKind, WorktreeRootFacts,
        register_representation, set_model_space_representation,
    };
    use local_rag_test_support::TempHome;

    use super::*;
    use crate::daemon::embedder_provider::ProviderProbe;
    use crate::indexing::register_new_worktree;

    /// A deterministic, non-random UUID source — same "monotone UUIDv7"
    /// convention `crates/local-rag/src/indexing/mod.rs`'s own test fixture
    /// (`SeqUuids`) already establishes.
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
            uuidv7_from(9_500_000 + n, [0x51; 10])
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

    /// A real temp store with one registered worktree (one seed file already
    /// on disk), ready to hand to [`spawn_worktree_task`]. Kept alive for the
    /// whole test — dropping `home` deletes the temp tree.
    struct Fixture {
        _home: TempHome,
        state: Arc<StateDb>,
        cache: Arc<CacheDb>,
        layout: StoreLayout,
        uuids: Arc<dyn UuidSource + Send + Sync>,
        locks: Arc<WorktreeLockRegistry>,
        embedder_provider: Arc<LazyEmbedderProvider>,
        jobs: JobRegistry,
        worktree_id: Uuid,
        root: std::path::PathBuf,
    }

    impl Fixture {
        async fn new(dir_name: &str) -> Self {
            let home = TempHome::new().expect("temp home");
            let layout = StoreLayout::new(home.join("local-rag"));
            layout.ensure().expect("ensure store tree");
            let root = home.join(dir_name);
            std::fs::create_dir_all(&root).expect("create worktree root");
            std::fs::write(
                root.join("main.rs"),
                "fn parse_config(path: &Path) -> Config { unimplemented!() }",
            )
            .expect("seed file");

            let state = Arc::new(StateDb::open(layout.state_db()).expect("open state.sqlite"));
            let cache = Arc::new(
                CacheDb::open(layout.cache_db(), "test-instance").expect("open cache.sqlite"),
            );
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
                || {
                    ProviderProbe::Ready(Arc::new(HashingEmbedder::new(
                        RepresentationKind::CodeRaw,
                    )))
                },
                || ProviderProbe::Ready(Arc::new(HashingEmbedder::new(RepresentationKind::Memory))),
            ));

            Fixture {
                _home: home,
                state,
                cache,
                layout,
                uuids,
                locks: Arc::new(WorktreeLockRegistry::new()),
                embedder_provider,
                jobs: JobRegistry::new(),
                worktree_id,
                root,
            }
        }

        fn params(&self) -> WorktreeTaskParams {
            WorktreeTaskParams {
                state: self.state.clone(),
                cache: self.cache.clone(),
                layout: self.layout.clone(),
                uuids: self.uuids.clone(),
                locks: self.locks.clone(),
                embedder_provider: self.embedder_provider.clone(),
                jobs: self.jobs.clone(),
                worktree_id: self.worktree_id,
                model_space_id: DEFAULT_MODEL_SPACE_ID.parse().expect("valid UUID"),
                retention: RetentionParams {
                    keep_last_k: 2,
                    window_ms: 7 * 24 * 60 * 60 * 1000,
                },
                data_policy: DataPolicy::LocalOnly,
                classifier: ClassifierConfig::new(1024 * 1024),
            }
        }
    }

    /// Bounded, event-driven wait: polls `check` on a short real interval
    /// until it becomes `true` or `deadline` elapses — this crate carries no
    /// `tokio` `test-util` feature (see `daemon::consolidation_trigger`'s own
    /// tests), so there is no paused virtual clock; `tokio::time::sleep`/
    /// `timeout` need only the plain `time` feature already enabled. Mirrors
    /// `crates/local-rag/tests/idle_shutdown.rs::wait_until_idle_eligible`'s
    /// established idiom: a real but tiny poll interval, bounded convergence,
    /// never a fixed sleep standing in for "enough time must have passed."
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
    async fn starting_the_task_indexes_and_projects_the_worktree_without_any_external_process() {
        let fx = Fixture::new("repo").await;
        let handle = spawn_worktree_task(fx.params()).await.expect("start task");

        wait_for(Duration::from_secs(10), || {
            handle.status().last_generation_id.is_some()
        })
        .await;

        let status = handle.status();
        assert!(status.last_generation_id.is_some());
        assert!(status.last_success_ms.is_some());
        assert_eq!(status.consecutive_failures, 0);
        assert!(status.last_error.is_none());

        // X-006: the same outcome is mirrored durably, so it outlives the task
        // (and the whole daemon) rather than dying with the in-memory status.
        let read = fx.state.open_read().expect("read conn");
        let durable = local_rag_store::indexing_status(&read, &fx.worktree_id.to_string())
            .expect("read indexing status")
            .expect("a completed cycle wrote its status");
        assert_eq!(
            durable.last_generation_id, status.last_generation_id,
            "the durable row names the generation the task just projected",
        );
        assert!(durable.last_success_at.is_some(), "a success was recorded");
        assert!(
            durable.last_attempt_at.is_some(),
            "so was the attempt that produced it",
        );
        assert_eq!(durable.consecutive_failures, 0);
        assert_eq!(durable.last_error, None);
        drop(read);

        // Prove it end to end through the real production `SearchEngine` —
        // the same `build_search_engine` the daemon itself uses.
        let query_embedder: Arc<dyn local_rag_search::QueryEmbedder> =
            Arc::new(crate::daemon::EmbedderQueryAdapter::new(Arc::new(
                HashingEmbedder::new(RepresentationKind::CodeRaw),
            )));
        let engine = crate::daemon::search::build_search_engine(
            fx.state.clone(),
            fx.cache.clone(),
            fx.layout.clone(),
            fx.uuids.clone(),
            query_embedder,
            8,
            fx.locks.clone(),
        );
        let response = engine
            .search_code(
                local_rag_search::SearchRequest {
                    // An indexing self-check, not a user query — nothing to translate.
                    query_degraded: None,
                    root: local_rag_store::RequestRoot {
                        worktree_root: Some(facts_for(&fx.root)),
                        repo_hint: None,
                    },
                    query: "parse_config".to_string(),
                    mode: local_rag_protocol::SearchMode::Hybrid,
                    limit: 5,
                    name_pattern: None,
                },
                system_now_ms(),
            )
            .await
            .expect("no infra error")
            .expect("no domain error");
        assert!(
            response.results.iter().any(|r| r.path.ends_with("main.rs")),
            "{:?}",
            response.results
        );

        handle.stop().await;
    }

    #[tokio::test]
    async fn a_file_change_produces_a_new_generation_observed_via_the_tasks_own_status() {
        let fx = Fixture::new("repo").await;
        let root = fx.root.clone();
        let handle = spawn_worktree_task(fx.params()).await.expect("start task");

        wait_for(Duration::from_secs(10), || {
            handle.status().last_generation_id.is_some()
        })
        .await;
        let first_generation = handle.status().last_generation_id;

        std::fs::write(root.join("main.rs"), "fn parse_config() {}\nfn two() {}")
            .expect("modify file");

        wait_for(Duration::from_secs(10), || {
            let s = handle.status();
            s.last_generation_id.is_some() && s.last_generation_id != first_generation
        })
        .await;

        handle.stop().await;
    }

    /// T20-07: `admin/reconcile_now`'s own mechanism, exercised directly —
    /// `trigger(Manual)` must force a fresh reconcile without depending on
    /// the filesystem watcher's own debounce window to eventually pick the
    /// change up on its own.
    #[tokio::test]
    async fn a_manual_trigger_forces_a_new_generation_without_the_watcher() {
        let fx = Fixture::new("repo").await;
        let root = fx.root.clone();
        let handle = spawn_worktree_task(fx.params()).await.expect("start task");

        wait_for(Duration::from_secs(10), || {
            handle.status().last_generation_id.is_some()
        })
        .await;
        let first_generation = handle.status().last_generation_id;

        std::fs::write(root.join("main.rs"), "fn parse_config() {}\nfn three() {}")
            .expect("modify file");
        handle
            .trigger(TriggerKind::Manual)
            .await
            .expect("reconciler still alive");

        wait_for(Duration::from_secs(10), || {
            let s = handle.status();
            s.last_generation_id.is_some() && s.last_generation_id != first_generation
        })
        .await;

        handle.stop().await;
    }

    /// Regression for the deadlock a naive `trigger`-sender-clone design
    /// introduces: `WorktreeReconciler::run` only returns once *every* clone
    /// of its trigger sender is dropped, so `WorktreeTaskHandle::stop` must
    /// drop its own clone before waiting on the background thread — proven
    /// here by actually calling `trigger()` first (the ordinary
    /// `handle.stop()` calls elsewhere in this file never exercise the
    /// held-clone path at all) and bounding `stop()` itself with a timeout,
    /// not just trusting "the test finished eventually."
    #[tokio::test]
    async fn stop_completes_promptly_even_after_a_manual_trigger_was_sent() {
        let fx = Fixture::new("repo").await;
        let handle = spawn_worktree_task(fx.params()).await.expect("start task");

        wait_for(Duration::from_secs(30), || {
            handle.status().last_generation_id.is_some()
        })
        .await;
        handle
            .trigger(TriggerKind::Manual)
            .await
            .expect("reconciler still alive");

        // 45s, not a tight bound: this asserts "does not hang forever," not
        // "is fast" — under heavy contention on this machine, a genuinely
        // non-deadlocked `stop()` has still been observed taking tens of
        // seconds (thread-scheduling delay, not the cyclic wait this test
        // guards against). A real deadlock never completes at all, so any
        // generous bound still catches it.
        tokio::time::timeout(Duration::from_secs(45), handle.stop())
            .await
            .expect(
                "stop() must not deadlock on its own retained trigger-sender clone \
                 after a trigger() call",
            );
    }

    /// Synchronizes on `project_one`'s own `JobGuard` window exactly (via
    /// [`spawn_worktree_task_instrumented`]'s [`JobStartedHook`]) instead of
    /// racing a real, possibly sub-millisecond in-progress window with a poll
    /// loop — the same `entered_tx`/blocking-`recv` handshake idiom
    /// `crates/store/tests/lock.rs` already uses for "prove a lock is held
    /// right now," adapted to a one-shot signal.
    async fn spawn_and_wait_for_job_started(params: WorktreeTaskParams) -> WorktreeTaskHandle {
        let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
        let hook: JobStartedHook = Arc::new(move || {
            entered_tx.send(()).ok();
        });
        let handle = spawn_worktree_task_instrumented(params, hook)
            .await
            .expect("start task");
        tokio::task::spawn_blocking(move || entered_rx.recv().expect("job started"))
            .await
            .expect("join entered-wait");
        handle
    }

    #[tokio::test]
    async fn cancelling_the_task_mid_cycle_leaves_the_store_valid_and_reusable() {
        let fx = Fixture::new("repo").await;
        let jobs = fx.jobs.clone();

        let handle = spawn_and_wait_for_job_started(fx.params()).await;
        // The hook fired strictly after `jobs.begin(...)` — no race.
        assert!(!jobs.is_empty(), "a job must be in flight right now");
        handle.abort();
        handle.stop().await;
        assert_eq!(jobs.len(), 0, "the aborted job's guard must still drop");

        // Strong drop-safety proof: run a fresh, uninterrupted cycle against
        // the very same store afterward and confirm it still builds and
        // activates a generation normally — corruption from the abort would
        // surface here, not as a special assertion about internal state.
        let handle2 = spawn_worktree_task(fx.params()).await.expect("start task");
        wait_for(Duration::from_secs(10), || {
            handle2.status().last_generation_id.is_some()
        })
        .await;
        assert_eq!(handle2.status().consecutive_failures, 0);
        handle2.stop().await;
    }

    /// `D-077`, and the property the deviation is actually about: a task stuck
    /// in a **synchronous** stretch must not hold the daemon's shutdown.
    ///
    /// The blocking hook stands in for the real one — `run_backfill`'s
    /// `blob_index` → `occurrences_for_fts`, which is called straight from an
    /// `async fn` with no `.await` before it and therefore cannot be preempted
    /// by any cancel. On the owner's 24 GB store `sample(1)` caught exactly
    /// that frame at 99.6% CPU five minutes after `daemon stopping`, while the
    /// daemon went on serving requests; `kill -9` was the only way out.
    ///
    /// This is the test that tells the fix from the defect. `stop_all` used to
    /// `await` each `handle.stop()` with no bound, and `stop` joins the OS
    /// thread — so with the hook still held it never returned at all and this
    /// test would hang to its timeout. It now cancels first and bounds the
    /// wait, so shutdown proceeds and the thread is left to finish its read
    /// and exit on its own (spec 02 §4.3: "Kill at any point is safe by
    /// construction").
    #[tokio::test]
    async fn a_task_stuck_in_synchronous_work_does_not_hold_up_shutdown() {
        use std::collections::HashMap;

        use crate::daemon::indexing::supervisor::{SHUTDOWN_JOIN_BUDGET, stop_all};

        let fx = Fixture::new("repo").await;
        let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        let hook: JobStartedHook = Arc::new(move || {
            entered_tx.send(()).ok();
            // Blocks the dedicated thread inside `project_one`, before its
            // first `.await` — the same shape as the real synchronous scan.
            let _ = release_rx.lock().expect("release mutex").recv();
        });
        let handle = spawn_worktree_task_instrumented(fx.params(), hook)
            .await
            .expect("start task");
        tokio::task::spawn_blocking(move || entered_rx.recv().expect("job started"))
            .await
            .expect("join entered-wait");

        let mut tasks = HashMap::new();
        tasks.insert(fx.worktree_id.to_string(), handle);

        // Generous against the budget itself: this asserts "shutdown is
        // bounded", not "the bound is tight". Unbounded is what it catches,
        // and unbounded never finishes.
        tokio::time::timeout(SHUTDOWN_JOIN_BUDGET * 4, stop_all(tasks))
            .await
            .expect("a task stuck in synchronous work must not hold shutdown open");

        // Let the stranded thread go, so the fixture's temp dir can be removed
        // without a live reader in it.
        let _ = release_tx.send(());
    }

    /// The other half of `D-077`, and the half the test above does **not**
    /// cover: `stop_all` must *cancel*, not merely bound its wait.
    ///
    /// Bounding alone would still be wrong in two ways. It would wait out the
    /// whole in-flight projection before giving up, and then abandon a thread
    /// in the middle of **writing** rather than reading. And it would leave
    /// the loop's shutdown branch to run — that branch flushes the reconciler
    /// and then calls `project_one` on whatever it published, which is how the
    /// owner's daemon came to log two fresh `indexing cycle started` lines
    /// *after* `daemon stopping`. Spec 02 §4.3 says shutdown cancels
    /// reconciles; starting another projection is the opposite of that.
    ///
    /// Asserted on the source because the runtime symptom is expensive to
    /// stage — it needs a projection long enough to matter and a generation
    /// pending at exactly the right instant — and cheap to lose: deleting one
    /// line brings the whole defect back. Same reason, and same shape, as
    /// `tests/memory_normalization_worker.rs::
    /// the_generator_pool_is_built_once_per_process`.
    #[test]
    fn shutdown_cancels_the_indexing_tasks_rather_than_waiting_for_them() {
        let source = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/daemon/indexing/supervisor.rs"
        ))
        .expect("read supervisor.rs");
        let body = source
            .split_once("async fn stop_all(")
            .expect("stop_all exists")
            .1;
        let body = body.split_once("\n}\n").expect("stop_all has a body").0;

        let abort_at = body
            .find("handle.abort()")
            .expect("stop_all must cancel each task (D-077), not only bound its wait");
        let stop_at = body
            .find("handle.stop()")
            .expect("stop_all still joins each task's thread after cancelling it");
        assert!(
            abort_at < stop_at,
            "the cancel must come before the join — `stop()` blocks on the thread, so calling \
             it first is exactly the wait D-077 removed",
        );
    }

    #[tokio::test]
    async fn the_job_registry_is_nonempty_only_during_an_active_projection() {
        let fx = Fixture::new("repo").await;
        let jobs = fx.jobs.clone();
        assert_eq!(jobs.len(), 0, "no work before the task starts");

        let handle = spawn_and_wait_for_job_started(fx.params()).await;
        assert!(!jobs.is_empty(), "a job must be in flight right now");

        wait_for(Duration::from_secs(10), || {
            handle.status().last_generation_id.is_some()
        })
        .await;
        // The tick that set `last_generation_id` has, by definition, already
        // dropped its guard (T20-05's own project_one drops `_job` before
        // updating `status`).
        assert_eq!(jobs.len(), 0, "idle again once the tick has finished");

        handle.stop().await;
    }

    /// D-049's forward note (T20-07): `in_progress_since` is `Some` strictly
    /// while a cycle is active and `None` at rest — same
    /// `JobStartedHook`-driven synchronization as the `JobRegistry` test
    /// right above, applied to the new status field instead.
    #[tokio::test]
    async fn in_progress_since_is_some_only_during_an_active_projection() {
        let fx = Fixture::new("repo").await;

        let handle = spawn_and_wait_for_job_started(fx.params()).await;
        assert!(
            handle.status().in_progress_since.is_some(),
            "a cycle must be in flight right now"
        );

        wait_for(Duration::from_secs(10), || {
            handle.status().last_generation_id.is_some()
        })
        .await;
        assert_eq!(
            handle.status().in_progress_since,
            None,
            "idle again once the tick has finished"
        );

        handle.stop().await;
    }

    #[tokio::test]
    async fn a_vanished_worktree_is_a_typed_start_error() {
        let fx = Fixture::new("repo").await;
        let mut params = fx.params();
        params.worktree_id = fx.uuids.next_uuid(); // never registered
        let err = spawn_worktree_task(params)
            .await
            .expect_err("no such worktree");
        assert!(matches!(err, WorktreeTaskStartError::WorktreeVanished));
    }
}
