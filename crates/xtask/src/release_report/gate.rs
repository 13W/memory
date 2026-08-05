//! Thin gate evaluation for the release report (T17-05): re-evaluates only
//! the two sub-gates that already have a committed pass/fail threshold —
//! `quality` (`crate::bench::gate`) and `memory-quality`
//! (`crate::memory_bench::gate`). `latency`/`resources` are recorded as this
//! release's first-established v2 baseline and are never gated (see the
//! `release_report` module doc); `reliability`/`consistency`/`sharing`/
//! `idempotency`/`rebuild` are [`crate::release_report::report::TestCitation`]s
//! built directly by `crate::release_report::run`, not evaluated here.

use crate::bench::report::BenchReport;
use crate::memory_bench::report::MemoryBenchReport;
use crate::release_report::report::{MemoryQualityGateSummary, QualityGateSummary};

/// Evaluate `report` against the shipped search-quality thresholds
/// (`fixtures/search/baseline/thresholds.json`). Mirrors `run_bench`'s own
/// "the run *is* the evidence thresholds are derived from" fallback (O2):
/// a missing thresholds file is not a failure, it means this run is
/// establishing the baseline.
pub fn evaluate_quality(report: BenchReport) -> QualityGateSummary {
    match crate::bench::gate::Thresholds::load(&crate::bench::thresholds_path()) {
        Ok(thresholds) => {
            let passed = crate::bench::gate::evaluate(&report, &thresholds).passed();
            QualityGateSummary {
                report,
                gated: true,
                passed,
            }
        }
        Err(_) => QualityGateSummary {
            report,
            gated: false,
            passed: true,
        },
    }
}

/// Evaluate `report` against the shipped memory-router thresholds
/// (`fixtures/memory/baseline/thresholds.json`). Mirrors
/// [`evaluate_quality`]'s own O2 fallback.
pub fn evaluate_memory_quality(report: MemoryBenchReport) -> MemoryQualityGateSummary {
    match crate::memory_bench::gate::Thresholds::load(&crate::memory_bench::thresholds_path()) {
        Ok(thresholds) => {
            let passed = crate::memory_bench::gate::evaluate(&report, &thresholds).passed();
            MemoryQualityGateSummary {
                report,
                gated: true,
                passed,
            }
        }
        Err(_) => MemoryQualityGateSummary {
            report,
            gated: false,
            passed: true,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::report::{Baseline, Latency, Provenance};
    use crate::bench::score::Metrics;

    fn bench_report(mrr: f64, recall_at_5: f64) -> BenchReport {
        BenchReport::new(
            Provenance {
                v2_commit: "c".to_string(),
                corpus_path: "p".to_string(),
                corpus_commit: "g".to_string(),
                corpus_subdir: None,
                corpus_version: "1.0.0".to_string(),
                model_id: "m".to_string(),
                mode: "hybrid".to_string(),
                dense_kind: "code_raw".to_string(),
                fusion_lexical_weight: None,
                files_indexed: 1,
                occurrences: 1,
                host: "h".to_string(),
            },
            Metrics {
                hit_at_1: 0.0,
                hit_at_3: 0.0,
                hit_at_5: recall_at_5,
                mrr,
                recall_at_5,
            },
            Vec::new(),
            Latency::default(),
        )
    }

    /// The shipped thresholds already exist (T12-05), so a run matching the
    /// baseline must come back `gated: true, passed: true` — never a silent
    /// "no thresholds yet" fallback for a repository that has them.
    #[test]
    fn a_run_matching_the_shipped_baseline_is_gated_and_passes() {
        let summary = evaluate_quality(bench_report(Baseline::V1.mrr, Baseline::V1.hit_at_5));
        assert!(summary.gated);
        assert!(summary.passed);
    }

    #[test]
    fn a_halved_run_is_gated_and_fails() {
        let summary = evaluate_quality(bench_report(
            Baseline::V1.mrr / 2.0,
            Baseline::V1.hit_at_5 / 2.0,
        ));
        assert!(summary.gated);
        assert!(!summary.passed);
    }

    use crate::memory_bench::report::{
        CaseResult, Latency as MemLatency, Provenance as MemProvenance,
    };
    use crate::memory_bench::score::Metrics as MemMetrics;

    fn memory_report(precision: f64, recall: f64) -> MemoryBenchReport {
        MemoryBenchReport::new(
            MemProvenance {
                commit: "c".to_string(),
                corpus_path: "p".to_string(),
                corpus_version: "1.0.0".to_string(),
                case_count: 1,
                model_id: "m".to_string(),
                sampling: "greedy".to_string(),
                router_version: "v0".to_string(),
                host: "h".to_string(),
            },
            MemMetrics {
                precision,
                recall,
                f1: 0.0,
                exact_match_rate: 0.0,
            },
            vec![CaseResult {
                id: "c1".to_string(),
                tags: vec![],
                expected: vec!["create".to_string()],
                predicted: vec!["create".to_string()],
                correct: true,
                error: None,
            }],
            MemLatency::default(),
        )
    }

    #[test]
    fn the_shipped_memory_baseline_is_gated_and_passes() {
        let report: MemoryBenchReport = serde_json::from_str(
            &std::fs::read_to_string(
                crate::memory_bench::baseline_dir().join("run-gemma-4-e2b.json"),
            )
            .expect("baseline run fixture readable"),
        )
        .expect("baseline run fixture parses");
        let summary = evaluate_memory_quality(report);
        assert!(summary.gated);
        assert!(summary.passed);
    }

    #[test]
    fn a_broken_memory_run_is_gated_and_fails() {
        let summary = evaluate_memory_quality(memory_report(0.01, 0.01));
        assert!(summary.gated);
        assert!(!summary.passed);
    }
}
