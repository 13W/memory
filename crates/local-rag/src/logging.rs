//! Live `tracing` logging for `local-rag serve` (X-004).
//!
//! Before this task, `local-rag serve` printed nothing while running in a
//! terminal — no logging subsystem existed anywhere in this workspace (spec
//! 07 §4's own as-built note said so plainly). This module is the daemon's
//! **only** subscriber: the global registry it installs applies process-wide,
//! but nothing outside `main.rs::serve()` ever calls [`init`] — `index`/
//! `watch`/the rest of the CLI stay exactly as silent (or as `eprintln!`-loud)
//! as they already were, and the library half of this crate
//! (`local_rag::daemon`) never links `tracing-subscriber` at all.
//!
//! There are **two sinks, one filter** (X-007):
//!
//! - **stderr**, for a human running `local-rag serve` in a terminal — the
//!   daemon's IPC is a Unix domain socket (spec 02 §2.1), never stdio, so
//!   stderr is free;
//! - **a file** under `StoreLayout::logs_dir`, rotated daily and capped at
//!   [`MAX_LOG_FILES`].
//!
//! X-004 shipped only the first and left `logs_dir` reserved. That turned out
//! to be a hole rather than a clean boundary: `local-rag-proxy` starts the
//! daemon with `Stdio::null()` on stderr (`connect.rs:76-82`), which is the
//! *normal* MCP setup, so every line X-004 emitted was discarded exactly when
//! someone would want to read it back. The file sink is therefore always on —
//! no config key gates it (an explicit owner decision): verbosity is already
//! `log_level`/`RUST_LOG`'s job, and volume is bounded by rotation.
//!
//! A file that cannot be opened (permissions, a full disk) is **not** fatal:
//! the daemon logs a warning to stderr and runs with that sink alone.
//!
//! Default verbosity comes from `config.daemon.log_level` (spec 02 §3.1) — a
//! `[SPEC]` field that has existed since T02-05 and been editable from the
//! TUI since T18-07, but until this task nothing ever read it.
//! `RUST_LOG`, when set and non-empty, overrides it completely (the standard
//! `tracing-subscriber` convention). Recalled memory and indexed repository
//! content are untrusted data (CLAUDE.md); no event this module's call sites
//! emit carries a request or response payload, only metadata (method, tool
//! name, session, byte counts, duration, status).

use std::path::Path;

use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// The default filter directive applied when `RUST_LOG` is unset: quiets two
/// verbose native-library emitters (`ort`'s session/graph diagnostics,
/// `llama_cpp_2`'s token-level tracing) that would otherwise drown out the
/// daemon's own request-level events at `debug`.
const QUIET_THIRD_PARTY_DIRECTIVE: &str = "ort=warn,llama_cpp_2=warn";

/// Rotated log files kept under `logs_dir` before the oldest is deleted.
///
/// Seven, matching the one week `X-001` already fixed as the retention horizon
/// for generations (`T = 168h`) — one number for "how far back this store
/// remembers anything" is easier to reason about than two. Deliberately a
/// constant rather than a config key: the owner's decision for X-007 was that
/// the file sink has no configuration surface at all, so spec 02 §3.1's pinned
/// `SPEC_CONFIG_TOML` (and its `default_matches_spec_toml` test) stay untouched.
const MAX_LOG_FILES: usize = 7;

/// Name parts of the rotated log files. `tracing-appender` puts the rotation
/// date *between* them, so the files read `daemon.2026-08-17.log`.
const LOG_FILENAME_PREFIX: &str = "daemon";
const LOG_FILENAME_SUFFIX: &str = "log";

/// The outcome of resolving a filter: the directive string to install, plus
/// an optional warning to surface (via `warn!`, once the subscriber is up —
/// this function itself never logs, so it stays pure and unit-testable).
pub struct ResolvedFilter {
    /// The `EnvFilter`-compatible directive string.
    pub directive: String,
    /// Set when either input was invalid and a fallback was substituted.
    pub warning: Option<String>,
}

/// Resolve the log filter from `config.daemon.log_level` and an optional
/// `RUST_LOG` value, applying `RUST_LOG` > `log_level` > `"info"` (spec 02
/// §3.1's as-built priority).
///
/// Pure: takes `rust_log` as a parameter rather than reading `std::env`
/// itself, so the whole priority/fallback matrix is unit-testable without
/// process-global environment state. A present-but-empty `RUST_LOG` is
/// treated as unset (the same "empty env var means unset" rule spec 02 §2.1
/// already applies elsewhere in this workspace). An invalid directive on
/// either side falls back to `"info"` and reports why — this never panics
/// and never silently degrades without a trace (spec 02 §6's "nothing
/// degrades silently").
pub fn resolve_filter(config_level: &str, rust_log: Option<&str>) -> ResolvedFilter {
    if let Some(rust_log) = rust_log.filter(|s| !s.is_empty()) {
        return match EnvFilter::try_new(rust_log) {
            Ok(_) => ResolvedFilter {
                directive: rust_log.to_string(),
                warning: None,
            },
            Err(e) => fallback_to_info(format!(
                "RUST_LOG={rust_log:?} is not a valid filter directive ({e}); using \"info\""
            )),
        };
    }

    match EnvFilter::try_new(config_level) {
        Ok(_) => ResolvedFilter {
            directive: format!("{config_level},{QUIET_THIRD_PARTY_DIRECTIVE}"),
            warning: None,
        },
        Err(e) => fallback_to_info(format!(
            "config.daemon.log_level={config_level:?} is not a valid filter directive ({e}); \
             using \"info\""
        )),
    }
}

