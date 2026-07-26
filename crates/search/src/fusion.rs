//! Reciprocal Rank Fusion of the two legs (spec 09 §4) — T12-03.
//!
//! `score(d) = Σ_legs 1 / (k + rank_leg(d))`, `k = 60`, with the deterministic
//! tie-break `(score desc, occurrence_id asc)`.
//!
//! A pure function over two already-ranked lists: no store, no clock, no lock —
//! which is what makes the hand-calculated goldens below possible, and what
//! keeps the pipeline's own tests free of arithmetic assertions.
//!
//! # Why RRF needs nothing but ranks
//!
//! The legs score in incomparable units — BM25 (more negative is better,
//! unbounded) and a dense similarity under whichever `distance_metric` the
//! model space declares (`cosine` in `[-1, 1]`, raw `dot`, negated `l2`).
//! RRF's whole point is that it never compares them: only each document's
//! *position* within its own leg matters. That is why [`LexicalHit`] and
//! [`DenseHit`] both carry a 1-based `rank`, and why their raw scores are
//! carried for diagnostics but never fused.
//!
//! # Determinism
//!
//! Two properties, both load-bearing for T12-03's "repeated output is
//! byte-stable":
//!
//! - the accumulator is `f64`. `1/(60+rank)` is not representable in binary
//!   floating point, and in `f32` two documents whose true scores differ in the
//!   seventh digit can collapse into a tie — or, worse, compare differently
//!   depending on which leg was added first. `f64` does not make the arithmetic
//!   exact, but it moves the error far below any rank difference this fusion can
//!   produce (`1/61 - 1/62 ≈ 2.6e-4`).
//! - the sort is total: `(score desc, occurrence_id asc)`, the same tie-break
//!   both legs already apply internally (spec 09 §4). Documents with genuinely
//!   equal scores — the common case when both legs return them at the same
//!   ranks — therefore still have exactly one legal order.

use std::collections::HashMap;

use local_rag_protocol::LegRanks;
use local_rag_store::LexicalHit;

use crate::pipeline::DenseHit;

/// The RRF constant `k` (spec 09 §4 `[SPEC]`).
///
/// Damps the contribution of deep ranks: consecutive ranks differ by
/// `1/((k + r)(k + r + 1))`, which shrinks quadratically in `k + r`, so the
/// rank-1/2 gap is ≈ 6.8× the rank-100/101 gap. A larger `k` flattens the curve
/// (ranks matter less, agreement across legs matters more); a smaller one
/// sharpens it. Tunable by T12-05 alongside the BM25 weights, like every other
/// retrieval constant.
pub const RRF_K: usize = 60;

/// One fused hit: the occurrence, its RRF score, and where each leg ranked it.
#[derive(Debug, Clone, PartialEq)]
pub struct FusedHit {
    /// The occurrence both legs are keyed by.
    pub occurrence_id: String,
    /// `Σ_legs 1 / (RRF_K + rank)`.
    pub score: f64,
    /// The per-leg ranks that produced `score` (spec 09 §7's `legs`).
    pub legs: LegRanks,
}

/// One leg's contribution to a document's score.
fn contribution(rank: usize) -> f64 {
    1.0 / (RRF_K + rank) as f64
}

