//! Developer task runner, invoked via the `cargo xtask` alias
//! (see `.cargo/config.toml`).
//!
//! `cargo xtask ci` runs the full quality gate documented in `CONTRIBUTING.md`.
//! It splits the gate into independent jobs (root workspace lint/test/doc, one
//! `--features failpoints` lane per crate that defines the feature, and the
//! separate `spike/` workspace) and runs them concurrently across a bounded
//! worker pool, letting every job run to completion so a single run reports
//! every failure instead of stopping at the first one.
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

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::sync::Mutex;
use std::time::{Duration, Instant};

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

/// One `cargo` invocation's argv (without the leading `cargo`).
type Argv = &'static [&'static str];

/// An independent unit of CI work: an ordered list of `cargo` invocations
/// that must run sequentially (build before test, sharing one lint/test
/// signal) but that has no ordering dependency on any other job. Jobs run
/// concurrently against a bounded worker pool — see `run_jobs`.
struct Job<T> {
    name: &'static str,
    payload: T,
}

enum JobOutcome {
    Passed,
    Failed(String),
}

struct JobResult {
    name: &'static str,
    outcome: JobOutcome,
    elapsed: Duration,
}

/// Runs `jobs` against a pool of `workers` threads (clamped to `[1,
/// jobs.len()]`), pulling from a shared FIFO queue so a worker that finishes
/// early immediately picks up the next queued job instead of idling until a
/// synchronized batch boundary. Every job runs to completion regardless of
/// sibling failures — callers get one result per job, not an early abort.
fn run_jobs<T: Send>(
    jobs: Vec<Job<T>>,
    workers: usize,
    run_one: impl Fn(&T) -> JobOutcome + Sync,
) -> Vec<JobResult> {
    let workers = workers.clamp(1, jobs.len().max(1));
    let queue = Mutex::new(VecDeque::from(jobs));
    let results = Mutex::new(Vec::new());
    let print_lock = Mutex::new(());

    std::thread::scope(|scope| {
        for _ in 0..workers {
            let queue = &queue;
            let run_one = &run_one;
            let results = &results;
            let print_lock = &print_lock;
            scope.spawn(move || {
                loop {
                    let job = queue.lock().unwrap().pop_front();
                    let Some(job) = job else { break };
                    let start = Instant::now();
                    let outcome = run_one(&job.payload);
                    let elapsed = start.elapsed();
                    {
                        let _guard = print_lock.lock().unwrap();
                        let mark = if matches!(outcome, JobOutcome::Passed) {
                            "ok  "
                        } else {
                            "FAIL"
                        };
                        eprintln!(
                            "[ci] {mark} {:<24} {:>6.1}s",
                            job.name,
                            elapsed.as_secs_f64()
                        );
                    }
                    results.lock().unwrap().push(JobResult {
                        name: job.name,
                        outcome,
                        elapsed,
                    });
                }
            });
        }
    });

    results.into_inner().unwrap()
}

/// Runs every step of one job's payload in order via `cargo`, capturing
/// output instead of inheriting the terminal (concurrent jobs would
/// otherwise interleave their stdout/stderr into an unreadable mix). Every
/// step runs regardless of earlier steps in the same job failing — a `-p X`
/// failure earlier in a chained job (see `ROOT_TEST_CHAIN`) must not hide a
/// `-p Y` result later in the same job.
fn run_cargo_job(steps: &&'static [Argv]) -> JobOutcome {
    let cargo = env!("CARGO");
    let mut failures = String::new();
    for args in *steps {
        let output = Command::new(cargo)
            .args(*args)
            .output()
            .expect("spawn cargo");
        if !output.status.success() {
            failures.push_str(&format!("+ cargo {}\n", args.join(" ")));
            failures.push_str(&String::from_utf8_lossy(&output.stdout));
            failures.push_str(&String::from_utf8_lossy(&output.stderr));
            failures.push('\n');
        }
    }
    if failures.is_empty() {
        JobOutcome::Passed
    } else {
        JobOutcome::Failed(failures)
    }
}

