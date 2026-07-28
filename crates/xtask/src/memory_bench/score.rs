//! Matching semantics and metric math for the memory-quality benchmark (spec
//! 08 §7, 14 §2's `memory-quality` row) — T14-07.
//!
//! # Op-kind vocabulary (broader than spec 08 §7's illustrative list)
//!
//! Spec 08 §7 names `create | reinforce | supersede | noop` as the ops this
//! benchmark covers. That list is illustrative, not exhaustive: spec 08 §3
//! `[FIXED]` already lists the full transactional op set (`create |
//! reinforce | resolve | supersede | retract | noop`), and 08 §4's router
//! output vocabulary adds `propose_candidate` on top of that. Scoring against
//! only the four named ops would hide exactly the failures this benchmark
//! exists to catch — a `create` that should have been downgraded to
//! `propose_candidate` by [`local_rag_memory::guard`]'s placement rules would
//! silently count as a correct `create` if `propose_candidate` were not its
//! own distinguishable class. [`op_kind`] therefore tags the full seven-way
//! vocabulary [`local_rag_store::GeneratedOp`]/[`local_rag_store::ProposedOperation`]
//! can actually produce.
//!
//! # Multiset matching, not positional
//!
//! A [`local_rag_memory::router::route`] call returns an *ordered* list, but
//! a small local model has no obligation to emit ops in exactly the order a
//! fixture author wrote observations in. Each case's `expected`/`predicted`
//! op-kind lists are therefore compared as multisets (bags): the same op
//! kind appearing twice in both counts as two true positives, not one.
//!
//! # Micro-averaged precision/recall
//!
//! True/false positives and false negatives are summed across every case
//! *before* dividing — a case with three expected ops is not diluted to the
//! same weight as a case with one, matching how spec 08 §7 talks about
//! "precision/recall on this set" as one pooled measurement, not a per-case
//! average.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use local_rag_store::{GeneratedOp, ProposedOperation};

/// Every op kind [`op_kind`] can produce — the benchmark's declared class
/// list (see the module doc for why it is a superset of spec 08 §7's
/// illustrative four).
pub const CLASSES: [&str; 7] = [
    "create",
    "reinforce",
    "resolve",
    "retract",
    "supersede",
    "noop",
    "propose_candidate",
];

/// Tag one [`GeneratedOp`] with its class name.
pub fn op_kind(op: &GeneratedOp) -> &'static str {
    match op {
        GeneratedOp::Materialize { operation, .. } => match operation {
            ProposedOperation::Create { .. } => "create",
            ProposedOperation::Reinforce { .. } => "reinforce",
            ProposedOperation::Resolve { .. } => "resolve",
            ProposedOperation::Retract { .. } => "retract",
            ProposedOperation::Supersede { .. } => "supersede",
        },
        GeneratedOp::Noop => "noop",
        GeneratedOp::ProposeCandidate { .. } => "propose_candidate",
    }
}

fn multiset(items: &[String]) -> HashMap<&str, usize> {
    let mut m = HashMap::new();
    for i in items {
        *m.entry(i.as_str()).or_insert(0) += 1;
    }
    m
}

/// One case's contribution to the aggregate counts (see the module doc's
/// "micro-averaged" section) plus whether it was an exact bag match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CaseTally {
    pub true_positive: usize,
    pub false_positive: usize,
    pub false_negative: usize,
    pub exact_match: bool,
}

/// Score one case: `expected`/`predicted` op-kind lists, compared as
/// multisets (see the module doc).
pub fn score_case(expected: &[String], predicted: &[String]) -> CaseTally {
    let e = multiset(expected);
    let p = multiset(predicted);

    let true_positive: usize = e
        .iter()
        .map(|(kind, &ec)| ec.min(p.get(kind).copied().unwrap_or(0)))
        .sum();
    let false_positive = predicted.len().saturating_sub(true_positive);
    let false_negative = expected.len().saturating_sub(true_positive);
    let exact_match = e == p;

    CaseTally {
        true_positive,
        false_positive,
        false_negative,
        exact_match,
    }
}

/// Aggregate metrics across every case's [`CaseTally`].
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Metrics {
    pub precision: f64,
    pub recall: f64,
    pub f1: f64,
    /// Fraction of cases whose predicted op-kind bag exactly matched the
    /// expected bag — a stricter, per-case signal alongside the pooled P/R.
    pub exact_match_rate: f64,
}

