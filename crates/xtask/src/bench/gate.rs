//! The quality gate (spec 14 §2's `quality` row) — T12-05.
//!
//! > MRR not worse than v1 baseline by more than X; Recall@5 ≥ Y `[BASELINE]`
//!
//! `X` and `Y` are open question O2's search half. O2's standing rule is
//! "collect metrics, never invent thresholds", so they are **not** constants in
//! this file: they live in `fixtures/search/baseline/thresholds.json`, together
//! with the rule that derived them and the run they were derived from. That is
//! what makes T12-05's "tuning changes are versioned" true — a retune is a diff
//! in a committed file with a stated justification, not an edited literal.

use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::bench::report::BenchReport;

/// The versioned gate thresholds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Thresholds {
    /// Schema version of this file.
    pub schema_version: u32,
    /// Spec 14 §2's `X`: how far below the v1 baseline v2's MRR may fall.
    pub mrr_regression_budget: f64,
    /// Spec 14 §2's `Y`: the minimum acceptable Recall@5.
    pub min_recall_at_5: f64,
    /// How these numbers were obtained — prose, deliberately required so a
    /// future reader never has to guess.
    pub derivation: String,
    /// The run they were derived from.
    pub derived_from: String,
}

impl Thresholds {
    /// Load thresholds from `path`.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{path:?}: {e}"))?;
        serde_json::from_str(&text).map_err(|e| format!("{path:?}: {e}"))
    }
}

/// One violated gate condition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Violation {
    /// MRR fell further below the baseline than the budget allows.
    MrrRegression {
        /// How far below the baseline v2 landed, in MRR points ×10⁴.
        observed_e4: i64,
        /// The budget, ×10⁴.
        budget_e4: i64,
    },
    /// Recall@5 is below the floor.
    RecallAt5 {
        /// Observed Recall@5, ×10⁴.
        observed_e4: i64,
        /// The floor, ×10⁴.
        floor_e4: i64,
    },
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Violation::MrrRegression {
                observed_e4,
                budget_e4,
            } => write!(
                f,
                "MRR regressed by {:.4} against the v1 baseline, budget is {:.4}",
                *observed_e4 as f64 / 1e4,
                *budget_e4 as f64 / 1e4
            ),
            Violation::RecallAt5 {
                observed_e4,
                floor_e4,
            } => write!(
                f,
                "Recall@5 is {:.4}, below the required {:.4}",
                *observed_e4 as f64 / 1e4,
                *floor_e4 as f64 / 1e4
            ),
        }
    }
}

/// The gate's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateOutcome {
    /// Every condition that failed; empty means the gate passed.
    pub violations: Vec<Violation>,
}

impl GateOutcome {
    /// Whether the gate passed.
    pub fn passed(&self) -> bool {
        self.violations.is_empty()
    }
}

/// Compare in units of 1e-4 so a threshold sitting exactly on a measured value
/// passes, instead of failing on the last bit of a float.
///
/// Both metrics are reported to four decimals, so this is the precision the
/// thresholds are actually stated at; comparing raw `f64`s would make
/// "Recall@5 ≥ 0.8367" fail against a measured 0.83673469… by an amount no one
/// can see or act on.
fn e4(value: f64) -> i64 {
    (value * 1e4).round() as i64
}