fn parse_worker_override(raw: Option<&str>) -> Option<usize> {
    raw.and_then(|v| v.parse::<usize>().ok()).filter(|n| *n > 0)
}

fn resolve_worker_count() -> usize {
    parse_worker_override(std::env::var("XTASK_CI_JOBS").ok().as_deref()).unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
    })
}

fn ensure_nextest_available() -> Result<(), String> {
    let cargo = env!("CARGO");
    match Command::new(cargo).args(["nextest", "--version"]).output() {
        Ok(output) if output.status.success() => Ok(()),
        _ => Err("cargo-nextest not found on PATH. Install it with \
             `cargo install cargo-nextest --locked` (see CONTRIBUTING.md), \
             then re-run `cargo xtask ci`."
            .to_string()),
    }
}

// `cargo`/`cargo-nextest` serialize concurrent invocations against the SAME
// workspace: two `cargo nextest run` processes started at once against this
// root workspace visibly block each other on cargo's own build-directory
// lock (confirmed empirically — one waits for the other's "Finished ... in
// Ns" before making progress), so running one `nextest run` per crate as
// separate concurrent jobs would silently re-serialize the exact cost this
// pipeline most needs to shrink (T14-05's ~100+ sequential test-binary
// spawns). `clippy`/`fmt`/`doc` do not show this — they stay independent,
// concurrent jobs. Every `nextest run` against the root workspace is
// therefore chained into ONE job (`ROOT_TEST_CHAIN`) that runs to
// completion regardless of an earlier crate's failure (see `run_cargo_job`),
// so one run still reports every crate's result. `--features failpoints`
// builds default features OFF, so lint and run those code paths explicitly.
// Scoped to each crate defining the feature — `local-rag-store` (migration
// crash seams, spec 13 §3, plus T13-06's S3/S4/S5 spool importer kill seams,
// spec 07 §7), `local-rag-hook` (T13-06's S1/S2 real-subprocess hook kill
// seam, spec 07 §7), `local-rag-index` (generation-builder phase seams, spec
// 04 §1 build → failed edge, T05-05), `local-rag-projection` (fake-shard
// fault matrix seams + inspect/corrupt controls, spec 05 §10, T07-01),
// `local-rag-search` (forwards to `local-rag-projection/failpoints` so
// `projection.switch.before_commit` fires under a concurrent search load,
// T09-04's `switch_failpoint_load.rs`), and `local-rag` (T15-01: the
// `LOCAL_RAG_TEST_RESUME_DELAY_MS` startup-resume pause knob
// `tests/serve_subprocess.rs`'s SIGTERM-at-safe-points scenario needs, spec
// 02 §4.3). Each `--features failpoints` lane also runs `test --doc`:
// `cargo nextest` never runs doctests, so that coverage would otherwise
// silently disappear.
const ROOT_FMT: &[Argv] = &[&["fmt", "--all", "--check"]];
const ROOT_CLIPPY: &[Argv] = &[&[
    "clippy",
    "--workspace",
    "--all-targets",
    "--",
    "-D",
    "warnings",
]];
const ROOT_DOC: &[Argv] = &[&["doc", "--workspace", "--no-deps"]];

