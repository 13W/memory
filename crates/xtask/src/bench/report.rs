//! The benchmark report: per-query detail, aggregate metrics, latency, and the
//! diff against the recorded v1 baseline (spec 14 §7) — T12-05.
//!
//! # Why the v1 diff is metric-level (D-015)
//!
//! T12-05's card asks for a "per-query diff vs v1". That is not obtainable: the
//! imported v1 artifact
//! (`fixtures/search/baseline/run-embeddinggemma-300m-2026-07-16.json`) holds
//! **aggregates only**, and v1's own runner folds ranks into counters inside its
//! scoring loop without ever emitting a per-query rank. Recovering them would
//! mean editing v1's source and re-running it, which T00-01 explicitly declined
//! ("v1 source was not modified") and which would invalidate comparability with
//! the very numbers being compared against.
//!
//! So this report carries **full per-query detail for v2** (rank, hit, the
//! matched candidate, latency) and diffs against v1 **at the metric level**.
//! [`QueryResult::v1_rank`] is reserved and always `None` today: if v1 is ever
//! re-run with per-query output, the diff becomes rank-level without a schema
//! change.
//!
//! # Determinism
//!
//! Metrics and per-query results are a deterministic function of the run, so two
//! runs over the same store serialize identically. Latency is not — it is wall
//! time — so every timing lives under [`Latency`], separated from the scored
//! part, and [`BenchReport::scored`] exposes exactly the subset a byte-stability
//! check may compare.

use serde::Serialize;

use crate::bench::score::Metrics;

/// The v1 baseline this run is compared against (spec 14 §7).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, serde::Deserialize)]
pub struct Baseline {
    /// v1's Hit@1.
    pub hit_at_1: f64,
    /// v1's Hit@3.
    pub hit_at_3: f64,
    /// v1's Hit@5 (== Recall@5, single-relevant).
    pub hit_at_5: f64,
    /// v1's MRR.
    pub mrr: f64,
}

impl Baseline {
    /// The recorded v1 numbers (`fixtures/search/baseline/baseline.md`,
    /// `embeddinggemma:300m`, 2026-07-16).
    pub const V1: Baseline = Baseline {
        hit_at_1: 0.5918367346938775,
        hit_at_3: 0.7959183673469388,
        hit_at_5: 0.8367346938775511,
        mrr: 0.6962585034013605,
    };
}

/// How one query fared.
#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
pub struct QueryResult {
    /// The corpus query id.
    pub id: String,
    /// The corpus group, for per-group reporting.
    pub group: String,
    /// 1-based rank of the ground-truth target, or `None` for a miss.
    pub rank: Option<usize>,
    /// The path of the matched result, when there was one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_path: Option<String>,
    /// The symbol of the matched result, when there was one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_name: Option<String>,
    /// How many results the search returned at all.
    pub returned: usize,
    /// v1's rank for this query — always `None`; see the module docs (D-015).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub v1_rank: Option<usize>,
}

/// Wall-clock measurements, deliberately outside the scored part.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, serde::Deserialize)]
pub struct Latency {
    /// Total time to index the corpus.
    pub index_ms: u64,
    /// Total time to embed every occurrence.
    pub embed_ms: u64,
    /// Warm per-query search time, median.
    pub search_p50_ms: f64,
    /// Warm per-query search time, 95th percentile (spec 14 §2's latency gate
    /// input).
    pub search_p95_ms: f64,
}

