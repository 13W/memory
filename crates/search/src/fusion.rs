//! Reciprocal Rank Fusion of the two legs (spec 09 §4) — T12-03, D-018.
//!
//! `score(d) = Σ_legs w_leg / (k + rank_leg(d))`, `k = 60`, with the
//! deterministic tie-break `(score desc, occurrence_id asc)`. The per-leg
//! weights are D-018's; [`FusionWeights`] carries both the numbers and the rule
//! they follow from.
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

/// The dense rank beyond which a lexical rank-1 hit may no longer displace the
/// dense leg's own rank-1 hit (D-018's displacement rule; see
/// [`FusionWeights::for_displacement_depth`]).
///
/// Reads as a policy rather than a number: *lexical evidence can overrule the
/// dense leg's first result only when the dense leg itself ranks the challenger
/// second.* The weight follows from it arithmetically — 0.0161 at `k = 60`.
///
/// **Why so shallow.** The depth was chosen by a rule fixed before the numbers
/// were seen: the largest derived depth at which the hybrid does not score below
/// its own dense leg on the 49-query benchmark. Measured, every deeper policy
/// costs quality — 50 → MRR 0.6255, 20 → 0.6378, 10 → 0.6622, 5 → 0.6667,
/// 3 → 0.6905, 2 → 0.7007, and the dense leg alone is 0.7007. That corpus is
/// entirely natural-language, i.e. BM25's worst case, so this says the lexical
/// leg has nothing to add *there* — not that it has nothing to add. It is still
/// the only leg that answers when the dense one is degraded (spec 02 §6), and
/// the corpus contains no identifier queries to speak for it.
///
/// At depth 2 the leg is damped, not muted: it cannot unseat the dense leader,
/// but it still reorders deeper ranks — where the reciprocal gaps are small and
/// the dense leg is least certain — and still contributes documents the dense
/// leg never returned.
pub const DISPLACEMENT_DEPTH: usize = 2;

/// One fused hit: the occurrence, its RRF score, and where each leg ranked it.
#[derive(Debug, Clone, PartialEq)]
pub struct FusedHit {
    /// The occurrence both legs are keyed by.
    pub occurrence_id: String,
    /// `Σ_legs w_leg / (RRF_K + rank_leg)`.
    pub score: f64,
    /// The per-leg ranks that produced `score` (spec 09 §7's `legs`).
    pub legs: LegRanks,
}

/// How much each leg's rank counts in the fusion sum (spec 09 §4, D-018).
///
/// # Why the legs are not equal
///
/// Unweighted RRF assumes legs of comparable strength. Measured on the 49-query
/// benchmark after D-017 they are not: the dense leg scores MRR 0.7007, the
/// lexical leg 0.4344, and the unweighted hybrid lands at 0.5721 — *below its
/// own dense leg*. The arithmetic says why. A document the dense leg ranks first
/// and the lexical leg never returns scores `1/61`; a document the lexical leg
/// ranks first and the dense leg ranks 50th scores `1/61 + 1/110` and wins. Every
/// weak-leg vote is therefore a vote against the strong leg's ordering, and per
/// query the fusion demoted 15 of the 49 (13 of them from dense rank 1).
///
/// # Where the number comes from
///
/// Not from a sweep over the benchmark — that corpus is single-relevant and
/// entirely natural-language, i.e. BM25's worst case and the fastest way to
/// overfit. The weight is derived from [`DISPLACEMENT_DEPTH`] by
/// [`FusionWeights::for_displacement_depth`]; the benchmark only *checks* the
/// derived value against neighbouring policies.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FusionWeights {
    /// Weight on the lexical leg's reciprocal.
    pub lexical: f64,
    /// Weight on the dense leg's reciprocal.
    pub dense: f64,
}

impl FusionWeights {
    /// Both legs count the same — spec 09 §4's formula before D-018.
    ///
    /// Kept as a named value because the arithmetic goldens in this module are
    /// hand-calculated against it, and because "unweighted" is the thing D-018
    /// argues against: it should be stateable, not merely absent.
    pub const UNWEIGHTED: Self = Self {
        lexical: 1.0,
        dense: 1.0,
    };

