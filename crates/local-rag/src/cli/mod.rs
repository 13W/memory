//! The `local-rag` binary's CLI surface beyond `version`/`serve` (spec 11 §6)
//! — T15-07: `status`/`stop`/`restart`/`init`, `index`/`reindex`/`watch`,
//! `repo`/`worktree`, `rebuild`. T15-08 (D-025): `memory`, `gc`, `stats`.
//! `inspect`/`export`/`purge`/`doctor` are D-025's deferred scope, owned by
//! T16-02/T16-03 — no domain code exists yet for them to adapt.
//!
//! Argument parsing is deliberately hand-rolled (`std::env::args()`, the same
//! convention `main.rs`/`local-rag-proxy`/`xtask`'s own `run_bench` already
//! use) — this workspace has never added a CLI-parsing crate, and the whole
//! surface here (one level of nesting, no repeated/multi-valued flags) does
//! not need one. Each command owns its own small flag loop in its own file
//! (module-per-concern, mirroring `daemon/{lock,probe,shutdown,...}.rs`),
//! not one central parser that would need to know every command's grammar.
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

pub mod gc;
pub mod index;
pub mod init;
pub mod memory;
pub mod rebuild;
pub mod repo;
pub mod service;
pub mod stats;
pub mod status;
pub mod watch;
pub mod worktree;

use std::future::Future;
use std::process::ExitCode;

use local_rag_core::config::{Config, ConfigError};
use local_rag_core::paths::{PathError, StoreLayout, SystemEnv, config_dir};

/// Usage/argument-parse error — reserved uniformly across every subcommand in
/// this module tree.
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
