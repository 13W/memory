//! The memory-router benchmark report (spec 08 §7, 14 §2's `memory-quality`
//! row) — T14-07.
//!
//! # No v1 baseline to diff against
//!
//! `crate::bench::report::BenchReport` diffs every run against a recorded v1
//! search baseline (D-015). There is no analog here: GAP-04 states plainly
//! that "the rev6 labeled observation-stream corpus... is absent in v1" —
//! this benchmark has no prior measurement to compare against, only the
//! floor [`crate::memory_bench::gate::Thresholds`] states once a baseline
//! run exists. [`MemoryBenchReport`] therefore carries no `baseline`/`diff`
//! fields at all, unlike its search-benchmark sibling.
//!
//! # Determinism
//!
//! `metrics`/`per_case` are a deterministic function of the model, the
//! corpus, and greedy decoding (`local_rag_generate::LlamaGenerator` only
//! implements `Sampling::Greedy` — see that crate's module doc) — so two
//! runs against the same installed model weights score identically. Wall
//! time is not, so it lives entirely under [`Latency`], excluded from
//! [`MemoryBenchReport::scored`]'s determinism-tested subset.

use serde::{Deserialize, Serialize};

use crate::memory_bench::score::Metrics;

/// Where a run came from, so a recorded number can be reproduced.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Provenance {
    /// The commit under test.
    pub commit: String,
    /// The case-index fixture that was scored.
    pub corpus_path: String,
    /// The case-index fixture's own `version` field.
    pub corpus_version: String,
    /// How many `memory.router.op.*` cases were scored.
    pub case_count: usize,
    /// The generator's catalog id (`local_rag_generate::DEFAULT_MODEL_ID`).
    pub model_id: String,
    /// Always `"greedy"` today — see the module doc.
    pub sampling: String,
    /// Spec 08 §2's `router_version` (confidence-weight config version) —
    /// `[SPEC values TBD]`; recorded here so a future weight retune is
    /// visibly a different `router_version`, not a silent change.
    pub router_version: String,
    /// Host triple.
    pub host: String,
}

/// One case's outcome.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaseResult {
    pub id: String,
    /// The fixture case's own tags (op kind / language / clean-or-adversarial)
    /// — carried through so a failing run's markdown/JSON can be grouped by
    /// category without re-opening the fixture file.
    #[serde(default)]
    pub tags: Vec<String>,
    pub expected: Vec<String>,
    pub predicted: Vec<String>,
    /// Exact multiset match between `expected` and `predicted`.
    pub correct: bool,
    /// Set when [`local_rag_memory::router::route`] itself returned `Err`
    /// for this case (a malformed generation that survived the one
    /// corrective re-prompt) — `predicted` is empty and `correct` is
    /// `false` in that case, but the reason is worth keeping.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Wall-clock measurements, deliberately outside the scored part.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Latency {
    /// Time to install the model (0 when weights were already cached).
    pub install_ms: u64,
    /// Time to load the model into memory.
    pub load_ms: u64,
    /// Per-case `route()` time, median, milliseconds.
    pub route_p50_ms: f64,
    /// Per-case `route()` time, 95th percentile, milliseconds.
    pub route_p95_ms: f64,
}

/// The deterministic subset of a report: everything except wall-clock time.
/// Only the determinism tests construct this (mirrors
/// `crate::bench::report::ScoredPart`).
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ScoredPart<'a> {
    pub metrics: &'a Metrics,
    pub per_case: &'a [CaseResult],
}

pub const REPORT_SCHEMA_VERSION: u32 = 1;

/// One memory-router benchmark run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryBenchReport {
    pub schema_version: u32,
    pub provenance: Provenance,
    pub metrics: Metrics,
    pub per_case: Vec<CaseResult>,
    pub latency: Latency,
}

impl MemoryBenchReport {
    pub fn new(
        provenance: Provenance,
        metrics: Metrics,
        per_case: Vec<CaseResult>,
        latency: Latency,
    ) -> Self {
        MemoryBenchReport {
            schema_version: REPORT_SCHEMA_VERSION,
            provenance,
            metrics,
            per_case,
            latency,
        }
    }

