//! The versioned release report itself (spec 14 §2's 9 acceptance gates) —
//! T17-05: [`ReleaseReport`] and its sub-summaries, and [`ReleaseReport::to_markdown`].
//!
//! `quality`/`memory-quality` embed the exact reports `cargo xtask bench`/
//! `cargo xtask memory-bench` already produce (`crate::bench::report::BenchReport`,
//! `crate::memory_bench::report::MemoryBenchReport`) plus a pass/fail verdict
//! from `crate::release_report::gate`. `latency`/`resources` carry this
//! release's first-established v2 baseline — never gated, see the
//! `release_report` module doc. `reliability`/`consistency`/`sharing`/
//! `idempotency`/`rebuild` are [`TestCitation`]s: a fresh re-run (not a quote
//! of old `PROGRESS.md` prose) of the real test suites spec 14 §2 already
//! relies on for each row, assembled by `crate::release_report::run`.

use serde::{Deserialize, Serialize};

use crate::bench::report::BenchReport;
use crate::memory_bench::report::MemoryBenchReport;
use crate::release_report::latency::ReconcileLatency;
use crate::release_report::resources::ResourceMetrics;

/// This report's schema version.
pub const REPORT_SCHEMA_VERSION: u32 = 1;

/// Where a release report came from.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provenance {
    /// The v2 commit under test.
    pub v2_commit: String,
    /// Host triple.
    pub host: String,
}

/// The `quality` gate row (spec 14 §2), backed by a real `cargo xtask bench`
/// run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QualityGateSummary {
    /// The full benchmark report — same shape `fixtures/search/baseline/`
    /// runs are stored in.
    pub report: BenchReport,
    /// Whether a thresholds file existed to evaluate against. `false` only
    /// if `fixtures/search/baseline/thresholds.json` is ever removed —
    /// mirrors `run_bench`'s own "the run *is* the evidence thresholds are
    /// derived from" fallback (O2).
    pub gated: bool,
    /// The verdict; `true` when `gated` is `false` (nothing to fail against).
    pub passed: bool,
}

/// The `memory-quality` gate row (spec 14 §2), backed by a real
/// `cargo xtask memory-bench` run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryQualityGateSummary {
    pub report: MemoryBenchReport,
    pub gated: bool,
    pub passed: bool,
}

/// The `latency` gate row (spec 14 §2): warm-search p95 (already measured by
/// `cargo xtask bench`) plus the two reconcile scenarios T17-05 adds. Never
/// gated — see the `release_report` module doc.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LatencySummary {
    pub search_p50_ms: f64,
    pub search_p95_ms: f64,
    pub reconcile: ReconcileLatency,
}

/// A fresh re-run of the named test suite(s) that already establish one
/// acceptance-gate row, plus a pointer to which named tests within them
/// cover it and which spec section states the requirement. `commands` are
/// exactly what was run — a reader can reproduce `passed` without
/// re-deriving anything.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestCitation {
    pub commands: Vec<String>,
    pub named_tests: Vec<String>,
    pub passed: bool,
    pub spec_refs: Vec<String>,
}

/// One full release-report run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseReport {
    pub schema_version: u32,
    pub provenance: Provenance,
    pub quality: QualityGateSummary,
    pub memory_quality: MemoryQualityGateSummary,
    pub latency: LatencySummary,
    pub resources: ResourceMetrics,
    pub reliability: TestCitation,
    pub consistency: TestCitation,
    pub sharing: TestCitation,
    pub idempotency: TestCitation,
    pub rebuild: TestCitation,
}

impl ReleaseReport {
    /// Whether every gated row passed. `latency`/`resources` are
    /// deliberately excluded — they carry no pass/fail verdict at all (see
    /// the module doc).
    pub fn overall_passed(&self) -> bool {
        self.quality.passed
            && self.memory_quality.passed
            && self.reliability.passed
            && self.consistency.passed
            && self.sharing.passed
            && self.idempotency.passed
            && self.rebuild.passed
    }

