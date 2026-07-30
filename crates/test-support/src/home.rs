//! Isolated, self-cleaning temporary `LOCAL_RAG_HOME`.
//!
//! A [`TempHome`] owns a fresh directory under [`std::env::temp_dir`] and
//! removes it when dropped. Isolation between two homes is by *path*: each has
//! its own root, so two of them created in parallel never interfere. The
//! `LOCAL_RAG_HOME` environment variable is set only in *child* process
//! environments (see [`TempHome::command`]), never in the parent — mutating a
//! process-global env var would race across parallel tests.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::next_seq;

/// A temporary store root that stands in for a real `LOCAL_RAG_HOME`.
///
/// Created under the system temp directory (never `$HOME`) and deleted on drop.
///
/// ```
/// let home = local_rag_test_support::TempHome::new().expect("temp home");
/// assert!(home.path().is_dir());
/// let path = home.path().to_path_buf();
/// drop(home);
/// assert!(!path.exists(), "temp home is removed on drop");
/// ```
#[derive(Debug)]
pub struct TempHome {
    path: PathBuf,
}

/// The shortest reliable temp-directory base for this platform (D-023).
///
/// Real daemon tests (`crates/local-rag/tests/*.rs`) bind a Unix domain
/// socket under a `TempHome`-rooted `StoreLayout::socket_path()`, and
/// `sockaddr_un.sun_path` has a small, fixed capacity (~104 bytes on
/// macOS/BSD) — nothing in this crate's control. `std::env::temp_dir()`
/// honors `$TMPDIR`, whose length is unbounded by anything normative; a
/// sandboxed environment that sets a long, deeply nested `$TMPDIR` silently
/// pushes every derived socket path over that limit, and the daemon fails
/// to bind non-deterministically (which specific test loses the race
/// depends on thread-scheduling order, not on anything this crate's tests
/// assert about). `/tmp` is the POSIX-guaranteed short alternative every
/// Unix provides (macOS symlinks it to `/private/tmp`) — used directly when
/// present, bypassing `$TMPDIR` entirely; falls back to `std::env::
/// temp_dir()` if `/tmp` itself is somehow missing, and on non-Unix
/// platforms (no `sockaddr_un` limit to worry about there).
#[cfg(unix)]
fn local_temp_root() -> PathBuf {
    let candidate = PathBuf::from("/tmp");
    if candidate.is_dir() {
        candidate
    } else {
        std::env::temp_dir()
    }
}

#[cfg(not(unix))]
fn local_temp_root() -> PathBuf {
    std::env::temp_dir()
}

impl TempHome {
    /// Create a fresh, empty temporary home.
    ///
    /// The directory name embeds the process id and a per-process counter, so
    /// concurrent calls (in one process or across processes) never collide.
    pub fn new() -> io::Result<Self> {
        let root = local_temp_root().join("local-rag-tests");
        let name = format!("home-{}-{}", std::process::id(), next_seq());
        let path = root.join(name);
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    /// The absolute path of this temporary home.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Join a relative path against the home root.
    pub fn join(&self, rel: impl AsRef<Path>) -> PathBuf {
        self.path.join(rel)
    }

    /// Build a [`Command`] for `program` whose child environment points
    /// `LOCAL_RAG_HOME` at this temporary home.
    ///
    /// `HOME` is removed from the child environment so a misbehaving binary
    /// cannot silently fall back to the developer's real home directory. The
    /// parent process environment is left untouched.
    pub fn command(&self, program: impl AsRef<OsStr>) -> Command {
        let mut cmd = Command::new(program);
        cmd.env("LOCAL_RAG_HOME", &self.path);
        cmd.env_remove("HOME");
        cmd
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        // Best-effort cleanup: a leaked temp dir is a nuisance, not a failure,
        // and panicking in `drop` (e.g. during unwinding) would abort.
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D-023 regression: a real `TempHome` today, plus the exact suffix a
    /// real daemon test derives from it (`local-rag/run/daemon.sock` —
    /// `local_rag_core::paths::StoreLayout::socket_path`'s literal shape),
    /// must fit `sockaddr_un.sun_path`'s tightest real-world bound (104
    /// bytes, macOS/BSD) with real margin.
    #[test]
    fn a_real_temp_home_socket_path_fits_within_sun_path_len() {
        let home = TempHome::new().expect("temp home");
        let socket_path = home.join("local-rag/run/daemon.sock");
        let len = socket_path.to_string_lossy().len();
        assert!(
            len < 100,
            "{len} bytes ({socket_path:?}) leaves no safety margin under sockaddr_un.sun_path's ~104-byte limit"
        );
    }

    /// The same bound holds even for a worst-case realistic `pid`/`seq` (a
    /// 6-digit pid, a 3-digit seq — this run's own actual values are
    /// smaller, so the test above alone would not catch a regression that
    /// only bites once enough tests in one binary have driven `seq` up).
    #[test]
    fn the_bound_holds_for_a_worst_case_pid_and_seq_too() {
        let root = local_temp_root().join("local-rag-tests");
        let worst_case_name = format!("home-{}-{}", 999_999, 999);
        let socket_path = root.join(worst_case_name).join("local-rag/run/daemon.sock");
        let len = socket_path.to_string_lossy().len();
        assert!(
            len < 100,
            "{len} bytes ({socket_path:?}) leaves no safety margin under sockaddr_un.sun_path's ~104-byte limit"
        );
    }

    #[test]
    fn a_real_temp_home_is_rooted_under_local_temp_root() {
        let home = TempHome::new().expect("temp home");
        assert!(
            home.path().starts_with(local_temp_root()),
            "{:?} must be rooted under {:?}",
            home.path(),
            local_temp_root()
        );
    }
}
