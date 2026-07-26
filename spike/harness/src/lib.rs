//! Backend-neutral conformance + benchmark harness for the T10 dense-backend
//! spike (open question O1, roadmap step 11).
//!
//! The harness runs any candidate backend through:
//!
//! - the shared, deterministic [`conformance`] suite (spec 05 §1 contract);
//! - the fixed metric matrix of spec 14 §7 ([`report::Metrics`]);
//!
//! over seeded, reproducible [`corpus`] datasets. It is the *only* thing T10-01
//! ships: the candidate adapters (brute-force, `usearch`, Qdrant Edge) are
//! T10-02/03/04, and the comparison/ADR is T10-05. The one adapter available
//! today is [`FakeAdapter`], wrapping the product's fake backend — a real
//! working backend, so it proves the suite is real rather than vacuous.
//!
//! A candidate is a [`SpikeAdapter`]. Crucially, an adapter reports its own
//! platform support, and [`run_spike`] records an unsupported target as an
//! explicit `supported: false` result — it is never a silent skip (the T10-01
//! "unsupported platform reported, not skipped" requirement).

pub mod brute_force;
pub mod conformance;
pub mod corpus;
pub mod oracle;
pub mod report;
pub mod usearch_backend;

use std::io;
use std::path::Path;
use std::time::Instant;

use local_rag_projection::{DenseQuery, FakeProjectionStore, ProjectionStore, ShardParams};

use crate::corpus::SeededDataset;
use crate::report::{
    ConformanceReport, DatasetSummary, Metrics, PlatformSupport, REPORT_SCHEMA_VERSION, SpikeReport,
};
pub use brute_force::BruteForceAdapter;
pub use usearch_backend::UsearchAdapter;

/// A spike candidate: a backend the harness can build and describe. Each of
/// T10-02/03/04 adds one implementor in this workspace; the product workspace
/// stays free of their dependencies (that is the whole point of the isolation).
pub trait SpikeAdapter {
    /// The candidate's name, used in the report and on the command line.
    fn name(&self) -> &str;

    /// Platform support for this adapter on the current target. Called *before*
    /// [`store`](SpikeAdapter::store); an `Unsupported` result is reported, not
    /// skipped.
    fn platform_support(&self) -> PlatformSupport;

    /// Whether this backend offers filtered-HNSW (spec 14 §7 — off the critical
    /// path, recorded anyway).
    fn filtered_hnsw_available(&self) -> bool;

    /// Build the backend, or `None` if unsupported on this target (mirroring
    /// [`platform_support`](SpikeAdapter::platform_support)).
    fn store(&self) -> Option<Box<dyn ProjectionStore>>;

    /// Whether recall@k against the exact-neighbour oracle is a genuine
    /// *measurement* for this backend (T10-03, spec 14 §7). Exact-by-
    /// construction backends (fake, brute-force) inherit the default
    /// `false` — a constant `1.0` would not be a measurement (T10-02
    /// as-built note, spec 14 §7, unchanged). Approximate backends
    /// (usearch) override this to `true`; the harness's internal metrics
    /// pass gates the recall computation itself on this flag, not just
    /// whether the field is surfaced, so exact backends never pay for an
    /// oracle pass they never asked for.
    fn reports_recall(&self) -> bool {
        false
    }
}

/// The current target as a coarse `arch-os` string (best effort; a full triple
/// is not available at runtime in stable Rust).
pub fn current_target() -> String {
    format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
}

/// The fake backend as a spike adapter — the T10-01 reference candidate. Fully
/// portable (pure `std`), so it is supported on every target and offers no
/// filtered HNSW.
#[derive(Debug, Default, Clone, Copy)]
pub struct FakeAdapter;

impl SpikeAdapter for FakeAdapter {
    fn name(&self) -> &str {
        "fake"
    }

    fn platform_support(&self) -> PlatformSupport {
        PlatformSupport {
            target: current_target(),
            supported: true,
            reason: None,
        }
    }

    fn filtered_hnsw_available(&self) -> bool {
        false
    }

    fn store(&self) -> Option<Box<dyn ProjectionStore>> {
        Some(Box::new(FakeProjectionStore::new()))
    }
}