const STORE_CLIPPY_FAILPOINTS: &[Argv] = &[&[
    "clippy",
    "-p",
    "local-rag-store",
    "--all-targets",
    "--features",
    "failpoints",
    "--",
    "-D",
    "warnings",
]];
const HOOK_CLIPPY_FAILPOINTS: &[Argv] = &[&[
    "clippy",
    "-p",
    "local-rag-hook",
    "--all-targets",
    "--features",
    "failpoints",
    "--",
    "-D",
    "warnings",
]];
const INDEX_CLIPPY_FAILPOINTS: &[Argv] = &[&[
    "clippy",
    "-p",
    "local-rag-index",
    "--all-targets",
    "--features",
    "failpoints",
    "--",
    "-D",
    "warnings",
]];
const PROJECTION_CLIPPY_FAILPOINTS: &[Argv] = &[&[
    "clippy",
    "-p",
    "local-rag-projection",
    "--all-targets",
    "--features",
    "failpoints",
    "--",
    "-D",
    "warnings",
]];
const SEARCH_CLIPPY_FAILPOINTS: &[Argv] = &[&[
    "clippy",
    "-p",
    "local-rag-search",
    "--all-targets",
    "--features",
    "failpoints",
    "--",
    "-D",
    "warnings",
]];
// `local-rag-embed` (T11-04): the backfill worker's
// `embed.backfill.between_batches` crash point, exercised by
// `backfill_resume.rs`'s kill-and-resume tests (spec 10 §4 step 2).
const EMBED_CLIPPY_FAILPOINTS: &[Argv] = &[&[
    "clippy",
    "-p",
    "local-rag-embed",
    "--all-targets",
    "--features",
    "failpoints",
    "--",
    "-D",
    "warnings",
]];
// `local-rag-models` (T11-06): the installer's
// `models.install.between_files` crash point, exercised by
// `install_faults.rs`'s interrupt-and-resume tests (spec 10 §5's
// "atomic download … offline operation afterwards").
const MODELS_CLIPPY_FAILPOINTS: &[Argv] = &[&[
    "clippy",
    "-p",
    "local-rag-models",
    "--all-targets",
    "--features",
    "failpoints",
    "--",
    "-D",
    "warnings",
]];
// `local-rag-generate` (T14-07): the identical
// `generate.install.between_files` crash point for the local
// generator's GGUF installer (spec 10 §5's atomic-download policy,
// applied to generation the same way `local-rag-models` applies it to
// embeddings; ADR-0006). Also the single heaviest compile unit in the
// workspace — `llama-cpp-sys-2` vendors and compiles llama.cpp/GGML from
// C/C++ source via `cmake`+`bindgen` on every clean build (CONTRIBUTING.md
// § Approved external dependencies).
const GENERATE_CLIPPY_FAILPOINTS: &[Argv] = &[&[
    "clippy",
    "-p",
    "local-rag-generate",
    "--all-targets",
    "--features",
    "failpoints",
    "--",
    "-D",
    "warnings",
]];
// `local-rag` (T15-01): the daemon's `LOCAL_RAG_TEST_RESUME_DELAY_MS`
// startup-resume pause knob, exercised by `tests/serve_subprocess.rs`'s
// real-subprocess "SIGTERM at safe points" scenario (spec 02 §4.3).
const LOCAL_RAG_CLIPPY_FAILPOINTS: &[Argv] = &[&[
    "clippy",
    "-p",
    "local-rag",
    "--all-targets",
    "--features",
    "failpoints",
    "--",
    "-D",
    "warnings",
]];

const ROOT_TEST_CHAIN: &[Argv] = &[
    &["nextest", "run", "--workspace"],
    &["test", "--doc", "--workspace"],
    &[
        "nextest",
        "run",
        "-p",
        "local-rag-store",
        "--features",
        "failpoints",
    ],
    &[
        "test",
        "--doc",
        "-p",
        "local-rag-store",
        "--features",
        "failpoints",
    ],
    &[
        "nextest",
        "run",
        "-p",
        "local-rag-hook",
        "--features",
        "failpoints",
    ],
    &[
        "test",
        "--doc",
        "-p",
        "local-rag-hook",
        "--features",
        "failpoints",
    ],
    &[
        "nextest",
        "run",
        "-p",
        "local-rag-index",
        "--features",
        "failpoints",
    ],
    &[
        "test",
        "--doc",
        "-p",
        "local-rag-index",
        "--features",
        "failpoints",
    ],
    &[
        "nextest",
        "run",
        "-p",
        "local-rag-projection",
        "--features",
        "failpoints",
    ],
    &[
        "test",
        "--doc",
        "-p",
        "local-rag-projection",
        "--features",
        "failpoints",
    ],
    &[
        "nextest",
        "run",
        "-p",
        "local-rag-search",
        "--features",
        "failpoints",
    ],
    &[
        "test",
        "--doc",
        "-p",
        "local-rag-search",
        "--features",
        "failpoints",
    ],
    &[
        "nextest",
        "run",
        "-p",
        "local-rag-embed",
        "--features",
        "failpoints",
    ],
    &[
        "test",
        "--doc",
        "-p",
        "local-rag-embed",
        "--features",
        "failpoints",
    ],
    &[
        "nextest",
        "run",
        "-p",
        "local-rag-models",
        "--features",
        "failpoints",
    ],
    &[
        "test",
        "--doc",
        "-p",
        "local-rag-models",
        "--features",
        "failpoints",
    ],
    &[
        "nextest",
        "run",
        "-p",
        "local-rag-generate",
        "--features",
        "failpoints",
    ],
    &[
        "test",
        "--doc",
        "-p",
        "local-rag-generate",
        "--features",
        "failpoints",
    ],
    &[
        "nextest",
        "run",
        "-p",
        "local-rag",
        "--features",
        "failpoints",
    ],
    &[
        "test",
        "--doc",
        "-p",
        "local-rag",
        "--features",
        "failpoints",
    ],
];

