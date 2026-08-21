//! Resource measurements for spec 14 §2's `resources` gate (T17-05): idle
//! RAM, index bytes/symbol, embedding-cache-budget adherence, and source/
//! worktree byte ratio. Recorded as this release's first-established v2
//! baseline — never gated (see `release_report` module doc).

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use local_rag_core::paths::StoreLayout;
use local_rag_test_support::TempHome;
use sysinfo::{Pid, ProcessesToUpdate, System};

/// How long to let a freshly-ready daemon settle (startup resume passes,
/// etc.) before the first RAM sample — chosen generously, not derived.
const SETTLE: Duration = Duration::from_secs(3);
/// How long to keep sampling once settled.
const SAMPLE_WINDOW: Duration = Duration::from_secs(5);
/// How often to sample within the window.
const SAMPLE_EVERY: Duration = Duration::from_millis(250);
/// How long to wait for `store.lock` to report `ready: true` before giving up.
const READY_TIMEOUT: Duration = Duration::from_secs(20);

/// Raw RSS samples (bytes), plus the derived summary — the card's own "with
/// raw artifacts" requirement, not just one collapsed number.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct IdleRamSample {
    pub samples_bytes: Vec<u64>,
    pub min_bytes: u64,
    pub max_bytes: u64,
    pub mean_bytes: u64,
    pub last_bytes: u64,
}

/// Real, measured resource numbers for spec 14 §2's `resources` gate —
/// recorded as this release's first-established v2 baseline, never gated
/// (see the `release_report` module doc: there is no prior v1/v2 measurement
/// to regress against, unlike `quality`'s MRR diff).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ResourceMetrics {
    /// `None` only if idle-RAM measurement itself failed (e.g. no sibling
    /// `local-rag` binary) — the rest of this struct does not depend on it.
    pub idle_ram: Option<IdleRamSample>,
    /// `state.sqlite` size (bytes) after a `TRUNCATE` checkpoint — "at rest",
    /// not however much WAL happened to still be unflushed at measurement
    /// time.
    pub state_db_bytes: u64,
    /// `cache.sqlite` size (bytes), same checkpoint discipline.
    pub cache_db_bytes: u64,
    /// Real on-disk size (bytes) of the active dense shard directory.
    pub shard_dir_bytes: u64,
    /// The indexed corpus's real occurrence count (`ReconcileReport::build::occurrences`).
    pub occurrences: usize,
    /// `(state_db_bytes + cache_db_bytes + shard_dir_bytes) / occurrences`.
    pub bytes_per_symbol: f64,
    /// `SUM(embedding_cache.byte_size)` — the real quantity
    /// `embedding_cache_budget_mb` bounds (`crate::store::eviction`), not the
    /// raw `cache.sqlite` file size (which also carries FTS/SQLite page
    /// overhead the budget knob does not govern).
    pub embedding_cache_total_bytes: u64,
    /// `storage.embedding_cache_budget_mb` (this run's default: no
    /// `config.toml` is loaded for a throwaway benchmark store) converted to
    /// bytes.
    pub embedding_cache_budget_bytes: u64,
    /// `embedding_cache_total_bytes / embedding_cache_budget_bytes` — under
    /// 1.0 means the run stayed within budget.
    pub cache_budget_ratio: f64,
    /// Real `SUM(file_revision.source_size)` across the indexed generation's
    /// files.
    pub source_bytes: u64,
    /// Real on-disk byte size of the indexed worktree root (same directory
    /// exclusions as the corpus indexing itself, `bench::run::PRUNED_DIRECTORIES`).
    pub worktree_bytes: u64,
    /// `source_bytes / worktree_bytes`.
    pub source_worktree_ratio: f64,
}

