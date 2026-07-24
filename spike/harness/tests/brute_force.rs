//! T10-02 acceptance tests for the brute-force spike candidate.
//!
//! The card's test bullet: "shared conformance + exact-neighbor oracle +
//! crash/reopen cases". Mapped 1:1:
//! - **shared conformance** (which itself exercises reopen/head/manifest/
//!   corruption — the same "crash/reopen cases" the card separately names, no
//!   bespoke fault code needed here, mirroring T10-01's own precedent for the
//!   fake adapter) — `brute_force_passes_the_shared_conformance_suite`.
//! - **exact-neighbor oracle** — `search_matches_the_exact_neighbor_oracle`,
//!   proving the adapter's own `search()` top-k/tie-break logic is correct
//!   against an independently computed reference, not merely that the oracle
//!   is self-consistent.
//!
//! Deterministic: an isolated scratch dir (removed on drop), fixed seeds, the
//! `tiny` fast preset, no assertions on any timing value.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use local_rag_projection::{ProjectionStore, ShardParams};
use local_rag_spike_harness::brute_force::BruteForceStore;
use local_rag_spike_harness::conformance::run_conformance;
use local_rag_spike_harness::{BruteForceAdapter, corpus, oracle, run_spike};

/// A unique scratch directory under the OS temp dir, removed on drop.
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "local-rag-spike-bruteforce-{}-{n}",
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
fn brute_force_passes_the_shared_conformance_suite() {
    let scratch = Scratch::new();
    let dataset = corpus::generate(&corpus::TINY, 42);
    let store = BruteForceStore::new();

    let report = run_conformance(&store, &dataset, scratch.path()).expect("run conformance");

    assert!(
        report.all_passed,
        "brute-force must pass every conformance case; failures: {:?}",
        report
            .cases
            .iter()
            .filter(|c| !c.passed)
            .collect::<Vec<_>>()
    );

    // The same four required areas the fake candidate proved at T10-01 — this
    // is what "crash/reopen cases" resolves to (spec 05 §10 detection, not a
    // bespoke fault-matrix reimplementation for the spike).
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
fn full_run_spike_on_brute_force_reports_a_supported_platform_and_passes() {
    let scratch = Scratch::new();
    let dataset = corpus::generate(&corpus::TINY, 7);

    // measure_timings = false -> deterministic conformance-only report.
    let report = run_spike(&BruteForceAdapter, &dataset, scratch.path(), false).expect("run spike");

    assert_eq!(report.adapter, "brute-force");
    assert!(report.platform.supported, "brute-force is pure std");
    assert!(report.platform.reason.is_none());
    assert!(report.conformance.all_passed);
    assert!(!report.conformance.cases.is_empty());
    assert!(report.metrics.warm_search_p95_ms.is_none());
    assert!(!report.metrics.filtered_hnsw_available);
    assert!(report.metrics.durability.contains("detected"));
}

#[test]
fn search_matches_the_exact_neighbor_oracle() {
    let scratch = Scratch::new();
    let dataset = corpus::generate(&corpus::TINY, 99);
    let store = BruteForceStore::new();
    let params = ShardParams {
        dimensions: dataset.dims,
    };

    let shard = store.open(scratch.path(), params).expect("open");
    shard.upsert(&dataset.points).expect("upsert");

    for query in &dataset.queries {
        let expected = oracle::exact_top_k(&dataset.points, query);
        let actual = shard.search(query).expect("search");
        assert_eq!(
            actual, expected,
            "brute-force search must match the independent exact-neighbor oracle"
        );
    }
}