// The dense-backend spike (T10-01) is a SEPARATE workspace with its own
// Cargo.lock (`spike/`, `exclude`d from the root — CONTRIBUTING.md §
// Workspace layout), so the root `nextest run --workspace` job never reaches
// it, and its own `nextest run` invocations only contend with each other,
// not with the root workspace's lock. `fmt` stays blanket across the whole
// spike workspace (formatting doesn't require successful compilation, so
// there is no isolation gap to close).
const SPIKE_FMT: &[Argv] = &[&[
    "fmt",
    "--manifest-path",
    "spike/Cargo.toml",
    "--all",
    "--check",
]];
// `clippy`/`test` are scoped per spike workspace member (T10-04), not
// blanket: `local-rag-spike-qdrant-edge` republishes the actual Qdrant
// server's WAL/segment engine (~80 transitive dependencies) in its own
// crate specifically so a build/platform problem there can never make
// `local-rag-spike-harness` (fake/brute-force/usearch, all already
// passing) uncompilable — but a single blanket job would still report one
// combined pass/fail bit either way, so `clippy` stays split into two
// independent jobs (mirroring the per-crate `clippy --features failpoints`
// jobs above); the two `nextest run` invocations are chained into one job
// for the same same-workspace-serialization reason `ROOT_TEST_CHAIN` is.
const SPIKE_CLIPPY_HARNESS: &[Argv] = &[&[
    "clippy",
    "--manifest-path",
    "spike/Cargo.toml",
    "-p",
    "local-rag-spike-harness",
    "--all-targets",
    "--",
    "-D",
    "warnings",
]];
const SPIKE_CLIPPY_QDRANT_EDGE: &[Argv] = &[&[
    "clippy",
    "--manifest-path",
    "spike/Cargo.toml",
    "-p",
    "local-rag-spike-qdrant-edge",
    "--all-targets",
    "--",
    "-D",
    "warnings",
]];
const SPIKE_TEST_CHAIN: &[Argv] = &[
    &[
        "nextest",
        "run",
        "--manifest-path",
        "spike/Cargo.toml",
        "-p",
        "local-rag-spike-harness",
    ],
    &[
        "test",
        "--doc",
        "--manifest-path",
        "spike/Cargo.toml",
        "-p",
        "local-rag-spike-harness",
    ],
    &[
        "nextest",
        "run",
        "--manifest-path",
        "spike/Cargo.toml",
        "-p",
        "local-rag-spike-qdrant-edge",
    ],
    &[
        "test",
        "--doc",
        "--manifest-path",
        "spike/Cargo.toml",
        "-p",
        "local-rag-spike-qdrant-edge",
    ],
];

