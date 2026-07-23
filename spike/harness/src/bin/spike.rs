//! The spike benchmark runner (T10-01).
//!
//! Runs one candidate adapter against one matrix dataset, then writes a
//! [`SpikeReport`](local_rag_spike_harness::report::SpikeReport) — conformance
//! plus the measured 14 §7 metric matrix — as pretty JSON to `--out` (or stdout).
//! The timing numbers are reproducible *artifacts*, not test assertions.
//!
//! ```text
//! spike --adapter fake --dataset small --seed 42 --out spike/artifacts/fake-small.json
//! ```
//!
//! Only the `fake` adapter exists in T10-01; `--adapter` is already a switch so
//! T10-02/03/04 add their candidates without changing the runner's shape.

use std::path::PathBuf;
use std::process::ExitCode;

use local_rag_spike_harness::{FakeAdapter, SpikeAdapter, corpus, run_spike};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("spike: {msg}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let args = Args::parse(std::env::args().skip(1))?;

    let adapter: Box<dyn SpikeAdapter> = match args.adapter.as_str() {
        "fake" => Box::new(FakeAdapter),
        other => {
            return Err(format!(
                "unknown adapter {other:?} (only `fake` exists at T10-01; \
                 brute-force/usearch/qdrant arrive at T10-02/03/04)"
            ));
        }
    };

    let spec = corpus::spec_by_name(&args.dataset).ok_or_else(|| {
        format!(
            "unknown dataset {:?} (matrix names: small, representative, large)",
            args.dataset
        )
    })?;
    let dataset = corpus::generate(&spec, args.seed);

    // A scratch directory under the OS temp dir, unique per process/run.
    let base = scratch_dir();
    std::fs::create_dir_all(&base).map_err(|e| format!("create scratch {base:?}: {e}"))?;

    let report = run_spike(adapter.as_ref(), &dataset, &base, true)
        .map_err(|e| format!("run spike: {e}"))?;

    // Best-effort cleanup; ignore failures (temp dir).
    let _ = std::fs::remove_dir_all(&base);

    let json = serde_json::to_string_pretty(&report).map_err(|e| format!("serialize: {e}"))?;
    match &args.out {
        Some(path) => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| format!("create {parent:?}: {e}"))?;
            }
            std::fs::write(path, json.as_bytes()).map_err(|e| format!("write {path:?}: {e}"))?;
            eprintln!("spike: wrote {}", path.display());
        }
        None => println!("{json}"),
    }

    if report.conformance.all_passed {
        Ok(())
    } else {
        Err("conformance failed — see the report".to_string())
    }
}

/// A per-run scratch directory. Uniqueness comes from the pid plus a monotonic
/// counter — no wall clock — so repeated runs in one process never collide.
fn scratch_dir() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("local-rag-spike-{}-{n}", std::process::id()))
}

/// Parsed command-line arguments.
struct Args {
    adapter: String,
    dataset: String,
    seed: u64,
    out: Option<PathBuf>,
}

impl Args {
    fn parse(mut args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut adapter = "fake".to_string();
        let mut dataset = "small".to_string();
        let mut seed = 42u64;
        let mut out = None;

        while let Some(flag) = args.next() {
            let mut value = || {
                args.next()
                    .ok_or_else(|| format!("flag {flag} needs a value"))
            };
            match flag.as_str() {
                "--adapter" => adapter = value()?,
                "--dataset" => dataset = value()?,
                "--seed" => {
                    seed = value()?
                        .parse()
                        .map_err(|e| format!("--seed must be a u64: {e}"))?;
                }
                "--out" => out = Some(PathBuf::from(value()?)),
                other => return Err(format!("unknown flag {other:?}")),
            }
        }

        Ok(Self {
            adapter,
            dataset,
            seed,
            out,
        })
    }
}
