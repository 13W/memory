//! The memory-recall benchmark report — X-010/X-011.
//!
//! Unlike `crate::bench::report`, a single [`MemoryRecallBenchReport`]
//! carries **no baseline diff**: nothing measured
//! `local_rag_memory::recall::pipeline::recall`'s retrieval quality before
//! X-010 (see this module's parent doc), so there was no prior number to
//! diff a first run against. X-011 adds the diff this crate never had one to
//! compare against: [`compare`] takes one report per [`super::run::Config`]
//! from the *same* run and diffs every non-baseline configuration against
//! whichever one in the set is `baseline` — a within-run comparison, not a
//! comparison against a separately recorded prior artifact.

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
    /// Which text variant fed the store and which fed the query —
    /// `baseline`/`store_en`/`query_en`/`both_en` (`super::run::Config::as_str`).
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

/// One benchmark run, one [`super::run::Config`].
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

    /// A human-readable summary of this one configuration's run.
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

        out.push_str("## Metrics\n\n");
        out.push_str(metrics_table_header());
        out.push_str(&metrics_row("overall", &self.metrics));
        for (pair, m) in &self.metrics_by_lang_pair {
            out.push_str(&metrics_row(pair, m));
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

fn metrics_table_header() -> &'static str {
    "| Group | Hit@1 | Hit@3 | Hit@5 | MRR | n |\n| --- | --- | --- | --- | --- | --- |\n"
}

fn metrics_row(name: &str, m: &Metrics) -> String {
    format!(
        "| {name} | {:.4} | {:.4} | {:.4} | {:.4} | {} |\n",
        m.hit_at_1, m.hit_at_3, m.hit_at_5, m.mrr, m.query_count
    )
}

/// `config - baseline`, per metric — positive means the configuration scored
/// higher than baseline on that metric.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct MetricsDelta {
    pub hit_at_1: f64,
    pub hit_at_3: f64,
    pub hit_at_5: f64,
    pub mrr: f64,
}

fn delta(base: &Metrics, m: &Metrics) -> MetricsDelta {
    MetricsDelta {
        hit_at_1: m.hit_at_1 - base.hit_at_1,
        hit_at_3: m.hit_at_3 - base.hit_at_3,
        hit_at_5: m.hit_at_5 - base.hit_at_5,
        mrr: m.mrr - base.mrr,
    }
}

/// One non-baseline configuration's report, plus its delta against the
/// baseline report in the same [`ComparisonReport`] — overall, and per
/// `lang_pair` (the per-pair breakdown is the point: an aggregate delta
/// dominated by same-language controls that were already at `1.0` would
/// hide exactly the cross-lingual effect this comparison exists to show).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigComparison {
    pub report: MemoryRecallBenchReport,
    pub delta_overall: MetricsDelta,
    pub delta_by_lang_pair: BTreeMap<String, MetricsDelta>,
}

/// Why [`compare`] could not build a comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompareError {
    /// None of the reports has `provenance.config == "baseline"`.
    NoBaseline,
    /// Two reports both claim `provenance.config == "baseline"`.
    DuplicateBaseline,
}

impl std::fmt::Display for CompareError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompareError::NoBaseline => write!(f, "no report in the set has config \"baseline\""),
            CompareError::DuplicateBaseline => {
                write!(f, "more than one report in the set has config \"baseline\"")
            }
        }
    }
}

impl std::error::Error for CompareError {}

/// The whole-run comparison: the baseline report, plus every other
/// configuration's report and its delta against it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComparisonReport {
    pub schema_version: u32,
    pub baseline: MemoryRecallBenchReport,
    /// In the same order `reports` was given, minus the baseline itself.
    pub configs: Vec<ConfigComparison>,
}

pub const COMPARISON_SCHEMA_VERSION: u32 = 1;