/// Fuse the two legs (spec 09 §4) and return the best `limit` hits.
///
/// Merging is by `occurrence_id`: a document found by both legs is **one**
/// result carrying both ranks, never two. `limit` — not the legs' own
/// `candidate_depth` — bounds the output: the deeper candidate lists exist so
/// fusion has material to reorder, not so the caller receives them.
pub fn rrf(lexical: &[LexicalHit], dense: &[DenseHit], limit: usize) -> Vec<FusedHit> {
    let mut by_occurrence: HashMap<&str, LegRanks> = HashMap::new();
    for hit in lexical {
        by_occurrence
            .entry(hit.occurrence_id.as_str())
            .or_default()
            .lexical = Some(hit.rank);
    }
    for hit in dense {
        by_occurrence
            .entry(hit.occurrence_id.as_str())
            .or_default()
            .dense = Some(hit.rank);
    }

    let mut fused: Vec<FusedHit> = by_occurrence
        .into_iter()
        .map(|(occurrence_id, legs)| FusedHit {
            occurrence_id: occurrence_id.to_string(),
            score: legs.lexical.map(contribution).unwrap_or(0.0)
                + legs.dense.map(contribution).unwrap_or(0.0),
            legs,
        })
        .collect();

    // `(score desc, occurrence_id asc)` — spec 09 §4's deterministic tie-break.
    // `partial_cmp` cannot be `None` here: every score is a sum of one or two
    // positive reciprocals, so no `NaN` can enter.
    fused.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.occurrence_id.cmp(&b.occurrence_id))
    });
    fused.truncate(limit);
    fused
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lex(occurrence_id: &str, rank: usize) -> LexicalHit {
        LexicalHit {
            occurrence_id: occurrence_id.to_string(),
            rank,
            bm25: -(rank as f64),
        }
    }

    fn dense(occurrence_id: &str, rank: usize) -> DenseHit {
        DenseHit {
            occurrence_id: occurrence_id.to_string(),
            rank,
            score: 1.0 / rank as f32,
        }
    }

    fn ids(hits: &[FusedHit]) -> Vec<&str> {
        hits.iter().map(|h| h.occurrence_id.as_str()).collect()
    }

    /// The formula, by hand: a document ranked 1 by the lexical leg and 3 by
    /// the dense leg scores exactly `1/61 + 1/63`.
    #[test]
    fn a_document_in_both_legs_sums_both_reciprocals() {
        let fused = rrf(&[lex("a", 1)], &[dense("a", 3)], 10);
        assert_eq!(fused.len(), 1, "one occurrence ⇒ one result");
        let expected = 1.0 / 61.0 + 1.0 / 63.0;
        assert!(
            (fused[0].score - expected).abs() < 1e-12,
            "{} != {expected}",
            fused[0].score
        );
        assert_eq!(
            fused[0].legs,
            LegRanks {
                lexical: Some(1),
                dense: Some(3),
            }
        );
    }

    /// A document only one leg found scores that leg's term alone.
    #[test]
    fn a_single_leg_document_scores_one_reciprocal() {
        let fused = rrf(&[lex("a", 2)], &[], 10);
        assert!((fused[0].score - 1.0 / 62.0).abs() < 1e-12);
        assert_eq!(
            fused[0].legs,
            LegRanks {
                lexical: Some(2),
                dense: None,
            }
        );
    }

    /// The canonical RRF property: agreement beats a single leg's top hit.
    /// `b` is second in **both** legs (`1/62 + 1/62 ≈ 0.03226`), while `a` and
    /// `c` are each first in **one** leg only (`1/61 ≈ 0.01639` apiece) — so `b`
    /// leads, and the two equal-scoring singles fall back to the
    /// `occurrence_id` tie-break.
    #[test]
    fn agreement_across_legs_outranks_a_single_legs_top_hit() {
        let fused = rrf(
            &[lex("a", 1), lex("b", 2)],
            &[dense("b", 2), dense("c", 1)],
            10,
        );
        assert_eq!(ids(&fused), ["b", "a", "c"]);
        assert!((fused[0].score - 2.0 / 62.0).abs() < 1e-12);
        assert!((fused[1].score - 1.0 / 61.0).abs() < 1e-12);
        assert_eq!(fused[1].score, fused[2].score, "a and c genuinely tie");
    }

    /// Duplicates across legs merge into one result, never two.
    #[test]
    fn duplicates_merge_rather_than_double_count() {
        let fused = rrf(
            &[lex("a", 1), lex("b", 2)],
            &[dense("a", 1), dense("b", 2)],
            10,
        );
        assert_eq!(ids(&fused), ["a", "b"]);
        assert_eq!(fused.len(), 2);
        assert_eq!(
            fused[0].legs,
            LegRanks {
                lexical: Some(1),
                dense: Some(1),
            }
        );
    }

    /// Genuinely equal scores are ordered by `occurrence_id` ascending — the
    /// only thing that makes repeated output byte-stable when every document
    /// ties, which is exactly what happens when both legs return the same set
    /// at the same ranks.
    #[test]
    fn equal_scores_break_ties_by_occurrence_id() {
        // Fed in descending id order; the result must be ascending.
        let fused = rrf(&[lex("c", 1), lex("b", 1), lex("a", 1)], &[], 10);
        assert_eq!(ids(&fused), ["a", "b", "c"]);
        assert!(fused.windows(2).all(|w| w[0].score == w[1].score));
    }

    /// `HashMap` iteration order is randomized per process; the sort must make
    /// that invisible. Fusing the same input repeatedly gives the same order.
    #[test]
    fn repeated_fusion_of_the_same_input_is_identical() {
        let lexical: Vec<LexicalHit> = (0..40).map(|i| lex(&format!("occ-{i:02}"), 1)).collect();
        let densely: Vec<DenseHit> = (0..40).map(|i| dense(&format!("occ-{i:02}"), 1)).collect();
        let first = rrf(&lexical, &densely, 40);
        for _ in 0..8 {
            assert_eq!(rrf(&lexical, &densely, 40), first);
        }
    }

    /// `limit` bounds the response, not the legs' candidate depth.
    #[test]
    fn limit_truncates_after_fusion_not_before() {
        let lexical: Vec<LexicalHit> = (1..=10).map(|i| lex(&format!("l{i}"), i)).collect();
        let densely: Vec<DenseHit> = (1..=10).map(|i| dense(&format!("d{i}"), i)).collect();
        let fused = rrf(&lexical, &densely, 3);
        assert_eq!(fused.len(), 3);
        // The two rank-1 documents lead, tie-broken by id; then the rank-2 pair.
        assert_eq!(ids(&fused), ["d1", "l1", "d2"]);
    }

    #[test]
    fn an_empty_leg_leaves_the_other_leg_untouched() {
        let lexical: Vec<LexicalHit> = (1..=3).map(|i| lex(&format!("l{i}"), i)).collect();
        let with_empty_dense = rrf(&lexical, &[], 10);
        assert_eq!(ids(&with_empty_dense), ["l1", "l2", "l3"]);
        assert!(with_empty_dense.iter().all(|h| h.legs.dense.is_none()));
    }

    #[test]
    fn two_empty_legs_fuse_to_nothing() {
        assert!(rrf(&[], &[], 10).is_empty());
    }

    #[test]
    fn a_zero_limit_returns_nothing() {
        assert!(rrf(&[lex("a", 1)], &[dense("a", 1)], 0).is_empty());
    }

    /// Rank damping: consecutive ranks matter less the deeper they are. The gap
    /// is `1/((k+r)(k+r+1))`, so it shrinks quadratically in `k + rank` — with
    /// `k = 60` the rank-1/2 gap is ≈ 6.8× the rank-100/101 gap, and the
    /// contribution itself is strictly decreasing everywhere.
    #[test]
    fn deep_ranks_contribute_progressively_less() {
        let top = contribution(1) - contribution(2);
        let deep = contribution(100) - contribution(101);
        assert!(top > deep, "top={top}, deep={deep}");
        assert!((top / deep - (160.0 * 161.0) / (61.0 * 62.0)).abs() < 1e-9);
        assert!(
            (1..500).all(|r| contribution(r) > contribution(r + 1)),
            "contribution must be strictly decreasing in rank"
        );
    }
}