fn fallback_to_info(warning: String) -> ResolvedFilter {
    ResolvedFilter {
        directive: format!("info,{QUIET_THIRD_PARTY_DIRECTIVE}"),
        warning: Some(warning),
    }
}

/// Install the process-wide `tracing` subscriber: plain (non-ANSI) lines to
/// stderr **and** to a daily-rotated file under `logs_dir`, both filtered by
/// the single directive [`resolve_filter`] produced.
///
/// `logs_dir` is created here if missing, via the same private-`0700`
/// [`ensure_dir`](local_rag_core::paths::ensure_dir) every other store
/// directory goes through (spec 12 §1: the store is not world-readable).
/// It cannot be left to `StoreLayout::ensure`, which runs later inside
/// `DaemonHandle::start` (`daemon/lifecycle.rs:307`) — on a brand-new store
/// that ordering would silently cost the first run its file log.
///
/// Idempotent by construction: `try_init`'s "already installed" error is
/// swallowed rather than panicking, since a test binary that spawns
/// `local-rag serve` as a real subprocess never shares a process with this
/// call, but a unit test calling this twice in one process must not abort
/// the test run. `with_ansi(false)` is deliberate, not merely conservative:
/// escape sequences would corrupt the byte-exact substring assertions
/// `tests/serve_logging.rs` makes against captured output, and add nothing
/// once the destination is a pipe or a log file rather than a live tty.
///
/// The appender is used **synchronously**, without
/// `tracing_appender::non_blocking`: that wrapper hands writing to a worker
/// thread and drops buffered lines unless its `WorkerGuard` outlives every
/// event, which would mean threading a guard from here through the whole of
/// `serve()` — and losing exactly the last lines before a crash, the ones most
/// worth having. One line per event to a local file needs no such machinery.
pub fn init(config_level: &str, logs_dir: &Path) {
    let resolved = resolve_filter(config_level, std::env::var("RUST_LOG").ok().as_deref());
    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_ansi(false);

    // A missing/unwritable log directory or file must not stop the daemon: fall
    // back to the stderr layer alone and say so (spec 02 §6 — nothing degrades
    // silently).
    let file_appender = local_rag_core::paths::ensure_dir(logs_dir)
        .map_err(|e| e.to_string())
        .and_then(|()| {
            RollingFileAppender::builder()
                .rotation(Rotation::DAILY)
                .filename_prefix(LOG_FILENAME_PREFIX)
                .filename_suffix(LOG_FILENAME_SUFFIX)
                .max_log_files(MAX_LOG_FILES)
                .build(logs_dir)
                .map_err(|e| e.to_string())
        });

    let file_warning = match file_appender {
        Ok(appender) => {
            let file_layer = tracing_subscriber::fmt::layer()
                .with_writer(appender)
                .with_ansi(false);
            let _ = tracing_subscriber::registry()
                .with(EnvFilter::new(&resolved.directive))
                .with(stderr_layer)
                .with(file_layer)
                .try_init();
            None
        }
        Err(e) => {
            let _ = tracing_subscriber::registry()
                .with(EnvFilter::new(&resolved.directive))
                .with(stderr_layer)
                .try_init();
            Some(format!(
                "could not open the log file in {}: {e}; logging to stderr only",
                logs_dir.display()
            ))
        }
    };

    if let Some(warning) = resolved.warning {
        tracing::warn!("{warning}");
    }
    if let Some(warning) = file_warning {
        tracing::warn!("{warning}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_log_unset_uses_config_level_plus_quiet_directives() {
        let r = resolve_filter("debug", None);
        assert_eq!(r.directive, "debug,ort=warn,llama_cpp_2=warn");
        assert!(r.warning.is_none());
    }

    #[test]
    fn rust_log_empty_is_treated_as_unset() {
        let r = resolve_filter("info", Some(""));
        assert_eq!(r.directive, "info,ort=warn,llama_cpp_2=warn");
        assert!(r.warning.is_none());
    }

    #[test]
    fn rust_log_overrides_completely_no_quiet_directives_appended() {
        let r = resolve_filter("debug", Some("local_rag=trace,tokio=warn"));
        assert_eq!(r.directive, "local_rag=trace,tokio=warn");
        assert!(r.warning.is_none());
    }

    #[test]
    fn invalid_config_level_falls_back_to_info_with_a_warning() {
        // A bare identifier (e.g. "not-a-level") is actually valid `EnvFilter`
        // syntax — it parses as a target-only directive with no level
        // restriction, matching nothing in practice but never erroring. A
        // genuinely malformed directive needs a `target=` prefix with a
        // level keyword `EnvFilter` cannot parse.
        let r = resolve_filter("local_rag=not-a-level", None);
        assert_eq!(r.directive, "info,ort=warn,llama_cpp_2=warn");
        assert!(r.warning.is_some());
        assert!(r.warning.unwrap().contains("local_rag=not-a-level"));
    }

    #[test]
    fn invalid_rust_log_falls_back_to_info_with_a_warning_ignoring_config_level() {
        let r = resolve_filter("debug", Some("=="));
        assert_eq!(r.directive, "info,ort=warn,llama_cpp_2=warn");
        assert!(r.warning.is_some());
        assert!(r.warning.unwrap().contains("=="));
    }

    #[test]
    fn a_valid_rust_log_directive_is_used_verbatim() {
        let r = resolve_filter("info", Some("warn"));
        assert_eq!(r.directive, "warn");
        assert!(r.warning.is_none());
    }
}
