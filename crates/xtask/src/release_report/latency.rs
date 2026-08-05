//! Reconcile-latency measurements for spec 14 §2's `latency` gate (T17-05):
//! one-file and branch-checkout reconcile p95/p50. Recorded as this
//! release's first-established v2 baseline — never gated (see the
//! `release_report` module doc; warm-search p95, the third `latency` row
//! scenario, is already measured by `crate::bench::run`).
//!
//! Both scenarios call the exact same production entry point,
//! `local_rag_index::reconcile::reconcile_once`, in `ScanMode::Fast` — the
//! same mode production's own `TriggerKind::FsChange`/`TriggerKind::GitHead`
//! triggers both select (`crates/index/src/reconcile/schedule.rs`); they
//! differ only in the *shape* of the on-disk change between calls, not in
//! scan mode. Both run against the exact same real, already-indexed
//! throwaway store `crate::bench::run::build_indexed_store` produces — no
//! second indexing harness — reusing its warmed `StatCache` so this measures
//! the real warm-cache path production uses, not a cold first touch of every
//! file.

use std::io::Write;
use std::time::Instant;

use local_rag_core::identity::path::CaseSensitivity;
use local_rag_core::redaction::Scanner;
use local_rag_index::classify::ClassifierConfig;
use local_rag_index::reconcile::{WorktreeMeta, reconcile_once};
use local_rag_index::scan::ScanMode;
use local_rag_store::WorktreeKind;

use crate::bench::run::{IndexedStore, PRUNED_DIRECTORIES};
use crate::stats::percentile;

/// Untimed warm-up passes before the measured ones — mirrors
/// `bench::run`'s own `WARMUP_PASSES`.
const WARMUP_PASSES: usize = 1;
/// Timed passes per scenario; p50/p95 are taken over all of them.
const TIMED_PASSES: usize = 5;
/// The branch-checkout scenario touches this fraction of already-indexed
/// files — reproducible independent of corpus size, bounded so it stays
/// meaningful on a tiny corpus and bounded on a huge one.
const BRANCH_CHECKOUT_FRACTION: f64 = 0.10;
const BRANCH_CHECKOUT_MIN_FILES: usize = 5;
const BRANCH_CHECKOUT_MAX_FILES: usize = 50;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ReconcileLatency {
    pub one_file_ms: Vec<f64>,
    pub one_file_p50_ms: f64,
    pub one_file_p95_ms: f64,
    pub branch_checkout_ms: Vec<f64>,
    pub branch_checkout_p50_ms: f64,
    pub branch_checkout_p95_ms: f64,
    /// How many files the branch-checkout scenario touched per cycle — a
    /// reader needs this to judge whether the two numbers are comparable
    /// across a differently-sized corpus.
    pub branch_checkout_files_changed: usize,
}

/// Measure both reconcile-latency scenarios against `indexed`'s real,
/// already-indexed store. Mutates real files under `indexed.root` (appends a
/// line — a genuine incremental change the warm `StatCache` must actually
/// detect, not a no-op).
pub async fn measure(indexed: &mut IndexedStore) -> Result<ReconcileLatency, String> {
    let meta = WorktreeMeta {
        worktree_id: indexed.worktree_id.to_string(),
        root: indexed.root.clone(),
        kind: WorktreeKind::Main,
        case: CaseSensitivity::Sensitive,
        prune_roots: PRUNED_DIRECTORIES.iter().map(|d| d.to_string()).collect(),
    };
    let classifier = ClassifierConfig::new(1024 * 1024);

    let files = indexed_absolute_paths(indexed).await?;
    if files.is_empty() {
        return Err("no indexed files to mutate for a reconcile-latency measurement".to_string());
    }

    let mut one_file_ms = Vec::with_capacity(TIMED_PASSES);
    for pass in 0..(WARMUP_PASSES + TIMED_PASSES) {
        touch_file(&files[pass % files.len()])?;
        let started = Instant::now();
        reconcile_once(
            &indexed.state,
            &meta,
            ScanMode::Fast,
            &mut indexed.stat_cache,
            &classifier,
            &Scanner::new(),
            indexed.uuids.as_ref(),
            indexed.now_ms,
        )
        .await
        .map_err(|e| format!("one-file reconcile: {e:?}"))?;
        let elapsed = started.elapsed().as_secs_f64() * 1000.0;
        if pass >= WARMUP_PASSES {
            one_file_ms.push(elapsed);
        }
    }

    let branch_checkout_files_changed = ((files.len() as f64 * BRANCH_CHECKOUT_FRACTION).ceil()
        as usize)
        .clamp(1, files.len())
        .clamp(
            BRANCH_CHECKOUT_MIN_FILES.min(files.len()),
            BRANCH_CHECKOUT_MAX_FILES.min(files.len()),
        );
    let mut branch_checkout_ms = Vec::with_capacity(TIMED_PASSES);
    for pass in 0..(WARMUP_PASSES + TIMED_PASSES) {
        for path in files.iter().take(branch_checkout_files_changed) {
            touch_file(path)?;
        }
        let started = Instant::now();
        reconcile_once(
            &indexed.state,
            &meta,
            ScanMode::Fast,
            &mut indexed.stat_cache,
            &classifier,
            &Scanner::new(),
            indexed.uuids.as_ref(),
            indexed.now_ms,
        )
        .await
        .map_err(|e| format!("branch-checkout reconcile: {e:?}"))?;
        let elapsed = started.elapsed().as_secs_f64() * 1000.0;
        if pass >= WARMUP_PASSES {
            branch_checkout_ms.push(elapsed);
        }
    }

    Ok(ReconcileLatency {
        one_file_p50_ms: percentile(&mut one_file_ms.clone(), 0.50),
        one_file_p95_ms: percentile(&mut one_file_ms.clone(), 0.95),
        one_file_ms,
        branch_checkout_p50_ms: percentile(&mut branch_checkout_ms.clone(), 0.50),
        branch_checkout_p95_ms: percentile(&mut branch_checkout_ms.clone(), 0.95),
        branch_checkout_ms,
        branch_checkout_files_changed,
    })
}

