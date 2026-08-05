//! SIGTERM/CTRL-C shutdown (spec 02 §4.3 `[FIXED]`: "stop accepting → cancel
//! reconciles at the next safe point (state tx boundaries) → flush WAL
//! checkpoint → release lock") — T15-01.

use std::sync::Arc;

use local_rag_core::paths::StoreLayout;
use local_rag_store::{CacheDb, CheckpointMode, StateDb};
use tokio::sync::oneshot;

use super::lock::StoreLockGuard;

/// A pre-installed OS shutdown-signal listener.
///
/// `tokio::signal::unix::signal` registers with the OS/tokio's signal driver
/// at **call** time, not at the first `.recv().await` — a signal delivered
/// any time after [`ShutdownSignal::install`] runs (even while later startup
/// steps, spec 02 §4.1, are still executing, well before anything ever polls
/// [`wait`](ShutdownSignal::wait)) is captured and observed on the first
/// `wait()` call, never lost to the OS default terminate-immediately
/// disposition. That is why [`ShutdownSignal::install`] must be the very
/// first thing `lifecycle::run` does, before [`super::lifecycle::
/// DaemonHandle::start`] — installing it lazily inside the wait loop instead
/// (this crate's own first draft) leaves a real window, between the lock
/// being marked ready and the wait loop actually starting, where a SIGTERM
/// kills the process ungracefully instead of draining it; measured directly
/// via `tests/serve_subprocess.rs` flaking under load before this fix.
#[cfg(unix)]
pub struct ShutdownSignal {
    term: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl ShutdownSignal {
    /// Install the SIGTERM handler now. Must run before any other startup
    /// work — see the type's own doc comment.
    pub fn install() -> Self {
        use tokio::signal::unix::{SignalKind, signal};
        let term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        ShutdownSignal { term }
    }

    /// Wait for SIGTERM or CTRL-C (SIGINT), whichever arrives first.
    pub async fn wait(&mut self) {
        tokio::select! {
            _ = self.term.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
    }
}

/// Windows has no SIGTERM; `tokio::signal::ctrl_c()` (CTRL_C_EVENT via
/// `SetConsoleCtrlHandler`, fully cross-platform in `tokio`) is the whole
/// story here — nothing platform-specific left to install ahead of time, so
/// `install()` is a no-op constructor kept only so both platforms share one
/// call site. Real functionality, not a stub: `local-rag watch` (this
/// type's only caller so far, `cli::watch`) never touches the daemon's own
/// still-Unix-only IPC transport (D-033), so it is not blocked by that gap.
#[cfg(windows)]
pub struct ShutdownSignal;

#[cfg(windows)]
impl ShutdownSignal {
    pub fn install() -> Self {
        ShutdownSignal
    }

    /// Wait for CTRL-C (or CTRL_BREAK/CTRL_CLOSE, which `tokio::signal::
    /// ctrl_c()` also observes on Windows).
    pub async fn wait(&mut self) {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Drain and release the store (spec 02 §4.3's four ordered steps).
///
/// 1. **Stop accepting**: signal the accept loop to return, then
///    best-effort unlink the socket file. Not required for correctness
///    (the next `acquire`'s success path already reclaims an orphaned socket
///    unconditionally, spec 02 §4.4), but an orderly shutdown leaving nothing
///    behind is strictly better than relying on that recovery path every
///    time.
/// 2. **Cancel at the next safe point**: T15-01's own scope has no
///    long-lived reconcile loop to cancel (see `daemon` module's scope
///    note) — the two startup resume passes are the only background work,
///    and the caller (`lifecycle::run`) already awaits their `JoinHandle`s
///    to completion *before* calling this function, which **is** "let the
///    current job finish, refuse new ones" here: a `StateWriter`/
///    `CacheWriter` job's only unit of work is already one SQL transaction,
///    so there is no smaller safe point to cancel down to.
/// 3. **Flush WAL checkpoint**: `TRUNCATE` on both databases (spec 03 §3;
///    03 §4's own as-built note adopting the same policy for `cache.sqlite`),
///    then [`CacheDb::close`] (the blocking, D-009-safe variant — exactly the
///    "process going away" case its own doc anticipates) *when this is
///    provably the last reference* — see below for why it sometimes is not,
///    and why that is still safe. `state.sqlite`'s writer thread stays
///    detached (`StateDb`'s own doc: safe by construction once nothing is
///    mid-transaction, which step 2 already guarantees).
/// 4. **Release lock**: [`StoreLockGuard::release`].
///
/// Every step is best-effort past the first failure — a checkpoint error, for
/// instance, must not skip releasing the lock afterward.
///
/// # Why `cache_db` is `Arc<CacheDb>`, and why `Arc::into_inner` can fail here
///
/// T15-03's `SearchEngine` needs `Arc<CacheDb>` to serve MCP queries, so
/// `DaemonHandle` now shares one clone with it. By the time this function
/// runs, `DaemonHandle::shutdown` has already aborted the accept loop
/// (`handshake_join`, dropping *its* clone) — but a connection already
/// mid-session when the trigger fired is deliberately never aborted
/// (`daemon::handshake`'s own doc: it must stay open until this very drain
/// finishes, so the requesting proxy observes EOF only after the drain is
/// real). That connection's task holds its own clone until it is reclaimed
/// by `main.rs::run_serve`'s `Runtime` drop, strictly *after* this function
/// returns. So `Arc::into_inner` can genuinely fail here — not a bug, a live
/// session racing shutdown. When it does, the checkpoint above has already
/// run (it only needs `&Arc<CacheDb>`), and this function simply does not
/// call `.close()` — the value drops later, when that session's task is
/// reclaimed. That bare drop is exactly what D-009 flagged as unsafe in
/// general, but not here: D-009's actual hazard is a *second* `CacheDb`
/// immediately reopening the same `cache.sqlite` path before the first's
/// writer thread finishes closing — and nothing reopens this store's path
/// before the `Runtime` drop reclaims every remaining clone (the upgrade
/// flow's own proxy-side `wait_for_close` only spawns a replacement daemon
/// *after* observing EOF, which cannot happen before that drop; a plain
/// SIGTERM/idle exit spawns nothing at all).
pub async fn drain_and_shutdown(
    layout: &StoreLayout,
    state_db: Option<Arc<StateDb>>,
    cache_db: Option<Arc<CacheDb>>,
    lock_guard: StoreLockGuard,
    handshake_stop: Option<oneshot::Sender<()>>,
) {
    if let Some(stop) = handshake_stop {
        let _ = stop.send(());
    }
    let _ = std::fs::remove_file(layout.socket_path());

    if let Some(ref db) = state_db {
        let _ = db.writer().checkpoint(CheckpointMode::Truncate).await;
    }
    if let Some(ref cache_db) = cache_db {
        let _ = cache_db.writer().checkpoint(CheckpointMode::Truncate).await;
    }
    if let Some(cache_db) = cache_db.and_then(Arc::into_inner) {
        cache_db.close();
    }

    lock_guard.release(layout);
}
