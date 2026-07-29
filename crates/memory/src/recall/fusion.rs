//! Reciprocal Rank Fusion of the recall pipeline's two legs (spec 08 §6) —
//! T14-08.
//!
//! `score(d) = Σ_legs 1 / (k + rank_leg(d))`, `k = 60` (spec 09 §4's `RRF_K`,
//! the only numeric constant this module borrows), with the deterministic
//! tie-break `(score desc, memory_id asc)`. A pure function over two
//! already-ranked lists — no store, no clock, no lock — mirroring
//! `local_rag_search::fusion::rrf`'s idiom exactly (same accumulator type,
//! same tie-break shape), just keyed by `memory_id` instead of
//! `occurrence_id`.
//!
//! # Unweighted, unlike the code-search leg
//!
//! `local_rag_search::fusion`'s weights (D-018) were *derived* from the
//! 49-query code benchmark after measuring that an unweighted hybrid scored
//! below its own dense leg there. No equivalent measurement exists for
//! memory recall — there is no per-query relevance-judged benchmark for it
//! (spec 08 §7's benchmark scores the *router*, not recall) — and inventing a
//! weight to fit the formula with nothing measured to derive it from is
//! exactly what spec 08 §2's own as-built note calls out for confidence
//! weights: "collect metrics, never invent thresholds" (O2). So this module
//! ships the pre-D-018 formula, both legs counted equally, until a
//! recall-quality fixture set exists to derive a weight from (out of this
//! task's card).

use std::collections::HashMap;

/// The RRF constant `k` (spec 09 §4 `[SPEC]`, reused verbatim — nothing about
/// its derivation is code-search-specific).
pub const RRF_K: usize = 60;

/// Per-leg 1-based ranks that produced a [`FusedRecallHit`]'s score.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecallLegRanks {
    /// The lexical (FTS) leg's rank, if it matched.
    pub lexical: Option<usize>,
    /// The dense (cosine) leg's rank, if it matched.
    pub dense: Option<usize>,
}

/// One fused hit: the entry, its RRF score, and where each leg ranked it.
#[derive(Debug, Clone, PartialEq)]
pub struct FusedRecallHit {
    /// The `memory_entry` both legs are keyed by.
    pub memory_id: String,
    /// `Σ_legs 1 / (RRF_K + rank_leg)`.
    pub score: f64,
    /// The per-leg ranks that produced `score`.
    pub legs: RecallLegRanks,
}

fn contribution(rank: usize) -> f64 {
    1.0 / (RRF_K + rank) as f64
}

/// A 1-based ranked hit from either leg — deliberately identical shape for
/// both, so fusion never has to translate identities.
#[derive(Debug, Clone, Copy)]
pub struct RankedHit<'a> {
    pub memory_id: &'a str,
    pub rank: usize,
}

