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

impl TempHome {
    /// Create a fresh, empty temporary home.
    ///
    /// The directory name embeds the process id and a per-process counter, so
    /// concurrent calls (in one process or across processes) never collide.
    pub fn new() -> io::Result<Self> {
        let root = std::env::temp_dir().join("local-rag-tests");
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
