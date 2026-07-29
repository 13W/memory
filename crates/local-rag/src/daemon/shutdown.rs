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

/// Drain and release the store (spec 02 §4.3's four ordered steps).
///
/// 1. **Stop accepting**: signal the handshake-stub accept loop to return,
///    then best-effort unlink the socket file. Not required for correctness
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
///    "process going away" case its own doc anticipates). `state.sqlite`'s
///    writer thread stays detached (`StateDb`'s own doc: safe by
///    construction once nothing is mid-transaction, which step 2 already
///    guarantees).
/// 4. **Release lock**: [`StoreLockGuard::release`].
///
/// Every step is best-effort past the first failure — a checkpoint error, for
/// instance, must not skip releasing the lock afterward.
pub async fn drain_and_shutdown(
    layout: &StoreLayout,
    state_db: Option<Arc<StateDb>>,
    cache_db: Option<CacheDb>,
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
    if let Some(cache_db) = cache_db {
        cache_db.close();
    }

    lock_guard.release(layout);
}