/// The full-check job list, in descending expected-cost order (a Longest
/// Processing Time heuristic that keeps a worker from idling on the queue
/// while the eventual longest job is still waiting behind cheap ones — see
/// PROGRESS.md's T14-05 evidence for the underlying timing data). The two
/// `*_TEST_CHAIN` jobs are each other's only real competition for "longest
/// job": every `nextest run` step against a given workspace is forced
/// sequential by that workspace's own lock (see the comment above
/// `ROOT_TEST_CHAIN`), so they are scheduled first and everything else —
/// all independent `clippy`/`fmt`/`doc` jobs — fills in around them.
fn ci_jobs() -> Vec<Job<&'static [Argv]>> {
    vec![
        Job {
            name: "root:test",
            payload: ROOT_TEST_CHAIN,
        },
        Job {
            name: "spike:test",
            payload: SPIKE_TEST_CHAIN,
        },
        Job {
            name: "root:clippy",
            payload: ROOT_CLIPPY,
        },
        Job {
            name: "store:clippy-failpoints",
            payload: STORE_CLIPPY_FAILPOINTS,
        },
        Job {
            name: "hook:clippy-failpoints",
            payload: HOOK_CLIPPY_FAILPOINTS,
        },
        Job {
            name: "index:clippy-failpoints",
            payload: INDEX_CLIPPY_FAILPOINTS,
        },
        Job {
            name: "projection:clippy-failpoints",
            payload: PROJECTION_CLIPPY_FAILPOINTS,
        },
        Job {
            name: "search:clippy-failpoints",
            payload: SEARCH_CLIPPY_FAILPOINTS,
        },
        Job {
            name: "embed:clippy-failpoints",
            payload: EMBED_CLIPPY_FAILPOINTS,
        },
        Job {
            name: "models:clippy-failpoints",
            payload: MODELS_CLIPPY_FAILPOINTS,
        },
        Job {
            name: "generate:clippy-failpoints",
            payload: GENERATE_CLIPPY_FAILPOINTS,
        },
        Job {
            name: "local-rag:clippy-failpoints",
            payload: LOCAL_RAG_CLIPPY_FAILPOINTS,
        },
        Job {
            name: "spike:clippy-harness",
            payload: SPIKE_CLIPPY_HARNESS,
        },
        Job {
            name: "spike:clippy-qdrant-edge",
            payload: SPIKE_CLIPPY_QDRANT_EDGE,
        },
        Job {
            name: "root:doc",
            payload: ROOT_DOC,
        },
        Job {
            name: "root:fmt",
            payload: ROOT_FMT,
        },
        Job {
            name: "spike:fmt",
            payload: SPIKE_FMT,
        },
    ]
}

/// The single full-check pipeline. Kept in sync with `CONTRIBUTING.md` and
/// asserted against the CI workflow by `tests/ci_config.rs`. Splits into
/// independent jobs (see `ci_jobs`) run concurrently over a bounded worker
/// pool (`run_jobs`); every job runs to completion and every failure is
/// reported, rather than stopping at the first one.
fn run_ci() -> ExitCode {
    if let Err(msg) = ensure_nextest_available() {
        eprintln!("xtask ci: {msg}");
        return ExitCode::FAILURE;
    }

    let jobs = ci_jobs();
    let total = jobs.len();
    let workers = resolve_worker_count();
    eprintln!("xtask ci: running {total} independent jobs, up to {workers} concurrently");

    let results = run_jobs(jobs, workers, run_cargo_job);

    let mut by_duration: Vec<&JobResult> = results.iter().collect();
    by_duration.sort_by_key(|r| std::cmp::Reverse(r.elapsed));
    eprintln!("\nxtask ci: job timings (slowest first)");
    for r in &by_duration {
        eprintln!("  {:<24} {:>6.1}s", r.name, r.elapsed.as_secs_f64());
    }

    let mut failed: Vec<&JobResult> = results
        .iter()
        .filter(|r| matches!(r.outcome, JobOutcome::Failed(_)))
        .collect();
    failed.sort_by_key(|r| r.name);

    if failed.is_empty() {
        eprintln!("\nxtask ci: all {total} jobs passed");
        return ExitCode::SUCCESS;
    }

    eprintln!("\nxtask ci: {} of {total} jobs failed:\n", failed.len());
    for r in &failed {
        if let JobOutcome::Failed(log) = &r.outcome {
            eprintln!("--- {} ---\n{log}", r.name);
        }
    }
    eprintln!("xtask ci: FAILED ({} of {total} jobs failed)", failed.len());
    ExitCode::FAILURE
}