/// Build a [`ComparisonReport`] from one run's per-[`super::run::Config`]
/// reports. Fails if the set does not contain exactly one `baseline` — a
/// comparison against a missing or ambiguous reference point would be
/// worse than no comparison at all.
pub fn compare(reports: Vec<MemoryRecallBenchReport>) -> Result<ComparisonReport, CompareError> {
    let baseline_positions: Vec<usize> = reports
        .iter()
        .enumerate()
        .filter(|(_, r)| r.provenance.config == "baseline")
        .map(|(i, _)| i)
        .collect();
    match baseline_positions.as_slice() {
        [] => return Err(CompareError::NoBaseline),
        [_one] => {}
        _ => return Err(CompareError::DuplicateBaseline),
    }
    let baseline_idx = baseline_positions[0];

    let mut reports = reports;
    let baseline = reports.remove(baseline_idx);

    let configs = reports
        .into_iter()
        .map(|r| {
            let delta_overall = delta(&baseline.metrics, &r.metrics);
            let delta_by_lang_pair = baseline
                .metrics_by_lang_pair
                .iter()
                .filter_map(|(pair, base_m)| {
                    r.metrics_by_lang_pair
                        .get(pair)
                        .map(|m| (pair.clone(), delta(base_m, m)))
                })
                .collect();
            ConfigComparison {
                report: r,
                delta_overall,
                delta_by_lang_pair,
            }
        })
        .collect();

    Ok(ComparisonReport {
        schema_version: COMPARISON_SCHEMA_VERSION,
        baseline,
        configs,
    })
}

impl ComparisonReport {
    /// A human-readable summary across every configuration.
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str("# Memory recall benchmark — configuration comparison\n\n");
        let p = &self.baseline.provenance;
        out.push_str("## Run provenance\n\n| Field | Value |\n| --- | --- |\n");
        out.push_str(&format!("| v2 commit | `{}` |\n", p.v2_commit));
        out.push_str(&format!(
            "| Corpus | `{}` @ `{}` |\n",
            p.corpus_path, p.corpus_version
        ));
        out.push_str(&format!("| Model | `{}` |\n", p.model_id));
        out.push_str(&format!(
            "| Corpus size | {} entries, {} queries |\n\n",
            p.entry_count, p.query_count
        ));

        out.push_str("## Overall MRR by configuration\n\n");
        out.push_str("| Config | Hit@1 | Hit@3 | Hit@5 | MRR | Δ MRR vs baseline |\n");
        out.push_str("| --- | --- | --- | --- | --- | --- |\n");
        out.push_str(&format!(
            "| {} | {:.4} | {:.4} | {:.4} | {:.4} | — |\n",
            self.baseline.provenance.config,
            self.baseline.metrics.hit_at_1,
            self.baseline.metrics.hit_at_3,
            self.baseline.metrics.hit_at_5,
            self.baseline.metrics.mrr,
        ));
        for c in &self.configs {
            out.push_str(&format!(
                "| {} | {:.4} | {:.4} | {:.4} | {:.4} | {:+.4} |\n",
                c.report.provenance.config,
                c.report.metrics.hit_at_1,
                c.report.metrics.hit_at_3,
                c.report.metrics.hit_at_5,
                c.report.metrics.mrr,
                c.delta_overall.mrr,
            ));
        }

