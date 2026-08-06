//! Reach a live daemon: connect if one already owns the store socket,
//! otherwise spawn one detached and retry with backoff (spec 13 §2's
//! "spawn retry", this card's own "daemon absent backoff... 20s cap").

use std::path::{Path, PathBuf};
use std::time::Duration;

#[cfg(unix)]
use tokio::net::UnixStream;

use crate::error::ProxyError;

/// The backoff schedule for [`connect_or_spawn`]'s retry loop.
///
/// A standalone copy of `crates/embed/src/pool.rs::RetryPolicy`'s *shape*
/// (250ms base, doubling, 4s cap — that module's own doc already cites spec
/// 02 §4.2 as the source for those numbers), not an import:
/// `local-rag-embed` pulls in `local-rag-store`, and this proxy must hold no
/// project state at all (see this crate's own `main.rs` doc). `total_
/// budget_ms` is this card's own number: a 20s cap on the *whole*
/// connect-or-spawn attempt, not on any single delay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackoffPolicy {
    pub base_ms: u64,
    pub max_delay_ms: u64,
    pub total_budget_ms: u64,
}

pub const DEFAULT_BACKOFF: BackoffPolicy = BackoffPolicy {
    base_ms: 250,
    max_delay_ms: 4_000,
    total_budget_ms: 20_000,
};

/// The delay before retry attempt `next_attempt` (1-based: the delay
/// *after* the first failure is `next_attempt = 2`) — mirrors
/// `crates/embed/src/pool.rs::retry_delay_ms`'s shape exactly: no delay
/// before the first attempt, then 250ms doubling, capped at `max_delay_ms`.
/// Pure and saturating: the exponent is clamped so the shift cannot
/// overflow.
pub fn backoff_delay_ms(policy: BackoffPolicy, next_attempt: u32) -> u64 {
    if next_attempt <= 1 {
        return 0;
    }
    let shift = (next_attempt - 2).min(31);
    policy
        .base_ms
        .saturating_mul(1_u64 << shift)
        .min(policy.max_delay_ms)
}

/// Locate the daemon binary next to this proxy binary (spec 13 §1: every
/// product binary ships side by side, one npm platform package /
/// `target/{debug,release}` directory).
pub fn resolve_daemon_binary_path(proxy_exe: &Path) -> Option<PathBuf> {
    let dir = proxy_exe.parent()?;
    let name = if cfg!(windows) {
        "local-rag.exe"
    } else {
        "local-rag"
    };
    let candidate = dir.join(name);
    candidate.is_file().then_some(candidate)
}

/// Spawn a fully detached daemon: its own process group (unix — so a signal
/// to this proxy's group, e.g. a terminal Ctrl-C, does not reach it) and
/// `Stdio::null()` on all three standard streams. Without that, the daemon
/// would inherit the very stdio channel this proxy uses to speak MCP with
/// its client — a silent, catastrophic corruption of that stream.
#[cfg(unix)]
pub fn spawn_detached_daemon(daemon_binary: &Path) -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    reap_exited_daemons();
    Command::new(daemon_binary)
        .arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()?;
    Ok(())
}

/// Collect any daemon this proxy spawned earlier that has since exited.
///
/// [`spawn_detached_daemon`] deliberately never waits on what it spawns — the
/// daemon outlives this proxy by design — so an exited one stays a zombie for
/// as long as this process lives. That was invisible while a proxy exited the
/// moment its daemon went away, but D-038's relay now outlives daemon
/// restarts, and a zombie answers `kill(pid, 0)` exactly as a live process
/// does — the very check store-lock ownership is decided by (spec 02 §4.4).
#[cfg(unix)]
fn reap_exited_daemons() {
    // SAFETY: `waitpid` with `WNOHANG` never blocks and writes nothing
    // through the null status pointer; `-1` means "any child", and this
    // process has no children other than daemons it spawned itself.
    #[allow(unsafe_code)]
    while unsafe { libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) } > 0 {}
}

