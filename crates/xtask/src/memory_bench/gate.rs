//! The memory-quality gate (spec 14 §2's `memory-quality` row) — T14-07.
//!
//! > router precision/recall on fixture set ≥ P/R `[BASELINE]`
//!
//! `P` and `R` are spec 08 §7's own open numbers ("Target P/R numbers are
//! set after the baseline run `[OPEN]`"). Mirroring `crate::bench::gate`'s
//! own rationale (O2: "collect metrics, never invent thresholds"), they are
//! **not** constants in this file — they live in
//! `fixtures/memory/baseline/thresholds.json` alongside the prose that
//! derived them and the run they came from.
//!
//! # No regression budget
//!
//! `crate::bench::gate::Thresholds` also carries an `mrr_regression_budget`
//! against a recorded v1 baseline. There is no v1 baseline here (see
//! `crate::memory_bench::report`'s module doc) — [`Thresholds`] is a floor
//! on `precision`/`recall` only, nothing to regress against yet.

use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::memory_bench::report::MemoryBenchReport;

/// The versioned gate thresholds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Thresholds {
    /// Schema version of this file.
    pub schema_version: u32,
    /// Spec 14 §2's `P`: the minimum acceptable precision.
    pub min_precision: f64,
    /// Spec 14 §2's `R`: the minimum acceptable recall.
    pub min_recall: f64,
    /// How these numbers were obtained — prose, deliberately required.
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
    /// Precision fell below the floor.
    Precision { observed_e4: i64, floor_e4: i64 },
    /// Recall fell below the floor.
    Recall { observed_e4: i64, floor_e4: i64 },
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Violation::Precision {
                observed_e4,
                floor_e4,
            } => write!(
                f,
                "precision is {:.4}, below the required {:.4}",
                *observed_e4 as f64 / 1e4,
                *floor_e4 as f64 / 1e4
            ),
            Violation::Recall {
                observed_e4,
                floor_e4,
            } => write!(
                f,
                "recall is {:.4}, below the required {:.4}",
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
    pub fn passed(&self) -> bool {
        self.violations.is_empty()
    }
}

/// Compare in units of 1e-4 so a threshold sitting exactly on a measured
/// value passes (mirrors `crate::bench::gate::e4`'s own rationale verbatim).
fn e4(value: f64) -> i64 {
    (value * 1e4).round() as i64
}