        out.push_str(
            "\n## MRR by lang_pair — where a normalized configuration would actually help\n\n",
        );
        out.push_str(
            "An aggregate delta dominated by same-language controls already at 1.0 hides\n",
        );
        out.push_str("the cross-lingual effect this table exists to show — read `ru-en`/`en-ru`\n");
        out.push_str("rows, not just `overall`, before drawing a conclusion.\n\n");
        let mut pairs: Vec<&String> = self.baseline.metrics_by_lang_pair.keys().collect();
        pairs.sort();
        out.push_str("| lang_pair | baseline MRR | ");
        for c in &self.configs {
            out.push_str(&format!("{} MRR (Δ) | ", c.report.provenance.config));
        }
        out.push_str("\n| --- | --- | ");
        for _ in &self.configs {
            out.push_str("--- | ");
        }
        out.push('\n');
        for pair in pairs {
            let base_m = &self.baseline.metrics_by_lang_pair[pair];
            out.push_str(&format!("| {pair} | {:.4} | ", base_m.mrr));
            for c in &self.configs {
                let m = c.report.metrics_by_lang_pair.get(pair);
                let d = c.delta_by_lang_pair.get(pair);
                match (m, d) {
                    (Some(m), Some(d)) => out.push_str(&format!("{:.4} ({:+.4}) | ", m.mrr, d.mrr)),
                    _ => out.push_str("— | "),
                }
            }
            out.push('\n');
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provenance(config: &str) -> Provenance {
        Provenance {
            v2_commit: "abc1234".to_string(),
            corpus_path: "fixtures/memory-recall/corpus.json".to_string(),
            corpus_version: "1.0.0".to_string(),
            model_id: "embeddinggemma-300m".to_string(),
            config: config.to_string(),
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

    fn metrics(mrr: f64, n: usize) -> Metrics {
        Metrics {
            hit_at_1: mrr,
            hit_at_3: mrr,
            hit_at_5: mrr,
            mrr,
            query_count: n,
        }
    }

    fn report(config: &str, mrr: f64, pair_mrr: &[(&str, f64)]) -> MemoryRecallBenchReport {
        let by_pair = pair_mrr
            .iter()
            .map(|(pair, m)| (pair.to_string(), metrics(*m, 4)))
            .collect();
        MemoryRecallBenchReport::new(
            provenance(config),
            metrics(mrr, 24),
            by_pair,
            vec![result("mrq-01", "ru-en", Some(1))],
            Latency::default(),
        )
    }

    #[test]
    fn markdown_reports_every_lang_pair_row_and_the_overall_row() {
        let mut by_pair = BTreeMap::new();
        by_pair.insert("ru-ru".to_string(), metrics(1.0, 8));
        let r = MemoryRecallBenchReport::new(
            provenance("baseline"),
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
        let md = r.to_markdown();
        assert!(md.contains("| overall |"), "{md}");
        assert!(md.contains("| ru-ru |"), "{md}");
        assert!(md.contains("| mrq-01 | ru-ru | 1 | mr-01 |"), "{md}");
        assert!(md.contains("| mrq-02 | en-ru | — | — |"), "{md}");
    }

    #[test]
    fn the_report_round_trips_through_json() {
        let r = report("baseline", 0.5, &[("ru-en", 0.5)]);
        let json = serde_json::to_string(&r).expect("serialize");
        let back: MemoryRecallBenchReport = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, r);
    }

    #[test]
    fn compare_computes_config_minus_baseline() {
        let reports = vec![
            report("baseline", 0.50, &[("ru-en", 0.25), ("en-en", 1.0)]),
            report("both_en", 0.75, &[("ru-en", 0.90), ("en-en", 1.0)]),
        ];
        let cmp = compare(reports).expect("compare");
        assert_eq!(cmp.baseline.provenance.config, "baseline");
        assert_eq!(cmp.configs.len(), 1);
        let both_en = &cmp.configs[0];
        assert_eq!(both_en.report.provenance.config, "both_en");
        assert!((both_en.delta_overall.mrr - 0.25).abs() < 1e-12);
        assert!((both_en.delta_by_lang_pair["ru-en"].mrr - 0.65).abs() < 1e-12);
        assert!((both_en.delta_by_lang_pair["en-en"].mrr - 0.0).abs() < 1e-12);
    }

    #[test]
    fn compare_refuses_a_set_with_no_baseline() {
        let reports = vec![report("store_en", 0.5, &[])];
        assert_eq!(compare(reports), Err(CompareError::NoBaseline));
    }

    #[test]
    fn compare_refuses_a_set_with_two_baselines() {
        let reports = vec![report("baseline", 0.5, &[]), report("baseline", 0.6, &[])];
        assert_eq!(compare(reports), Err(CompareError::DuplicateBaseline));
    }

    #[test]
    fn a_lang_pair_missing_from_a_non_baseline_config_is_skipped_not_panicked() {
        let reports = vec![
            report("baseline", 0.5, &[("ru-en", 0.5), ("en-ru", 0.3)]),
            report("query_en", 0.6, &[("ru-en", 0.6)]),
        ];
        let cmp = compare(reports).expect("compare");
        let query_en = &cmp.configs[0];
        assert!(query_en.delta_by_lang_pair.contains_key("ru-en"));
        assert!(!query_en.delta_by_lang_pair.contains_key("en-ru"));
    }

    #[test]
    fn comparison_markdown_includes_every_config_and_lang_pair() {
        let reports = vec![
            report("baseline", 0.50, &[("ru-en", 0.25)]),
            report("query_en", 0.60, &[("ru-en", 0.60)]),
            report("both_en", 0.75, &[("ru-en", 0.90)]),
        ];
        let cmp = compare(reports).expect("compare");
        let md = cmp.to_markdown();
        assert!(md.contains("| baseline |"), "{md}");
        assert!(md.contains("| query_en |"), "{md}");
        assert!(md.contains("| both_en |"), "{md}");
        assert!(md.contains("| ru-en |"), "{md}");
        assert!(md.contains("+0.1000"), "{md}");
    }
}