/// Connect to `socket_path`, spawning a detached daemon on the first
/// failure and retrying with `policy`'s backoff until either a connection
/// succeeds or `policy.total_budget_ms` elapses.
///
/// The spawn happens **once**, before the retry loop starts — not once per
/// failed attempt: a daemon takes real, non-instant time to reach step 4 of
/// its own startup (spec 02 §4.1), so re-spawning on every connect failure
/// would race a slow-starting daemon against a flood of redundant sibling
/// processes all fighting over the same store lock.
#[cfg(unix)]
pub async fn connect_or_spawn(
    socket_path: &Path,
    daemon_binary: &Path,
    policy: BackoffPolicy,
) -> Result<UnixStream, ProxyError> {
    if let Ok(stream) = UnixStream::connect(socket_path).await {
        return Ok(stream);
    }
    spawn_detached_daemon(daemon_binary).map_err(ProxyError::Spawn)?;
    retry_with_backoff(policy, || UnixStream::connect(socket_path)).await
}

/// The retry-with-backoff core, generic over how a single attempt is made —
/// production passes `UnixStream::connect`; tests pass a synthetic
/// always-fails probe, so the delay schedule and the overall budget cap can
/// be verified with a paused clock without a real socket or a real daemon
/// process.
async fn retry_with_backoff<F, Fut, T, E>(
    policy: BackoffPolicy,
    mut try_connect: F,
) -> Result<T, ProxyError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
{
    let retry = async {
        let mut attempt = 1u32;
        loop {
            attempt += 1;
            tokio::time::sleep(Duration::from_millis(backoff_delay_ms(policy, attempt))).await;
            if let Ok(value) = try_connect().await {
                return value;
            }
        }
    };
    tokio::time::timeout(Duration::from_millis(policy.total_budget_ms), retry)
        .await
        .map_err(|_| ProxyError::ConnectTimedOut)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn backoff_doubles_from_the_base_and_caps() {
        let p = DEFAULT_BACKOFF;
        assert_eq!(
            backoff_delay_ms(p, 1),
            0,
            "no delay before the first attempt"
        );
        assert_eq!(backoff_delay_ms(p, 2), 250);
        assert_eq!(backoff_delay_ms(p, 3), 500);
        assert_eq!(backoff_delay_ms(p, 4), 1_000);
        assert_eq!(backoff_delay_ms(p, 5), 2_000);
        assert_eq!(backoff_delay_ms(p, 6), 4_000);
        assert_eq!(backoff_delay_ms(p, 7), 4_000, "capped");
        assert_eq!(backoff_delay_ms(p, u32::MAX), 4_000, "no overflow");
    }

    #[tokio::test(start_paused = true)]
    async fn a_permanently_absent_daemon_is_retried_on_schedule_then_gives_up_at_the_budget() {
        let delays: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
        let start = tokio::time::Instant::now();
        let delays_for_probe = Arc::clone(&delays);
        let probe_start = start;
        let result = retry_with_backoff(DEFAULT_BACKOFF, move || {
            let delays = Arc::clone(&delays_for_probe);
            async move {
                delays
                    .lock()
                    .unwrap()
                    .push(probe_start.elapsed().as_millis() as u64);
                Err::<(), ()>(())
            }
        })
        .await;

        assert!(matches!(result, Err(ProxyError::ConnectTimedOut)));
        assert_eq!(start.elapsed(), Duration::from_millis(20_000));

        // Attempt 1 (the immediate first `connect_or_spawn` probe) is not
        // part of this loop — `retry_with_backoff` starts from attempt 2.
        // Delays: 250, 500, 1000, 2000, 4000, then capped at 4000 for every
        // attempt after — recorded as cumulative elapsed time at each probe.
        let recorded = delays.lock().unwrap().clone();
        let expected_cumulative = [250, 750, 1_750, 3_750, 7_750, 11_750, 15_750, 19_750];
        assert_eq!(
            &recorded[..expected_cumulative.len()],
            &expected_cumulative[..]
        );
    }
}