/// Measure every `resources` gate number except idle RAM against an already
/// fully indexed+embedded+switched throwaway store
/// (`crate::bench::run::build_indexed_store`) — reusing the real corpus run
/// rather than building a second indexing harness (T17-05).
pub async fn measure(
    indexed: &crate::bench::run::IndexedStore,
    idle_ram: Option<IdleRamSample>,
) -> Result<ResourceMetrics, String> {
    // "At rest" sizes: checkpoint WAL -> main before stat'ing either file, so
    // the number does not depend on how much was still unflushed.
    indexed
        .state
        .writer()
        .checkpoint(local_rag_store::CheckpointMode::Truncate)
        .await
        .map_err(|e| format!("state checkpoint: {e}"))?;
    indexed
        .cache
        .writer()
        .checkpoint(local_rag_store::CheckpointMode::Truncate)
        .await
        .map_err(|e| format!("cache checkpoint: {e}"))?;

    let state_db_bytes = std::fs::metadata(indexed.layout.state_db())
        .map_err(|e| format!("stat state.sqlite: {e}"))?
        .len();
    let cache_db_bytes = std::fs::metadata(indexed.layout.cache_db())
        .map_err(|e| format!("stat cache.sqlite: {e}"))?
        .len();

    let shard_dir = local_rag_projection::shard_dir(
        &indexed.layout,
        &indexed.worktree_id,
        &indexed.model_space,
    );
    let shard_dir_bytes = dir_size_bytes(&shard_dir, &[])?;

    let occurrences = indexed.report.expect_built().occurrences;
    let bytes_per_symbol = if occurrences == 0 {
        0.0
    } else {
        (state_db_bytes + cache_db_bytes + shard_dir_bytes) as f64 / occurrences as f64
    };

    let embedding_cache_total_bytes: u64 = {
        let read = indexed
            .cache
            .open_read()
            .map_err(|e| format!("cache open_read: {e}"))?;
        local_rag_store::all_embedding_meta(&read)
            .map_err(|e| format!("all_embedding_meta: {e}"))?
            .iter()
            .map(|m| m.byte_size.max(0) as u64)
            .sum()
    };
    // No `config.toml` is loaded for this throwaway benchmark store — the
    // default is the real value production ships until an operator
    // overrides it.
    let embedding_cache_budget_bytes =
        local_rag_core::config::StorageConfig::default().embedding_cache_budget_mb * 1024 * 1024;
    let cache_budget_ratio = if embedding_cache_budget_bytes == 0 {
        0.0
    } else {
        embedding_cache_total_bytes as f64 / embedding_cache_budget_bytes as f64
    };

    let source_bytes: u64 = {
        let read = indexed
            .state
            .open_read()
            .map_err(|e| format!("state open_read: {e}"))?;
        let total: i64 = read
            .query_row(
                "SELECT COALESCE(SUM(fr.source_size), 0) FROM generation_file gf \
                 JOIN file_revision fr ON fr.file_revision_id = gf.file_revision_id \
                 WHERE gf.generation_id = ?1",
                [&indexed.report.expect_built().generation_id],
                |r| r.get(0),
            )
            .map_err(|e| format!("source_bytes query: {e}"))?;
        total.max(0) as u64
    };
    let worktree_bytes = dir_size_bytes(&indexed.root, crate::bench::run::PRUNED_DIRECTORIES)?;
    let source_worktree_ratio = if worktree_bytes == 0 {
        0.0
    } else {
        source_bytes as f64 / worktree_bytes as f64
    };

    Ok(ResourceMetrics {
        idle_ram,
        state_db_bytes,
        cache_db_bytes,
        shard_dir_bytes,
        occurrences,
        bytes_per_symbol,
        embedding_cache_total_bytes,
        embedding_cache_budget_bytes,
        cache_budget_ratio,
        source_bytes,
        worktree_bytes,
        source_worktree_ratio,
    })
}

/// Recursive real on-disk byte size of `dir`, skipping any directory whose
/// file name is in `pruned` (mirrors `bench::run::PRUNED_DIRECTORIES`'s own
/// exclusion so `worktree_bytes` measures the same tree that was actually
/// indexed). A missing `dir` (e.g. a not-yet-created shard directory) is
/// `0`, not an error.
fn dir_size_bytes(dir: &Path, pruned: &[&str]) -> Result<u64, String> {
    let mut total = 0u64;
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(format!("read_dir {}: {e}", dir.display())),
    };
    for entry in entries {
        let entry = entry.map_err(|e| format!("dir entry under {}: {e}", dir.display()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|e| format!("file type of {}: {e}", path.display()))?;
        if file_type.is_dir() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str())
                && pruned.contains(&name)
            {
                continue;
            }
            total += dir_size_bytes(&path, pruned)?;
        } else if file_type.is_file() {
            total += entry
                .metadata()
                .map_err(|e| format!("stat {}: {e}", path.display()))?
                .len();
        }
        // Symlinks are neither: skipped, matching the indexer's own
        // no-follow-symlink discipline.
    }
    Ok(total)
}

