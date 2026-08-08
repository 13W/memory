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
//! Sink is always **stderr**: the daemon's IPC is a Unix domain socket (spec
//! 02 §2.1), never stdio, so stderr is free for a human running `local-rag
//! serve` directly in a terminal. A file-based log
//! (`StoreLayout::logs_dir`) stays reserved and unfilled, the same boundary
//! `T18-08`'s own card already drew for its in-memory ring buffer.
//!
//! Default verbosity comes from `config.daemon.log_level` (spec 02 §3.1) — a
//! `[SPEC]` field that has existed since T02-05 and been editable from the
//! TUI since T18-07, but until this task nothing ever read it.
//! `RUST_LOG`, when set and non-empty, overrides it completely (the standard
//! `tracing-subscriber` convention). Recalled memory and indexed repository
//! content are untrusted data (CLAUDE.md); no event this module's call sites
//! emit carries a request or response payload, only metadata (method, tool
//! name, session, byte counts, duration, status).

use tracing_subscriber::EnvFilter;

/// The default filter directive applied when `RUST_LOG` is unset: quiets two
/// verbose native-library emitters (`ort`'s session/graph diagnostics,
/// `llama_cpp_2`'s token-level tracing) that would otherwise drown out the
/// daemon's own request-level events at `debug`.
const QUIET_THIRD_PARTY_DIRECTIVE: &str = "ort=warn,llama_cpp_2=warn";

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
/// stderr, filtered per [`resolve_filter`].
///
/// Idempotent by construction: `try_init`'s "already installed" error is
/// swallowed rather than panicking, since a test binary that spawns
/// `local-rag serve` as a real subprocess never shares a process with this
/// call, but a unit test calling this twice in one process must not abort
/// the test run. `with_ansi(false)` is deliberate, not merely conservative:
/// escape sequences would corrupt the byte-exact substring assertions
/// `tests/serve_logging.rs` makes against captured stderr, and add nothing
/// once stderr is redirected to a pipe or a log file rather than a live tty.
pub fn init(config_level: &str) {
    let resolved = resolve_filter(config_level, std::env::var("RUST_LOG").ok().as_deref());
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(&resolved.directive))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
    if let Some(warning) = resolved.warning {
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