/// Evaluate `report` against `thresholds` (spec 14 §2's `memory-quality`
/// row). Checks every condition and collects all violations rather than
/// short-circuiting on the first (mirrors `crate::bench::gate::evaluate`).
pub fn evaluate(report: &MemoryBenchReport, thresholds: &Thresholds) -> GateOutcome {
    let mut violations = Vec::new();

    let precision = e4(report.metrics.precision);
    let precision_floor = e4(thresholds.min_precision);
    if precision < precision_floor {
        violations.push(Violation::Precision {
            observed_e4: precision,
            floor_e4: precision_floor,
        });
    }

    let recall = e4(report.metrics.recall);
    let recall_floor = e4(thresholds.min_recall);
    if recall < recall_floor {
        violations.push(Violation::Recall {
            observed_e4: recall,
            floor_e4: recall_floor,
        });
    }

    GateOutcome { violations }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_bench::report::{CaseResult, Latency, Provenance};
    use crate::memory_bench::score::Metrics;

    fn thresholds(min_precision: f64, min_recall: f64) -> Thresholds {
        Thresholds {
            schema_version: 1,
            min_precision,
            min_recall,
            derivation: "test".to_string(),
            derived_from: "test".to_string(),
        }
    }

    fn report_with(precision: f64, recall: f64) -> MemoryBenchReport {
        MemoryBenchReport::new(
            Provenance {
                commit: "c".to_string(),
                corpus_path: "p".to_string(),
                corpus_version: "1.0.0".to_string(),
                case_count: 1,
                model_id: "m".to_string(),
                sampling: "greedy".to_string(),
                router_version: "v0".to_string(),
                host: "h".to_string(),
            },
            Metrics {
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
            Latency::default(),
        )
    }

    #[test]
    fn a_run_meeting_both_floors_passes() {
        let r = report_with(0.8, 0.8);
        assert!(evaluate(&r, &thresholds(0.7, 0.7)).passed());
    }

    #[test]
    fn the_precision_floor_boundary_is_inclusive() {
        let exactly = report_with(0.75, 0.9);
        assert!(evaluate(&exactly, &thresholds(0.75, 0.7)).passed());

        let under = report_with(0.749, 0.9);
        let outcome = evaluate(&under, &thresholds(0.75, 0.7));
        assert!(!outcome.passed());
        assert!(matches!(outcome.violations[0], Violation::Precision { .. }));
    }

    #[test]
    fn the_recall_floor_boundary_is_inclusive() {
        let exactly = report_with(0.9, 0.75);
        assert!(evaluate(&exactly, &thresholds(0.7, 0.75)).passed());

        let under = report_with(0.9, 0.749);
        let outcome = evaluate(&under, &thresholds(0.7, 0.75));
        assert!(!outcome.passed());
        assert!(matches!(outcome.violations[0], Violation::Recall { .. }));
    }

    #[test]
    fn a_doubly_broken_run_reports_both_violations() {
        let broken = report_with(0.1, 0.1);
        let outcome = evaluate(&broken, &thresholds(0.7, 0.7));
        assert!(!outcome.passed());
        assert_eq!(outcome.violations.len(), 2, "{:?}", outcome.violations);
    }

    #[test]
    fn differences_below_the_reported_precision_do_not_flip_the_verdict() {
        let floor = 0.75;
        let noisy = report_with(floor - 1e-9, 0.9);
        assert!(evaluate(&noisy, &thresholds(floor, 0.7)).passed());
    }

    fn fixtures_path(rel: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures")
            .join(rel)
    }

    /// T14-07's real, measured baseline (never invented, O2) must clear the
    /// thresholds derived from it — the whole point of deriving a floor a
    /// few points *below* the measured run rather than exactly at it.
    #[test]
    fn the_shipped_baseline_run_passes_its_own_derived_thresholds() {
        // `run-gemma-4-e2b.json` is the *current* baseline (ADR-0006's
        // second round, T14-07 Phase 7) -- the thresholds file is derived
        // from it, not from the superseded `run.json`/`run-1.5b.json` (kept
        // on disk as historical evidence only; see gate.rs's and
        // thresholds.json's own docs for why those two no longer clear this
        // gate).
        let report: MemoryBenchReport = serde_json::from_str(
            &std::fs::read_to_string(fixtures_path("memory/baseline/run-gemma-4-e2b.json"))
                .expect("baseline run fixture readable"),
        )
        .expect("baseline run fixture parses as a report");
        let thresholds = Thresholds::load(&fixtures_path("memory/baseline/thresholds.json"))
            .expect("shipped thresholds parse");

        let outcome = evaluate(&report, &thresholds);
        assert!(
            outcome.passed(),
            "the run the thresholds were derived from must pass them: {:?}",
            outcome.violations
        );
    }

    #[test]
    fn the_shipped_thresholds_file_parses_and_documents_itself() {
        let thresholds = Thresholds::load(&fixtures_path("memory/baseline/thresholds.json"))
            .expect("shipped thresholds parse");
        assert_eq!(thresholds.schema_version, 1);
        assert!(
            !thresholds.derivation.trim().is_empty() && !thresholds.derived_from.trim().is_empty(),
            "O2 forbids a threshold without a stated derivation"
        );
        assert!((0.0..=1.0).contains(&thresholds.min_precision));
        assert!((0.0..=1.0).contains(&thresholds.min_recall));
    }
}