/// Where a run came from, so a recorded number can be reproduced.
#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
pub struct Provenance {
    /// The v2 commit under test.
    pub v2_commit: String,
    /// The corpus checkout that was indexed.
    pub corpus_path: String,
    /// The corpus checkout's commit.
    pub corpus_commit: String,
    /// The subdirectory that was indexed, when the run restricted the corpus
    /// (D-016). `None` means the whole checkout — two runs that differ only here
    /// measure different corpora, so it belongs in the provenance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub corpus_subdir: Option<String>,
    /// The query-corpus fixture version the run was scored against.
    pub corpus_version: String,
    /// The embedding model id.
    pub model_id: String,
    /// Search mode the queries ran in.
    pub mode: String,
    /// The representation the dense leg searched (D-016). Two runs that differ
    /// only here measure different *representations* of the same corpus, so —
    /// like `corpus_subdir` — it belongs in the provenance rather than in a
    /// filename.
    #[serde(default = "default_dense_kind")]
    pub dense_kind: String,
    /// The lexical leg's fusion weight (D-018). Like `dense_kind`, two runs that
    /// differ only here measure different *rankings* of the same candidates, so
    /// the number travels with the metrics instead of living in a filename.
    /// Absent in artifacts recorded before D-018 — those are all `1.0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fusion_lexical_weight: Option<f64>,
    /// How many files were indexed.
    pub files_indexed: usize,
    /// How many occurrences the generation holds.
    pub occurrences: usize,
    /// Host triple.
    pub host: String,
}

/// Runs archived before D-016 have no `dense_kind`; they all searched `code_raw`.
fn default_dense_kind() -> String {
    "code_raw".to_string()
}

/// The metric-level comparison against v1.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, serde::Deserialize)]
pub struct BaselineDiff {
    /// `v2 - v1`, negative when v2 is worse.
    pub hit_at_1: f64,
    /// `v2 - v1`.
    pub hit_at_3: f64,
    /// `v2 - v1`.
    pub hit_at_5: f64,
    /// `v2 - v1`.
    pub mrr: f64,
}

impl BaselineDiff {
    /// How far v2's MRR falls **below** v1's; `0.0` when v2 is at least as good.
    pub fn mrr_regression(self) -> f64 {
        (-self.mrr).max(0.0)
    }
}

/// One benchmark run.
#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
pub struct BenchReport {
    /// Report schema version, so a future shape change is detectable.
    pub schema_version: u32,
    /// Where the run came from.
    pub provenance: Provenance,
    /// v2's aggregate metrics.
    pub metrics: Metrics,
    /// The v1 numbers compared against.
    pub baseline: Baseline,
    /// `v2 - v1`, per metric.
    pub diff: BaselineDiff,
    /// Per-query detail, in corpus order.
    pub per_query: Vec<QueryResult>,
    /// Wall-clock measurements (not part of [`BenchReport::scored`]).
    pub latency: Latency,
}

/// The deterministic subset of a report: everything except wall-clock time.
///
/// Only the determinism tests construct this — it exists to *name* the boundary
/// between "a function of the run" and "a function of the machine", which is the
/// property T12-05's byte-stability requirement rests on.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ScoredPart<'a> {
    /// v2's aggregate metrics.
    pub metrics: &'a Metrics,
    /// `v2 - v1`, per metric.
    pub diff: &'a BaselineDiff,
    /// Per-query detail.
    pub per_query: &'a [QueryResult],
}

/// This report's schema version.
pub const REPORT_SCHEMA_VERSION: u32 = 1;

impl BenchReport {
    /// Assemble a report from scored results.
    pub fn new(
        provenance: Provenance,
        metrics: Metrics,
        per_query: Vec<QueryResult>,
        latency: Latency,
    ) -> Self {
        let baseline = Baseline::V1;
        BenchReport {
            schema_version: REPORT_SCHEMA_VERSION,
            provenance,
            metrics,
            baseline,
            diff: BaselineDiff {
                hit_at_1: metrics.hit_at_1 - baseline.hit_at_1,
                hit_at_3: metrics.hit_at_3 - baseline.hit_at_3,
                hit_at_5: metrics.hit_at_5 - baseline.hit_at_5,
                mrr: metrics.mrr - baseline.mrr,
            },
            per_query,
            latency,
        }
    }

