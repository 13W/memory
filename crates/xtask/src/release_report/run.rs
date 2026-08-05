//! Orchestrates one full release-report run (T17-05, spec 14 §2's 9
//! acceptance gates), against a real corpus and real model weights.
//!
//! One real indexed store (`crate::bench::run::build_indexed_store`) backs
//! *three* of the nine rows — `quality` (`crate::bench::run::score_queries`),
//! `resources`, and `latency`'s reconcile scenarios — so this never indexes
//! the corpus twice. Order matters: `latency::measure` mutates real files
//! under the indexed root (`touch_file`), so it must run **last** against
//! that store, after `score_queries`/`resources::measure` have both read it.
//! `memory-quality` measures a wholly separate corpus (the memory-router
//! case index) and gets its own real run via `crate::memory_bench::run`.
//! `reliability`/`consistency`/`sharing`/`idempotency`/`rebuild` are fresh
//! re-runs of the `--features failpoints` test suites spec 14 §2 already
//! relies on for each row (never a quote of old `PROGRESS.md` prose).
//!
//! # The corpus is copied, never indexed in place (T17-05's own incident)
//!
//! `latency::measure` mutates real files under the indexed root
//! (`touch_file`). Every `--corpus` this command has ever actually been run
//! against in this repository's history is someone's real git checkout —
//! `cargo xtask bench`'s own recorded evidence uses `/opt/soft/local-rag`,
//! the v1 legacy checkout kept on disk specifically as a stable text corpus.
//! Indexing (and then mutating) that checkout **in place** would corrupt a
//! real repository exactly the way this same task's own `latency`/
//! `resources` tests once corrupted this crate's own `bench/*.rs` files
//! before `disposable_corpus_copy()` was invented to stop it (see
//! `latency::tests`' and `resources::tests`' own doc comments). [`run`]
//! therefore copies `options.corpus_dir` into a disposable temp directory
//! (skipping `bench::run::PRUNED_DIRECTORIES` — `node_modules`/`dist` can be
//! large and are never indexed anyway) and only ever touches that copy.
//! `.git` does not survive the copy (irrelevant to what's indexed, and
//! usually the largest single subtree), so `corpus_commit` is captured from
//! the **original** checkout up front and patched into the report
//! afterward, rather than left for `bench::run` to look up on a `.git`-less
//! copy.

use std::path::{Path, PathBuf};

use crate::release_report::report::{
    LatencySummary, Provenance, REPORT_SCHEMA_VERSION, ReleaseReport, TestCitation,
};
use crate::release_report::{gate, latency, resources};

/// What `cargo xtask release-report` was asked to do.
pub struct Options {
    /// The corpus checkout the search benchmark indexes (same meaning as
    /// `crate::bench::run::Options::corpus_dir`).
    pub corpus_dir: PathBuf,
    /// Optional subdirectory of that checkout to index instead of the whole
    /// thing (same meaning as `crate::bench::run::Options::subdir`).
    pub subdir: Option<String>,
    /// The memory-router case-index fixture to score; defaults to
    /// `crate::memory_bench::case_index_path()`.
    pub memory_corpus_path: Option<PathBuf>,
    /// A catalog `model_id` to run the memory-router benchmark with instead
    /// of `local_rag_generate::DEFAULT_MODEL_ID`.
    pub memory_model_id: Option<String>,
    /// A built `local-rag` binary to spawn for the idle-RAM measurement.
    /// `None` skips idle RAM (recorded as `None` in the report, not
    /// fabricated) — mirrors `resources::measure_idle_ram`'s own
    /// "bring your own binary" precondition.
    pub local_rag_bin: Option<PathBuf>,
}

