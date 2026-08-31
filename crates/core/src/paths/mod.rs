//! Store/config path resolution and permissions (spec 02 §2 & §2.1).
//!
//! This module is the platform abstraction every binary shares: it resolves the
//! data and config directories from the environment, exposes the full store
//! layout ([`StoreLayout`]), the MCP [`Endpoint`], and the directory/file
//! permission primitives ([`ensure_dir`], [`ensure_file_0600`],
//! [`verify_owner`]).
//!
//! Resolution takes an injected [`Env`] rather than reading process-global state
//! directly, so precedence is unit-testable without mutating the real
//! environment (mirroring the `Clock`/`IdSource` seams in `test-support`).
//!
//! `[SPEC]` note (02 §2.1): the table lists `LOCAL_RAG_HOME` only in the
//! `<data_dir>` row, but the prose states it "overrides everything (tests,
//! containers)". A test/container that sets only `LOCAL_RAG_HOME` (with `HOME`
//! removed and no `XDG_CONFIG_HOME`) would otherwise have no resolvable config
//! dir, so `LOCAL_RAG_HOME` also overrides `<config_dir>`, resolving it to
//! `$LOCAL_RAG_HOME/config` — a sibling of the store root
//! `$LOCAL_RAG_HOME/local-rag`, keeping a container fully self-contained.

pub mod perms;

pub use perms::{ExpectedKind, audit_path, ensure_dir, ensure_file_0600, verify_owner};

use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

/// An injectable view of the process environment used for path resolution.
///
/// The production implementation is [`SystemEnv`]; tests supply a mock so
/// precedence is exercised without touching process-global env vars.
pub trait Env {
    /// The value of environment variable `key`, if set (as an `OsString` so
    /// non-UTF-8 paths survive).
    fn var(&self, key: &str) -> Option<OsString>;

    /// The user's home directory (`~`), consulted only for XDG fallbacks.
    fn home_dir(&self) -> Option<PathBuf>;
}

/// Reads the real process environment.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemEnv;

impl Env for SystemEnv {
    fn var(&self, key: &str) -> Option<OsString> {
        std::env::var_os(key)
    }

    fn home_dir(&self) -> Option<PathBuf> {
        // `std::env::home_dir` was un-deprecated in Rust 1.85; on unix it reads
        // `$HOME` and falls back to `getpwuid_r` inside std, so home resolution
        // needs no `libc` dependency of our own.
        #[allow(deprecated)]
        std::env::home_dir()
    }
}

/// An error resolving, creating, or validating a store path.
#[derive(Debug)]
#[non_exhaustive]
pub enum PathError {
    /// No base directory could be resolved for `which` (`"data_dir"` /
    /// `"config_dir"`): neither `LOCAL_RAG_HOME`, the platform var, nor a home
    /// dir was available.
    NoBaseDir {
        /// Which directory failed to resolve.
        which: &'static str,
    },
    /// A filesystem operation on `path` failed.
    Io {
        /// The path being operated on.
        path: PathBuf,
        /// The underlying I/O error.
        source: io::Error,
    },
    /// `path` exists but is owned by another uid (POSIX).
    WrongOwner {
        /// The offending path.
        path: PathBuf,
        /// The current effective uid.
        expected_uid: u32,
        /// The uid that actually owns `path`.
        found_uid: u32,
    },
    /// `path` exists but is not the `expected` kind (`"directory"` / `"file"`),
    /// e.g. a symlink where a real directory was required.
    UnexpectedType {
        /// The offending path.
        path: PathBuf,
        /// The kind that was required.
        expected: &'static str,
    },
    /// `path` exists, is owned by us, and is the right kind, but its POSIX
    /// mode is not the normative `0700`/`0600` (spec 12 §6). Read-only
    /// finding — `local-rag doctor`'s permissions audit ([`perms::audit_path`],
    /// T16-03) is the only producer; `ensure_dir`/`ensure_file_0600`
    /// self-heal this instead of reporting it.
    WrongMode {
        /// The offending path.
        path: PathBuf,
        /// The normative mode (`0o700` for a directory, `0o600` for a file).
        expected: u32,
        /// The mode actually found.
        found: u32,
    },
}

