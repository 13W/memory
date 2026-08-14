//! The async reconcile driver (spec 06 §1–2) — T05-04, Layer 2.
//!
//! Wraps the pure scheduling engine ([`super::schedule`]) around the reconcile
//! pipeline: one [`WorktreeReconciler`] task per worktree consumes a trigger
//! channel, debounces/coalesces via a [`Debouncer`], and — when a deadline is
//! reached — runs one [`reconcile_once`] (scan → [`build_generation`]) to
//! completion. Because a worktree has exactly one such task and the reconcile is
//! awaited inline, reconciles for a worktree are strictly serialized — the write
//! side of the per-worktree L2 lock (spec 02 §5), realized structurally rather than
//! with an explicit lock (the L2 read side and the projection switch are later
//! groups).
//!
//! Cancellation is drop-safe at the state-writer transaction boundary: each
//! `db.writer().transaction().await` is atomic and either not-yet-enqueued (its slot
//! frees cleanly) or run-to-completion (spec 02 §4.3, `StateWriter`). Since
//! [`build_generation`] never activates a generation and its rows are a disjoint
//! set, dropping an in-flight reconcile leaves any already-active generation intact.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use local_rag_core::identity::UuidSource;
use local_rag_core::identity::path::CaseSensitivity;
use local_rag_core::redaction::Scanner;
use local_rag_store::{
    OpenError, StateDb, WorktreeKind, WorktreeState, current_worktree_path, rusqlite,
    worktree_summary, worktrees_of_repo,
};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until};

use super::build::{BuildError, BuildOutcome, build_generation};
use super::clock::WallClock;
use super::schedule::{Debouncer, PlannedReconcile, ScheduleConfig, TriggerKind};
use crate::classify::ClassifierConfig;
use crate::scan::{ScanMode, ScanStats, StatCache, scan};

/// The routing/config facts a reconcile needs about one worktree.
///
/// Its `worktree_id` is the durable identity (never path-derived, spec 01 §5); the
/// on-disk `root` and `prune_roots` are resolved from the registry
/// ([`load_worktree_meta`]). `case` is **supplied out-of-band**: the filesystem's
/// case sensitivity is not persisted in `state.sqlite`, so the daemon determines it
/// (platform default / probe) and passes it in.
#[derive(Debug, Clone)]
pub struct WorktreeMeta {
    /// The worktree's stable id.
    pub worktree_id: String,
    /// The worktree's canonical absolute root path.
    pub root: PathBuf,
    /// Whether it is a main tree, a linked worktree, or a non-git directory.
    pub kind: WorktreeKind,
    /// The filesystem's case sensitivity (out-of-band; not persisted).
    pub case: CaseSensitivity,
    /// Worktree-relative subtrees to exclude from the scan (nested registered
    /// worktrees of the same repo, spec 06 §1).
    pub prune_roots: Vec<String>,
}

impl WorktreeMeta {
    /// Whether git triggers/awareness apply (a main tree or a linked worktree).
    pub fn is_git(&self) -> bool {
        matches!(self.kind, WorktreeKind::Main | WorktreeKind::Linked)
    }
}

/// Why one [`reconcile_once`] cycle failed.
#[derive(Debug)]
pub enum ReconcileError {
    /// The authoritative tree scan failed (I/O).
    Scan(std::io::Error),
    /// The generation build failed (the generation was transitioned to `failed`).
    Build(BuildError),
}

impl std::fmt::Display for ReconcileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReconcileError::Scan(e) => write!(f, "scan failed: {e}"),
            ReconcileError::Build(e) => write!(f, "build failed: {e}"),
        }
    }
}

impl std::error::Error for ReconcileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ReconcileError::Scan(e) => Some(e),
            ReconcileError::Build(e) => Some(e),
        }
    }
}

impl ReconcileError {
    /// The generation that failed to build, when the build got far enough to
    /// allocate one (spec 06 §2). A scan-phase failure (I/O before any generation
    /// exists) has none.
    fn failed_generation_id(&self) -> Option<String> {
        match self {
            ReconcileError::Scan(_) => None,
            ReconcileError::Build(e) => Some(e.generation_id.clone()),
        }
    }
}