    /// A human-readable summary, in the shape of `BenchReport`/
    /// `MemoryBenchReport`'s own `.report.md`.
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str("# Release report — spec 14 §2 acceptance gates\n\n");
        out.push_str("## Provenance\n\n| Field | Value |\n| --- | --- |\n");
        out.push_str(&format!(
            "| v2 commit | `{}` |\n",
            self.provenance.v2_commit
        ));
        out.push_str(&format!("| Host | {} |\n\n", self.provenance.host));

        out.push_str("## Gate summary\n\n| Gate | Verdict |\n| --- | --- |\n");
        let verdict = |gated: bool, passed: bool| -> &'static str {
            match (gated, passed) {
                (false, _) => "no thresholds yet — baseline only",
                (true, true) => "PASS",
                (true, false) => "FAIL",
            }
        };
        out.push_str(&format!(
            "| quality | {} |\n",
            verdict(self.quality.gated, self.quality.passed)
        ));
        out.push_str(&format!(
            "| memory-quality | {} |\n",
            verdict(self.memory_quality.gated, self.memory_quality.passed)
        ));
        out.push_str("| latency | baseline only, never gated |\n");
        out.push_str("| resources | baseline only, never gated |\n");
        let cited = |c: &TestCitation| if c.passed { "PASS" } else { "FAIL" };
        out.push_str(&format!("| reliability | {} |\n", cited(&self.reliability)));
        out.push_str(&format!("| consistency | {} |\n", cited(&self.consistency)));
        out.push_str(&format!("| sharing | {} |\n", cited(&self.sharing)));
        out.push_str(&format!("| idempotency | {} |\n", cited(&self.idempotency)));
        out.push_str(&format!("| rebuild | {} |\n", cited(&self.rebuild)));
        out.push_str(&format!(
            "\n**Overall: {}**\n",
            if self.overall_passed() {
                "PASS"
            } else {
                "FAIL"
            }
        ));

        out.push_str("\n## Quality (search)\n\n");
        out.push_str(&format!(
            "Hit@1={:.4} Hit@3={:.4} Hit@5={:.4} MRR={:.4} (v1: {:.4}/{:.4}/{:.4}/{:.4})\n",
            self.quality.report.metrics.hit_at_1,
            self.quality.report.metrics.hit_at_3,
            self.quality.report.metrics.hit_at_5,
            self.quality.report.metrics.mrr,
            self.quality.report.baseline.hit_at_1,
            self.quality.report.baseline.hit_at_3,
            self.quality.report.baseline.hit_at_5,
            self.quality.report.baseline.mrr,
        ));

        out.push_str("\n## Memory quality (router)\n\n");
        out.push_str(&format!(
            "Precision={:.4} Recall={:.4} F1={:.4}\n",
            self.memory_quality.report.metrics.precision,
            self.memory_quality.report.metrics.recall,
            self.memory_quality.report.metrics.f1,
        ));

        out.push_str(
            "\n## Latency (baseline)\n\n| Scenario | p50 ms | p95 ms |\n| --- | --- | --- |\n",
        );
        out.push_str(&format!(
            "| warm search | {:.3} | {:.3} |\n",
            self.latency.search_p50_ms, self.latency.search_p95_ms
        ));
        out.push_str(&format!(
            "| reconcile: one file | {:.3} | {:.3} |\n",
            self.latency.reconcile.one_file_p50_ms, self.latency.reconcile.one_file_p95_ms
        ));
        out.push_str(&format!(
            "| reconcile: branch checkout ({} files) | {:.3} | {:.3} |\n",
            self.latency.reconcile.branch_checkout_files_changed,
            self.latency.reconcile.branch_checkout_p50_ms,
            self.latency.reconcile.branch_checkout_p95_ms
        ));

        out.push_str("\n## Resources (baseline)\n\n| Metric | Value |\n| --- | --- |\n");
        if let Some(ram) = &self.resources.idle_ram {
            out.push_str(&format!(
                "| Idle RAM (min/mean/max/last, bytes) | {} / {} / {} / {} |\n",
                ram.min_bytes, ram.mean_bytes, ram.max_bytes, ram.last_bytes
            ));
        } else {
            out.push_str("| Idle RAM | not measured (no `local-rag` binary given) |\n");
        }
        out.push_str(&format!(
            "| state.sqlite bytes | {} |\n",
            self.resources.state_db_bytes
        ));
        out.push_str(&format!(
            "| cache.sqlite bytes | {} |\n",
            self.resources.cache_db_bytes
        ));
        out.push_str(&format!(
            "| shard dir bytes | {} |\n",
            self.resources.shard_dir_bytes
        ));
        out.push_str(&format!(
            "| bytes/symbol ({} occurrences) | {:.2} |\n",
            self.resources.occurrences, self.resources.bytes_per_symbol
        ));
        out.push_str(&format!(
            "| embedding cache budget ratio | {:.4} ({} / {} bytes) |\n",
            self.resources.cache_budget_ratio,
            self.resources.embedding_cache_total_bytes,
            self.resources.embedding_cache_budget_bytes
        ));
        out.push_str(&format!(
            "| source/worktree byte ratio | {:.4} ({} / {} bytes) |\n",
            self.resources.source_worktree_ratio,
            self.resources.source_bytes,
            self.resources.worktree_bytes
        ));

        out.push_str("\n## Test citations\n\n");
        let render_citation = |out: &mut String, name: &str, c: &TestCitation| {
            out.push_str(&format!(
                "### {name} — {}\n\n",
                if c.passed { "PASS" } else { "FAIL" }
            ));
            out.push_str("Commands:\n\n");
            for cmd in &c.commands {
                out.push_str(&format!("- `{cmd}`\n"));
            }
            out.push_str("\nNamed tests:\n\n");
            for t in &c.named_tests {
                out.push_str(&format!("- {t}\n"));
            }
            out.push_str("\nSpec refs:\n\n");
            for r in &c.spec_refs {
                out.push_str(&format!("- {r}\n"));
            }
            out.push('\n');
        };
        render_citation(&mut out, "Reliability", &self.reliability);
        render_citation(&mut out, "Consistency", &self.consistency);
        render_citation(&mut out, "Sharing", &self.sharing);
        render_citation(&mut out, "Idempotency", &self.idempotency);
        render_citation(&mut out, "Rebuild", &self.rebuild);

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::report::{Latency as BenchLatency, Provenance as BenchProvenance};
    use crate::bench::score::Metrics as BenchMetrics;
    use crate::memory_bench::report::{Latency as MemLatency, Provenance as MemProvenance};
    use crate::memory_bench::score::Metrics as MemMetrics;
    use crate::release_report::resources::ResourceMetrics;

    fn bench_report() -> BenchReport {
        BenchReport::new(
            BenchProvenance {
                v2_commit: "abc".to_string(),
                corpus_path: "p".to_string(),
                corpus_commit: "g".to_string(),
                corpus_subdir: None,
                corpus_version: "1.0.0".to_string(),
                model_id: "m".to_string(),
                mode: "hybrid".to_string(),
                dense_kind: "code_raw".to_string(),
                fusion_lexical_weight: Some(1.0),
                files_indexed: 10,
                occurrences: 50,
                host: "h".to_string(),
            },
            BenchMetrics {
                hit_at_1: 0.6,
                hit_at_3: 0.8,
                hit_at_5: 0.85,
                mrr: 0.7,
                recall_at_5: 0.85,
            },
            Vec::new(),
            BenchLatency {
                index_ms: 1,
                embed_ms: 2,
                search_p50_ms: 3.0,
                search_p95_ms: 9.0,
            },
        )
    }

    fn memory_report() -> MemoryBenchReport {
        MemoryBenchReport::new(
            MemProvenance {
                commit: "abc".to_string(),
                corpus_path: "p".to_string(),
                corpus_version: "1.0.0".to_string(),
                case_count: 2,
                model_id: "m".to_string(),
                sampling: "greedy".to_string(),
                router_version: "v0".to_string(),
                host: "h".to_string(),
            },
            MemMetrics {
                precision: 0.9,
                recall: 0.8,
                f1: 0.85,
                exact_match_rate: 0.7,
            },
            Vec::new(),
            MemLatency {
                install_ms: 0,
                load_ms: 1,
                route_p50_ms: 2.0,
                route_p95_ms: 3.0,
            },
        )
    }

    fn citation(passed: bool) -> TestCitation {
        TestCitation {
            commands: vec!["cargo test -p x".to_string()],
            named_tests: vec!["x::y (G05)".to_string()],
            passed,
            spec_refs: vec!["14-acceptance-and-testing.md §2".to_string()],
        }
    }

    fn report(all_passed: bool) -> ReleaseReport {
        ReleaseReport {
            schema_version: REPORT_SCHEMA_VERSION,
            provenance: Provenance {
                v2_commit: "abc1234".to_string(),
                host: "aarch64-apple-darwin".to_string(),
            },
            quality: QualityGateSummary {
                report: bench_report(),
                gated: true,
                passed: all_passed,
            },
            memory_quality: MemoryQualityGateSummary {
                report: memory_report(),
                gated: true,
                passed: all_passed,
            },
            latency: LatencySummary {
                search_p50_ms: 3.0,
                search_p95_ms: 9.0,
                reconcile: ReconcileLatency {
                    one_file_ms: vec![1.0],
                    one_file_p50_ms: 1.0,
                    one_file_p95_ms: 1.0,
                    branch_checkout_ms: vec![2.0],
                    branch_checkout_p50_ms: 2.0,
                    branch_checkout_p95_ms: 2.0,
                    branch_checkout_files_changed: 5,
                },
            },
            resources: ResourceMetrics {
                idle_ram: None,
                state_db_bytes: 100,
                cache_db_bytes: 200,
                shard_dir_bytes: 300,
                occurrences: 50,
                bytes_per_symbol: 12.0,
                embedding_cache_total_bytes: 1_000,
                embedding_cache_budget_bytes: 500_000_000,
                cache_budget_ratio: 0.000002,
                source_bytes: 10_000,
                worktree_bytes: 20_000,
                source_worktree_ratio: 0.5,
            },
            reliability: citation(all_passed),
            consistency: citation(all_passed),
            sharing: citation(all_passed),
            idempotency: citation(all_passed),
            rebuild: citation(all_passed),
        }
    }

    #[test]
    fn overall_passed_requires_every_gated_row() {
        assert!(report(true).overall_passed());
        assert!(!report(false).overall_passed());
    }

    /// Baseline rows never gate: their own `passed` field must not
    /// participate in [`ReleaseReport::overall_passed`], since they carry no
    /// verdict at all (see the `release_report` module doc).
    #[test]
    fn latency_and_resources_never_affect_the_overall_verdict() {
        let mut r = report(true);
        // Nothing to flip: `LatencySummary`/`ResourceMetrics` carry no
        // `passed` field at all, which is itself the point being tested.
        assert!(r.overall_passed());
        r.quality.passed = false;
        assert!(!r.overall_passed());
    }

    #[test]
    fn to_markdown_names_every_gate_and_is_never_empty_for_a_failure() {
        let r = report(false);
        let md = r.to_markdown();
        assert!(md.contains("**Overall: FAIL**"), "{md}");
        assert!(md.contains("| quality | FAIL |"), "{md}");
        assert!(md.contains("### Reliability — FAIL"), "{md}");
        assert!(md.contains("cargo test -p x"), "{md}");
    }

    #[test]
    fn to_markdown_reports_pass_when_every_gated_row_passes() {
        let md = report(true).to_markdown();
        assert!(md.contains("**Overall: PASS**"), "{md}");
        assert!(md.contains("| quality | PASS |"), "{md}");
    }

    #[test]
    fn an_ungated_quality_row_renders_as_baseline_only_not_pass_or_fail() {
        let mut r = report(true);
        r.quality.gated = false;
        let md = r.to_markdown();
        assert!(
            md.contains("| quality | no thresholds yet — baseline only |"),
            "{md}"
        );
    }
}
