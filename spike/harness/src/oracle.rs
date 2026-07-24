//! The exact-neighbour oracle (T10-02, spec 05 §1 as-built note).
//!
//! A pure, in-memory reference top-k computation, decoupled from any
//! candidate's on-disk shard format: it works directly off a dataset's
//! already-generated [`ProjectionPoint`]s, so a future candidate's recall@k
//! test (T10-03 "recall vs oracle", T10-04) can call it without depending on
//! [`crate::brute_force`]'s storage internals at all. [`crate::brute_force`]'s
//! own `search()` is tested against this independently (T10-02's
//! "exact-neighbor oracle" acceptance test) — proving the *adapter's* top-k,
//! truncation, and tie-break logic is correct, not merely that this function
//! is self-consistent.
//!
//! Scoring convention: dot product, "higher is closer" (matching
//! [`ScoredPoint`]'s existing doc and the product fake backend's own
//! convention, spec 05 §1 as-built note T10-02) — pinned here so every future
//! candidate's recall@k is computed against the same metric.

use local_rag_projection::{DenseQuery, ProjectionPoint, ScoredPoint};

/// The exact top-`query.k` neighbours of `query` among `points`, by dot
/// product, ties broken by point id ascending (same convention
/// [`crate::brute_force`]'s `search()` uses). Pure: no I/O, no shared state.
pub fn exact_top_k(points: &[ProjectionPoint], query: &DenseQuery) -> Vec<ScoredPoint> {
    let mut scored: Vec<ScoredPoint> = points
        .iter()
        .map(|p| ScoredPoint {
            point_id: p.point_id.clone(),
            score: dot(&query.vector, &p.vector),
        })
        .collect();
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.point_id.cmp(&b.point_id))
    });
    scored.truncate(query.k);
    scored
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Recall@k of `approx` against the independent exact-neighbour reference
/// `exact` (T10-03, spec 14 §7 "recall vs oracle"): the fraction of `exact`'s
/// point ids also present in `approx`, ignoring order and score value —
/// recall@k cares about set overlap, not exact ranking agreement. Vacuously
/// `1.0` when `exact` is empty (nothing to have missed).
pub fn recall_at_k(exact: &[ScoredPoint], approx: &[ScoredPoint]) -> f64 {
    if exact.is_empty() {
        return 1.0;
    }
    let approx_ids: std::collections::HashSet<&str> =
        approx.iter().map(|p| p.point_id.as_str()).collect();
    let hits = exact
        .iter()
        .filter(|p| approx_ids.contains(p.point_id.as_str()))
        .count();
    hits as f64 / exact.len() as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use local_rag_projection::PointId;

    fn point(id: &str, vector: Vec<f32>) -> ProjectionPoint {
        ProjectionPoint {
            point_id: PointId::from_hex(id.to_string()),
            vector,
        }
    }

    #[test]
    fn orders_by_descending_dot_product() {
        let points = vec![
            point("a", vec![1.0, 0.0]),
            point("b", vec![0.5, 0.0]),
            point("c", vec![0.9, 0.0]),
        ];
        let query = DenseQuery {
            vector: vec![1.0, 0.0],
            k: 10,
        };
        let top = exact_top_k(&points, &query);
        let ids: Vec<&str> = top.iter().map(|s| s.point_id.as_str()).collect();
        assert_eq!(ids, ["a", "c", "b"], "descending by score: 1.0, 0.9, 0.5");
    }

    #[test]
    fn ties_break_by_point_id_ascending() {
        let points = vec![
            point("zz", vec![1.0]),
            point("aa", vec![1.0]),
            point("mm", vec![1.0]),
        ];
        let query = DenseQuery {
            vector: vec![1.0],
            k: 10,
        };
        let top = exact_top_k(&points, &query);
        let ids: Vec<&str> = top.iter().map(|s| s.point_id.as_str()).collect();
        assert_eq!(
            ids,
            ["aa", "mm", "zz"],
            "equal scores tie-break ascending by id"
        );
    }

    #[test]
    fn truncates_to_k() {
        let points = vec![
            point("a", vec![3.0]),
            point("b", vec![2.0]),
            point("c", vec![1.0]),
        ];
        let query = DenseQuery {
            vector: vec![1.0],
            k: 2,
        };
        assert_eq!(exact_top_k(&points, &query).len(), 2);
    }

    #[test]
    fn empty_points_yields_empty_result() {
        let query = DenseQuery {
            vector: vec![1.0],
            k: 5,
        };
        assert!(exact_top_k(&[], &query).is_empty());
    }

    fn scored(id: &str, score: f32) -> ScoredPoint {
        ScoredPoint {
            point_id: PointId::from_hex(id.to_string()),
            score,
        }
    }

    #[test]
    fn recall_full_overlap_is_one() {
        let exact = vec![scored("a", 3.0), scored("b", 2.0)];
        let approx = vec![scored("b", 2.0), scored("a", 3.0)]; // order doesn't matter
        assert_eq!(recall_at_k(&exact, &approx), 1.0);
    }

    #[test]
    fn recall_no_overlap_is_zero() {
        let exact = vec![scored("a", 3.0), scored("b", 2.0)];
        let approx = vec![scored("x", 1.0), scored("y", 0.5)];
        assert_eq!(recall_at_k(&exact, &approx), 0.0);
    }

    #[test]
    fn recall_partial_overlap() {
        let exact = vec![scored("a", 3.0), scored("b", 2.0), scored("c", 1.0)];
        let approx = vec![scored("a", 3.0), scored("b", 2.0), scored("x", 0.1)];
        assert_eq!(recall_at_k(&exact, &approx), 2.0 / 3.0);
    }

    #[test]
    fn recall_with_empty_exact_is_vacuously_one() {
        assert_eq!(recall_at_k(&[], &[scored("a", 1.0)]), 1.0);
    }
}