impl PathError {
    fn io(path: &Path, source: io::Error) -> Self {
        PathError::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PathError::NoBaseDir { which } => write!(
                f,
                "cannot resolve {which}: set LOCAL_RAG_HOME or the platform data/config directory"
            ),
            PathError::Io { path, source } => {
                write!(f, "i/o error at {}: {source}", path.display())
            }
            PathError::WrongOwner {
                path,
                expected_uid,
                found_uid,
            } => write!(
                f,
                "{} is owned by uid {found_uid}, expected {expected_uid}",
                path.display()
            ),
            PathError::UnexpectedType { path, expected } => {
                write!(f, "{} exists but is not a {expected}", path.display())
            }
            PathError::WrongMode {
                path,
                expected,
                found,
            } => write!(
                f,
                "{} has mode {found:04o}, expected {expected:04o}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for PathError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PathError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Return the env var `key` only if it is set and non-empty (an empty value is
/// treated as unset, per the XDG Base Directory spec).
fn nonempty_var(env: &impl Env, key: &str) -> Option<OsString> {
    match env.var(key) {
        Some(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

/// Accept a path only if it is absolute (the XDG spec says relative values in
/// the base-directory vars must be ignored).
fn absolute(value: OsString) -> Option<PathBuf> {
    let path = PathBuf::from(value);
    path.is_absolute().then_some(path)
}

/// Resolve `<data_dir>` (spec 02 §2.1).
///
/// POSIX: `$LOCAL_RAG_HOME`, else `$XDG_DATA_HOME` (absolute), else
/// `<home>/.local/share`. Windows: `$LOCAL_RAG_HOME`, else `%LOCALAPPDATA%`.
pub fn data_dir(env: &impl Env) -> Result<PathBuf, PathError> {
    if let Some(home) = nonempty_var(env, "LOCAL_RAG_HOME") {
        return Ok(PathBuf::from(home));
    }
    platform_data_dir(env)
}

/// Resolve `<config_dir>` (spec 02 §2.1, with the `LOCAL_RAG_HOME` override
/// documented at the module level).
///
/// POSIX: `$LOCAL_RAG_HOME/config`, else `$XDG_CONFIG_HOME/local-rag`
/// (absolute), else `<home>/.config/local-rag`. Windows: `$LOCAL_RAG_HOME/config`,
/// else `%APPDATA%\local-rag`.
pub fn config_dir(env: &impl Env) -> Result<PathBuf, PathError> {
    if let Some(home) = nonempty_var(env, "LOCAL_RAG_HOME") {
        return Ok(PathBuf::from(home).join("config"));
    }
    platform_config_dir(env)
}

#[cfg(not(windows))]
fn platform_data_dir(env: &impl Env) -> Result<PathBuf, PathError> {
    if let Some(xdg) = nonempty_var(env, "XDG_DATA_HOME").and_then(absolute) {
        return Ok(xdg);
    }
    if let Some(home) = env.home_dir() {
        return Ok(home.join(".local").join("share"));
    }
    Err(PathError::NoBaseDir { which: "data_dir" })
}

#[cfg(windows)]
fn platform_data_dir(env: &impl Env) -> Result<PathBuf, PathError> {
    if let Some(local) = nonempty_var(env, "LOCALAPPDATA") {
        return Ok(PathBuf::from(local));
    }
    Err(PathError::NoBaseDir { which: "data_dir" })
}

#[cfg(not(windows))]
fn platform_config_dir(env: &impl Env) -> Result<PathBuf, PathError> {
    if let Some(xdg) = nonempty_var(env, "XDG_CONFIG_HOME").and_then(absolute) {
        return Ok(xdg.join("local-rag"));
    }
    if let Some(home) = env.home_dir() {
        return Ok(home.join(".config").join("local-rag"));
    }
    Err(PathError::NoBaseDir {
        which: "config_dir",
    })
}

#[cfg(windows)]
fn platform_config_dir(env: &impl Env) -> Result<PathBuf, PathError> {
    if let Some(appdata) = nonempty_var(env, "APPDATA") {
        return Ok(PathBuf::from(appdata).join("local-rag"));
    }
    Err(PathError::NoBaseDir {
        which: "config_dir",
    })
}

/// The on-disk layout of one store, rooted at `<data_dir>/local-rag` (spec 02 §2).
///
/// ```
/// use local_rag_core::paths::StoreLayout;
/// use std::path::Path;
/// let layout = StoreLayout::new("/srv/example/local-rag".into());
/// assert_eq!(layout.state_db(), Path::new("/srv/example/local-rag/state.sqlite"));
/// assert_eq!(layout.socket_path(), Path::new("/srv/example/local-rag/run/daemon.sock"));
/// ```
#[derive(Debug, Clone)]
pub struct StoreLayout {
    root: PathBuf,
}

impl StoreLayout {
    /// Wrap an explicit store root (`<data_dir>/local-rag`). Prefer
    /// [`StoreLayout::resolve`] outside tests.
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Resolve the store root from `env` as `<data_dir>/local-rag`.
    pub fn resolve(env: &impl Env) -> Result<Self, PathError> {
        Ok(Self::new(data_dir(env)?.join("local-rag")))
    }

    /// The store root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `store.lock` — the daemon store lock (created by T15, not by [`ensure`](Self::ensure)).
    pub fn store_lock(&self) -> PathBuf {
        self.root.join("store.lock")
    }

    /// `migration.lock` — L1, the schema-migration lock (spec 02 §5). Created on
    /// demand (0600) by the migration runner, not by [`ensure`](Self::ensure).
    pub fn migration_lock(&self) -> PathBuf {
        self.root.join("migration.lock")
    }

    /// `state.sqlite` — the canonical source of truth.
    pub fn state_db(&self) -> PathBuf {
        self.root.join("state.sqlite")
    }

    /// `cache.sqlite` — the rebuildable cache.
    pub fn cache_db(&self) -> PathBuf {
        self.root.join("cache.sqlite")
    }

    /// `projection/` — dense shards, one subdirectory per worktree.
    pub fn projection_dir(&self) -> PathBuf {
        self.root.join("projection")
    }

    /// The dense shard **root** for `worktree_id` (spec 05 §2:
    /// `projection/<worktree_id>/`).
    ///
    /// One root per worktree, for the worktree's whole lifetime — an attach or a
    /// move keeps it (spec 05 §8 `[FIXED]`: "same shard directory (keyed by
    /// `worktree_id`), never a second shard"). Shard lifecycle sweeps
    /// (`local_rag_store::housekeeping`) operate on this root and remove it
    /// recursively, so they cover everything below it.
    pub fn projection_shard(&self, worktree_id: &str) -> PathBuf {
        self.projection_dir().join(worktree_id)
    }

    /// The dense shard directory for one `(worktree_id, model_space_id)` pair —
    /// `projection/<worktree_id>/<model_space_id>/` (T11-05).
    ///
    /// Spec 05 §2 leaves the *contents* of a worktree's shard root
    /// backend-defined; this splits them per model space, which is what makes
    /// two `[FIXED]` requirements of spec 10 §4 simultaneously true during a
    /// model migration:
    ///
    /// * "Different dimensions ⇒ separate shard layout / named-vector — never in
    ///   place": a model space with a different `representation.dimensions`
    ///   opens its own directory with its own [`ShardParams`], instead of
    ///   attempting an impossible in-place widening of an existing shard;
    /// * "until step 4 commits for a worktree, that worktree still runs A
    ///   entirely": the outgoing space's shard is never touched while the
    ///   incoming one is filled, so a crash mid-switch leaves a fully serviceable
    ///   old shard rather than a half-rewritten one.
    ///
    /// [`ShardParams`]: https://docs.rs/local-rag-projection
    pub fn projection_shard_space(&self, worktree_id: &str, model_space_id: &str) -> PathBuf {
        self.projection_shard(worktree_id).join(model_space_id)
    }

    /// `spool/` — durable observation segments, one subdirectory per session.
    pub fn spool_dir(&self) -> PathBuf {
        self.root.join("spool")
    }

    /// The spool directory for `session_id`.
    pub fn spool_session(&self, session_id: &str) -> PathBuf {
        self.spool_dir().join(session_id)
    }

    /// `models/` — downloaded model assets.
    pub fn models_dir(&self) -> PathBuf {
        self.root.join("models")
    }

    /// The asset directory for `model_id`.
    pub fn model_dir(&self, model_id: &str) -> PathBuf {
        self.models_dir().join(model_id)
    }

    /// `run/` — runtime endpoint directory.
    pub fn run_dir(&self) -> PathBuf {
        self.root.join("run")
    }

    /// `run/daemon.sock` — the POSIX unix-domain socket path.
    pub fn socket_path(&self) -> PathBuf {
        self.run_dir().join("daemon.sock")
    }

    /// `logs/` — rotated daemon logs.
    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }

    /// `quarantine/` — corrupted shards moved here before rebuild.
    pub fn quarantine_dir(&self) -> PathBuf {
        self.root.join("quarantine")
    }

    /// `backups/` — pre-mutation `state.sqlite` snapshots taken via `VACUUM INTO`
    /// before a destructive migration (spec 13 §3). Individual backup files are
    /// named `state-<version>-<ts>.sqlite` by the migration runner.
    pub fn backups_dir(&self) -> PathBuf {
        self.root.join("backups")
    }

    /// Idempotently create the store directory tree.
    ///
    /// The store root and its managed subdirectories are created `0700` (POSIX)
    /// and verified to be owned by the current uid; ancestors of the root (e.g.
    /// `~/.local/share`) may be shared and are created at the platform default
    /// mode. Files (`store.lock`, `*.sqlite`) are created by later tasks, not
    /// here; individual backup files are written by the migration runner into the
    /// `backups/` directory created here.
    pub fn ensure(&self) -> Result<(), PathError> {
        if let Some(parent) = self.root.parent() {
            std::fs::create_dir_all(parent).map_err(|e| PathError::io(parent, e))?;
        }
        perms::ensure_dir(&self.root)?;
        for dir in [
            self.projection_dir(),
            self.spool_dir(),
            self.models_dir(),
            self.run_dir(),
            self.logs_dir(),
            self.quarantine_dir(),
            self.backups_dir(),
        ] {
            perms::ensure_dir(&dir)?;
        }
        Ok(())
    }

    /// Read-only permission/ownership audit of the store tree (spec 12 §6,
    /// T16-03's `local-rag doctor`) — the same paths [`ensure`](Self::ensure)
    /// creates/re-asserts, plus `store.lock`/`state.sqlite`/`cache.sqlite`
    /// (files owned by later tasks, checked here if and only if they already
    /// exist). Unlike `ensure`, this never creates or chmods anything — a
    /// path that does not exist yet is silently skipped (its absence is a
    /// different diagnostic's business: versions/cache-binding/orphans, not
    /// permissions), not reported as a finding.
    pub fn audit_permissions(&self) -> Vec<PathError> {
        let dirs = [
            self.root.clone(),
            self.projection_dir(),
            self.spool_dir(),
            self.models_dir(),
            self.run_dir(),
            self.logs_dir(),
            self.quarantine_dir(),
            self.backups_dir(),
        ];
        let files = [
            self.store_lock(),
            self.migration_lock(),
            self.state_db(),
            self.cache_db(),
        ];
        dirs.iter()
            .filter_map(|p| perms::audit_path(p, ExpectedKind::Dir))
            .chain(
                files
                    .iter()
                    .filter_map(|p| perms::audit_path(p, ExpectedKind::File)),
            )
            .collect()
    }

    /// The MCP endpoint for this store (spec 02 §2.1): a unix-domain socket on
    /// POSIX, a named pipe on Windows.
    #[cfg(unix)]
    pub fn endpoint(&self) -> Result<Endpoint, PathError> {
        Ok(Endpoint::Socket(self.socket_path()))
    }

    /// The MCP endpoint for this store (spec 02 §2.1).
    #[cfg(windows)]
    pub fn endpoint(&self) -> Result<Endpoint, PathError> {
        Ok(Endpoint::Pipe(pipe_name(&current_user_sid()?)))
    }
}

/// The MCP endpoint address for a store (spec 02 §2.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    /// POSIX unix-domain socket at `<root>/run/daemon.sock`.
    #[cfg(unix)]
    Socket(PathBuf),
    /// Windows named pipe `\\.\pipe\local-rag-<12 hex>`.
    #[cfg(windows)]
    Pipe(String),
}

/// The Windows named-pipe name for a user SID: `\\.\pipe\local-rag-<first 12
/// lowercase hex of sha256(sid)>` (spec 02 §2.1).
///
/// Pure and compiled on every target, so it is exercised by the POSIX CI host.
///
/// ```
/// let name = local_rag_core::paths::pipe_name("S-1-5-21-1-2-3-1001");
/// let prefix = r"\\.\pipe\local-rag-";
/// assert!(name.starts_with(prefix));
/// assert_eq!(name.len(), prefix.len() + 12);
/// ```
pub fn pipe_name(sid: &str) -> String {
    let digest = crate::hash::sha256_hex(sid.as_bytes());
    format!(r"\\.\pipe\local-rag-{}", &digest[..12])
}

#[cfg(windows)]
fn current_user_sid() -> Result<String, PathError> {
    // TODO(Windows enablement, T17): resolve the current user's SID via the
    // Win32 API. Windows is not yet in the CI matrix (spec 13 §1), so the SID
    // lookup is deferred; `pipe_name` itself is implemented and tested.
    unimplemented!("Windows SID lookup is deferred until Windows joins the CI matrix")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[derive(Default)]
    struct MockEnv {
        vars: HashMap<String, OsString>,
        home: Option<PathBuf>,
    }

    impl MockEnv {
        fn set(mut self, key: &str, val: impl Into<OsString>) -> Self {
            self.vars.insert(key.to_string(), val.into());
            self
        }

        fn home(mut self, home: impl Into<PathBuf>) -> Self {
            self.home = Some(home.into());
            self
        }
    }

    impl Env for MockEnv {
        fn var(&self, key: &str) -> Option<OsString> {
            self.vars.get(key).cloned()
        }
        fn home_dir(&self) -> Option<PathBuf> {
            self.home.clone()
        }
    }

    #[test]
    fn local_rag_home_overrides_data_dir() {
        // Even with XDG and home set, LOCAL_RAG_HOME wins.
        let env = MockEnv::default()
            .set("LOCAL_RAG_HOME", "/override/home")
            .set("XDG_DATA_HOME", "/xdg/data")
            .home("/home/user");
        assert_eq!(data_dir(&env).unwrap(), PathBuf::from("/override/home"));
    }

    #[test]
    fn config_dir_under_local_rag_home() {
        // LOCAL_RAG_HOME overrides config_dir too (module-level [SPEC] note):
        // sibling `config`, distinct from the store root `.../local-rag`.
        let env = MockEnv::default().set("LOCAL_RAG_HOME", "/override/home");
        assert_eq!(
            config_dir(&env).unwrap(),
            PathBuf::from("/override/home/config")
        );
        assert_eq!(
            StoreLayout::resolve(&env).unwrap().root(),
            Path::new("/override/home/local-rag")
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn data_dir_precedence_posix() {
        // XDG (absolute) beats home.
        let env = MockEnv::default()
            .set("XDG_DATA_HOME", "/xdg/data")
            .home("/home/user");
        assert_eq!(data_dir(&env).unwrap(), PathBuf::from("/xdg/data"));

        // home fallback when XDG unset.
        let env = MockEnv::default().home("/home/user");
        assert_eq!(
            data_dir(&env).unwrap(),
            PathBuf::from("/home/user/.local/share")
        );

        // empty XDG is treated as unset → home fallback.
        let env = MockEnv::default()
            .set("XDG_DATA_HOME", "")
            .home("/home/user");
        assert_eq!(
            data_dir(&env).unwrap(),
            PathBuf::from("/home/user/.local/share")
        );

        // relative XDG is ignored per XDG spec → home fallback.
        let env = MockEnv::default()
            .set("XDG_DATA_HOME", "relative/data")
            .home("/home/user");
        assert_eq!(
            data_dir(&env).unwrap(),
            PathBuf::from("/home/user/.local/share")
        );

        // nothing resolvable.
        let env = MockEnv::default();
        assert!(matches!(
            data_dir(&env),
            Err(PathError::NoBaseDir { which: "data_dir" })
        ));
    }

    #[cfg(not(windows))]
    #[test]
    fn config_dir_precedence_posix() {
        let env = MockEnv::default()
            .set("XDG_CONFIG_HOME", "/xdg/config")
            .home("/home/user");
        assert_eq!(
            config_dir(&env).unwrap(),
            PathBuf::from("/xdg/config/local-rag")
        );

        let env = MockEnv::default().home("/home/user");
        assert_eq!(
            config_dir(&env).unwrap(),
            PathBuf::from("/home/user/.config/local-rag")
        );

        // empty XDG is treated as unset → home fallback (same helper as data_dir).
        let env = MockEnv::default()
            .set("XDG_CONFIG_HOME", "")
            .home("/home/user");
        assert_eq!(
            config_dir(&env).unwrap(),
            PathBuf::from("/home/user/.config/local-rag")
        );

        // relative XDG is ignored per XDG spec → home fallback.
        let env = MockEnv::default()
            .set("XDG_CONFIG_HOME", "relative/config")
            .home("/home/user");
        assert_eq!(
            config_dir(&env).unwrap(),
            PathBuf::from("/home/user/.config/local-rag")
        );

        let env = MockEnv::default();
        assert!(matches!(
            config_dir(&env),
            Err(PathError::NoBaseDir {
                which: "config_dir"
            })
        ));
    }

    #[test]
    fn unicode_and_space_paths_are_preserved() {
        let base = "/base/naïve dir/日本語/local rag home";
        let env = MockEnv::default().set("LOCAL_RAG_HOME", base);
        assert_eq!(data_dir(&env).unwrap(), PathBuf::from(base));
        assert_eq!(
            StoreLayout::resolve(&env).unwrap().root(),
            Path::new(&format!("{base}/local-rag"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_path_survives() {
        use std::os::unix::ffi::OsStringExt;
        // A byte sequence that is not valid UTF-8.
        let raw = OsString::from_vec(vec![b'/', 0x66, 0x80, 0x6f]);
        let env = MockEnv::default().set("LOCAL_RAG_HOME", raw.clone());
        assert_eq!(data_dir(&env).unwrap(), PathBuf::from(raw));
    }

    #[test]
    fn store_layout_maps_every_path() {
        let layout = StoreLayout::new(PathBuf::from("/s/local-rag"));
        assert_eq!(layout.store_lock(), Path::new("/s/local-rag/store.lock"));
        assert_eq!(
            layout.migration_lock(),
            Path::new("/s/local-rag/migration.lock")
        );
        assert_eq!(layout.state_db(), Path::new("/s/local-rag/state.sqlite"));
        assert_eq!(layout.cache_db(), Path::new("/s/local-rag/cache.sqlite"));
        assert_eq!(
            layout.projection_shard("wt-1"),
            Path::new("/s/local-rag/projection/wt-1")
        );
        // Per-model-space shard directory nests *under* the worktree root, so a
        // sweep of the root still covers every space (T11-05).
        assert_eq!(
            layout.projection_shard_space("wt-1", "ms-1"),
            Path::new("/s/local-rag/projection/wt-1/ms-1")
        );
        assert!(
            layout
                .projection_shard_space("wt-1", "ms-1")
                .starts_with(layout.projection_shard("wt-1"))
        );
        assert_eq!(
            layout.spool_session("sess-1"),
            Path::new("/s/local-rag/spool/sess-1")
        );
        assert_eq!(
            layout.model_dir("m-1"),
            Path::new("/s/local-rag/models/m-1")
        );
        assert_eq!(
            layout.socket_path(),
            Path::new("/s/local-rag/run/daemon.sock")
        );
        assert_eq!(layout.logs_dir(), Path::new("/s/local-rag/logs"));
        assert_eq!(
            layout.quarantine_dir(),
            Path::new("/s/local-rag/quarantine")
        );
        assert_eq!(layout.backups_dir(), Path::new("/s/local-rag/backups"));
    }

    #[test]
    fn pipe_name_fixture() {
        let prefix = r"\\.\pipe\local-rag-";
        // Pinned known-answer table: `sid → first 12 hex of sha256(sid.as_bytes())`,
        // ground truth computed independently (`printf %s <sid> | shasum -a 256`).
        // Pinning the exact suffix — not just its length/charset — catches a
        // regression that sliced a different 12 chars of the digest.
        let cases = [
            ("S-1-5-18", "593347bdfcc9"),
            ("S-1-5-21-1-2-3-1001", "c169ebe52e9c"),
        ];
        for (sid, expected_suffix) in cases {
            let name = pipe_name(sid);
            assert!(name.starts_with(prefix), "prefix for {sid}");
            let suffix = &name[prefix.len()..];
            assert_eq!(suffix.len(), 12, "12 hex chars for {sid}");
            assert!(
                suffix
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()),
                "lowercase hex for {sid}"
            );
            assert_eq!(suffix, expected_suffix, "pinned digest slice for {sid}");
            // Determinism.
            assert_eq!(name, pipe_name(sid));
        }
        // Distinct SIDs yield distinct names.
        assert_ne!(pipe_name("S-1-5-18"), pipe_name("S-1-5-19"));
    }

    #[cfg(unix)]
    #[test]
    fn endpoint_is_socket_on_unix() {
        let layout = StoreLayout::new(PathBuf::from("/s/local-rag"));
        assert_eq!(
            layout.endpoint().unwrap(),
            Endpoint::Socket(PathBuf::from("/s/local-rag/run/daemon.sock"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn ensure_creates_full_tree_idempotently() {
        use local_rag_test_support::TempHome;
        use std::os::unix::fs::PermissionsExt;

        let home = TempHome::new().expect("temp home");
        // Root's parent (the temp home) exists; the store root does not.
        let layout = StoreLayout::new(home.join("local-rag"));

        for _ in 0..2 {
            layout.ensure().expect("ensure is idempotent");
        }

        for dir in [
            layout.root().to_path_buf(),
            layout.projection_dir(),
            layout.spool_dir(),
            layout.models_dir(),
            layout.run_dir(),
            layout.logs_dir(),
            layout.quarantine_dir(),
            layout.backups_dir(),
        ] {
            assert!(dir.is_dir(), "{} exists", dir.display());
            let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{} is 0700", dir.display());
        }
    }

    #[test]
    fn audit_permissions_is_empty_on_a_freshly_ensured_tree() {
        use local_rag_test_support::TempHome;

        let home = TempHome::new().expect("temp home");
        let layout = StoreLayout::new(home.join("local-rag"));
        layout.ensure().expect("ensure store tree");

        let findings = layout.audit_permissions();
        assert!(
            findings.is_empty(),
            "a freshly ensured tree has no findings, got {findings:?}"
        );
    }

    #[test]
    fn audit_permissions_skips_files_that_do_not_exist_yet() {
        use local_rag_test_support::TempHome;

        let home = TempHome::new().expect("temp home");
        let layout = StoreLayout::new(home.join("local-rag"));
        layout.ensure().expect("ensure store tree");
        // `state.sqlite`/`cache.sqlite`/`store.lock`/`migration.lock` are never
        // created by `ensure()` — their absence must not be reported.
        assert!(!layout.state_db().exists());
        assert!(!layout.cache_db().exists());
        assert!(!layout.store_lock().exists());
        assert!(!layout.migration_lock().exists());

        let findings = layout.audit_permissions();
        assert!(
            findings.is_empty(),
            "absent files are not permission findings, got {findings:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn audit_permissions_finds_a_widened_managed_directory() {
        use local_rag_test_support::TempHome;
        use std::os::unix::fs::PermissionsExt;

        let home = TempHome::new().expect("temp home");
        let layout = StoreLayout::new(home.join("local-rag"));
        layout.ensure().expect("ensure store tree");
        std::fs::set_permissions(layout.spool_dir(), std::fs::Permissions::from_mode(0o755))
            .expect("widen spool_dir mode");

        let findings = layout.audit_permissions();
        assert_eq!(
            findings.len(),
            1,
            "exactly the widened dir, got {findings:?}"
        );
        match &findings[0] {
            PathError::WrongMode {
                path,
                expected,
                found,
            } => {
                assert_eq!(path, &layout.spool_dir());
                assert_eq!(*expected, 0o700);
                assert_eq!(*found, 0o755);
            }
            other => panic!("expected WrongMode, got {other:?}"),
        }

        // Read-only: the widened mode is still there after the audit.
        let mode = std::fs::metadata(layout.spool_dir())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o755,
            "audit_permissions must never fix what it finds"
        );
    }
}