/// The observable record of the most recent failed reconcile (T05-05).
///
/// A failed generation is never routed (it never becomes `active`, spec 04 §1), so
/// this record is pure **observability**: the failure counter that drives the
/// scheduler's exponential backoff, the backoff deadline, the failed generation's id
/// (when the build allocated one), and the human `last_error`. The `last_error` is
/// the value the projection switch (group 07) will persist into
/// `worktree_projection_state.last_error` (spec 04 §1); here it lives in memory only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileFailure {
    /// The generation that failed to build, if allocation succeeded.
    pub generation_id: Option<String>,
    /// The human-readable cause (`Display` of the [`ReconcileError`]).
    pub last_error: String,
    /// Consecutive failed reconciles since the last success (≥ 1 here).
    pub consecutive_failures: u32,
    /// The monotonic millisecond before which no automatic retry may fire.
    pub backoff_until_ms: i64,
}

/// The result of one reconcile cycle: the scan mode used, its fast-path telemetry,
/// and the built generation.
#[derive(Debug)]
pub struct ReconcileReport {
    /// The scan mode this cycle ran in.
    pub mode: ScanMode,
    /// Fast-path telemetry (hashed vs cache-reused candidates).
    pub scan: ScanStats,
    /// The generation that was built (state `projection_ready`).
    pub build: BuildOutcome,
}

/// Run one reconcile cycle for `meta`: the authoritative [`scan`] then
/// [`build_generation`] (spec 06 §1–2). Stops at `projection_ready` (no activation).
///
/// The scan is synchronous blocking I/O run inline (as `build_generation` reads
/// file bytes inline) — the only work handed to another thread is the SQLite writes
/// inside the writer's transaction closures. `cache` is the caller-owned advisory
/// stat cache, threaded across cycles; `mode` selects fast (trust the cache) vs
/// strict (re-hash all).
#[allow(clippy::too_many_arguments)]
pub async fn reconcile_once(
    db: &StateDb,
    meta: &WorktreeMeta,
    mode: ScanMode,
    cache: &mut StatCache,
    cfg: &ClassifierConfig,
    scanner: &Scanner,
    uuids: &(dyn UuidSource + Send + Sync),
    now_ms: i64,
) -> Result<ReconcileReport, ReconcileError> {
    let (manifest, scan_stats) = scan(
        &meta.root,
        meta.kind,
        meta.case,
        cfg.max_file_size_bytes,
        mode,
        &meta.prune_roots,
        cache,
    )
    .map_err(ReconcileError::Scan)?;

    let build = build_generation(
        db,
        &meta.worktree_id,
        &meta.root,
        &manifest,
        cfg,
        scanner,
        uuids,
        now_ms,
    )
    .await
    .map_err(ReconcileError::Build)?;

    Ok(ReconcileReport {
        mode,
        scan: scan_stats,
        build,
    })
}

/// A long-lived per-worktree reconcile task: it owns the worktree's advisory
/// [`StatCache`] and [`Debouncer`] and drives one reconcile at a time.
pub struct WorktreeReconciler {
    db: Arc<StateDb>,
    meta: WorktreeMeta,
    cache: StatCache,
    debouncer: Debouncer,
    cfg: ClassifierConfig,
    scanner: Scanner,
    uuids: Arc<dyn UuidSource + Send + Sync>,
    /// Wall-clock seam for the durable `_at` columns one reconcile writes
    /// (D-062). Deliberately **not** the loop's monotonic origin: that one
    /// belongs to [`Debouncer`] arithmetic alone, and feeding it into
    /// [`reconcile_once`] is exactly the defect D-062 fixed.
    clock: Arc<dyn WallClock>,
    /// Publishes the most recent failure (`None` while healthy) — the observability
    /// seam a daemon / group-07 reads via [`ReconcileHandle::failures`] (T05-05).
    failure_tx: watch::Sender<Option<ReconcileFailure>>,
    /// Publishes the `generation_id` of the most recently *successfully*
    /// built generation (`None` before the first success) — the
    /// observability seam `local-rag watch` (T15-07) reads via
    /// [`ReconcileHandle::successes`] to drive its own embed/switch/
    /// materialize_fts follow-up after each reconcile.
    success_tx: watch::Sender<Option<String>>,
}