/// Run every measurement and re-run every citation, assembling one
/// [`ReleaseReport`].
pub async fn run(options: &Options) -> Result<ReleaseReport, String> {
    // Captured from the real checkout before it is ever touched — see the
    // module doc for why the copy below has no `.git` to look this up from.
    let real_corpus_commit = crate::git::git_short_head(&options.corpus_dir);

    let corpus_home =
        local_rag_test_support::TempHome::new().map_err(|e| format!("temp home: {e}"))?;
    let disposable_corpus_dir = corpus_home.join("corpus");
    copy_dir_recursive(
        &options.corpus_dir,
        &disposable_corpus_dir,
        crate::bench::run::PRUNED_DIRECTORIES,
    )?;

    let bench_options = crate::bench::run::Options {
        corpus_dir: disposable_corpus_dir,
        subdir: options.subdir.clone(),
        mode: local_rag_protocol::SearchMode::Hybrid,
        dense_kind: local_rag_store::RepresentationKind::CodeRaw,
        lexical_weights: Vec::new(),
    };
    let corpus = crate::bench::corpus::Corpus::load(&crate::bench::corpus_fixture_path())
        .map_err(|e| format!("search corpus: {e}"))?;
    let mut indexed = crate::bench::run::build_indexed_store(&bench_options).await?;

    // `quality`: score the 49 queries against the store before anything else
    // touches it (see module doc for why order matters here).
    let mut bench_reports =
        crate::bench::run::score_queries(&indexed, &bench_options, &corpus).await?;
    let mut bench_report = bench_reports
        .pop()
        .ok_or_else(|| "score_queries returned no report".to_string())?;
    if let Some(commit) = real_corpus_commit {
        bench_report.provenance.corpus_commit = commit;
    }
    let search_p50_ms = bench_report.latency.search_p50_ms;
    let search_p95_ms = bench_report.latency.search_p95_ms;
    let quality = gate::evaluate_quality(bench_report);

    // `resources`: read-only against the same store (checkpoints both DBs,
    // stats file sizes — never mutates corpus content).
    let idle_ram = match &options.local_rag_bin {
        Some(bin) => match resources::measure_idle_ram(bin) {
            Ok(sample) => Some(sample),
            Err(e) => {
                eprintln!("[release-report] idle RAM: {e}");
                None
            }
        },
        None => {
            eprintln!(
                "[release-report] idle RAM: skipped (no --local-rag-bin given, e.g. \
                 `cargo build -p local-rag` then pass `target/debug/local-rag`)"
            );
            None
        }
    };
    let resource_metrics = resources::measure(&indexed, idle_ram).await?;

    // `latency`: runs last against this store — `touch_file` mutates real
    // corpus files in place.
    let reconcile = latency::measure(&mut indexed).await?;
    let latency_summary = LatencySummary {
        search_p50_ms,
        search_p95_ms,
        reconcile,
    };

    // `memory-quality`: a wholly separate real run, its own corpus.
    let memory_report = crate::memory_bench::run::run(&crate::memory_bench::run::Options {
        case_index_path: options
            .memory_corpus_path
            .clone()
            .unwrap_or_else(crate::memory_bench::case_index_path),
        model_id: options.memory_model_id.clone(),
    })
    .await?;
    let memory_quality = gate::evaluate_memory_quality(memory_report);

    // `reliability`/`consistency`/`sharing`/`idempotency`/`rebuild`: fresh
    // re-runs, each underlying crate suite run exactly once even though some
    // crates cover more than one gate row (F1-F12 lives in
    // `local-rag-projection`, S1/S2 in `local-rag-hook`'s `kill_matrix.rs`,
    // S3-S8 in `local-rag-store`'s `spool_kill_matrix.rs`, generation-mixing
    // in `local-rag-search`, structural-sharing/idempotent-retry in
    // `local-rag-index`, FTS validate/rebuild in `local-rag-store`, dense
    // rebuild-on-doubt/fault-matrix in `local-rag-projection`).
    let store = run_failpoint_tests("local-rag-store");
    let hook = run_failpoint_tests("local-rag-hook");
    let index = run_failpoint_tests("local-rag-index");
    let projection = run_failpoint_tests("local-rag-projection");
    let search = run_failpoint_tests("local-rag-search");

    let reliability = TestCitation {
        commands: vec![
            store.command.clone(),
            hook.command.clone(),
            projection.command.clone(),
        ],
        named_tests: vec![
            "local-rag-projection::fault_matrix / fault_matrix_coverage (F1-F12, spec 05 §10)"
                .to_string(),
            "local-rag-hook::kill_matrix (S1-S2, spec 07 §7)".to_string(),
            "local-rag-store::spool_kill_matrix (S3-S8, spec 07 §7)".to_string(),
        ],
        passed: store.passed && hook.passed && projection.passed,
        spec_refs: vec![
            "docs/specification/14-acceptance-and-testing.md §2 (reliability)".to_string(),
        ],
    };
    let consistency = TestCitation {
        commands: vec![search.command.clone()],
        named_tests: vec![
            "local-rag-search::generation_mixing / switch_concurrency / \
             switch_failpoint_load (G09)"
                .to_string(),
        ],
        passed: search.passed,
        spec_refs: vec![
            "docs/specification/14-acceptance-and-testing.md §2 (consistency)".to_string(),
        ],
    };
    let sharing = TestCitation {
        commands: vec![index.command.clone()],
        named_tests: vec![
            "local-rag-index::persist / reconcile (structural sharing, G05)".to_string(),
        ],
        passed: index.passed,
        spec_refs: vec!["docs/specification/14-acceptance-and-testing.md §2 (sharing)".to_string()],
    };
    let idempotency = TestCitation {
        commands: vec![store.command.clone(), index.command.clone()],
        named_tests: vec![
            "local-rag-store::memory_op (idempotency-key replay, G14)".to_string(),
            "local-rag-index::reconcile / persist (retry produces no duplicate content, G05)"
                .to_string(),
        ],
        passed: store.passed && index.passed,
        spec_refs: vec![
            "docs/specification/14-acceptance-and-testing.md §2 (idempotency)".to_string(),
        ],
    };
    let rebuild = TestCitation {
        commands: vec![store.command, projection.command],
        named_tests: vec![
            "local-rag-store::fts_validate / fts_corruption (validate-on-open / rebuild, G08)"
                .to_string(),
            "local-rag-projection::rebuild_faults / fault_matrix (dense rebuild-on-doubt, G07/G08)"
                .to_string(),
        ],
        passed: store.passed && projection.passed,
        spec_refs: vec!["docs/specification/14-acceptance-and-testing.md §2 (rebuild)".to_string()],
    };

    Ok(ReleaseReport {
        schema_version: REPORT_SCHEMA_VERSION,
        provenance: Provenance {
            v2_commit: crate::git::git_short_head(std::path::Path::new("."))
                .unwrap_or_else(|| "unknown".to_string()),
            host: std::env::consts::ARCH.to_string() + "-" + std::env::consts::OS,
        },
        quality,
        memory_quality,
        latency: latency_summary,
        resources: resource_metrics,
        reliability,
        consistency,
        sharing,
        idempotency,
        rebuild,
    })
}