#[cfg(test)]
mod ci_scheduler_tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn every_job_runs_exactly_once_regardless_of_worker_count() {
        for workers in [1, 2, 4, 8] {
            let calls = Arc::new(AtomicUsize::new(0));
            let jobs: Vec<Job<usize>> = (0..10)
                .map(|i| Job {
                    name: "job",
                    payload: i,
                })
                .collect();
            let calls_in_job = Arc::clone(&calls);
            let results = run_jobs(jobs, workers, move |_payload: &usize| {
                calls_in_job.fetch_add(1, Ordering::SeqCst);
                JobOutcome::Passed
            });
            assert_eq!(results.len(), 10, "workers={workers}");
            assert_eq!(calls.load(Ordering::SeqCst), 10, "workers={workers}");
        }
    }

    #[test]
    fn a_failing_job_does_not_cancel_or_block_siblings() {
        let jobs: Vec<Job<usize>> = (0..6)
            .map(|i| Job {
                name: "job",
                payload: i,
            })
            .collect();
        let results = run_jobs(jobs, 3, |payload: &usize| {
            if *payload == 2 {
                JobOutcome::Failed("boom".to_string())
            } else {
                JobOutcome::Passed
            }
        });
        assert_eq!(results.len(), 6, "every job must still produce a result");
        let failed_count = results
            .iter()
            .filter(|r| matches!(r.outcome, JobOutcome::Failed(_)))
            .count();
        assert_eq!(failed_count, 1);
    }

    #[test]
    fn exit_code_reflects_any_failure() {
        let all_pass: Vec<Job<usize>> = (0..4)
            .map(|i| Job {
                name: "job",
                payload: i,
            })
            .collect();
        let results = run_jobs(all_pass, 2, |_: &usize| JobOutcome::Passed);
        assert!(
            results
                .iter()
                .all(|r| matches!(r.outcome, JobOutcome::Passed))
        );

        let one_fails: Vec<Job<usize>> = (0..4)
            .map(|i| Job {
                name: "job",
                payload: i,
            })
            .collect();
        let results = run_jobs(one_fails, 2, |payload: &usize| {
            if *payload == 0 {
                JobOutcome::Failed("x".to_string())
            } else {
                JobOutcome::Passed
            }
        });
        assert!(
            results
                .iter()
                .any(|r| matches!(r.outcome, JobOutcome::Failed(_)))
        );
    }

    #[test]
    fn worker_count_is_never_exceeded() {
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let jobs: Vec<Job<usize>> = (0..12)
            .map(|i| Job {
                name: "job",
                payload: i,
            })
            .collect();
        let active_in_job = Arc::clone(&active);
        let max_active_in_job = Arc::clone(&max_active);
        let workers = 3;
        run_jobs(jobs, workers, move |_payload: &usize| {
            let now = active_in_job.fetch_add(1, Ordering::SeqCst) + 1;
            max_active_in_job.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(5));
            active_in_job.fetch_sub(1, Ordering::SeqCst);
            JobOutcome::Passed
        });
        assert!(max_active.load(Ordering::SeqCst) <= workers);
    }

    #[test]
    fn zero_workers_falls_back_to_at_least_one() {
        let jobs: Vec<Job<usize>> = vec![Job {
            name: "job",
            payload: 1,
        }];
        let results = run_jobs(jobs, 0, |_: &usize| JobOutcome::Passed);
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn worker_override_parses_valid_positive_values() {
        assert_eq!(parse_worker_override(Some("4")), Some(4));
        assert_eq!(parse_worker_override(Some("1")), Some(1));
    }

    #[test]
    fn worker_override_rejects_zero_and_garbage() {
        assert_eq!(parse_worker_override(Some("0")), None);
        assert_eq!(parse_worker_override(Some("not a number")), None);
        assert_eq!(parse_worker_override(Some("-1")), None);
        assert_eq!(parse_worker_override(None), None);
    }
}
