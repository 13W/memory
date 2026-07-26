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
        // spec 13 §3), `local-rag-index` (generation-builder phase seams, spec 04
        // §1 build → failed edge, T05-05), `local-rag-projection` (fake-shard
        // fault matrix seams + inspect/corrupt controls, spec 05 §10, T07-01), and
        // `local-rag-search` (forwards to `local-rag-projection/failpoints` so
        // `projection.switch.before_commit` fires under a concurrent search load,
        // T09-04's `switch_failpoint_load.rs`).
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
        &[
            "clippy",
            "-p",
            "local-rag-projection",
            "--all-targets",
            "--features",
            "failpoints",
            "--",
            "-D",
            "warnings",
        ],
        &[
            "test",
            "-p",
            "local-rag-projection",
            "--features",
            "failpoints",
        ],
        &[
            "clippy",
            "-p",
            "local-rag-search",
            "--all-targets",
            "--features",
            "failpoints",
            "--",
            "-D",
            "warnings",
        ],
        &["test", "-p", "local-rag-search", "--features", "failpoints"],
        // `local-rag-embed` (T11-04): the backfill worker's
        // `embed.backfill.between_batches` crash point, exercised by
        // `backfill_resume.rs`'s kill-and-resume tests (spec 10 §4 step 2).
        &[
            "clippy",
            "-p",
            "local-rag-embed",
            "--all-targets",
            "--features",
            "failpoints",
            "--",
            "-D",
            "warnings",
        ],
        &["test", "-p", "local-rag-embed", "--features", "failpoints"],
        // The dense-backend spike (T10-01) is a SEPARATE workspace with its own
        // Cargo.lock (`spike/`, `exclude`d from the root — CONTRIBUTING.md §
        // Workspace layout), so `test --workspace` above never reaches it. `fmt`
        // stays blanket across the whole spike workspace (formatting doesn't
        // require successful compilation, so there is no isolation gap to close).
        &[
            "fmt",
            "--manifest-path",
            "spike/Cargo.toml",
            "--all",
            "--check",
        ],
        // `clippy`/`test` are scoped per spike workspace member (T10-04), not
        // blanket: `local-rag-spike-qdrant-edge` republishes the actual Qdrant
        // server's WAL/segment engine (~80 transitive dependencies) in its own
        // crate specifically so a build/platform problem there can never make
        // `local-rag-spike-harness` (fake/brute-force/usearch, all already
        // passing) uncompilable — but a single blanket `test --manifest-path`
        // step would still report one combined pass/fail bit either way, so it
        // is split here into two step-pairs (harness first) to preserve each
        // candidate's own signal, mirroring the existing `--features
        // failpoints` per-crate steps above for `store`/`index`/`projection`/
        // `search`.
        &[
            "clippy",
            "--manifest-path",
            "spike/Cargo.toml",
            "-p",
            "local-rag-spike-harness",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
        &[
            "test",
            "--manifest-path",
            "spike/Cargo.toml",
            "-p",
            "local-rag-spike-harness",
        ],
        &[
            "clippy",
            "--manifest-path",
            "spike/Cargo.toml",
            "-p",
            "local-rag-spike-qdrant-edge",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
        &[
            "test",
            "--manifest-path",
            "spike/Cargo.toml",
            "-p",
            "local-rag-spike-qdrant-edge",
        ],
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