/// One crate's `--features failpoints` suite, run once and reused across
/// every [`TestCitation`] its tests belong to.
struct SuiteResult {
    command: String,
    passed: bool,
}

fn run_failpoint_tests(crate_name: &str) -> SuiteResult {
    let args = ["test", "-p", crate_name, "--features", "failpoints"];
    let command = format!("cargo {}", args.join(" "));
    eprintln!("[release-report] running: {command}");
    let cargo = env!("CARGO");
    let (passed, log) = match std::process::Command::new(cargo).args(args).output() {
        Ok(output) => {
            let mut log = String::new();
            log.push_str(&String::from_utf8_lossy(&output.stdout));
            log.push_str(&String::from_utf8_lossy(&output.stderr));
            (output.status.success(), log)
        }
        Err(e) => (false, format!("spawn `{command}`: {e}")),
    };
    if !passed {
        eprintln!("[release-report] FAILED: {command}\n{log}");
    }
    SuiteResult { command, passed }
}

/// Copies `src` into `dst` (created if absent), skipping any directory whose
/// file name is in `pruned` — see the module doc for why this exists at all
/// (the corpus is never indexed, and therefore never mutated, in place).
/// Symlinks are skipped, mirroring `resources::dir_size_bytes`'s own
/// no-follow-symlink discipline.
fn copy_dir_recursive(src: &Path, dst: &Path, pruned: &[&str]) -> Result<(), String> {
    std::fs::create_dir_all(dst).map_err(|e| format!("create {}: {e}", dst.display()))?;
    for entry in std::fs::read_dir(src).map_err(|e| format!("read_dir {}: {e}", src.display()))? {
        let entry = entry.map_err(|e| format!("dir entry under {}: {e}", src.display()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|e| format!("file type of {}: {e}", path.display()))?;
        let dst_path = dst.join(entry.file_name());
        if file_type.is_dir() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str())
                && pruned.contains(&name)
            {
                continue;
            }
            copy_dir_recursive(&path, &dst_path, pruned)?;
        } else if file_type.is_file() {
            std::fs::copy(&path, &dst_path)
                .map_err(|e| format!("copy {} -> {}: {e}", path.display(), dst_path.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_files_and_skips_pruned_subdirectories() {
        let src = local_rag_test_support::TempHome::new().expect("src temp home");
        std::fs::write(src.join("a.rs"), b"fn a() {}").unwrap();
        std::fs::create_dir_all(src.join("keep")).unwrap();
        std::fs::write(src.join("keep/b.rs"), b"fn b() {}").unwrap();
        std::fs::create_dir_all(src.join("node_modules")).unwrap();
        std::fs::write(src.join("node_modules/junk.js"), b"junk").unwrap();

        let dst = local_rag_test_support::TempHome::new().expect("dst temp home");
        let dst_dir = dst.join("copy");
        copy_dir_recursive(src.path(), &dst_dir, &["node_modules"]).expect("copy");

        assert!(dst_dir.join("a.rs").is_file());
        assert!(dst_dir.join("keep/b.rs").is_file());
        assert!(!dst_dir.join("node_modules").exists());
        // The source is never touched — the whole point of copying first.
        assert!(src.join("a.rs").is_file());
        assert!(src.join("node_modules/junk.js").is_file());
    }
}