impl WorktreeReconciler {
    /// Build a reconciler for `meta`. `db` is the shared state handle (one writer
    /// for the whole store); `cfg`/`scanner` are the classification inputs;
    /// `uuids` is the id seam (`SystemUuidV7` in production); `clock` is the
    /// wall-clock seam for durable `_at` columns ([`SystemWallClock`](super::
    /// clock::SystemWallClock) in production, a fixed source in tests — D-062);
    /// `sched` carries the debounce/periodic intervals.
    pub fn new(
        db: Arc<StateDb>,
        meta: WorktreeMeta,
        cfg: ClassifierConfig,
        scanner: Scanner,
        uuids: Arc<dyn UuidSource + Send + Sync>,
        clock: Arc<dyn WallClock>,
        sched: ScheduleConfig,
    ) -> Self {
        let debouncer = Debouncer::new(sched, meta.is_git(), 0);
        let (failure_tx, _) = watch::channel(None);
        let (success_tx, _) = watch::channel(None);
        Self {
            db,
            meta,
            cache: StatCache::new(),
            debouncer,
            cfg,
            scanner,
            uuids,
            clock,
            failure_tx,
            success_tx,
        }
    }

    /// Subscribe to this reconciler's failure observability (`None` while healthy).
    /// [`spawn_reconciler`] wires the receiver into the returned
    /// [`ReconcileHandle`]; a daemon may take further receivers before spawning.
    pub fn subscribe_failures(&self) -> watch::Receiver<Option<ReconcileFailure>> {
        self.failure_tx.subscribe()
    }

    /// Subscribe to this reconciler's success observability (`None` before
    /// the first successfully built generation). [`spawn_reconciler`] wires
    /// the receiver into the returned [`ReconcileHandle`].
    pub fn subscribe_successes(&self) -> watch::Receiver<Option<String>> {
        self.success_tx.subscribe()
    }

    /// Drive the reconcile loop until every trigger sender is dropped.
    ///
    /// Each iteration waits for the sooner of the debounce/periodic deadline and the
    /// next trigger. A `biased` select checks the timer first, so a due reconcile
    /// fires deterministically before servicing more triggers. Triggers arriving
    /// during a build stay buffered and are recorded on the next iteration, so a
    /// burst coalesces into at most one follow-up reconcile. On graceful shutdown
    /// (channel closed) any scheduled reconcile is flushed before returning.
    ///
    /// Every millisecond in this loop is **monotonic** (`origin`-relative) and
    /// belongs to the debouncer alone. The wall-clock time a reconcile persists
    /// comes from [`self.clock`](WorktreeReconciler) instead — the two must never
    /// be conflated (D-062).
    pub async fn run(mut self, mut rx: mpsc::Receiver<TriggerKind>) {
        let origin = Instant::now();
        loop {
            let wake = instant_of(origin, self.debouncer.next_wake());
            tokio::select! {
                biased;
                _ = sleep_until(wake) => {
                    let mono_ms = now_ms(origin);
                    if let Some(plan) = self.debouncer.take_due(mono_ms) {
                        self.run_and_observe(plan, mono_ms).await;
                    }
                }
                got = rx.recv() => match got {
                    Some(kind) => {
                        let mono_ms = now_ms(origin);
                        self.debouncer.record(kind, mono_ms);
                    }
                    None => {
                        // All senders dropped: flush a scheduled reconcile, then stop.
                        if let Some(plan) = self.debouncer.take_pending() {
                            let mono_ms = now_ms(origin);
                            self.run_and_observe(plan, mono_ms).await;
                        }
                        break;
                    }
                },
            }
        }
    }