/// Evaluate `report` against `thresholds` (spec 14 §2's `quality` row).
pub fn evaluate(report: &BenchReport, thresholds: &Thresholds) -> GateOutcome {
    let mut violations = Vec::new();

    let regression = e4(report.diff.mrr_regression());
    let budget = e4(thresholds.mrr_regression_budget);
    if regression > budget {
        violations.push(Violation::MrrRegression {
            observed_e4: regression,
            budget_e4: budget,
        });
    }

    let recall = e4(report.metrics.recall_at_5);
    let floor = e4(thresholds.min_recall_at_5);
    if recall < floor {
        violations.push(Violation::RecallAt5 {
            observed_e4: recall,
            floor_e4: floor,
        });
    }

    GateOutcome { violations }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::report::{Baseline, Latency, Provenance};
    use crate::bench::score::Metrics;

    fn thresholds(budget: f64, floor: f64) -> Thresholds {
        Thresholds {
            schema_version: 1,
            mrr_regression_budget: budget,
            min_recall_at_5: floor,
            derivation: "test".to_string(),
            derived_from: "test".to_string(),
        }
    }

    fn report_with(mrr: f64, recall_at_5: f64) -> BenchReport {
        BenchReport::new(
            Provenance {
                v2_commit: "c".to_string(),
                corpus_path: "p".to_string(),
                corpus_commit: "g".to_string(),
                corpus_version: "1.0.0".to_string(),
                model_id: "m".to_string(),
                mode: "hybrid".to_string(),
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

    #[test]
    fn a_run_matching_the_baseline_passes() {
        let r = report_with(Baseline::V1.mrr, Baseline::V1.hit_at_5);
        let outcome = evaluate(&r, &thresholds(0.02, 0.80));
        assert!(outcome.passed(), "{:?}", outcome.violations);
    }

    #[test]
    fn a_better_run_passes_and_consumes_no_budget() {
        let r = report_with(Baseline::V1.mrr + 0.1, 0.95);
        assert!(evaluate(&r, &thresholds(0.0, 0.80)).passed());
    }

    /// Exactly on the threshold passes; a hair below fails. Both directions,
    /// because a gate that is wrong on its own boundary is worse than no gate.
    #[test]
    fn the_mrr_budget_boundary_is_inclusive() {
        let budget = 0.02;
        let exactly = report_with(Baseline::V1.mrr - budget, 0.90);
        assert!(
            evaluate(&exactly, &thresholds(budget, 0.80)).passed(),
            "a regression of exactly the budget is allowed"
        );

        let over = report_with(Baseline::V1.mrr - budget - 0.001, 0.90);
        let outcome = evaluate(&over, &thresholds(budget, 0.80));
        assert!(!outcome.passed());
        assert!(matches!(
            outcome.violations[0],
            Violation::MrrRegression { .. }
        ));
    }

    #[test]
    fn the_recall_floor_boundary_is_inclusive() {
        let floor = 0.8367;
        let exactly = report_with(Baseline::V1.mrr, floor);
        assert!(evaluate(&exactly, &thresholds(0.02, floor)).passed());

        let under = report_with(Baseline::V1.mrr, floor - 0.001);
        let outcome = evaluate(&under, &thresholds(0.02, floor));
        assert!(!outcome.passed());
        assert!(matches!(outcome.violations[0], Violation::RecallAt5 { .. }));
    }

    /// **The card's own requirement**: a deliberately degraded run must fail the
    /// gate, and the failure must name every condition it broke rather than
    /// stopping at the first.
    #[test]
    fn a_degraded_run_fails_the_gate_on_every_broken_condition() {
        // Half the queries lost entirely: MRR collapses and Recall@5 with it.
        let degraded = report_with(Baseline::V1.mrr / 2.0, Baseline::V1.hit_at_5 / 2.0);
        let outcome = evaluate(&degraded, &thresholds(0.02, 0.80));

        assert!(!outcome.passed());
        assert_eq!(outcome.violations.len(), 2, "{:?}", outcome.violations);
        assert!(
            outcome
                .violations
                .iter()
                .any(|v| matches!(v, Violation::MrrRegression { .. }))
        );
        assert!(
            outcome
                .violations
                .iter()
                .any(|v| matches!(v, Violation::RecallAt5 { .. }))
        );
        // The message states both the observation and the limit, so a CI log is
        // actionable without re-running anything.
        let rendered = outcome.violations[0].to_string();
        assert!(rendered.contains("MRR regressed by"), "{rendered}");
        assert!(rendered.contains("budget is"), "{rendered}");
    }

    /// Float noise below the reported precision must not decide a gate.
    #[test]
    fn differences_below_the_reported_precision_do_not_flip_the_verdict() {
        let floor = 0.8367;
        let noisy = report_with(Baseline::V1.mrr, floor - 1e-9);
        assert!(
            evaluate(&noisy, &thresholds(0.02, floor)).passed(),
            "a 1e-9 shortfall is not a regression anyone can act on"
        );
    }

    /// The card's requirement, against a **committed** fixture rather than an
    /// in-memory one: `run-degraded.json` is a deliberately halved run, and the
    /// shipped thresholds must reject it. This is what keeps the gate from
    /// silently becoming a no-op if a threshold is ever mis-edited — a change
    /// that let the degraded fixture through would fail this test.
    #[test]
    fn the_shipped_thresholds_reject_the_committed_degraded_run() {
        let report: BenchReport = serde_json::from_str(
            &std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../fixtures/search/baseline/run-degraded.json"),
            )
            .expect("degraded fixture readable"),
        )
        .expect("degraded fixture parses as a report");

        let thresholds = Thresholds::load(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../fixtures/search/baseline/thresholds.json"),
        )
        .expect("shipped thresholds parse");

        let outcome = evaluate(&report, &thresholds);
        assert!(
            !outcome.passed(),
            "a run at half the baseline must never pass the gate"
        );
        assert_eq!(outcome.violations.len(), 2, "{:?}", outcome.violations);
    }

    #[test]
    fn the_shipped_thresholds_file_parses_and_documents_itself() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/search/baseline/thresholds.json");
        let t = Thresholds::load(&path).expect("shipped thresholds parse");
        assert_eq!(t.schema_version, 1);
        assert!(
            !t.derivation.trim().is_empty() && !t.derived_from.trim().is_empty(),
            "O2 forbids a threshold without a stated derivation"
        );
        assert!((0.0..=1.0).contains(&t.min_recall_at_5));
        assert!(t.mrr_regression_budget >= 0.0);
    }
}
