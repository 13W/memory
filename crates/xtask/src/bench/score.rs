//! Benchmark scoring: matching semantics and metric math (spec 14 §7) — T12-05.
//!
//! Pure functions over already-obtained results, so every number the gate acts
//! on is hand-checkable in a unit test rather than only observable through a
//! full index+search run.
//!
//! # Matching semantics
//!
//! Taken **verbatim from the corpus's own description**, because they define
//! what the v1 baseline numbers mean:
//!
//! > file = substring of the target file path; symbol = substring of the target
//! > symbol name; `symbol=null` means file-level match (any symbol).
//!
//! Substring, not equality — v1 scored `"embedder"` against
//! `src/indexer/embedder.ts`. Reimplementing this as a stricter or looser rule
//! would silently change what every recorded metric means.
//!
//! # Metrics
//!
//! The corpus is **single-relevant** (one ground-truth target per query, no
//! grades), which fixes the shape of every metric:
//!
//! - `hit@k` — the fraction of queries whose target appears in the top `k`.
//! - `MRR` — mean of `1/rank` over queries, `0` for a query that missed.
//! - `Recall@5` — with exactly one relevant document per query, "how many of the
//!   relevant documents did we retrieve in the top 5" is `1` when the single
//!   target is there and `0` otherwise, so **`Recall@5 == hit@5` by
//!   construction**. Both names are reported because spec 14 §2's gate is
//!   phrased in terms of `Recall@5` while the corpus declares `hit@5`; they are
//!   the same number, and stating that is better than quietly reporting one
//!   under the other's name.

use crate::bench::corpus::Query;

/// One search result, reduced to what scoring looks at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// The result's path.
    pub path: String,
    /// The result's symbol name (empty when the unit has none).
    pub name: String,
}

/// Whether `candidate` satisfies `query`'s ground truth.
pub fn matches(query: &Query, candidate: &Candidate) -> bool {
    if !candidate.path.contains(&query.expected.file) {
        return false;
    }
    match query.expected.symbol.as_deref() {
        // File-level target: any symbol of the right file counts.
        None => true,
        Some(symbol) => candidate.name.contains(symbol),
    }
}

/// The 1-based rank of the first matching candidate, or `None` if none match.
pub fn rank_of_match(query: &Query, candidates: &[Candidate]) -> Option<usize> {
    candidates
        .iter()
        .position(|c| matches(query, c))
        .map(|i| i + 1)
}

/// Aggregate metrics over the whole corpus.
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
    /// Identical to [`Metrics::hit_at_5`] on a single-relevant corpus; see the
    /// module docs.
    pub recall_at_5: f64,
}

