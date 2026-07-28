//! Developer task runner, invoked via the `cargo xtask` alias
//! (see `.cargo/config.toml`).
//!
//! `cargo xtask ci` runs the full quality gate documented in `CONTRIBUTING.md`,
//! failing on the first step that fails.
//!
//! `cargo xtask bench` runs the 49-query search benchmark (spec 14 §7, T12-05).
//! It is **not** part of `ci`: it needs model weights and a corpus checkout that
//! the repository does not ship.
//!
//! `cargo xtask memory-bench` runs the memory-router benchmark (spec 08 §7,
//! T14-07). Also **not** part of `ci`: it needs the installed GGUF weights
//! and the `llama-cpp-2` toolchain (ADR-0006).

mod bench;
mod memory_bench;

use std::path::PathBuf;
use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        Some("ci") => run_ci(),
        Some("bench") => run_bench(),
        Some("memory-bench") => run_memory_bench(),
        other => {
            eprintln!("usage: cargo xtask <ci|bench|memory-bench>");
            eprintln!("unknown task: {}", other.unwrap_or("<none>"));
            ExitCode::from(2)
        }
    }
}

/// `cargo xtask bench --corpus <dir> [--out <path>] [--mode hybrid|lexical|code]`
/// `[--dense-kind code_raw|code_context] [--lexical-weight w[,w…]]`
fn run_bench() -> ExitCode {
    let mut corpus_dir: Option<PathBuf> = None;
    let mut out: Option<PathBuf> = None;
    let mut mode = local_rag_protocol::SearchMode::Hybrid;
    let mut subdir: Option<String> = None;
    let mut dense_kind = local_rag_store::RepresentationKind::CodeRaw;
    let mut lexical_weights: Vec<f64> = Vec::new();

    let mut args = std::env::args().skip(2);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--corpus" => corpus_dir = args.next().map(PathBuf::from),
            "--out" => out = args.next().map(PathBuf::from),
            "--subdir" => subdir = args.next(),
            "--dense-kind" => {
                let Some(raw) = args.next() else {
                    eprintln!("--dense-kind needs a value");
                    return ExitCode::from(2);
                };
                match local_rag_store::RepresentationKind::from_db(&raw) {
                    Some(local_rag_store::RepresentationKind::CodeRaw) => {
                        dense_kind = local_rag_store::RepresentationKind::CodeRaw;
                    }
                    Some(local_rag_store::RepresentationKind::CodeContext) => {
                        dense_kind = local_rag_store::RepresentationKind::CodeContext;
                    }
                    // The other two kinds exist in the schema but have no code
                    // subjects to embed, so a run over them would measure an
                    // empty shard rather than fail loudly.
                    _ => {
                        eprintln!("--dense-kind must be code_raw or code_context, got {raw:?}");
                        return ExitCode::from(2);
                    }
                }
            }
            "--lexical-weight" => {
                let Some(raw) = args.next() else {
                    eprintln!("--lexical-weight needs a value (comma-separated for a sweep)");
                    return ExitCode::from(2);
                };
                for piece in raw.split(',') {
                    match piece.trim().parse::<f64>() {
                        Ok(w) if w >= 0.0 && w.is_finite() => lexical_weights.push(w),
                        _ => {
                            eprintln!("--lexical-weight takes finite weights >= 0, got {piece:?}");
                            return ExitCode::from(2);
                        }
                    }
                }
            }
            "--mode" => {
                let Some(raw) = args.next() else {
                    eprintln!("--mode needs a value");
                    return ExitCode::from(2);
                };
                match local_rag_protocol::SearchMode::from_wire(&raw) {
                    Some(m) => mode = m,
                    None => {
                        eprintln!("unknown mode {raw:?}");
                        return ExitCode::from(2);
                    }
                }
            }
            other => {
                eprintln!("unknown argument {other:?}");
                return ExitCode::from(2);
            }
        }
    }

    let Some(corpus_dir) = corpus_dir else {
        eprintln!(
            "usage: cargo xtask bench --corpus <dir> [--subdir <rel>] [--out <path>] \
             [--mode <mode>] [--dense-kind code_raw|code_context] \
             [--lexical-weight w[,w...]]"
        );
        return ExitCode::from(2);
    };
    let out = out.unwrap_or_else(|| bench::baseline_dir().join("run-v2.json"));

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("bench: tokio runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let sweep = !lexical_weights.is_empty() && lexical_weights.len() > 1;
    let reports = match runtime.block_on(bench::run::run(&bench::run::Options {
        corpus_dir,
        subdir,
        mode,
        dense_kind,
        lexical_weights,
    })) {
        Ok(reports) => reports,
        Err(e) => {
            eprintln!("bench: {e}");
            return ExitCode::FAILURE;
        }
    };

    // A sweep writes one artifact per point, suffixed by the weight, so no run
    // silently overwrites another's numbers.
    let mut all_passed = true;
    for report in &reports {
        let out = if sweep {
            let weight = report.provenance.fusion_lexical_weight.unwrap_or(1.0);
            let stem = out
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "run-v2".to_string());
            out.with_file_name(format!("{stem}-lw{:.3}.json", weight))
        } else {
            out.clone()
        };
        let json = serde_json::to_string_pretty(report).expect("report serializes");
        if let Err(e) = std::fs::write(&out, json + "\n") {
            eprintln!("bench: writing {}: {e}", out.display());
            return ExitCode::FAILURE;
        }
        let md_path = out.with_extension("report.md");
        if let Err(e) = std::fs::write(&md_path, report.to_markdown()) {
            eprintln!("bench: writing {}: {e}", md_path.display());
            return ExitCode::FAILURE;
        }
        eprintln!("bench: wrote {} and {}", out.display(), md_path.display());

        eprintln!(
            "[bench] lexical_weight={:.4} Hit@1={:.4} Hit@3={:.4} Hit@5={:.4} MRR={:.4} \
             (v1: {:.4}/{:.4}/{:.4}/{:.4})",
            report.provenance.fusion_lexical_weight.unwrap_or(1.0),
            report.metrics.hit_at_1,
            report.metrics.hit_at_3,
            report.metrics.hit_at_5,
            report.metrics.mrr,
            report.baseline.hit_at_1,
            report.baseline.hit_at_3,
            report.baseline.hit_at_5,
            report.baseline.mrr,
        );

        // The gate only runs once thresholds exist; before that the run *is* the
        // evidence they are derived from (O2: collect metrics, never invent them).
        match bench::gate::Thresholds::load(&bench::thresholds_path()) {
            Ok(thresholds) => {
                let outcome = bench::gate::evaluate(report, &thresholds);
                if outcome.passed() {
                    eprintln!("[bench] gate: PASS");
                } else {
                    for violation in &outcome.violations {
                        eprintln!("[bench] gate: {violation}");
                    }
                    all_passed = false;
                }
            }
            Err(e) => {
                eprintln!("[bench] gate: skipped (no thresholds yet: {e})");
            }
        }
    }

    if all_passed {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// `cargo xtask memory-bench [--corpus <path>] [--model <catalog-id>] [--out <path>]`
fn run_memory_bench() -> ExitCode {
    let mut case_index_path: Option<PathBuf> = None;
    let mut model_id: Option<String> = None;
    let mut out: Option<PathBuf> = None;

    let mut args = std::env::args().skip(2);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--corpus" => case_index_path = args.next().map(PathBuf::from),
            "--model" => model_id = args.next(),
            "--out" => out = args.next().map(PathBuf::from),
            other => {
                eprintln!("unknown argument {other:?}");
                return ExitCode::from(2);
            }
        }
    }
    let case_index_path = case_index_path.unwrap_or_else(memory_bench::case_index_path);
    let out = out.unwrap_or_else(|| memory_bench::baseline_dir().join("run.json"));

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("memory-bench: tokio runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let report = match runtime.block_on(memory_bench::run::run(&memory_bench::run::Options {
        case_index_path,
        model_id,
    })) {
        Ok(report) => report,
        Err(e) => {
            eprintln!("memory-bench: {e}");
            return ExitCode::FAILURE;
        }
    };

    let json = serde_json::to_string_pretty(&report).expect("report serializes");
    if let Err(e) = std::fs::write(&out, json + "\n") {
        eprintln!("memory-bench: writing {}: {e}", out.display());
        return ExitCode::FAILURE;
    }
    let md_path = out.with_extension("report.md");
    if let Err(e) = std::fs::write(&md_path, report.to_markdown()) {
        eprintln!("memory-bench: writing {}: {e}", md_path.display());
        return ExitCode::FAILURE;
    }
    eprintln!(
        "memory-bench: wrote {} and {}",
        out.display(),
        md_path.display()
    );
    eprintln!(
        "[memory-bench] precision={:.4} recall={:.4} f1={:.4} exact_match_rate={:.4}",
        report.metrics.precision,
        report.metrics.recall,
        report.metrics.f1,
        report.metrics.exact_match_rate,
    );

    // The gate only runs once thresholds exist; before that the run *is* the
    // evidence they are derived from (O2: collect metrics, never invent them).
    match memory_bench::gate::Thresholds::load(&memory_bench::thresholds_path()) {
        Ok(thresholds) => {
            let outcome = memory_bench::gate::evaluate(&report, &thresholds);
            if outcome.passed() {
                eprintln!("[memory-bench] gate: PASS");
                ExitCode::SUCCESS
            } else {
                for violation in &outcome.violations {
                    eprintln!("[memory-bench] gate: {violation}");
                }
                ExitCode::FAILURE
            }
        }
        Err(e) => {
            eprintln!("[memory-bench] gate: skipped (no thresholds yet: {e})");
            ExitCode::SUCCESS
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
        // spec 13 §3, plus T13-06's S3/S4/S5 spool importer kill seams, spec 07 §7),
        // `local-rag-hook` (T13-06's S1/S2 real-subprocess hook kill seam, spec 07
        // §7), `local-rag-index` (generation-builder phase seams, spec 04
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
            "local-rag-hook",
            "--all-targets",
            "--features",
            "failpoints",
            "--",
            "-D",
            "warnings",
        ],
        &["test", "-p", "local-rag-hook", "--features", "failpoints"],
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
        // `local-rag-models` (T11-06): the installer's
        // `models.install.between_files` crash point, exercised by
        // `install_faults.rs`'s interrupt-and-resume tests (spec 10 §5's
        // "atomic download … offline operation afterwards").
        &[
            "clippy",
            "-p",
            "local-rag-models",
            "--all-targets",
            "--features",
            "failpoints",
            "--",
            "-D",
            "warnings",
        ],
        &["test", "-p", "local-rag-models", "--features", "failpoints"],
        // `local-rag-generate` (T14-07): the identical
        // `generate.install.between_files` crash point for the local
        // generator's GGUF installer (spec 10 §5's atomic-download policy,
        // applied to generation the same way `local-rag-models` applies it to
        // embeddings; ADR-0006).
        &[
            "clippy",
            "-p",
            "local-rag-generate",
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
            "local-rag-generate",
            "--features",
            "failpoints",
        ],
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
