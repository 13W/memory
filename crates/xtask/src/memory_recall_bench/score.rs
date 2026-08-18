//! Memory-recall benchmark scoring: rank lookup and metric math — X-010.
//!
//! Matching is exact `memory_id` equality against a query's single
//! ground-truth target, unlike `crate::bench::score`'s path/symbol substring
//! rule — a memory entry has no analogous "reasonable near-miss" shape, so
//! there is nothing substring matching would buy here that exact equality
//! does not already give more simply. The metric math itself (`hit@k`, MRR)
//! is the same well-known formula `crate::bench::score` and
//! `crate::memory_bench::score` each already implement independently; this
//! module is a third, deliberately independent copy rather than a shared
//! helper — see this task's own module doc on why each benchmark family
//! keeps its own scorer.

use std::collections::BTreeMap;

/// The 1-based rank of `expected_id` in `ranked_ids`, or `None` if absent.
pub fn rank_of_match(expected_id: &str, ranked_ids: &[String]) -> Option<usize> {
    ranked_ids
        .iter()
        .position(|id| id == expected_id)
        .map(|i| i + 1)
}

/// Aggregate metrics over a set of queries.
#[derive(Debug, Clone, Copy, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct Metrics {
    /// Fraction of queries whose target ranked first.
    pub hit_at_1: f64,
    /// Fraction whose target ranked in the top 3.
    pub hit_at_3: f64,
    /// Fraction whose target ranked in the top 5.
    pub hit_at_5: f64,
    /// Mean reciprocal rank.
    pub mrr: f64,
    /// How many queries this aggregate was computed over — carried alongside
    /// the metrics themselves so a small per-`lang_pair` slice (as few as 4
    /// queries) is never misread as having the same statistical weight as
    /// the full 24.
    pub query_count: usize,
}

/// Aggregate `ranks` (one entry per query, `None` = miss) into [`Metrics`].
///
/// An empty slice yields all-zero metrics rather than `NaN` — mirrors
/// `crate::bench::score::aggregate`'s identical choice, for the identical
/// reason: a division that silently poisoned every comparison would be worse
/// than a visible zero.
pub fn aggregate(ranks: &[Option<usize>]) -> Metrics {
    let n = ranks.len();
    if n == 0 {
        return Metrics::default();
    }
    let total = n as f64;
    let count_within = |k: usize| {
        ranks
            .iter()
            .filter(|r| matches!(r, Some(rank) if *rank <= k))
            .count() as f64
            / total
    };
    let mrr = ranks
        .iter()
        .map(|r| r.map_or(0.0, |rank| 1.0 / rank as f64))
        .sum::<f64>()
        / total;
    Metrics {
        hit_at_1: count_within(1),
        hit_at_3: count_within(3),
        hit_at_5: count_within(5),
        mrr,
        query_count: n,
    }
}

/// Aggregate `ranks` once overall and once per `lang_pair` — the corpus is
/// deliberately stratified (`corpus::KNOWN_LANG_PAIRS`) so a cross-lingual
/// slice's number is never hidden inside an average dominated by the
/// same-language controls.
pub fn aggregate_by_lang_pair(entries: &[(String, Option<usize>)]) -> BTreeMap<String, Metrics> {
    let mut by_pair: BTreeMap<String, Vec<Option<usize>>> = BTreeMap::new();
    for (pair, rank) in entries {
        by_pair.entry(pair.clone()).or_default().push(*rank);
    }
    by_pair
        .into_iter()
        .map(|(pair, ranks)| (pair, aggregate(&ranks)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn rank_is_one_based_and_finds_the_exact_id() {
        let ranked = ids(&["a", "b", "c"]);
        assert_eq!(rank_of_match("b", &ranked), Some(2));
        assert_eq!(rank_of_match("a", &ranked), Some(1));
    }

    #[test]
    fn a_miss_has_no_rank() {
        assert_eq!(rank_of_match("x", &ids(&["a", "b"])), None);
        assert_eq!(rank_of_match("x", &[]), None);
    }

    /// Exact equality, unlike `bench::score::matches`'s substring rule — an id
    /// that merely contains the target as a substring is not a match.
    #[test]
    fn a_substring_of_the_target_id_does_not_match() {
        let ranked = ids(&["mr-1", "mr-10"]);
        assert_eq!(rank_of_match("mr-1", &ranked), Some(1));
        assert_eq!(rank_of_match("mr-100", &ranked), None);
    }

    #[test]
    fn a_single_first_place_hit_scores_everything_at_one() {
        let m = aggregate(&[Some(1)]);
        assert_eq!(m.hit_at_1, 1.0);
        assert_eq!(m.hit_at_3, 1.0);
        assert_eq!(m.hit_at_5, 1.0);
        assert_eq!(m.mrr, 1.0);
        assert_eq!(m.query_count, 1);
    }

    #[test]
    fn reciprocal_rank_is_one_over_rank() {
        assert_eq!(aggregate(&[Some(2)]).mrr, 0.5);
        assert!((aggregate(&[Some(3)]).mrr - 1.0 / 3.0).abs() < 1e-12);
        assert_eq!(aggregate(&[Some(4)]).mrr, 0.25);
        assert_eq!(aggregate(&[Some(5)]).mrr, 0.2);
    }

    #[test]
    fn a_miss_contributes_zero_not_a_skipped_query() {
        let m = aggregate(&[Some(1), None]);
        assert_eq!(m.mrr, 0.5);
        assert_eq!(m.hit_at_1, 0.5);
        assert_eq!(m.hit_at_5, 0.5);
        assert_eq!(m.query_count, 2);
    }

    #[test]
    fn hit_at_k_is_inclusive_of_rank_k() {
        let m = aggregate(&[Some(3)]);
        assert_eq!(m.hit_at_1, 0.0);
        assert_eq!(m.hit_at_3, 1.0, "rank 3 counts for hit@3");
        assert_eq!(m.hit_at_5, 1.0);

        let m = aggregate(&[Some(6)]);
        assert_eq!(m.hit_at_5, 0.0, "rank 6 does not");
        assert!((m.mrr - 1.0 / 6.0).abs() < 1e-12, "but still scores in MRR");
    }

    #[test]
    fn an_empty_slice_scores_zero_rather_than_nan() {
        let m = aggregate(&[]);
        assert_eq!(m.mrr, 0.0);
        assert!(!m.mrr.is_nan());
        assert_eq!(m.hit_at_5, 0.0);
        assert_eq!(m.query_count, 0);
    }

    #[test]
    fn grouping_keeps_each_lang_pairs_metrics_separate() {
        let entries = vec![
            ("ru-ru".to_string(), Some(1)),
            ("ru-ru".to_string(), Some(1)),
            ("en-ru".to_string(), None),
        ];
        let grouped = aggregate_by_lang_pair(&entries);
        assert_eq!(grouped.len(), 2);
        assert_eq!(grouped["ru-ru"].mrr, 1.0);
        assert_eq!(grouped["ru-ru"].query_count, 2);
        assert_eq!(grouped["en-ru"].mrr, 0.0);
        assert_eq!(grouped["en-ru"].query_count, 1);
    }
}
