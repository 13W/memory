//! T10-01 acceptance tests for the spike harness.
//!
//! The card's four checks:
//! - **seeded dataset repeatability** — lives in `corpus`'s unit tests.
//! - **metric schema validation** — lives in `report`'s unit tests.
//! - **adapter conformance includes reopen/corruption/head/manifest** — here,
//!   running the real fake backend through the shared suite.
//! - **unsupported platform reported, not skipped silently** — here, via a
//!   deliberately-unsupported adapter.
//!
//! Deterministic: an isolated scratch dir (removed on drop), a fixed seed, the
//! `tiny` fast preset, and no assertions on any timing value.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use local_rag_projection::ProjectionStore;
use local_rag_spike_harness::conformance::run_conformance;
use local_rag_spike_harness::report::PlatformSupport;
use local_rag_spike_harness::{FakeAdapter, SpikeAdapter, corpus, run_spike};

/// A unique scratch directory under the OS temp dir, removed on drop. Uniqueness
/// is pid + a monotonic counter — no wall clock.
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("local-rag-spike-test-{}-{n}", std::process::id()));
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
fn fake_backend_passes_the_shared_conformance_suite() {
    let scratch = Scratch::new();
    let dataset = corpus::generate(&corpus::TINY, 42);
    let store = local_rag_projection::FakeProjectionStore::new();

    let report = run_conformance(&store, &dataset, scratch.path()).expect("run conformance");

    assert!(
        report.all_passed,
        "fake must pass every conformance case; failures: {:?}",
        report
            .cases
            .iter()
            .filter(|c| !c.passed)
            .collect::<Vec<_>>()
    );

    // The card names four required areas — assert each is actually covered.
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
fn full_run_spike_on_fake_reports_a_supported_platform_and_passes() {
    let scratch = Scratch::new();
    let dataset = corpus::generate(&corpus::TINY, 7);

    // measure_timings = false → deterministic conformance-only report.
    let report = run_spike(&FakeAdapter, &dataset, scratch.path(), false).expect("run spike");

    assert_eq!(report.adapter, "fake");
    assert!(report.platform.supported);
    assert!(report.platform.reason.is_none());
    assert!(report.conformance.all_passed);
    assert!(!report.conformance.cases.is_empty());
    // Timings absent (not measured), structural facts present.
    assert!(report.metrics.warm_search_p95_ms.is_none());
    assert!(!report.metrics.filtered_hnsw_available);
    assert!(report.metrics.durability.contains("detected"));
}

/// A deliberately-unsupported adapter, standing in for e.g. a candidate that
/// fails to build on win32 (T10-03/04). Its `store()` returns `None`.
struct AlwaysUnsupported;

impl SpikeAdapter for AlwaysUnsupported {
    fn name(&self) -> &str {
        "always-unsupported"
    }

    fn platform_support(&self) -> PlatformSupport {
        PlatformSupport {
            target: local_rag_spike_harness::current_target(),
            supported: false,
            reason: Some("simulated: candidate does not build on this target".to_string()),
        }
    }

    fn filtered_hnsw_available(&self) -> bool {
        false
    }

    fn store(&self) -> Option<Box<dyn ProjectionStore>> {
        None
    }
}

#[test]
fn unsupported_platform_is_reported_not_skipped() {
    let scratch = Scratch::new();
    let dataset = corpus::generate(&corpus::TINY, 1);

    let report = run_spike(&AlwaysUnsupported, &dataset, scratch.path(), true).expect("run spike");

    // The run produced a REPORT (not a skip): the adapter and target are named,
    // support is explicitly false, and the reason is carried through.
    assert_eq!(report.adapter, "always-unsupported");
    assert!(!report.platform.supported);
    assert_eq!(
        report.platform.reason.as_deref(),
        Some("simulated: candidate does not build on this target")
    );
    // Conformance is explicitly "not run" (empty case list) — distinguishable
    // from "passed everything", never a hidden skip.
    assert!(report.conformance.cases.is_empty());
    assert!(report.metrics.durability.contains("unsupported"));
}

#[test]
fn committed_schema_example_matches_the_current_schema() {
    // The committed artifact `spike/artifacts/schema-example.json` documents the
    // report shape. Loading it with `deny_unknown_fields` proves it has not drifted
    // from the code — a field renamed in `report.rs` without updating the example
    // fails here.
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("artifacts")
        .join("schema-example.json");
    let json =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let report: local_rag_spike_harness::report::SpikeReport =
        serde_json::from_str(&json).expect("committed example must match the schema");
    assert_eq!(report.adapter, "fake");
    assert!(report.conformance.all_passed);
    // The example is deterministic: timing measurements are null so the committed
    // artifact never churns per machine (real candidate timings land in T10-02+).
    assert!(report.metrics.warm_search_p95_ms.is_none());
}

#[test]
fn matrix_dataset_names_resolve() {
    // The spike matrix is small/representative/large (14 §7); tiny is test-only.
    assert!(corpus::spec_by_name("small").is_some());
    assert!(corpus::spec_by_name("representative").is_some());
    assert!(corpus::spec_by_name("large").is_some());
    assert!(corpus::spec_by_name("tiny").is_none());
}
