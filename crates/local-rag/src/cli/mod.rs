//! The `local-rag` binary's CLI surface beyond `serve` (spec 11 §6) —
//! T15-07: `version`/`status`/`stop`/`restart`/`init`, `index`/`reindex`/
//! `watch`, `repo`/`worktree`, `rebuild`. T15-08 (D-025): `memory`, `gc`,
//! `stats`. T16-02 (D-025): `inspect`, `export`, `purge`, over the domain
//! layer in `local_rag_store::privacy`. T16-03 (D-025): `doctor`.
//!
//! Argument parsing is `clap` (`derive` feature, X-002 — explicit post-G17
//! product decision; see `docs/specification/11-interfaces.md` §6's X-002
//! as-built note and `CONTRIBUTING.md`'s dependency table for why hand-rolled
//! `std::env::args()`, T15-07's original as-built choice, stopped being
//! enough). [`Cli`] is the single root `#[derive(Parser)]`; [`Command`] is
//! the top-level `#[derive(Subcommand)]` enum `main.rs` matches on. Each
//! subcommand still owns its own `Args`/`Subcommand` type and `run` function
//! in its own file (module-per-concern, mirroring
//! `daemon/{lock,probe,shutdown,...}.rs`) — only the per-file hand-written
//! flag loop moved to a derive; a command's business logic, once past its own
//! typed `run(args: ...)` entry point, is unchanged.
//!
//! `local-rag-proxy`/`local-rag-hook`/`xtask` are unaffected: none of them
//! has a comparable multi-command surface, and they keep hand-rolled
//! `std::env::args()`.
//!
//! None of these commands ever take `store.lock` (`daemon::lock::acquire`) —
//! that lock is exclusive to *one running daemon instance*, not "the only
//! writer ever." Every command here opens `StateDb`/`CacheDb` directly,
//! exactly like `crates/xtask/src/bench/run.rs` already does for its own
//! one-shot indexing runs — safe to run alongside a live `serve`, since
//! `state.sqlite`/`cache.sqlite` are WAL-mode with `busy_timeout=5000` (spec
//! 03 §2), and this project's own generation/switch model is additive by
//! construction (concurrent indexers of the *same* worktree are wasteful,
//! never unsafe).

pub mod coverage;
pub mod delivery;
pub mod doctor;
pub mod export;
pub mod freshness;
pub mod gc;
pub mod index;
pub mod init;
pub mod inspect;
pub mod memory;
pub mod project;
pub mod purge;
pub mod rebuild;
pub mod repo;
pub mod service;
pub mod stats;
pub mod status;
pub mod vacuum;
pub mod watch;
pub mod worktree;

use std::future::Future;
use std::process::ExitCode;
use std::time::Duration;

use local_rag::daemon::{LIVENESS_PROBE_TIMEOUT_MS, StoreLockFileState, read_store_lock_file};
use local_rag_core::config::{Config, ConfigError};
use local_rag_core::identity::Uuid;
use local_rag_core::paths::{PathError, StoreLayout, SystemEnv, config_dir};
use local_rag_core::process::pid_exists;
use local_rag_store::StateDb;

// `#[command(version)]` defaults to this crate's own `CARGO_PKG_VERSION` —
// numerically identical to `local_rag_core::VERSION` (both resolve
// `version.workspace = true`), so `--version`/`-V` print the same
// `{bin} {version}` line the explicit `Command::Version` subcommand below
// prints via `local_rag_core::version_line`. A plain `//` comment, not
// `///`: clap's derive lifts a doc comment on `Cli` itself into `--help`'s
// about text, which this implementation note is not meant to be.
#[derive(Debug, clap::Parser)]
#[command(
    name = "local-rag",
    version,
    about = "local-rag: daemon + CLI (spec 11 §6)"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, clap::Subcommand)]
pub enum Command {
    /// Print the binary's name and version.
    Version,
    /// Run the daemon (spec 02 §4).
    Serve,
    /// Report whether a daemon is running against this store.
    Status(status::StatusArgs),
    /// Stop a running daemon.
    Stop,
    /// Stop, then start a fresh daemon.
    Restart,
    /// Register the default embedding model's `code_raw` representation.
    Init(init::InitArgs),
    /// Index a directory as a new (or already-known) worktree.
    Index(index::IndexArgs),
    /// Re-index the current directory's already-registered worktree.
    Reindex,
    /// Continuously reconcile the current directory's worktree until interrupted.
    Watch,
    /// Repository registry operations.
    Repo {
        #[command(subcommand)]
        command: repo::RepoCommand,
    },
    /// Worktree registry operations.
    Worktree {
        #[command(subcommand)]
        command: worktree::WorktreeCommand,
    },
    /// Daemon-managed background indexing (spec 11 §8, T20-08).
    Project {
        #[command(subcommand)]
        command: project::ProjectCommand,
    },
    /// Force-rebuild the FTS view and/or dense projection from already-indexed content.
    Rebuild(rebuild::RebuildArgs),
    /// Durable memory review and mutation.
    Memory {
        #[command(subcommand)]
        command: memory::MemoryCommand,
    },
    /// Run retention/GC sweeps.
    Gc(gc::GcArgs),
    /// Reclaim the free space GC left inside the database file.
    Vacuum(vacuum::VacuumArgs),
    /// Report store-wide counts and queue occupancy.
    Stats(stats::StatsArgs),
    /// Read one observation/memory/generation row as JSON.
    Inspect(inspect::InspectArgs),
    /// Export a scoped, deterministic JSON dump of memory entries.
    Export(export::ExportArgs),
    /// Hard-delete a memory entry, session, or everything.
    Purge(purge::PurgeArgs),
    /// Store-wide, read-only health report.
    Doctor(doctor::DoctorArgs),
}