    /// The part of the report that is a deterministic function of the run.
    #[cfg(test)]
    pub fn scored(&self) -> ScoredPart<'_> {
        ScoredPart {
            metrics: &self.metrics,
            diff: &self.diff,
            per_query: &self.per_query,
        }
    }

    /// A human-readable summary, in the shape of v1's own `.report.md`.
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str("# 49-query search benchmark — v2 run\n\n");
        out.push_str("## Run provenance\n\n| Field | Value |\n| --- | --- |\n");
        let p = &self.provenance;
        out.push_str(&format!("| v2 commit | `{}` |\n", p.v2_commit));
        out.push_str(&format!(
            "| Corpus | `{}` @ `{}` |\n",
            p.corpus_path, p.corpus_commit
        ));
        out.push_str(&format!("| Model | `{}` |\n", p.model_id));
        out.push_str(&format!("| Mode | `{}` |\n", p.mode));
        out.push_str(&format!("| Dense representation | `{}` |\n", p.dense_kind));
        if let Some(weight) = p.fusion_lexical_weight {
            out.push_str(&format!("| Lexical fusion weight | {weight:.4} |\n"));
        }
        out.push_str(&format!(
            "| Corpus size | {} files, {} occurrences |\n",
            p.files_indexed, p.occurrences
        ));
        out.push_str(&format!("| Host | {} |\n\n", p.host));

        out.push_str("## Metrics vs v1\n\n");
        out.push_str("| Metric | v1 | v2 | Δ |\n| --- | --- | --- | --- |\n");
        let row = |name: &str, v1: f64, v2: f64, d: f64| {
            format!("| {name} | {v1:.4} | {v2:.4} | {d:+.4} |\n")
        };
        out.push_str(&row(
            "Hit@1",
            self.baseline.hit_at_1,
            self.metrics.hit_at_1,
            self.diff.hit_at_1,
        ));
        out.push_str(&row(
            "Hit@3",
            self.baseline.hit_at_3,
            self.metrics.hit_at_3,
            self.diff.hit_at_3,
        ));
        out.push_str(&row(
            "Hit@5 / Recall@5",
            self.baseline.hit_at_5,
            self.metrics.hit_at_5,
            self.diff.hit_at_5,
        ));
        out.push_str(&row(
            "MRR",
            self.baseline.mrr,
            self.metrics.mrr,
            self.diff.mrr,
        ));

        out.push_str(&format!(
            "\n## Latency\n\n| Stage | ms |\n| --- | --- |\n| index | {} |\n| embed | {} |\n\
             | warm search p50 | {:.3} |\n| warm search p95 | {:.3} |\n",
            self.latency.index_ms,
            self.latency.embed_ms,
            self.latency.search_p50_ms,
            self.latency.search_p95_ms
        ));

        out.push_str("\n## Per-query (v2)\n\n");
        out.push_str("v1 recorded no per-query ranks (D-015), so this table is v2-only.\n\n");
        out.push_str("| id | group | rank | matched |\n| --- | --- | --- | --- |\n");
        for q in &self.per_query {
            let rank = q.rank.map_or("—".to_string(), |r| r.to_string());
            let matched = match (&q.matched_path, &q.matched_name) {
                (Some(path), Some(name)) if !name.is_empty() => format!("`{path}` / `{name}`"),
                (Some(path), _) => format!("`{path}`"),
                _ => "—".to_string(),
            };
            out.push_str(&format!(
                "| {} | {} | {rank} | {matched} |\n",
                q.id, q.group
            ));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provenance() -> Provenance {
        Provenance {
            v2_commit: "abc1234".to_string(),
            corpus_path: "/opt/soft/local-rag".to_string(),
            corpus_commit: "31dfba2".to_string(),
            corpus_version: "1.0.0".to_string(),
            corpus_subdir: Some("src".to_string()),
            model_id: "embeddinggemma-300m".to_string(),
            mode: "hybrid".to_string(),
            dense_kind: "code_raw".to_string(),
            fusion_lexical_weight: None,
            files_indexed: 96,
            occurrences: 544,
            host: "aarch64-apple-darwin".to_string(),
        }
    }

    fn result(id: &str, rank: Option<usize>) -> QueryResult {
        QueryResult {
            id: id.to_string(),
            group: "embedder".to_string(),
            rank,
            matched_path: rank.map(|_| "src/embedder.ts".to_string()),
            matched_name: rank.map(|_| "embedBatch".to_string()),
            returned: 5,
            v1_rank: None,
        }
    }

    fn report(metrics: Metrics) -> BenchReport {
        BenchReport::new(
            provenance(),
            metrics,
            vec![result("sc-01", Some(1)), result("sc-02", None)],
            Latency {
                index_ms: 1,
                embed_ms: 2,
                search_p50_ms: 3.5,
                search_p95_ms: 9.5,
            },
        )
    }

    #[test]
    fn the_diff_is_v2_minus_v1() {
        let metrics = Metrics {
            hit_at_1: 0.6,
            hit_at_3: 0.8,
            hit_at_5: 0.9,
            mrr: 0.7,
            recall_at_5: 0.9,
        };
        let r = report(metrics);
        assert!((r.diff.mrr - (0.7 - Baseline::V1.mrr)).abs() < 1e-12);
        assert!(r.diff.hit_at_5 > 0.0, "v2 above v1 is a positive delta");
    }

    /// A regression is reported as a positive *budget consumption*, and an
    /// improvement consumes none — the asymmetry the gate depends on.
    #[test]
    fn mrr_regression_is_zero_when_v2_is_at_least_as_good() {
        let better = BaselineDiff {
            hit_at_1: 0.0,
            hit_at_3: 0.0,
            hit_at_5: 0.0,
            mrr: 0.05,
        };
        assert_eq!(better.mrr_regression(), 0.0);

        let worse = BaselineDiff {
            mrr: -0.05,
            ..better
        };
        assert!((worse.mrr_regression() - 0.05).abs() < 1e-12);

        let equal = BaselineDiff { mrr: 0.0, ..better };
        assert_eq!(equal.mrr_regression(), 0.0);
    }

    /// The scored part excludes wall-clock time, so two runs that differ only in
    /// timing serialize identically where it matters.
    #[test]
    fn timing_is_excluded_from_the_deterministic_part() {
        let metrics = Metrics {
            hit_at_1: 0.5,
            hit_at_3: 0.5,
            hit_at_5: 0.5,
            mrr: 0.5,
            recall_at_5: 0.5,
        };
        let mut a = report(metrics);
        let mut b = report(metrics);
        b.latency = Latency {
            index_ms: 999,
            embed_ms: 888,
            search_p50_ms: 77.7,
            search_p95_ms: 66.6,
        };

        let sa = serde_json::to_vec(&a.scored()).expect("serialize");
        let sb = serde_json::to_vec(&b.scored()).expect("serialize");
        assert_eq!(sa, sb, "latency must not perturb the scored part");

        // …while the full reports do differ, so timing is not silently dropped.
        assert_ne!(
            serde_json::to_vec(&a).expect("serialize"),
            serde_json::to_vec(&b).expect("serialize")
        );

        // And a genuine scoring difference is visible.
        a.per_query[0].rank = Some(4);
        assert_ne!(
            serde_json::to_vec(&a.scored()).expect("serialize"),
            serde_json::to_vec(&b.scored()).expect("serialize")
        );
    }

    #[test]
    fn the_scored_part_is_byte_stable_across_repeated_serialization() {
        let r = report(Metrics::default());
        let first = serde_json::to_vec(&r.scored()).expect("serialize");
        for _ in 0..5 {
            assert_eq!(serde_json::to_vec(&r.scored()).expect("serialize"), first);
        }
    }

    /// D-015: v1 ranks are absent, so the per-query table is v2-only and says so
    /// — rather than rendering an empty "v1" column that looks like data.
    #[test]
    fn the_report_states_that_v1_has_no_per_query_ranks() {
        let r = report(Metrics::default());
        assert!(r.per_query.iter().all(|q| q.v1_rank.is_none()));
        let md = r.to_markdown();
        assert!(md.contains("v1 recorded no per-query ranks"), "{md}");
        assert!(md.contains("| Metric | v1 | v2 | Δ |"), "{md}");
        // A missed query renders as an em dash, never as rank 0.
        assert!(md.contains("| sc-02 | embedder | — | — |"), "{md}");
    }
}
