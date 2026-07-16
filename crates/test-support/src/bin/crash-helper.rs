//! Tiny helper process used to exercise the subprocess/artifact-bundle harness.
//!
//! Behaviour is driven by the first argument, so a test can request a
//! deterministic, cross-platform outcome:
//!
//! - `abort` — abort the process (non-zero, signal-like termination).
//! - `exit2` — exit with code 2.
//! - anything else (incl. no argument) — print a marker and exit 0.
//!
//! It also echoes a line to stdout and stderr first, so the persisted bundle
//! has non-empty capture files to assert on.

use std::process::ExitCode;

fn main() -> ExitCode {
    println!("crash-helper: stdout marker");
    eprintln!("crash-helper: stderr marker");

    match std::env::args().nth(1).as_deref() {
        Some("abort") => std::process::abort(),
        Some("exit2") => ExitCode::from(2),
        _ => ExitCode::SUCCESS,
    }
}
