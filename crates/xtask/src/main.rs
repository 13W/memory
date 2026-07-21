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
        // Feature-gated crash/error seams: the default steps above build with
        // `failpoints` OFF, so lint and run those code paths explicitly. Scoped to
        // each crate defining the feature — `local-rag-store` (migration crash seams,
        // spec 13 §3) and `local-rag-index` (generation-builder phase seams, spec 04
        // §1 build → failed edge, T05-05).
        &[
            "clippy",
            "-p",
            "local-rag-store",
            "--all-targets",
            "--features",
            "failpoints",
            "--",
            "-D",
            "warnings",
        ],
        &["test", "-p", "local-rag-store", "--features", "failpoints"],
        &[
            "clippy",
            "-p",
            "local-rag-index",
            "--all-targets",
            "--features",
            "failpoints",
            "--",
            "-D",
            "warnings",
        ],
        &["test", "-p", "local-rag-index", "--features", "failpoints"],
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
