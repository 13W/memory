//! The memory-recall benchmark report — X-010.
//!
//! Unlike `crate::bench::report`, this report carries **no baseline diff**:
//! nothing has measured `local_rag_memory::recall::pipeline::recall`'s
//! retrieval quality before this task (see this module's parent doc), so
//! there is no prior number to diff against. This run's own metrics, once
//! recorded under `fixtures/memory-recall/baseline/`, become the reference
//! point a later run (X-011's English-normalized configurations) is compared
//! against instead — that comparison lives in X-011, not here.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::memory_recall_bench::score::Metrics;

/// Where a run came from, so a recorded number can be reproduced.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Provenance {
    /// The v2 commit under test.
    pub v2_commit: String,
    /// The corpus fixture path.
    pub corpus_path: String,
    /// The corpus fixture's own declared version.
    pub corpus_version: String,
    /// The embedding model id.
    pub model_id: String,
    /// Which text variant fed the store and which fed the query — `baseline`
    /// here (X-010 measures only the as-is pipeline); X-011 adds
    /// `store_en`/`query_en`/`both_en`.
    pub config: String,
    /// How many entries were seeded.
    pub entry_count: usize,
    /// How many queries were run.
    pub query_count: usize,
    /// Host triple.
    pub host: String,
}

/// How one query fared.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryResult {
    /// The corpus query id.
    pub id: String,
    /// `(entry_language, query_language)` — see `corpus::Query::lang_pair`.
    pub lang_pair: String,
    /// The corpus entry id this query's ground truth is.
    pub expected_entry_id: String,
    /// 1-based rank of the ground-truth entry, or `None` for a miss.
    pub rank: Option<usize>,
    /// The corpus entry id `recall()` actually ranked first, if any — a
    /// near-miss diagnostic: distinct from `expected_entry_id` exactly when
    /// `rank` is `None` or greater than 1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_result_id: Option<String>,
    /// How many entries `recall()` returned at all.
    pub returned: usize,
}

/// Wall-clock measurements, deliberately outside the scored part (mirrors
/// `crate::bench::report::Latency`'s determinism boundary).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Latency {
    pub install_ms: u64,
    pub embed_ms: u64,
    pub recall_p50_ms: f64,
    pub recall_p95_ms: f64,
}

/// This report's schema version.
pub const REPORT_SCHEMA_VERSION: u32 = 1;

/// One benchmark run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryRecallBenchReport {
    pub schema_version: u32,
    pub provenance: Provenance,
    /// Aggregate metrics over all 24 queries.
    pub metrics: Metrics,
    /// The same metrics, split by `lang_pair` — see `score::aggregate_by_lang_pair`.
    pub metrics_by_lang_pair: BTreeMap<String, Metrics>,
    /// Per-query detail, in corpus order.
    pub per_query: Vec<QueryResult>,
    pub latency: Latency,
}

impl MemoryRecallBenchReport {
    pub fn new(
        provenance: Provenance,
        metrics: Metrics,
        metrics_by_lang_pair: BTreeMap<String, Metrics>,
        per_query: Vec<QueryResult>,
        latency: Latency,
    ) -> Self {
        MemoryRecallBenchReport {
            schema_version: REPORT_SCHEMA_VERSION,
            provenance,
            metrics,
            metrics_by_lang_pair,
            per_query,
            latency,
        }
    }

    /// A human-readable summary.
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str("# Memory recall benchmark\n\n");
        out.push_str("## Run provenance\n\n| Field | Value |\n| --- | --- |\n");
        let p = &self.provenance;
        out.push_str(&format!("| v2 commit | `{}` |\n", p.v2_commit));
        out.push_str(&format!(
            "| Corpus | `{}` @ `{}` |\n",
            p.corpus_path, p.corpus_version
        ));
        out.push_str(&format!("| Model | `{}` |\n", p.model_id));
        out.push_str(&format!("| Config | `{}` |\n", p.config));
        out.push_str(&format!(
            "| Corpus size | {} entries, {} queries |\n",
            p.entry_count, p.query_count
        ));
        out.push_str(&format!("| Host | {} |\n\n", p.host));