/// Micro-average `tallies` into one [`Metrics`] value. An empty slice scores
/// as all zeros, never `NaN` (mirrors `crate::bench::score::aggregate`'s own
/// "empty corpus is zero, not a division-by-zero panic" convention).
pub fn aggregate(tallies: &[CaseTally]) -> Metrics {
    if tallies.is_empty() {
        return Metrics::default();
    }
    let tp: usize = tallies.iter().map(|t| t.true_positive).sum();
    let fp: usize = tallies.iter().map(|t| t.false_positive).sum();
    let fn_: usize = tallies.iter().map(|t| t.false_negative).sum();

    let precision = if tp + fp == 0 {
        0.0
    } else {
        tp as f64 / (tp + fp) as f64
    };
    let recall = if tp + fn_ == 0 {
        0.0
    } else {
        tp as f64 / (tp + fn_) as f64
    };
    let f1 = if precision + recall == 0.0 {
        0.0
    } else {
        2.0 * precision * recall / (precision + recall)
    };
    let exact_match_rate =
        tallies.iter().filter(|t| t.exact_match).count() as f64 / tallies.len() as f64;

    Metrics {
        precision,
        recall,
        f1,
        exact_match_rate,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn an_exact_single_op_match_is_a_true_positive() {
        let t = score_case(&v(&["create"]), &v(&["create"]));
        assert_eq!(t.true_positive, 1);
        assert_eq!(t.false_positive, 0);
        assert_eq!(t.false_negative, 0);
        assert!(t.exact_match);
    }

    #[test]
    fn a_wrong_single_op_is_both_a_false_positive_and_a_false_negative() {
        let t = score_case(&v(&["create"]), &v(&["noop"]));
        assert_eq!(t.true_positive, 0);
        assert_eq!(t.false_positive, 1);
        assert_eq!(t.false_negative, 1);
        assert!(!t.exact_match);
    }

    #[test]
    fn an_extra_predicted_op_is_a_false_positive_only() {
        let t = score_case(&v(&["create"]), &v(&["create", "noop"]));
        assert_eq!(t.true_positive, 1);
        assert_eq!(t.false_positive, 1);
        assert_eq!(t.false_negative, 0);
        assert!(!t.exact_match);
    }

    #[test]
    fn a_missing_expected_op_is_a_false_negative_only() {
        let t = score_case(&v(&["create", "noop"]), &v(&["create"]));
        assert_eq!(t.true_positive, 1);
        assert_eq!(t.false_positive, 0);
        assert_eq!(t.false_negative, 1);
    }

    #[test]
    fn duplicate_kinds_are_matched_as_a_multiset_not_deduplicated() {
        // Two expected "reinforce" ops, only one predicted -- exactly one TP,
        // one FN. A naive set (not multiset) comparison would wrongly call
        // this an exact match.
        let t = score_case(&v(&["reinforce", "reinforce"]), &v(&["reinforce"]));
        assert_eq!(t.true_positive, 1);
        assert_eq!(t.false_negative, 1);
        assert!(!t.exact_match);
    }

    #[test]
    fn order_does_not_matter() {
        let t = score_case(
            &v(&["create", "noop", "reinforce"]),
            &v(&["reinforce", "create", "noop"]),
        );
        assert!(t.exact_match);
        assert_eq!(t.true_positive, 3);
    }

    #[test]
    fn aggregate_of_no_cases_is_zero_not_nan() {
        let m = aggregate(&[]);
        assert_eq!(m, Metrics::default());
        assert_eq!(m.precision, 0.0);
    }

    #[test]
    fn aggregate_micro_averages_across_cases_not_per_case() {
        // Case A: 3 expected, all correct. Case B: 1 expected, wrong.
        // Micro precision/recall = 3/(3+1) since totals pool before dividing,
        // not (1.0 + 0.0) / 2 as a per-case macro average would give.
        let a = score_case(
            &v(&["create", "noop", "reinforce"]),
            &v(&["create", "noop", "reinforce"]),
        );
        let b = score_case(&v(&["retract"]), &v(&["noop"]));
        let m = aggregate(&[a, b]);
        assert!((m.precision - 0.75).abs() < 1e-12, "{m:?}");
        assert!((m.recall - 0.75).abs() < 1e-12, "{m:?}");
    }

    #[test]
    fn every_op_kind_variant_tags_to_a_distinct_class() {
        use local_rag_store::{MemoryKind, ScopeKind};

        let create = GeneratedOp::Materialize {
            operation: ProposedOperation::Create {
                memory_id: "m1".to_string(),
                kind: MemoryKind::Fact.as_str().to_string(),
                text: "t".to_string(),
                canonical_key: None,
                scope_kind: ScopeKind::Global.as_str().to_string(),
                scope_owner_id: local_rag_store::GLOBAL_SCOPE_OWNER_ID.to_string(),
                confidence: 0.5,
                importance: 0.5,
                valid_from_tree: None,
                last_verified_tree: None,
            },
            evidence_observation_ids: vec![],
        };
        assert_eq!(op_kind(&create), "create");
        assert_eq!(op_kind(&GeneratedOp::Noop), "noop");
        let propose = GeneratedOp::ProposeCandidate {
            candidate_id: "c1".to_string(),
            operation: match &create {
                GeneratedOp::Materialize { operation, .. } => operation.clone(),
                _ => unreachable!(),
            },
            conflicts: vec![],
            evidence_observation_ids: vec![],
        };
        assert_eq!(op_kind(&propose), "propose_candidate");
    }

    #[test]
    fn classes_has_no_duplicates() {
        let mut sorted = CLASSES.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), CLASSES.len());
    }
}
