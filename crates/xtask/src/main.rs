//! Developer task runner, invoked via the `cargo xtask` alias
//! (see `.cargo/config.toml`).
//!
//! `cargo xtask ci` runs the full quality gate documented in `CONTRIBUTING.md`,
//! failing on the first step that fails.

use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        Some("ci") => run_ci(),
        other => {
            eprintln!("usage: cargo xtask ci");
            eprintln!("unknown task: {}", other.unwrap_or("<none>"));
            ExitCode::from(2)
        }
    }
}

/// The single full-check pipeline. Kept in sync with `CONTRIBUTING.md` and
/// asserted against the CI workflow by `tests/ci_config.rs`.
fn run_ci() -> ExitCode {
    let steps: &[&[&str]] = &[
        &["fmt", "--all", "--check"],
        &[
            "clippy",
            "--workspace",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
        &["test", "--workspace"],
        &["doc", "--workspace", "--no-deps"],
    ];

    let cargo = env!("CARGO");
    for args in steps {
        eprintln!("+ cargo {}", args.join(" "));
        let status = Command::new(cargo)
            .args(*args)
            .status()
            .expect("spawn cargo");
        if !status.success() {
            eprintln!("xtask ci: step failed: cargo {}", args.join(" "));
            return ExitCode::FAILURE;
        }
    }
    eprintln!("xtask ci: all checks passed");
    ExitCode::SUCCESS
}