/// Aggregate `ranks` (one entry per query, `None` = miss) into [`Metrics`].
///
/// An empty corpus yields all-zero metrics rather than `NaN` — a division that
/// silently poisoned every comparison would be far worse than a visible zero.
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
    let hit_at_5 = count_within(5);
    Metrics {
        hit_at_1: count_within(1),
        hit_at_3: count_within(3),
        hit_at_5,
        mrr,
        recall_at_5: hit_at_5,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::corpus::Expected;

    fn query(file: &str, symbol: Option<&str>) -> Query {
        Query {
            id: "sc-01".to_string(),
            group: "g".to_string(),
            query: "q".to_string(),
            expected: Expected {
                file: file.to_string(),
                symbol: symbol.map(str::to_string),
                match_mode: "substring".to_string(),
            },
        }
    }

    fn candidate(path: &str, name: &str) -> Candidate {
        Candidate {
            path: path.to_string(),
            name: name.to_string(),
        }
    }

    // ---- matching ----------------------------------------------------------

    #[test]
    fn a_target_matches_on_substrings_of_both_path_and_name() {
        let q = query("embedder", Some("embedBatch"));
        assert!(matches(
            &q,
            &candidate("src/indexer/embedder.ts", "embedBatch")
        ));
        // Substring, not equality — on either axis.
        assert!(matches(
            &q,
            &candidate("src/indexer/embedder.ts", "embedBatchAttempt")
        ));
    }

    #[test]
    fn the_wrong_file_never_matches_however_right_the_symbol_is() {
        let q = query("embedder", Some("embedBatch"));
        assert!(!matches(
            &q,
            &candidate("src/search/query.ts", "embedBatch")
        ));
    }

    #[test]
    fn the_wrong_symbol_never_matches_however_right_the_file_is() {
        let q = query("embedder", Some("embedBatch"));
        assert!(!matches(
            &q,
            &candidate("src/indexer/embedder.ts", "generateDescription")
        ));
    }

    /// `symbol: null` is a *file-level* target — the one corpus entry that would
    /// score zero if a scorer treated a missing symbol as "match nothing".
    #[test]
    fn a_file_level_target_accepts_any_symbol_of_that_file() {
        let q = query("config", None);
        assert!(matches(&q, &candidate("src/config.ts", "loadConfig")));
        assert!(matches(&q, &candidate("src/config.ts", "")));
        assert!(!matches(&q, &candidate("src/other.ts", "loadConfig")));
    }

    // ---- rank --------------------------------------------------------------

    #[test]
    fn rank_is_one_based_and_finds_the_first_match() {
        let q = query("embedder", Some("embedBatch"));
        let results = [
            candidate("src/a.ts", "other"),
            candidate("src/indexer/embedder.ts", "embedBatch"),
            candidate("src/indexer/embedder.ts", "embedBatch"),
        ];
        assert_eq!(rank_of_match(&q, &results), Some(2));
    }

    #[test]
    fn a_miss_has_no_rank() {
        let q = query("embedder", Some("embedBatch"));
        assert_eq!(rank_of_match(&q, &[candidate("src/a.ts", "b")]), None);
        assert_eq!(rank_of_match(&q, &[]), None);
    }

    // ---- metric math (hand-calculated) -------------------------------------

    #[test]
    fn a_single_first_place_hit_scores_everything_at_one() {
        let m = aggregate(&[Some(1)]);
        assert_eq!(m.hit_at_1, 1.0);
        assert_eq!(m.hit_at_3, 1.0);
        assert_eq!(m.hit_at_5, 1.0);
        assert_eq!(m.mrr, 1.0);
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
        // Two queries, one perfect and one missed: MRR is 0.5, not 1.0.
        let m = aggregate(&[Some(1), None]);
        assert_eq!(m.mrr, 0.5);
        assert_eq!(m.hit_at_1, 0.5);
        assert_eq!(m.hit_at_5, 0.5);
    }

    /// `hit@k` is inclusive of rank `k` and exclusive of `k + 1` — the boundary
    /// a rank-5 hit sits exactly on.
    #[test]
    fn hit_at_k_is_inclusive_of_rank_k() {
        let m = aggregate(&[Some(3)]);
        assert_eq!(m.hit_at_1, 0.0);
        assert_eq!(m.hit_at_3, 1.0, "rank 3 counts for hit@3");
        assert_eq!(m.hit_at_5, 1.0);

        let m = aggregate(&[Some(5)]);
        assert_eq!(m.hit_at_3, 0.0);
        assert_eq!(m.hit_at_5, 1.0, "rank 5 counts for hit@5");

        let m = aggregate(&[Some(6)]);
        assert_eq!(m.hit_at_5, 0.0, "rank 6 does not");
        assert!((m.mrr - 1.0 / 6.0).abs() < 1e-12, "but still scores in MRR");
    }

    #[test]
    fn recall_at_5_equals_hit_at_5_on_a_single_relevant_corpus() {
        for ranks in [
            vec![Some(1), Some(5), None],
            vec![None, None],
            vec![Some(2)],
        ] {
            let m = aggregate(&ranks);
            assert_eq!(m.recall_at_5, m.hit_at_5, "{ranks:?}");
        }
    }

    /// The exact v1 baseline shape: 49 queries, 29 at rank 1, 39 within 3, 41
    /// within 5 — reproducing the recorded Hit@k to the digit proves this
    /// aggregation is the same one that produced the numbers we compare against.
    #[test]
    fn the_recorded_v1_hit_rates_are_reproduced_by_this_aggregation() {
        let mut ranks = Vec::new();
        ranks.extend(std::iter::repeat_n(Some(1), 29));
        ranks.extend(std::iter::repeat_n(Some(3), 39 - 29));
        ranks.extend(std::iter::repeat_n(Some(5), 41 - 39));
        ranks.extend(std::iter::repeat_n(None, 49 - 41));
        assert_eq!(ranks.len(), 49);

        let m = aggregate(&ranks);
        assert!((m.hit_at_1 - 0.5918367346938775).abs() < 1e-12);
        assert!((m.hit_at_3 - 0.7959183673469388).abs() < 1e-12);
        assert!((m.hit_at_5 - 0.8367346938775511).abs() < 1e-12);
    }

    #[test]
    fn an_empty_corpus_scores_zero_rather_than_nan() {
        let m = aggregate(&[]);
        assert_eq!(m.mrr, 0.0);
        assert!(!m.mrr.is_nan());
        assert_eq!(m.hit_at_5, 0.0);
    }
}