/// Fuse the two legs and return the best `limit` hits.
///
/// Merging is by `memory_id`: an entry found by both legs is **one** result
/// carrying both ranks, never two. `limit` bounds the output, not either
/// leg's own candidate depth — the caller decides how deep each leg searched
/// before calling this.
pub fn rrf(
    lexical: &[RankedHit<'_>],
    dense: &[RankedHit<'_>],
    limit: usize,
) -> Vec<FusedRecallHit> {
    let mut by_memory: HashMap<&str, RecallLegRanks> = HashMap::new();
    for hit in lexical {
        by_memory.entry(hit.memory_id).or_default().lexical = Some(hit.rank);
    }
    for hit in dense {
        by_memory.entry(hit.memory_id).or_default().dense = Some(hit.rank);
    }

    let mut fused: Vec<FusedRecallHit> = by_memory
        .into_iter()
        .map(|(memory_id, legs)| FusedRecallHit {
            memory_id: memory_id.to_string(),
            score: legs.lexical.map(contribution).unwrap_or(0.0)
                + legs.dense.map(contribution).unwrap_or(0.0),
            legs,
        })
        .collect();

    // `(score desc, memory_id asc)` — the same deterministic tie-break shape
    // spec 09 §4 uses, generalized: a `HashMap`'s iteration order is
    // randomized per process, so the sort is the only thing that makes
    // repeated output byte-stable.
    fused.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.memory_id.cmp(&b.memory_id))
    });
    fused.truncate(limit);
    fused
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(memory_id: &str, rank: usize) -> RankedHit<'_> {
        RankedHit { memory_id, rank }
    }

    fn ids(hits: &[FusedRecallHit]) -> Vec<&str> {
        hits.iter().map(|h| h.memory_id.as_str()).collect()
    }

    /// The formula, by hand: an entry ranked 1 by the lexical leg and 3 by the
    /// dense leg scores exactly `1/61 + 1/63`.
    #[test]
    fn a_document_in_both_legs_sums_both_reciprocals() {
        let fused = rrf(&[hit("a", 1)], &[hit("a", 3)], 10);
        assert_eq!(fused.len(), 1, "one memory_id ⇒ one result");
        let expected = 1.0 / 61.0 + 1.0 / 63.0;
        assert!((fused[0].score - expected).abs() < 1e-12);
        assert_eq!(
            fused[0].legs,
            RecallLegRanks {
                lexical: Some(1),
                dense: Some(3),
            }
        );
    }

    #[test]
    fn a_single_leg_document_scores_one_reciprocal() {
        let fused = rrf(&[hit("a", 2)], &[], 10);
        assert!((fused[0].score - 1.0 / 62.0).abs() < 1e-12);
        assert_eq!(
            fused[0].legs,
            RecallLegRanks {
                lexical: Some(2),
                dense: None,
            }
        );
    }

    /// The canonical RRF property: agreement beats a single leg's top hit.
    #[test]
    fn agreement_across_legs_outranks_a_single_legs_top_hit() {
        let fused = rrf(&[hit("a", 1), hit("b", 2)], &[hit("b", 2), hit("c", 1)], 10);
        assert_eq!(ids(&fused), ["b", "a", "c"]);
        assert!((fused[0].score - 2.0 / 62.0).abs() < 1e-12);
        assert!((fused[1].score - 1.0 / 61.0).abs() < 1e-12);
        assert_eq!(fused[1].score, fused[2].score, "a and c genuinely tie");
    }

    #[test]
    fn duplicates_merge_rather_than_double_count() {
        let fused = rrf(&[hit("a", 1), hit("b", 2)], &[hit("a", 1), hit("b", 2)], 10);
        assert_eq!(ids(&fused), ["a", "b"]);
        assert_eq!(fused.len(), 2);
    }

    /// Genuinely equal scores are ordered by `memory_id` ascending.
    #[test]
    fn equal_scores_break_ties_by_memory_id() {
        let fused = rrf(&[hit("c", 1), hit("b", 1), hit("a", 1)], &[], 10);
        assert_eq!(ids(&fused), ["a", "b", "c"]);
        assert!(fused.windows(2).all(|w| w[0].score == w[1].score));
    }

    /// `HashMap` iteration order is randomized per process; the sort must make
    /// that invisible.
    #[test]
    fn repeated_fusion_of_the_same_input_is_identical() {
        let lexical: Vec<RankedHit<'_>> = (0..40).map(|i| hit(LEAKED[i], 1)).collect();
        let dense: Vec<RankedHit<'_>> = (0..40).map(|i| hit(LEAKED[i], 1)).collect();
        let first = rrf(&lexical, &dense, 40);
        for _ in 0..8 {
            assert_eq!(rrf(&lexical, &dense, 40), first);
        }
    }

    /// A fixed pool of leaked (intentionally static) ids for the determinism
    /// test above — avoids building 40 owned `String`s just to borrow them.
    const LEAKED: [&str; 40] = [
        "occ-00", "occ-01", "occ-02", "occ-03", "occ-04", "occ-05", "occ-06", "occ-07", "occ-08",
        "occ-09", "occ-10", "occ-11", "occ-12", "occ-13", "occ-14", "occ-15", "occ-16", "occ-17",
        "occ-18", "occ-19", "occ-20", "occ-21", "occ-22", "occ-23", "occ-24", "occ-25", "occ-26",
        "occ-27", "occ-28", "occ-29", "occ-30", "occ-31", "occ-32", "occ-33", "occ-34", "occ-35",
        "occ-36", "occ-37", "occ-38", "occ-39",
    ];

    /// `limit` bounds the response, not either leg's candidate depth.
    #[test]
    fn limit_truncates_after_fusion_not_before() {
        let lexical: Vec<RankedHit<'_>> = vec![hit("l1", 1), hit("l2", 2)];
        let dense: Vec<RankedHit<'_>> = vec![hit("d1", 1), hit("d2", 2)];
        let fused = rrf(&lexical, &dense, 3);
        assert_eq!(fused.len(), 3);
        assert_eq!(ids(&fused), ["d1", "l1", "d2"]);
    }

    #[test]
    fn two_empty_legs_fuse_to_nothing() {
        assert!(rrf(&[], &[], 10).is_empty());
    }

    #[test]
    fn a_zero_limit_returns_nothing() {
        assert!(rrf(&[hit("a", 1)], &[hit("a", 1)], 0).is_empty());
    }
}
