//! T10-05 acceptance test: "report regeneration check".
//!
//! The T10-05 card's own deliverable is a comparison built from *reproducible*
//! raw results (`docs/adr/0003-dense-backend-selection.md`,
//! `spike/artifacts/*-small.json` / `*-representative.json`). This proves the
//! claim mechanically for the winning candidate (brute-force, ADR-0003): running
//! `run_spike` twice against the same seeded dataset regenerates a report whose
//! *structural* fields — dataset summary, conformance outcome, recall, platform
//! support, filtered-HNSW — are byte-identical, even though the two runs use
//! distinct scratch directories and process instances. Timing fields are
//! deliberately **not** compared: they are measurements, not identity (O2 —
//! collect metrics, never invent thresholds; the existing schema/round-trip
//! tests in `report.rs` already treat every timing field as an `Option` for
//! exactly this reason).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use local_rag_spike_harness::{BruteForceAdapter, corpus, run_spike};

/// A unique scratch directory under the OS temp dir, removed on drop.
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("local-rag-spike-regen-{}-{n}", std::process::id()));
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
fn regenerating_a_report_reproduces_every_structural_field() {
    let dataset = corpus::generate(&corpus::TINY, 42);

    let scratch_a = Scratch::new();
    let report_a =
        run_spike(&BruteForceAdapter, &dataset, scratch_a.path(), true).expect("run spike #1");

    let scratch_b = Scratch::new();
    let report_b =
        run_spike(&BruteForceAdapter, &dataset, scratch_b.path(), true).expect("run spike #2");

    // Same dataset summary: name/dims/points/queries/seed are pure functions of
    // `corpus::generate`'s input, never of the run.
    assert_eq!(report_a.dataset, report_b.dataset);

    // Same platform verdict.
    assert_eq!(report_a.platform, report_b.platform);

    // Same conformance outcome, case by case (name + pass/fail; `detail` strings
    // may embed machine-specific paths and are intentionally not compared).
    assert_eq!(
        report_a.conformance.all_passed,
        report_b.conformance.all_passed
    );
    assert_eq!(
        report_a.conformance.cases.len(),
        report_b.conformance.cases.len()
    );
    for (a, b) in report_a
        .conformance
        .cases
        .iter()
        .zip(&report_b.conformance.cases)
    {
        assert_eq!(a.name, b.name, "case order/identity must be stable");
        assert_eq!(a.passed, b.passed, "case {} outcome must be stable", a.name);
    }

    // Same measurement-independent metrics: brute-force is exact by
    // construction (`recall_at_k` stays `None`, spec 05 §1's as-built note) and
    // never reports filtered-HNSW.
    assert_eq!(report_a.metrics.recall_at_k, report_b.metrics.recall_at_k);
    assert_eq!(
        report_a.metrics.filtered_hnsw_available,
        report_b.metrics.filtered_hnsw_available
    );
    assert_eq!(report_a.metrics.durability, report_b.metrics.durability);

    // Timing fields are measurements, not identity: assert only that both runs
    // actually measured them (present), never that the two values are equal.
    assert!(report_a.metrics.warm_search_p95_ms.is_some());
    assert!(report_b.metrics.warm_search_p95_ms.is_some());
    assert!(report_a.metrics.open_ms.is_some());
    assert!(report_b.metrics.open_ms.is_some());
}
