//! T10-03 acceptance tests for the `usearch` spike candidate.
//!
//! The card's test bullet: "identical shared conformance, recall vs oracle,
//! F1–F12 applicable scenarios; win32 build smoke or explicit evidence of
//! failure". Mapped 1:1:
//! - **identical shared conformance** (which itself exercises reopen/head/
//!   manifest/corruption — the F5–F8/F12 rows of spec 05 §10 that are even
//!   reachable at a bare `ShardHandle` level; F1–F4/F9–F11 are write-ahead/
//!   `switch()`/`state.sqlite` concerns already covered at product-crate
//!   scope, T07-05, and out of a spike adapter's structural reach) —
//!   `usearch_passes_the_shared_conformance_suite`.
//! - **recall vs oracle** — `search_recall_clears_the_calibrated_lower_bound`,
//!   comparing real `search()` output against the independent
//!   `oracle::exact_top_k` over the measured `SMALL` baseline corpus.
//! - **win32 build smoke or explicit evidence of failure** — not a `#[test]`;
//!   a manual `cargo build --target x86_64-pc-windows-msvc` attempt, its
//!   output captured verbatim into `PROGRESS.md`'s evidence row (this
//!   sandbox has no MSVC/mingw toolchain, so the *captured failure* is the
//!   deliverable, per the card's own accepted fallback).
//!
//! Deterministic: an isolated scratch dir (removed on drop), fixed seeds, no
//! assertions on any timing value.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use local_rag_projection::{ProjectionStore, ShardParams};
use local_rag_spike_harness::conformance::run_conformance;
use local_rag_spike_harness::usearch_backend::UsearchStore;
use local_rag_spike_harness::{UsearchAdapter, corpus, oracle, run_spike};

/// A unique scratch directory under the OS temp dir, removed on drop.
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "local-rag-spike-usearch-{}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create scratch");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[test]
fn usearch_passes_the_shared_conformance_suite() {
    let scratch = Scratch::new();
    let dataset = corpus::generate(&corpus::TINY, 42);
    let store = UsearchStore;

    let report = run_conformance(&store, &dataset, scratch.path()).expect("run conformance");

    assert!(
        report.all_passed,
        "usearch must pass every conformance case; failures: {:?}",
        report
            .cases
            .iter()
            .filter(|c| !c.passed)
            .collect::<Vec<_>>()
    );

    let names: Vec<&str> = report.cases.iter().map(|c| c.name.as_str()).collect();
    assert!(names.iter().any(|n| n.contains("reopen")), "reopen case");
    assert!(names.iter().any(|n| n.contains("head")), "head case");
    assert!(
        names.iter().any(|n| n.contains("manifest")),
        "manifest case"
    );
    assert!(
        names.iter().any(|n| n.contains("corruption")),
        "corruption case"
    );
}

#[test]
fn full_run_spike_on_usearch_reports_a_supported_platform_and_passes() {
    let scratch = Scratch::new();
    let dataset = corpus::generate(&corpus::TINY, 7);

    // measure_timings = false -> deterministic conformance-only report.
    let report = run_spike(&UsearchAdapter, &dataset, scratch.path(), false).expect("run spike");

    assert_eq!(report.adapter, "usearch");
    assert!(report.platform.supported, "usearch builds natively here");
    assert!(report.platform.reason.is_none());
    assert!(report.conformance.all_passed);
    assert!(!report.conformance.cases.is_empty());
    assert!(report.metrics.warm_search_p95_ms.is_none());
    // The first candidate to honestly report real filtered-HNSW support.
    assert!(report.metrics.filtered_hnsw_available);
    assert!(report.metrics.durability.contains("detected"));
}

/// "Recall vs oracle" (the card's own wording): builds a real shard over the
/// measured `SMALL` baseline (544 points, dim 768 — large enough that a
/// default-tuned HNSW graph could plausibly diverge from the exact top-k,
/// unlike `TINY`'s 24 points, where near-perfect recall would prove nothing),
/// runs every query, and compares against the independent
/// `oracle::exact_top_k`. Calibrated, not guessed (O2: collect metrics, never
/// invent thresholds): the observed average recall@k on this exact
/// dataset/seed, measured three times via the `spike` binary during
/// implementation, was a stable `0.9795918367346937` (deterministic HNSW
/// construction in this environment/version — identical across repeated
/// runs). The bound below sits comfortably beneath that, with margin for
/// legitimate cross-machine/usearch-version variance, while still failing
/// loudly on a real integration regression (a wrong metric sign or a broken
/// key mapping would crater recall far below this bound, not graze it).
#[test]
fn search_recall_clears_the_calibrated_lower_bound() {
    let scratch = Scratch::new();
    let dataset = corpus::generate(&corpus::SMALL, 42);
    let store = UsearchStore;
    let params = ShardParams::with_dimensions(dataset.dims);

    let shard = store.open(scratch.path(), params).expect("open");
    shard.upsert(&dataset.points).expect("upsert");

    let mut samples = Vec::with_capacity(dataset.queries.len());
    for query in &dataset.queries {
        let exact = oracle::exact_top_k(&dataset.points, query);
        let approx = shard.search(query).expect("search");
        samples.push(oracle::recall_at_k(&exact, &approx));
    }
    let average = samples.iter().sum::<f64>() / samples.len() as f64;

    const CALIBRATED_LOWER_BOUND: f64 = 0.9;
    assert!(
        average >= CALIBRATED_LOWER_BOUND,
        "average recall@k {average} fell below the calibrated bound {CALIBRATED_LOWER_BOUND} \
         over {} queries",
        samples.len()
    );
}