/// Spawn `local_rag_bin serve` against a fresh, empty store, wait for
/// readiness, let it settle, sample real RSS via `sysinfo` (not a `ps`
/// shell-out — see `Cargo.toml`'s own dependency-approval comment for why:
/// this reads correctly on every real v0 platform target, including
/// `win32-x64`), then stop it.
///
/// `local_rag_bin` must already be built (e.g. `cargo build -p local-rag`) —
/// `xtask` does not build it as a side effect, the same "bring your own
/// corpus/weights" precondition `cargo xtask bench` already has.
pub fn measure_idle_ram(local_rag_bin: &Path) -> Result<IdleRamSample, String> {
    if !local_rag_bin.is_file() {
        return Err(format!(
            "{} does not exist — build it first (e.g. `cargo build -p local-rag`)",
            local_rag_bin.display()
        ));
    }

    let home = TempHome::new().map_err(|e| format!("temp home: {e}"))?;
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().map_err(|e| format!("store layout: {e}"))?;

    let mut child = home
        .command(local_rag_bin)
        .arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn {}: {e}", local_rag_bin.display()))?;

    let result = sample_while_running(&layout, child.id());

    // Best-effort teardown regardless of how sampling went — a dev-tool
    // measurement, not a test that needs a graceful-exit assertion.
    let _ = child.kill();
    let _ = child.wait();

    result
}

fn sample_while_running(layout: &StoreLayout, pid: u32) -> Result<IdleRamSample, String> {
    wait_until_ready(layout, READY_TIMEOUT)?;
    std::thread::sleep(SETTLE);

    let pid = Pid::from_u32(pid);
    let mut system = System::new();
    let mut samples_bytes = Vec::new();
    let deadline = Instant::now() + SAMPLE_WINDOW;
    loop {
        system.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
        let bytes = system
            .process(pid)
            .ok_or_else(|| "the daemon process disappeared mid-sample".to_string())?
            .memory();
        samples_bytes.push(bytes);
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(SAMPLE_EVERY);
    }

    let min_bytes = samples_bytes.iter().copied().min().unwrap_or(0);
    let max_bytes = samples_bytes.iter().copied().max().unwrap_or(0);
    let mean_bytes = if samples_bytes.is_empty() {
        0
    } else {
        samples_bytes.iter().sum::<u64>() / samples_bytes.len() as u64
    };
    let last_bytes = samples_bytes.last().copied().unwrap_or(0);

    Ok(IdleRamSample {
        samples_bytes,
        min_bytes,
        max_bytes,
        mean_bytes,
        last_bytes,
    })
}