    #[cfg(test)]
    pub fn scored(&self) -> ScoredPart<'_> {
        ScoredPart {
            metrics: &self.metrics,
            per_case: &self.per_case,
        }
    }

    /// A human-readable summary, in the shape of the search benchmark's own
    /// `.report.md`.
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str("# Memory-router benchmark run\n\n");
        out.push_str("## Run provenance\n\n| Field | Value |\n| --- | --- |\n");
        let p = &self.provenance;
        out.push_str(&format!("| Commit | `{}` |\n", p.commit));
        out.push_str(&format!(
            "| Corpus | `{}` (v{}, {} cases) |\n",
            p.corpus_path, p.corpus_version, p.case_count
        ));
        out.push_str(&format!("| Model | `{}` |\n", p.model_id));
        out.push_str(&format!("| Sampling | {} |\n", p.sampling));
        out.push_str(&format!("| Router version | {} |\n", p.router_version));
        out.push_str(&format!("| Host | {} |\n\n", p.host));

        out.push_str("## Metrics\n\n| Metric | Value |\n| --- | --- |\n");
        out.push_str(&format!("| Precision | {:.4} |\n", self.metrics.precision));
        out.push_str(&format!("| Recall | {:.4} |\n", self.metrics.recall));
        out.push_str(&format!("| F1 | {:.4} |\n", self.metrics.f1));
        out.push_str(&format!(
            "| Exact match rate | {:.4} |\n",
            self.metrics.exact_match_rate
        ));

        out.push_str(&format!(
            "\n## Latency\n\n| Stage | ms |\n| --- | --- |\n| install | {} |\n| load | {} |\n\
             | route p50 | {:.3} |\n| route p95 | {:.3} |\n",
            self.latency.install_ms,
            self.latency.load_ms,
            self.latency.route_p50_ms,
            self.latency.route_p95_ms
        ));

        out.push_str(
            "\n## Per-case\n\n| id | expected | predicted | correct |\n| --- | --- | --- | --- |\n",
        );
        for c in &self.per_case {
            let mark = if c.correct { "yes" } else { "no" };
            out.push_str(&format!(
                "| {} | {} | {} | {mark} |\n",
                c.id,
                c.expected.join(","),
                if c.predicted.is_empty() {
                    c.error.clone().unwrap_or_else(|| "(none)".to_string())
                } else {
                    c.predicted.join(",")
                },
            ));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provenance() -> Provenance {
        Provenance {
            commit: "abc1234".to_string(),
            corpus_path: "fixtures/memory/index.json".to_string(),
            corpus_version: "1.0.0".to_string(),
            case_count: 2,
            model_id: "qwen2.5-0.5b-instruct-gguf-q4km".to_string(),
            sampling: "greedy".to_string(),
            router_version: "v0".to_string(),
            host: "aarch64-apple-darwin".to_string(),
        }
    }

    fn case(id: &str, correct: bool) -> CaseResult {
        CaseResult {
            id: id.to_string(),
            tags: vec!["create".to_string(), "en".to_string()],
            expected: vec!["create".to_string()],
            predicted: if correct {
                vec!["create".to_string()]
            } else {
                vec!["noop".to_string()]
            },
            correct,
            error: None,
        }
    }

    fn report(metrics: Metrics) -> MemoryBenchReport {
        MemoryBenchReport::new(
            provenance(),
            metrics,
            vec![case("c1", true), case("c2", false)],
            Latency {
                install_ms: 0,
                load_ms: 500,
                route_p50_ms: 120.0,
                route_p95_ms: 250.0,
            },
        )
    }

    #[test]
    fn timing_is_excluded_from_the_deterministic_part() {
        let metrics = Metrics {
            precision: 0.5,
            recall: 0.5,
            f1: 0.5,
            exact_match_rate: 0.5,
        };
        let mut a = report(metrics);
        let mut b = report(metrics);
        b.latency = Latency {
            install_ms: 999,
            load_ms: 888,
            route_p50_ms: 77.7,
            route_p95_ms: 66.6,
        };

        assert_eq!(
            serde_json::to_vec(&a.scored()).expect("serialize"),
            serde_json::to_vec(&b.scored()).expect("serialize"),
            "latency must not perturb the scored part"
        );
        assert_ne!(
            serde_json::to_vec(&a).expect("serialize"),
            serde_json::to_vec(&b).expect("serialize")
        );

        a.per_case[0].correct = false;
        assert_ne!(
            serde_json::to_vec(&a.scored()).expect("serialize"),
            serde_json::to_vec(&b.scored()).expect("serialize")
        );
    }

    #[test]
    fn the_scored_part_is_byte_stable_across_repeated_serialization() {
        let r = report(Metrics::default());
        let first = serde_json::to_vec(&r.scored()).expect("serialize");
        for _ in 0..5 {
            assert_eq!(serde_json::to_vec(&r.scored()).expect("serialize"), first);
        }
    }

    #[test]
    fn markdown_names_every_case_and_the_metrics_table() {
        let r = report(Metrics {
            precision: 0.75,
            recall: 0.5,
            f1: 0.6,
            exact_match_rate: 0.5,
        });
        let md = r.to_markdown();
        assert!(md.contains("| Precision | 0.7500 |"), "{md}");
        assert!(md.contains("c1"), "{md}");
        assert!(md.contains("c2"), "{md}");
    }

    #[test]
    fn an_error_case_shows_the_error_not_an_empty_cell() {
        let mut r = report(Metrics::default());
        r.per_case[1] = CaseResult {
            id: "c2".to_string(),
            tags: vec![],
            expected: vec!["create".to_string()],
            predicted: vec![],
            correct: false,
            error: Some("router output still malformed after one corrective re-prompt".to_string()),
        };
        let md = r.to_markdown();
        assert!(md.contains("router output still malformed"), "{md}");
    }
}