/// Usage/argument-parse error — reserved uniformly across every subcommand in
/// this module tree (`clap`'s own default mismatch exit code is the same 2).
pub const EXIT_USAGE: u8 = 2;

/// Resolve `(layout, config)` the same way `main.rs::serve` does, minus the
/// full daemon `StartOptions` wiring no one-shot subcommand needs. Every
/// command in this module tree starts here.
pub fn resolve_layout_and_config() -> Result<(StoreLayout, Config), String> {
    let env = SystemEnv;
    let layout = StoreLayout::resolve(&env)
        .map_err(|e: PathError| format!("could not resolve the store directory: {e}"))?;
    let config = config_dir(&env)
        .map_err(|e| format!("could not resolve the config directory: {e}"))
        .and_then(|dir| {
            Config::load(&dir).map_err(|e: ConfigError| format!("could not load config.toml: {e}"))
        })?;
    Ok((layout, config))
}

/// Print `{BIN}: {message}` to stderr and return [`ExitCode::FAILURE`] — the
/// uniform error-reporting shape `main.rs`'s own `serve` already establishes.
pub fn fail(bin: &str, message: &str) -> ExitCode {
    eprintln!("{bin}: {message}");
    ExitCode::FAILURE
}

/// Run one async operation to completion on a fresh, throwaway runtime.
///
/// Every one-shot subcommand that touches `state.sqlite` needs *an* executor
/// (`StateWriter::transaction` awaits `tokio::sync` channels), but none of
/// them is `main.rs::serve`'s long-lived daemon — building a whole runtime per
/// invocation is the simplest correct thing, not a hot path.
pub(crate) fn block_on<F: Future>(fut: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a current-thread tokio runtime")
        .block_on(fut)
}

/// A `clap` `value_parser` for `local_rag_store::ScopeKind` — shared by
/// `export --scope` and `memory list --scope`, the CLI's only two consumers
/// of this scope vocabulary. A free function, not a `ValueEnum` derive on
/// `ScopeKind` itself: that type lives in `local-rag-store`, outside this
/// crate, so the orphan rule rules a derive there out; `ScopeKind::from_db`
/// already exists as the exact string vocabulary to defer to.
pub(crate) fn parse_scope_kind(raw: &str) -> Result<local_rag_store::ScopeKind, String> {
    local_rag_store::ScopeKind::from_db(raw)
        .ok_or_else(|| "must be one of global/repository/worktree".to_string())
}

/// The current wall-clock time as Unix milliseconds — mirrors
/// `main.rs::system_now_ms` exactly; shared here because every write-side CLI
/// command in this module tree needs one, unlike `main.rs`'s single call site.
pub(crate) fn system_now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// T20-09: the fixed-wording stderr advisory `index`/`reindex`/`watch` print
/// when the target worktree is both daemon-managed (T20-01) and a live daemon
/// answers the liveness probe — spec 11 §6's own as-built note quotes this
/// constant verbatim, so the wording lives in exactly one place.
pub(crate) const DAEMON_MANAGED_ADVISORY: &str = "local-rag: this worktree is managed by a running daemon — `local-rag project reindex` \
     avoids duplicate indexing; continuing anyway";

/// Print [`DAEMON_MANAGED_ADVISORY`] to stderr, once, iff `worktree_id` is
/// enrolled in `managed_worktree` (`local_rag_store::is_managed`, T20-01 —
/// regardless of `enabled`, matching that function's own doc: a paused
/// project is still daemon-managed territory) **and** a live daemon answers
/// the same `read_store_lock_file` → `pid_exists` → `fetch_welcome` liveness
/// sequence `cli::status::compute_status` already uses. Never fails, never
/// changes the caller's exit code or stdout — this is fail-open by the
/// card's own explicit "не в scope: отказ выполнять команду": the printed
/// line is the only observable effect, the command itself always continues.
pub(crate) fn advise_if_daemon_managed(layout: &StoreLayout, state: &StateDb, worktree_id: Uuid) {
    let managed = state
        .open_read()
        .ok()
        .and_then(|conn| local_rag_store::is_managed(&conn, &worktree_id.to_string()).ok())
        .unwrap_or(false);
    if !managed {
        return;
    }

    #[cfg(unix)]
    let daemon_alive = match read_store_lock_file(layout) {
        StoreLockFileState::Parsed(info) if info.ready && pid_exists(info.pid) => {
            local_rag::daemon::fetch_welcome(
                &layout.socket_path(),
                Duration::from_millis(LIVENESS_PROBE_TIMEOUT_MS),
            )
            .is_some_and(|w| w.store_instance_uuid == info.instance_uuid)
        }
        _ => false,
    };
    #[cfg(not(unix))]
    let daemon_alive = false;

    if daemon_alive {
        eprintln!("{DAEMON_MANAGED_ADVISORY}");
    }
}