    /// Run one reconcile with the planned mode. The typed outcome is folded into the
    /// failure observability by [`run_and_observe`](Self::run_and_observe); a failed
    /// build has already been recorded `failed` in the store (spec 04 §1) and is
    /// never routed.
    ///
    /// `now_ms` is **wall-clock** Unix milliseconds (spec 03): it lands in
    /// `generation.created_at` and the row timestamps underneath it.
    async fn run_reconcile(
        &mut self,
        plan: PlannedReconcile,
        now_ms: i64,
    ) -> Result<ReconcileReport, ReconcileError> {
        reconcile_once(
            &self.db,
            &self.meta,
            plan.mode,
            &mut self.cache,
            &self.cfg,
            &self.scanner,
            &*self.uuids,
            now_ms,
        )
        .await
    }

    /// Run one reconcile and fold its outcome into the failure observability (T05-05):
    ///
    /// - **success** resets the backoff/counter and publishes `None`;
    /// - **failure** records the typed [`ReconcileFailure`], increments the counter,
    ///   arms the exponential backoff (so a persistently failing worktree does not
    ///   hot-loop), and publishes it.
    ///
    /// The error is never propagated: a failed generation is already `failed` in the
    /// store (spec 04 §1) and is never selected for routing; this only updates the
    /// in-memory observability the daemon / group-07 reads.
    ///
    /// `mono_ms` is the caller's monotonic loop time and is used **only** for the
    /// debouncer's backoff arithmetic (`ReconcileFailure::backoff_until_ms` is
    /// documented on that same scale). The reconcile itself is handed a wall clock
    /// read taken here — before the scan starts, so the timestamp names when the
    /// generation began, not when it finished (D-062).
    async fn run_and_observe(&mut self, plan: PlannedReconcile, mono_ms: i64) {
        let wall_ms = self.clock.now_ms();
        match self.run_reconcile(plan, wall_ms).await {
            Ok(report) => {
                self.debouncer.record_success();
                let _ = self.failure_tx.send(None);
                let _ = self.success_tx.send(Some(report.build.generation_id));
            }
            Err(e) => {
                self.debouncer.record_failure(mono_ms);
                let failure = ReconcileFailure {
                    generation_id: e.failed_generation_id(),
                    last_error: e.to_string(),
                    consecutive_failures: self.debouncer.consecutive_failures(),
                    backoff_until_ms: self.debouncer.retry_backoff_until(),
                };
                let _ = self.failure_tx.send(Some(failure));
            }
        }
    }
}

/// A handle to a spawned [`WorktreeReconciler`]: send triggers on `sender`; drop all
/// senders to shut the task down (a scheduler keeps one handle per worktree_id — the
/// per-worktree registry that structurally guarantees a single writer per worktree).
pub struct ReconcileHandle {
    /// Submit a trigger to this worktree's reconciler.
    pub sender: mpsc::Sender<TriggerKind>,
    /// The reconciler task's join handle.
    pub join: JoinHandle<()>,
    /// Watch the reconciler's most recent failure (`None` while healthy) — the
    /// observability seam group 07 reads to persist
    /// `worktree_projection_state.last_error` (T05-05).
    pub failures: watch::Receiver<Option<ReconcileFailure>>,
    /// Watch the `generation_id` of the most recently successfully built
    /// generation (`None` before the first success) — the observability seam
    /// `local-rag watch` (T15-07) reads to drive its own embed/switch/
    /// materialize_fts follow-up after each reconcile.
    pub successes: watch::Receiver<Option<String>>,
}

/// Spawn `reconciler` on the current runtime with a bounded trigger channel.
pub fn spawn_reconciler(reconciler: WorktreeReconciler, capacity: usize) -> ReconcileHandle {
    let (sender, rx) = mpsc::channel(capacity.max(1));
    let failures = reconciler.subscribe_failures();
    let successes = reconciler.subscribe_successes();
    let join = tokio::spawn(reconciler.run(rx));
    ReconcileHandle {
        sender,
        join,
        failures,
        successes,
    }
}