    /// The heaviest lexical weight that still satisfies the displacement rule
    /// for `depth`: *a document the dense leg ranks first is not displaced by a
    /// document the lexical leg ranks first unless the dense leg also ranks the
    /// challenger within its top `depth`.*
    ///
    /// Writing the incumbent's and challenger's scores out and solving:
    ///
    /// ```text
    /// w_l/(k+1) + w_d/(k+depth) ≤ w_d/(k+1)
    /// w_l                       ≤ w_d · [1 − (k+1)/(k+depth)]
    /// ```
    ///
    /// With `k = 60` and `w_d = 1`: depth 10 → 0.129, depth 20 → 0.238,
    /// depth 50 → 0.446. Each is a policy one can say out loud, which is the
    /// point — the alternative was picking the argmax of a 49-query curve.
    ///
    /// `depth <= 1` yields `0.0`: "may never displace" leaves the lexical leg no
    /// weight at all, and the formula says so rather than going negative.
    pub fn for_displacement_depth(depth: usize) -> Self {
        let dense = 1.0;
        let ratio = (RRF_K + 1) as f64 / (RRF_K + depth.max(1)) as f64;
        Self {
            lexical: dense * (1.0 - ratio).max(0.0),
            dense,
        }
    }
}

impl Default for FusionWeights {
    /// v0's shipped weights: [`FusionWeights::for_displacement_depth`] at
    /// [`DISPLACEMENT_DEPTH`].
    fn default() -> Self {
        Self::for_displacement_depth(DISPLACEMENT_DEPTH)
    }
}

/// One leg's contribution to a document's score.
fn contribution(rank: usize, weight: f64) -> f64 {
    weight / (RRF_K + rank) as f64
}

