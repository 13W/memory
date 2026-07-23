//! The typed spike report — the fixed metric matrix of spec 14 §7 (T10-01).
//!
//! `#[serde(deny_unknown_fields)]` on every struct makes *deserialization itself*
//! the schema check (the "metric schema validation" acceptance test): a report
//! with a missing required field or an unknown extra field fails to parse. This
//! mirrors `crates/index`'s fixture models (T04-03) rather than adding a runtime
//! JSON-Schema dependency.
//!
//! Timing/RAM fields are `Option` because they are *measurements*: `None` when a
//! run did not measure them (e.g. a conformance-only run, or a metric whose
//! probe is a later task). Their concrete values are deliberately **not**
//! asserted by any test — only the report *shape* is — because measurements are
//! non-deterministic (O2: collect metrics, never invent thresholds). Structural
//! facts (`filtered_hnsw_available`, `durability`, platform support) are always
//! present.

use serde::{Deserialize, Serialize};

/// The report schema version. Bump on any shape change so old artifacts are
/// detectably stale.
pub const REPORT_SCHEMA_VERSION: u32 = 1;

/// One backend's full spike result against one dataset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpikeReport {
    /// [`REPORT_SCHEMA_VERSION`] at the time of writing.
    pub report_schema_version: u32,
    /// The adapter (candidate backend) name.
    pub adapter: String,
    /// The dataset the run used.
    pub dataset: DatasetSummary,
    /// Platform support for this adapter on this target (spec 14 §7 "platform
    /// support"). Always recorded — an unsupported target is *reported*, never
    /// silently skipped.
    pub platform: PlatformSupport,
    /// The 14 §7 metric matrix.
    pub metrics: Metrics,
    /// The shared ProjectionStore conformance result.
    pub conformance: ConformanceReport,
}

/// A compact description of the dataset a run used.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatasetSummary {
    /// The matrix name (`small`/`representative`/`large`).
    pub name: String,
    /// Vector dimensionality.
    pub dims: usize,
    /// Number of points.
    pub points: usize,
    /// Number of queries.
    pub queries: usize,
    /// The seed the dataset was generated from (reproducibility).
    pub seed: u64,
}

/// Whether an adapter is buildable/usable on the current target (spec 14 §7,
/// "platform support (win32)"). `supported = false` with a `reason` is the
/// explicit "reported, not skipped" signal the T10-01 card requires.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlatformSupport {
    /// The target triple (`std::env::consts::ARCH`-`OS`, best effort).
    pub target: String,
    /// Whether the adapter is supported on this target.
    pub supported: bool,
    /// Why it is unsupported (`None` when supported).
    pub reason: Option<String>,
}

/// The fixed metric matrix of spec 14 §7. Each field cites the matrix row it
/// realizes. `Option` = a measurement that may be absent for a given run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Metrics {
    /// 14 §7 "warm search p95": warm search latency p95, milliseconds.
    pub warm_search_p95_ms: Option<f64>,
    /// 14 §7 "RAM/shard": resident bytes attributable to one open shard. Left
    /// `None` in T10-01 — a portable per-target RSS probe is a later refinement.
    pub ram_bytes_per_shard: Option<u64>,
    /// 14 §7 "open/close cost": shard open latency, milliseconds.
    pub open_ms: Option<f64>,
    /// 14 §7 "open/close cost": shard close/destroy latency, milliseconds.
    pub close_ms: Option<f64>,
    /// 14 §7 "startup with a large registry": time to open N shards, milliseconds.
    pub registry_startup_ms: Option<f64>,
    /// 14 §7 "LRU behavior": a summary string; the real per-backend LRU numbers
    /// come from wiring through `ShardManager` (group 12/15), so `None` here.
    pub lru: Option<String>,
    /// 14 §7 "durability/validate-on-open semantics": what validate-on-open did
    /// with an out-of-band corruption (from the conformance run).
    pub durability: String,
    /// 14 §7 "filtered-HNSW available": off the critical path, recorded anyway.
    pub filtered_hnsw_available: bool,
    /// Recall@k against the exact-neighbour oracle. `None` in T10-01 (the oracle
    /// is the brute-force adapter, T10-02); the field exists so the schema is
    /// stable across the whole spike.
    pub recall_at_k: Option<f64>,
}