/// Monotonic milliseconds since the loop's `origin` (the engine's logical clock).
fn now_ms(origin: Instant) -> i64 {
    origin.elapsed().as_millis() as i64
}

/// The [`Instant`] corresponding to logical millisecond `due_ms` (clamped at 0).
fn instant_of(origin: Instant, due_ms: i64) -> Instant {
    origin + Duration::from_millis(due_ms.max(0) as u64)
}

/// Why loading worktree metadata from the registry failed.
#[derive(Debug)]
pub enum MetaError {
    /// Opening the read connection failed.
    Open(OpenError),
    /// A registry query failed.
    Sqlite(rusqlite::Error),
}

impl std::fmt::Display for MetaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MetaError::Open(e) => write!(f, "open read connection: {e}"),
            MetaError::Sqlite(e) => write!(f, "registry query failed: {e}"),
        }
    }
}

impl std::error::Error for MetaError {}

/// Assemble [`WorktreeMeta`] for `worktree_id` from the registry (spec 03 §2.1), or
/// `None` if the worktree is absent or has no current path. `case` is supplied by
/// the caller (it is not persisted).
pub fn load_worktree_meta(
    db: &StateDb,
    worktree_id: &str,
    case: CaseSensitivity,
) -> Result<Option<WorktreeMeta>, MetaError> {
    let conn = db.open_read().map_err(MetaError::Open)?;
    let Some(summary) = worktree_summary(&conn, worktree_id).map_err(MetaError::Sqlite)? else {
        return Ok(None);
    };
    let Some(root) = current_worktree_path(&conn, worktree_id).map_err(MetaError::Sqlite)? else {
        return Ok(None);
    };
    let prune_roots = compute_prune_roots(&conn, worktree_id, &summary.repo_id, Path::new(&root))
        .map_err(MetaError::Sqlite)?;
    Ok(Some(WorktreeMeta {
        worktree_id: worktree_id.to_string(),
        root: PathBuf::from(root),
        kind: summary.kind,
        case,
        prune_roots,
    }))
}

/// The worktree-relative prune roots for `worktree_id` rooted at `root`: the
/// same-repo sibling worktrees whose current path lies inside `root` (spec 06 §1).
///
/// Same-repo only — the registry has no global cross-repo enumeration, so a checkout
/// of a *different* repository nested inside `root` is not covered here (its own
/// `.git` is still pruned unconditionally by the scan).
pub fn nested_prune_roots(
    db: &StateDb,
    worktree_id: &str,
    root: &Path,
) -> Result<Vec<String>, MetaError> {
    let conn = db.open_read().map_err(MetaError::Open)?;
    let Some(summary) = worktree_summary(&conn, worktree_id).map_err(MetaError::Sqlite)? else {
        return Ok(Vec::new());
    };
    compute_prune_roots(&conn, worktree_id, &summary.repo_id, root).map_err(MetaError::Sqlite)
}

/// Shared body of [`nested_prune_roots`]/[`load_worktree_meta`]: the active sibling
/// worktrees of `repo_id` whose current path is under `root`, as sorted unique
/// worktree-relative `/`-separated strings.
fn compute_prune_roots(
    conn: &rusqlite::Connection,
    worktree_id: &str,
    repo_id: &str,
    root: &Path,
) -> rusqlite::Result<Vec<String>> {
    let mut roots = Vec::new();
    for sibling in worktrees_of_repo(conn, repo_id)? {
        if sibling.worktree_id == worktree_id || sibling.state != WorktreeState::Active {
            continue;
        }
        if let Some(path) = current_worktree_path(conn, &sibling.worktree_id)?
            && let Some(rel) = relative_under_root(root, Path::new(&path))
        {
            roots.push(rel);
        }
    }
    roots.sort();
    roots.dedup();
    Ok(roots)
}

/// `path` expressed relative to `root` as a `/`-separated string, or `None` if it is
/// not strictly inside `root`.
fn relative_under_root(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let joined = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    if joined.is_empty() {
        None
    } else {
        Some(joined)
    }
}