/// The real, absolute paths of every file the indexed generation actually
/// contains — read from `generation_file` (the DB's own record of what was
/// indexed) rather than re-walking the filesystem, so this can never drift
/// from what `reconcile_once` itself considers "already indexed".
async fn indexed_absolute_paths(indexed: &IndexedStore) -> Result<Vec<std::path::PathBuf>, String> {
    let read = indexed
        .state
        .open_read()
        .map_err(|e| format!("state open_read: {e}"))?;
    let mut stmt = read
        .prepare(
            "SELECT normalized_path FROM generation_file WHERE generation_id = ?1 \
             ORDER BY normalized_path",
        )
        .map_err(|e| format!("prepare: {e}"))?;
    let rows = stmt
        .query_map([&indexed.report.build.generation_id], |r| {
            r.get::<_, String>(0)
        })
        .map_err(|e| format!("query generation_file: {e}"))?;
    rows.map(|r| r.map(|p| indexed.root.join(p)))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("collect generation_file rows: {e}"))
}

/// Append a real, tiny, harmless line to `path` — a genuine incremental
/// change (new size, new mtime) the warm `StatCache` must actually detect,
/// not a no-op touch.
fn touch_file(path: &std::path::Path) -> Result<(), String> {
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(|e| format!("open {} for append: {e}", path.display()))?;
    file.write_all(b"\n")
        .map_err(|e| format!("append to {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small, disposable **copy** of a few real `.rs` files — never the
    /// live repository tree. [`measure`] mutates files in place
    /// (`touch_file`); pointing it at `CARGO_MANIFEST_DIR` directly once
    /// really did append newlines to this crate's own tracked source files
    /// (caught and reverted during T17-05's own development — never repeat
    /// that mistake).
    fn disposable_corpus_copy() -> local_rag_test_support::TempHome {
        let home = local_rag_test_support::TempHome::new().expect("temp home");
        let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/bench");
        let dst_dir = home.join("corpus");
        std::fs::create_dir_all(&dst_dir).expect("create corpus dir");
        for entry in std::fs::read_dir(&src_dir).expect("read bench src dir") {
            let path = entry.expect("dir entry").path();
            if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                let dst = dst_dir.join(path.file_name().expect("file name"));
                std::fs::copy(&path, &dst).expect("copy fixture file");
            }
        }
        home
    }

    /// Real, end-to-end: a real indexed+embedded throwaway store (a
    /// disposable copy of a few real `.rs` files, kept fast), real file
    /// mutations against that copy only, real `reconcile_once` timings.
    /// Skips loudly (not a failure) when no real ONNX Runtime is available —
    /// mirrors `crate::bench::run`'s own real-weights precondition.
    #[test]
    fn measures_real_reconcile_latency_against_a_real_indexed_corpus() {
        if std::env::var_os("ORT_DYLIB_PATH").is_none() {
            eprintln!(
                "SKIP: ORT_DYLIB_PATH is unset — set it to exercise this test for real \
                 (the default model, once installed under ~/.local/share/local-rag-bench, is \
                 reused automatically)."
            );
            return;
        }
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        rt.block_on(async {
            let corpus_home = disposable_corpus_copy();
            let options = crate::bench::run::Options {
                corpus_dir: corpus_home.join("corpus"),
                subdir: None,
                mode: local_rag_protocol::SearchMode::Hybrid,
                dense_kind: local_rag_store::RepresentationKind::CodeRaw,
                lexical_weights: vec![],
            };
            let mut indexed = crate::bench::run::build_indexed_store(&options)
                .await
                .expect("build a real indexed store");
            let latency = measure(&mut indexed)
                .await
                .expect("measure real reconcile latency");

            assert_eq!(latency.one_file_ms.len(), TIMED_PASSES, "{latency:?}");
            assert!(
                latency.one_file_p95_ms >= latency.one_file_p50_ms,
                "{latency:?}"
            );
            assert_eq!(
                latency.branch_checkout_ms.len(),
                TIMED_PASSES,
                "{latency:?}"
            );
            assert!(
                latency.branch_checkout_p95_ms >= latency.branch_checkout_p50_ms,
                "{latency:?}"
            );
            assert!(latency.branch_checkout_files_changed >= 1, "{latency:?}");
        });
    }
}