        let row = |name: &str, m: &Metrics| {
            format!(
                "| {name} | {:.4} | {:.4} | {:.4} | {:.4} | {} |\n",
                m.hit_at_1, m.hit_at_3, m.hit_at_5, m.mrr, m.query_count
            )
        };
        out.push_str("## Metrics\n\n");
        out.push_str(
            "| Group | Hit@1 | Hit@3 | Hit@5 | MRR | n |\n| --- | --- | --- | --- | --- | --- |\n",
        );
        out.push_str(&row("overall", &self.metrics));
        for (pair, m) in &self.metrics_by_lang_pair {
            out.push_str(&row(pair, m));
        }

        out.push_str(&format!(
            "\n## Latency\n\n| Stage | ms |\n| --- | --- |\n| install | {} |\n| embed | {} |\n\
             | warm recall p50 | {:.3} |\n| warm recall p95 | {:.3} |\n",
            self.latency.install_ms,
            self.latency.embed_ms,
            self.latency.recall_p50_ms,
            self.latency.recall_p95_ms
        ));

        out.push_str("\n## Per-query\n\n");
        out.push_str("| id | lang_pair | rank | top result |\n| --- | --- | --- | --- |\n");
        for q in &self.per_query {
            let rank = q.rank.map_or("—".to_string(), |r| r.to_string());
            let top = q.top_result_id.as_deref().unwrap_or("—");
            out.push_str(&format!(
                "| {} | {} | {rank} | {top} |\n",
                q.id, q.lang_pair
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
            corpus_path: "fixtures/memory-recall/corpus.json".to_string(),
            corpus_version: "1.0.0".to_string(),
            model_id: "embeddinggemma-300m".to_string(),
            config: "baseline".to_string(),
            entry_count: 24,
            query_count: 24,
            host: "aarch64-apple-darwin".to_string(),
        }
    }

    fn result(id: &str, lang_pair: &str, rank: Option<usize>) -> QueryResult {
        QueryResult {
            id: id.to_string(),
            lang_pair: lang_pair.to_string(),
            expected_entry_id: "mr-01".to_string(),
            rank,
            top_result_id: rank.map(|_| "mr-01".to_string()),
            returned: 5,
        }
    }

    #[test]
    fn markdown_reports_every_lang_pair_row_and_the_overall_row() {
        let mut by_pair = BTreeMap::new();
        by_pair.insert(
            "ru-ru".to_string(),
            Metrics {
                hit_at_1: 1.0,
                hit_at_3: 1.0,
                hit_at_5: 1.0,
                mrr: 1.0,
                query_count: 8,
            },
        );
        let report = MemoryRecallBenchReport::new(
            provenance(),
            Metrics::default(),
            by_pair,
            vec![
                result("mrq-01", "ru-ru", Some(1)),
                result("mrq-02", "en-ru", None),
            ],
            Latency {
                install_ms: 1,
                embed_ms: 2,
                recall_p50_ms: 3.5,
                recall_p95_ms: 9.5,
            },
        );
        let md = report.to_markdown();
        assert!(md.contains("| overall |"), "{md}");
        assert!(md.contains("| ru-ru |"), "{md}");
        assert!(md.contains("| mrq-01 | ru-ru | 1 | mr-01 |"), "{md}");
        assert!(md.contains("| mrq-02 | en-ru | — | — |"), "{md}");
    }

    #[test]
    fn the_report_round_trips_through_json() {
        let report = MemoryRecallBenchReport::new(
            provenance(),
            Metrics::default(),
            BTreeMap::new(),
            vec![result("mrq-01", "ru-ru", Some(2))],
            Latency::default(),
        );
        let json = serde_json::to_string(&report).expect("serialize");
        let back: MemoryRecallBenchReport = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, report);
    }
}