/// Poll `store.lock` until it parses with `ready: true`, or error after
/// `timeout` — mirrors `crates/local-rag/tests/serve_subprocess.rs`'s own
/// `wait_until_ready` idiom (a dev-tool cousin, not a test, so this returns
/// `Result` instead of panicking).
fn wait_until_ready(layout: &StoreLayout, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(bytes) = std::fs::read(layout.store_lock())
            && let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes)
            && json.get("ready").and_then(|v| v.as_bool()) == Some(true)
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "store.lock did not become ready within {timeout:?}"
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A nonexistent directory is `0` bytes, not an error — a not-yet-created
    /// shard directory is the normal case for a fresh worktree.
    #[test]
    fn a_missing_directory_is_zero_not_an_error() {
        assert_eq!(
            dir_size_bytes(Path::new("/does/not/exist/anywhere"), &[]).unwrap(),
            0
        );
    }

    #[test]
    fn sums_real_file_sizes_and_skips_pruned_subdirectories() {
        let dir = local_rag_test_support::TempHome::new().expect("temp dir");
        std::fs::write(dir.join("a.txt"), vec![0u8; 10]).unwrap();
        std::fs::create_dir_all(dir.join("keep")).unwrap();
        std::fs::write(dir.join("keep/b.txt"), vec![0u8; 20]).unwrap();
        std::fs::create_dir_all(dir.join("node_modules")).unwrap();
        std::fs::write(dir.join("node_modules/junk.txt"), vec![0u8; 1_000]).unwrap();

        let total = dir_size_bytes(dir.path(), &["node_modules"]).unwrap();
        assert_eq!(total, 30, "node_modules must be excluded from the total");
    }

    /// A small, disposable **copy** of a few real `.rs` files — never the
    /// live repository tree directly (see `release_report::latency`'s own
    /// identical helper for why: a sibling measurement mutates files in
    /// place, and this one shares the same `build_indexed_store` call, so it
    /// keeps the same discipline even though `measure` itself is read-only).
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
    /// disposable copy of a few real `.rs` files, kept fast), real
    /// `state.sqlite`/`cache.sqlite`/shard sizes, real `embedding_cache`
    /// accounting. Skips loudly (not a failure) when no real ONNX Runtime is
    /// available — mirrors `crate::bench::run`'s own real-weights
    /// precondition.
    #[test]
    fn measures_real_resource_numbers_against_a_real_indexed_corpus() {
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
            let indexed = crate::bench::run::build_indexed_store(&options)
                .await
                .expect("build a real indexed store");
            let metrics = measure(&indexed, None)
                .await
                .expect("measure real resource numbers");

            assert!(metrics.occurrences > 0, "{metrics:?}");
            assert!(metrics.state_db_bytes > 0, "{metrics:?}");
            assert!(metrics.cache_db_bytes > 0, "{metrics:?}");
            assert!(metrics.bytes_per_symbol > 0.0, "{metrics:?}");
            assert!(metrics.embedding_cache_total_bytes > 0, "{metrics:?}");
            assert!(metrics.embedding_cache_budget_bytes > 0, "{metrics:?}");
            assert!(metrics.source_bytes > 0, "{metrics:?}");
            assert!(metrics.worktree_bytes > 0, "{metrics:?}");
            assert!(metrics.source_worktree_ratio > 0.0, "{metrics:?}");
            assert!(
                metrics.idle_ram.is_none(),
                "no idle-RAM sample was passed in"
            );
        });
    }

    /// The dev-tool precondition is checked before anything is spawned — a
    /// missing binary must be a clear, immediate error, not a hang waiting
    /// for `store.lock` that will never appear.
    #[test]
    fn a_missing_binary_is_reported_before_anything_is_spawned() {
        let err = measure_idle_ram(Path::new("/does/not/exist/local-rag")).unwrap_err();
        assert!(err.contains("does not exist"), "{err}");
        assert!(err.contains("cargo build"), "{err}");
    }

    /// A sibling `local-rag` binary next to this test binary (`target/
    /// <profile>/deps/xtask-<hash>` → one directory up), if one has already
    /// been built (e.g. `cargo build -p local-rag`) — mirrors
    /// `local-rag-hook/tests/recall_rpc.rs::local_rag_binary_path`'s own
    /// trick, adapted to skip loudly rather than fail when absent, the same
    /// convention every `with_real_model` test in this workspace already
    /// uses for an optional real-binary/real-weights precondition.
    fn sibling_local_rag_binary() -> Option<std::path::PathBuf> {
        let exe = std::env::current_exe().ok()?;
        let deps_dir = exe.parent()?;
        let profile_dir = deps_dir.parent()?;
        let candidate = profile_dir.join("local-rag");
        candidate.is_file().then_some(candidate)
    }

    /// Real, end-to-end: a genuine `local-rag serve` process, genuine
    /// `sysinfo` RSS samples. Skips loudly (not a failure) when no sibling
    /// `local-rag` binary has been built yet.
    #[test]
    fn measures_real_rss_of_a_real_daemon_process() {
        let Some(bin) = sibling_local_rag_binary() else {
            eprintln!(
                "SKIP: no sibling local-rag binary found — run `cargo build -p local-rag` first \
                 to exercise this test for real."
            );
            return;
        };
        let sample = measure_idle_ram(&bin).expect("measure idle ram of a real daemon");
        assert!(
            !sample.samples_bytes.is_empty(),
            "at least one RSS sample must be taken"
        );
        assert!(sample.min_bytes > 0, "a real process reports nonzero RSS");
        assert!(sample.max_bytes >= sample.min_bytes);
        assert!(sample.mean_bytes >= sample.min_bytes && sample.mean_bytes <= sample.max_bytes);
    }
}
