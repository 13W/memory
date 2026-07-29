//! Process-liveness check (spec 02 §4.1: "verify the owning process exists").
//!
//! `std` exposes no portable liveness probe, so this calls libc directly on
//! unix (see the dependency note in `CONTRIBUTING.md`) — the same rationale
//! `paths::perms::effective_uid` already gives for its own libc use.

/// Whether a process with `pid` currently exists.
///
/// PID alone is never identity (a dead PID can be reused by an unrelated
/// process) — callers that need to distinguish "the same process we saw
/// before" from "a different process that happens to reuse its PID" MUST
/// pair this with an independent liveness check (spec 02 §4.1's "instance
/// UUID matches a live handshake on the socket").
#[cfg(unix)]
pub fn pid_exists(pid: u32) -> bool {
    // SAFETY: `kill` with signal `0` sends no signal; it only performs the
    // permission/existence checks and reports the result via `errno`. `pid`
    // is a plain integer, no memory is read or written.
    #[allow(unsafe_code)]
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        return true;
    }
    // EPERM: the process exists but is owned by another user — still alive.
    // ESRCH (or anything else): no such process.
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Whether a process with `pid` currently exists.
#[cfg(not(unix))]
pub fn pid_exists(_pid: u32) -> bool {
    // TODO(Windows enablement, T17): resolve liveness via the Win32 API.
    // Windows is not yet in the CI matrix (spec 13 §1).
    unimplemented!("Windows PID liveness is deferred until Windows joins the CI matrix")
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn own_pid_is_alive() {
        assert!(pid_exists(std::process::id()));
    }

    #[test]
    fn a_reaped_child_pid_is_dead() {
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn a trivial child");
        let pid = child.id();
        let status = child.wait().expect("wait for the child to exit");
        assert!(status.success());
        assert!(
            !pid_exists(pid),
            "a fully reaped child must not report as alive"
        );
    }

    #[test]
    fn pid_zero_and_a_very_large_pid_do_not_panic() {
        // Not asserting a specific answer for these edge values (platform-
        // dependent), only that the call is safe and returns.
        let _ = pid_exists(0);
        let _ = pid_exists(u32::MAX);
    }
}