/// Run the full spike for `adapter` against `dataset`, using `base` as a scratch
/// directory. An unsupported target yields a report with `platform.supported =
/// false`, empty conformance (`not_run`), and unmeasured metrics — reported, never
/// silently skipped.
///
/// `measure_timings` gates the non-deterministic measurement pass: pass `false`
/// (the default for tests) to get a deterministic conformance-only report; pass
/// `true` (the benchmark binary) to also fill the timing metrics.
pub fn run_spike(
    adapter: &dyn SpikeAdapter,
    dataset: &SeededDataset,
    base: &Path,
    measure_timings: bool,
) -> io::Result<SpikeReport> {
    let platform = adapter.platform_support();
    let dataset_summary = DatasetSummary {
        name: dataset.name.clone(),
        dims: dataset.dims,
        points: dataset.points.len(),
        queries: dataset.queries.len(),
        seed: dataset.seed,
    };

    if !platform.supported {
        // Reported, not skipped: the report exists, names the target and reason,
        // and carries an explicitly empty conformance run.
        return Ok(SpikeReport {
            report_schema_version: REPORT_SCHEMA_VERSION,
            adapter: adapter.name().to_string(),
            dataset: dataset_summary,
            platform,
            metrics: Metrics::unmeasured(
                adapter.filtered_hnsw_available(),
                "not exercised: unsupported platform",
            ),
            conformance: ConformanceReport::not_run(),
        });
    }

    let store = adapter
        .store()
        .expect("a supported adapter must build a store");

    let conformance = conformance::run_conformance(&*store, dataset, base)?;
    let durability = conformance::durability_summary(&conformance);

    let mut metrics = Metrics::unmeasured(adapter.filtered_hnsw_available(), durability);
    if measure_timings {
        let measured = measure_metrics(adapter, &*store, dataset, &base.join("bench"))?;
        metrics.warm_search_p95_ms = measured.warm_search_p95_ms;
        metrics.open_ms = measured.open_ms;
        metrics.close_ms = measured.close_ms;
        metrics.registry_startup_ms = measured.registry_startup_ms;
        metrics.recall_at_k = measured.recall_at_k;
    }

    Ok(SpikeReport {
        report_schema_version: REPORT_SCHEMA_VERSION,
        adapter: adapter.name().to_string(),
        dataset: dataset_summary,
        platform,
        metrics,
        conformance,
    })
}

/// A subset of [`Metrics`] the timing pass fills. Non-deterministic — never
/// asserted by a test, only emitted as a benchmark artifact. `recall_at_k` is
/// the one exception: it *is* a real comparison against the exact-neighbour
/// oracle (T10-03), just gated on [`SpikeAdapter::reports_recall`] so it is
/// only computed for adapters that asked for it.
struct MeasuredTimings {
    warm_search_p95_ms: Option<f64>,
    open_ms: Option<f64>,
    close_ms: Option<f64>,
    registry_startup_ms: Option<f64>,
    recall_at_k: Option<f64>,
}

/// Measure the timing metrics of spec 14 §7. Uses [`Instant`]; values are
/// measurements, not assertions. RAM/LRU are left `None` here (see
/// [`Metrics`] field docs for why each is a later refinement). `recall_at_k`
/// is computed (T10-03) by reusing the warm-search loop's own results — no
/// extra `search()` calls — only when `adapter.reports_recall()` is true.
fn measure_metrics(
    adapter: &dyn SpikeAdapter,
    store: &dyn ProjectionStore,
    dataset: &SeededDataset,
    base: &Path,
) -> io::Result<MeasuredTimings> {
    let params = ShardParams::with_dimensions(dataset.dims);
    let dir = base.join("shard");
    std::fs::create_dir_all(&dir)?;

    // open + build once (warm).
    let open_start = Instant::now();
    let shard = store
        .open(&dir, params)
        .expect("open a bench shard on a supported adapter");
    let open_ms = open_start.elapsed().as_secs_f64() * 1000.0;
    shard.upsert(&dataset.points).expect("bench upsert");

    // warm search p95 across the dataset's queries; recall@k against the
    // exact-neighbour oracle is accumulated in the same pass when the
    // adapter opts in, so an exact-by-construction adapter (fake,
    // brute-force) never pays for an oracle comparison it never asked for.
    let mut latencies_ms: Vec<f64> = Vec::with_capacity(dataset.queries.len());
    let mut recall_samples: Vec<f64> = Vec::new();
    for query in &dataset.queries {
        let q = DenseQuery {
            vector: query.vector.clone(),
            k: query.k,
        };
        let start = Instant::now();
        let hits = shard.search(&q).expect("bench search");
        latencies_ms.push(start.elapsed().as_secs_f64() * 1000.0);

        if adapter.reports_recall() {
            let exact = crate::oracle::exact_top_k(&dataset.points, &q);
            recall_samples.push(crate::oracle::recall_at_k(&exact, &hits));
        }
    }
    let warm_search_p95_ms = percentile(&mut latencies_ms, 95.0);
    let recall_at_k = if recall_samples.is_empty() {
        None
    } else {
        Some(recall_samples.iter().sum::<f64>() / recall_samples.len() as f64)
    };

    // close: the trait exposes no distinct flush-then-release op, so this
    // times dropping the handle — for a backend holding significant
    // in-process state (e.g. brute-force's contiguous vector table) that is a
    // real proxy for release cost, not a stand-in for some future backend's
    // own explicit close.
    let close_start = Instant::now();
    drop(shard);
    let close_ms = close_start.elapsed().as_secs_f64() * 1000.0;

    // registry startup: open a handful of shards and time it (spec 14 §7).
    let registry_startup_ms = {
        let start = Instant::now();
        for i in 0..8 {
            let d = base.join(format!("reg-{i}"));
            std::fs::create_dir_all(&d)?;
            let _ = store.open(&d, params).expect("bench registry open");
        }
        Some(start.elapsed().as_secs_f64() * 1000.0)
    };

    Ok(MeasuredTimings {
        warm_search_p95_ms,
        open_ms: Some(open_ms),
        close_ms: Some(close_ms),
        registry_startup_ms,
        recall_at_k,
    })
}