impl Metrics {
    /// A metrics record with every measurement absent — the shape a
    /// conformance-only or unsupported-platform run emits.
    pub fn unmeasured(filtered_hnsw_available: bool, durability: impl Into<String>) -> Self {
        Self {
            warm_search_p95_ms: None,
            ram_bytes_per_shard: None,
            open_ms: None,
            close_ms: None,
            registry_startup_ms: None,
            lru: None,
            durability: durability.into(),
            filtered_hnsw_available,
            recall_at_k: None,
        }
    }
}

/// The shared conformance result: one row per contract case (spec 05 §1 —
/// reopen / head / manifest / corruption).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConformanceReport {
    /// Whether every case passed.
    pub all_passed: bool,
    /// The individual cases, in run order.
    pub cases: Vec<ConformanceCase>,
}

impl ConformanceReport {
    /// Assemble a report from its cases, computing `all_passed`.
    pub fn new(cases: Vec<ConformanceCase>) -> Self {
        Self {
            all_passed: cases.iter().all(|c| c.passed),
            cases,
        }
    }

    /// The empty report used when no conformance run happened (unsupported
    /// platform). `all_passed` is vacuously true, but `cases` is empty — a
    /// consumer distinguishes "passed everything" from "ran nothing" by the
    /// case count, never by a hidden skip.
    pub fn not_run() -> Self {
        Self {
            all_passed: true,
            cases: Vec::new(),
        }
    }
}

/// One conformance case outcome.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConformanceCase {
    /// The case name.
    pub name: String,
    /// Whether it passed.
    pub passed: bool,
    /// Human-readable detail (the observed signal).
    pub detail: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> SpikeReport {
        SpikeReport {
            report_schema_version: REPORT_SCHEMA_VERSION,
            adapter: "fake".to_string(),
            dataset: DatasetSummary {
                name: "small".to_string(),
                dims: 768,
                points: 544,
                queries: 49,
                seed: 42,
            },
            platform: PlatformSupport {
                target: "aarch64-macos".to_string(),
                supported: true,
                reason: None,
            },
            metrics: Metrics::unmeasured(false, "validate-on-open: detected"),
            conformance: ConformanceReport::new(vec![ConformanceCase {
                name: "reopen".to_string(),
                passed: true,
                detail: "head and points survived a reopen".to_string(),
            }]),
        }
    }

    #[test]
    fn report_round_trips() {
        let report = sample();
        let json = serde_json::to_string_pretty(&report).expect("serialize");
        let back: SpikeReport = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(report, back);
    }

    #[test]
    fn unknown_field_is_rejected() {
        let mut value = serde_json::to_value(sample()).expect("to value");
        value
            .as_object_mut()
            .expect("object")
            .insert("surprise".to_string(), serde_json::json!(1));
        let err = serde_json::from_value::<SpikeReport>(value).expect_err("must reject");
        assert!(err.to_string().contains("surprise"), "got {err}");
    }

    #[test]
    fn missing_required_field_is_rejected() {
        let mut value = serde_json::to_value(sample()).expect("to value");
        value.as_object_mut().expect("object").remove("adapter");
        assert!(
            serde_json::from_value::<SpikeReport>(value).is_err(),
            "a missing required field must fail the schema check"
        );
    }

    #[test]
    fn all_metric_matrix_fields_are_present() {
        // Every 14 §7 metric must be a key in the serialized metrics object, so a
        // future edit that drops one is caught here.
        let metrics = serde_json::to_value(Metrics::unmeasured(true, "x")).expect("to value");
        let obj = metrics.as_object().expect("object");
        for key in [
            "warm_search_p95_ms",
            "ram_bytes_per_shard",
            "open_ms",
            "close_ms",
            "registry_startup_ms",
            "lru",
            "durability",
            "filtered_hnsw_available",
            "recall_at_k",
        ] {
            assert!(obj.contains_key(key), "metric field `{key}` is missing");
        }
    }

    #[test]
    fn not_run_conformance_is_empty_not_a_silent_pass() {
        let report = ConformanceReport::not_run();
        assert!(report.all_passed);
        assert!(
            report.cases.is_empty(),
            "a skipped run is distinguishable by its empty case list"
        );
    }
}