/// Fuse the two legs (spec 09 §4) and return the best `limit` hits.
///
/// Merging is by `occurrence_id`: a document found by both legs is **one**
/// result carrying both ranks, never two. `limit` — not the legs' own
/// `candidate_depth` — bounds the output: the deeper candidate lists exist so
/// fusion has material to reorder, not so the caller receives them.
///
/// `weights` scales each leg's reciprocal before summing ([`FusionWeights`]);
/// [`FusionWeights::UNWEIGHTED`] reproduces the pre-D-018 formula exactly.
pub fn rrf(
    lexical: &[LexicalHit],
    dense: &[DenseHit],
    limit: usize,
    weights: FusionWeights,
) -> Vec<FusedHit> {
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
            score: legs
                .lexical
                .map(|rank| contribution(rank, weights.lexical))
                .unwrap_or(0.0)
                + legs
                    .dense
                    .map(|rank| contribution(rank, weights.dense))
                    .unwrap_or(0.0),
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

    /// The pre-D-018 formula — every hand-calculated golden in this module is
    /// written against equal weights, so they stay readable arithmetic instead
    /// of arithmetic times a constant. The weighted behaviour has its own tests.
    fn fuse(lexical: &[LexicalHit], dense: &[DenseHit], limit: usize) -> Vec<FusedHit> {
        rrf(lexical, dense, limit, FusionWeights::UNWEIGHTED)
    }

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

    /// The weight is the displacement rule, solved. Each depth is a policy
    /// ("lexical may overrule dense's first result only when dense also has the
    /// challenger in its top `d`"), and the numbers are what that policy costs.
    #[test]
    fn the_lexical_weight_follows_from_the_displacement_depth() {
        for (depth, expected) in [(10, 0.128_571), (20, 0.237_5), (50, 0.445_454)] {
            let w = FusionWeights::for_displacement_depth(depth);
            assert_eq!(w.dense, 1.0, "the dense leg is the unit of account");
            assert!(
                (w.lexical - expected).abs() < 1e-6,
                "depth {depth}: {} != {expected}",
                w.lexical
            );
        }
        // "may never displace" is a legal policy, and it means zero weight —
        // not a negative one.
        assert_eq!(FusionWeights::for_displacement_depth(1).lexical, 0.0);
        assert_eq!(FusionWeights::for_displacement_depth(0).lexical, 0.0);
        // The shipped default is that formula at the shipped depth, never a
        // literal typed in twice.
        assert_eq!(
            FusionWeights::default(),
            FusionWeights::for_displacement_depth(DISPLACEMENT_DEPTH)
        );
    }

    /// The rule itself, as behaviour: a document the dense leg ranks first is
    /// **not** displaced by one the lexical leg ranks first while the dense leg
    /// holds the challenger below the displacement depth.
    ///
    /// This is the regression D-018 exists for. Under equal weights the
    /// challenger wins (`1/61 + 1/110` against `1/61`), which is how a hybrid
    /// ended up scoring below its own dense leg.
    #[test]
    fn a_dense_first_hit_survives_a_lexical_first_challenger_ranked_deeper() {
        let incumbent_only_dense = [dense("incumbent", 1), dense("challenger", 50)];
        let challenger_leads_lexically = [lex("challenger", 1)];

        let weighted = rrf(
            &challenger_leads_lexically,
            &incumbent_only_dense,
            2,
            FusionWeights::default(),
        );
        assert_eq!(
            ids(&weighted),
            ["incumbent", "challenger"],
            "the derived weight keeps the dense leg's first result first"
        );

        let unweighted = fuse(&challenger_leads_lexically, &incumbent_only_dense, 2);
        assert_eq!(
            ids(&unweighted),
            ["challenger", "incumbent"],
            "…and the pre-D-018 formula is what inverted it"
        );
    }

    /// The boundary the depth names: *inside* it the lexical leg still
    /// overrules. Stated against an explicit depth, so the property survives a
    /// change of the shipped one.
    #[test]
    fn inside_the_displacement_depth_the_lexical_leg_still_wins() {
        let fused = rrf(
            &[lex("challenger", 1)],
            &[dense("incumbent", 1), dense("challenger", 5)],
            2,
            FusionWeights::for_displacement_depth(10),
        );
        assert_eq!(ids(&fused), ["challenger", "incumbent"]);
    }

    /// *At* the depth the rule names, the two are exactly equal — that is what
    /// solving `w_l/(k+1) + w_d/(k+d) = w_d/(k+1)` means, and it is why
    /// displacement happens strictly inside the depth, not at it.
    #[test]
    fn at_exactly_the_displacement_depth_the_scores_tie() {
        for depth in [3, 10, 20, 50] {
            let fused = rrf(
                &[lex("challenger", 1)],
                &[dense("incumbent", 1), dense("challenger", depth)],
                2,
                FusionWeights::for_displacement_depth(depth),
            );
            assert!(
                (fused[0].score - fused[1].score).abs() < 1e-12,
                "depth {depth}: {:?}",
                fused
            );
        }
    }

    /// The shipped default damps the lexical leg without muting it: it cannot
    /// unseat the dense leader, but deeper down — where consecutive reciprocals
    /// differ by less than its contribution — it still reorders.
    #[test]
    fn the_shipped_default_still_reorders_deeper_ranks() {
        let fused = rrf(
            &[lex("deeper", 1)],
            &[dense("shallower", 3), dense("deeper", 4)],
            2,
            FusionWeights::default(),
        );
        assert_eq!(
            ids(&fused),
            ["deeper", "shallower"],
            "1/64 + w/61 must beat 1/63 at the shipped weight"
        );
    }

    /// A document only the lexical leg found no longer ties with the dense
    /// leg's first result — under equal weights both scored exactly `1/61` and
    /// the winner was decided by `occurrence_id`, i.e. by chance.
    #[test]
    fn a_lexical_only_document_no_longer_ties_with_the_dense_leader() {
        let lexical_only = [lex("aaa-lexical-only", 1)];
        let dense_leader = [dense("zzz-dense-leader", 1)];

        let tied = fuse(&lexical_only, &dense_leader, 2);
        assert_eq!(tied[0].score, tied[1].score, "equal weights tie exactly");
        assert_eq!(
            ids(&tied),
            ["aaa-lexical-only", "zzz-dense-leader"],
            "and the id tie-break, not the evidence, decided it"
        );

        let weighted = rrf(&lexical_only, &dense_leader, 2, FusionWeights::default());
        assert_eq!(ids(&weighted), ["zzz-dense-leader", "aaa-lexical-only"]);
    }

    /// Weights scale, they do not reorder within a leg: with the dense leg
    /// silent, the lexical leg's own order survives any positive weight.
    #[test]
    fn weighting_a_leg_does_not_reorder_that_leg() {
        let lexical: Vec<LexicalHit> = (1..=5).map(|i| lex(&format!("l{i}"), i)).collect();
        for weights in [
            FusionWeights::UNWEIGHTED,
            FusionWeights::default(),
            FusionWeights::for_displacement_depth(50),
        ] {
            let fused = rrf(&lexical, &[], 5, weights);
            assert_eq!(ids(&fused), ["l1", "l2", "l3", "l4", "l5"], "{weights:?}");
        }
    }

    /// A zero-weight leg contributes nothing at all: the fusion collapses to the
    /// other leg's ordering, and a document only the muted leg found scores 0.
    #[test]
    fn a_zero_weight_leg_contributes_nothing() {
        let muted = FusionWeights {
            lexical: 0.0,
            dense: 1.0,
        };
        let fused = rrf(&[lex("lex-only", 1)], &[dense("d", 2)], 2, muted);
        assert_eq!(ids(&fused), ["d", "lex-only"]);
        assert_eq!(fused[1].score, 0.0);
    }

    /// The formula, by hand: a document ranked 1 by the lexical leg and 3 by
    /// the dense leg scores exactly `1/61 + 1/63`.
    #[test]
    fn a_document_in_both_legs_sums_both_reciprocals() {
        let fused = fuse(&[lex("a", 1)], &[dense("a", 3)], 10);
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
        let fused = fuse(&[lex("a", 2)], &[], 10);
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
        let fused = fuse(
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
        let fused = fuse(
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
        let fused = fuse(&[lex("c", 1), lex("b", 1), lex("a", 1)], &[], 10);
        assert_eq!(ids(&fused), ["a", "b", "c"]);
        assert!(fused.windows(2).all(|w| w[0].score == w[1].score));
    }

    /// `HashMap` iteration order is randomized per process; the sort must make
    /// that invisible. Fusing the same input repeatedly gives the same order.
    #[test]
    fn repeated_fusion_of_the_same_input_is_identical() {
        let lexical: Vec<LexicalHit> = (0..40).map(|i| lex(&format!("occ-{i:02}"), 1)).collect();
        let densely: Vec<DenseHit> = (0..40).map(|i| dense(&format!("occ-{i:02}"), 1)).collect();
        let first = fuse(&lexical, &densely, 40);
        for _ in 0..8 {
            assert_eq!(fuse(&lexical, &densely, 40), first);
        }
    }

    /// `limit` bounds the response, not the legs' candidate depth.
    #[test]
    fn limit_truncates_after_fusion_not_before() {
        let lexical: Vec<LexicalHit> = (1..=10).map(|i| lex(&format!("l{i}"), i)).collect();
        let densely: Vec<DenseHit> = (1..=10).map(|i| dense(&format!("d{i}"), i)).collect();
        let fused = fuse(&lexical, &densely, 3);
        assert_eq!(fused.len(), 3);
        // The two rank-1 documents lead, tie-broken by id; then the rank-2 pair.
        assert_eq!(ids(&fused), ["d1", "l1", "d2"]);
    }

    #[test]
    fn an_empty_leg_leaves_the_other_leg_untouched() {
        let lexical: Vec<LexicalHit> = (1..=3).map(|i| lex(&format!("l{i}"), i)).collect();
        let with_empty_dense = fuse(&lexical, &[], 10);
        assert_eq!(ids(&with_empty_dense), ["l1", "l2", "l3"]);
        assert!(with_empty_dense.iter().all(|h| h.legs.dense.is_none()));
    }

    #[test]
    fn two_empty_legs_fuse_to_nothing() {
        assert!(fuse(&[], &[], 10).is_empty());
    }

    #[test]
    fn a_zero_limit_returns_nothing() {
        assert!(fuse(&[lex("a", 1)], &[dense("a", 1)], 0).is_empty());
    }

    /// Rank damping: consecutive ranks matter less the deeper they are. The gap
    /// is `1/((k+r)(k+r+1))`, so it shrinks quadratically in `k + rank` — with
    /// `k = 60` the rank-1/2 gap is ≈ 6.8× the rank-100/101 gap, and the
    /// contribution itself is strictly decreasing everywhere.
    #[test]
    fn deep_ranks_contribute_progressively_less() {
        let top = contribution(1, 1.0) - contribution(2, 1.0);
        let deep = contribution(100, 1.0) - contribution(101, 1.0);
        assert!(top > deep, "top={top}, deep={deep}");
        assert!((top / deep - (160.0 * 161.0) / (61.0 * 62.0)).abs() < 1e-9);
        assert!(
            (1..500).all(|r| contribution(r, 1.0) > contribution(r + 1, 1.0)),
            "contribution must be strictly decreasing in rank"
        );
    }
}