/// The `p`-th percentile of `samples` (nearest-rank), or `None` if empty.
fn percentile(samples: &mut [f64], p: f64) -> Option<f64> {
    if samples.is_empty() {
        return None;
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let rank = ((p / 100.0) * samples.len() as f64).ceil() as usize;
    let idx = rank.saturating_sub(1).min(samples.len() - 1);
    Some(samples[idx])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_nearest_rank() {
        assert_eq!(percentile(&mut [], 95.0), None);
        assert_eq!(percentile(&mut [5.0], 95.0), Some(5.0));
        let mut xs: Vec<f64> = (1..=100).map(f64::from).collect();
        assert_eq!(percentile(&mut xs, 95.0), Some(95.0));
    }

    /// T10-02: `close_ms` is a new generic field on `measure_metrics`, wired
    /// for every adapter. Only shape is asserted (`.is_some()`) — never a
    /// value, matching this module's own house style for measurements.
    #[test]
    fn measure_metrics_populates_close_ms() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "local-rag-spike-harness-lib-test-{}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&base).expect("create scratch");

        let dataset = crate::corpus::generate(&crate::corpus::TINY, 11);
        let store = FakeProjectionStore::new();
        let measured =
            measure_metrics(&FakeAdapter, &store, &dataset, &base).expect("measure_metrics");

        assert!(measured.open_ms.is_some());
        assert!(measured.close_ms.is_some(), "close_ms must be populated");
        assert!(measured.warm_search_p95_ms.is_some());
        assert!(measured.registry_startup_ms.is_some());
        assert!(
            measured.recall_at_k.is_none(),
            "fake does not opt into recall (T10-02 as-built note, unchanged)"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// A test-only adapter wrapping the fake backend, overriding
    /// `reports_recall` — proves `measure_metrics` gates the recall
    /// computation on the trait flag without depending on the `usearch`
    /// crate from this crate's own unit tests.
    #[derive(Debug, Default, Clone, Copy)]
    struct RecallOptInFakeAdapter;

    impl SpikeAdapter for RecallOptInFakeAdapter {
        fn name(&self) -> &str {
            "recall-opt-in-fake"
        }
        fn platform_support(&self) -> PlatformSupport {
            PlatformSupport {
                target: current_target(),
                supported: true,
                reason: None,
            }
        }
        fn filtered_hnsw_available(&self) -> bool {
            false
        }
        fn reports_recall(&self) -> bool {
            true
        }
        fn store(&self) -> Option<Box<dyn ProjectionStore>> {
            Some(Box::new(FakeProjectionStore::new()))
        }
    }

    /// T10-03: `measure_metrics` computes `recall_at_k` only when
    /// `adapter.reports_recall()` is true. The fake backend is exact by
    /// construction and shares the oracle's exact dot-product/tie-break
    /// convention (proven byte-identical by T10-02's own test), so an
    /// opted-in fake deterministically recalls `1.0` — not a timing-based
    /// assertion.
    #[test]
    fn measure_metrics_reports_recall_only_when_the_adapter_opts_in() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "local-rag-spike-harness-lib-test-recall-{}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&base).expect("create scratch");

        let dataset = crate::corpus::generate(&crate::corpus::TINY, 13);
        let store = FakeProjectionStore::new();
        let measured = measure_metrics(&RecallOptInFakeAdapter, &store, &dataset, &base)
            .expect("measure_metrics");

        assert_eq!(measured.recall_at_k, Some(1.0));

        let _ = std::fs::remove_dir_all(&base);
    }
}
