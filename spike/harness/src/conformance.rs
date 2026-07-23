//! The shared `ProjectionStore` conformance suite (T10-01, spec 05 §1).
//!
//! Every spike candidate — the fake now, brute-force/usearch/Qdrant at
//! T10-02/03/04 — is run through the *same* [`run_conformance`], so the choice
//! at T10-05 rests on identical correctness checks, not per-adapter ad-hoc tests.
//! The card's four required areas map to the cases below:
//!
//! - **reopen**: build a shard, drop the handle, reopen — points and head survive;
//! - **head**: an unwritten head reads back as a clean `None` (not an error), and a
//!   written head reads back with the exact tuple/op it was built with;
//! - **manifest**: the head's `manifest_hash` equals the value recomputed from the
//!   shard's *actual* `point_ids()` (never a stored expectation), and a set that
//!   differs by a single id — with the **same count** — hashes differently
//!   (the D-006 property, the whole point of a manifest over a bare count);
//! - **corruption**: an out-of-band mutation of the shard's on-disk bytes is
//!   *detected* on the next open (open error, or a head/manifest/count mismatch) —
//!   never served as a silently-wrong result.
//!
//! The corruption case is **backend-agnostic**: it truncates the largest regular
//! file in the shard directory rather than using any fake-specific
//! `Corruption` control (which lives behind `crates/projection`'s `failpoints`
//! feature). "Largest file" = the data file for any file-based backend.
//!
//! Manifest recomputation deliberately hashes only the point-id *set* (spec 05
//! §4): the head does not commit to vector contents, so corrupting a vector's
//! bytes is by-design invisible here. The corruption case therefore changes the
//! id set (by truncation), which the contract *does* guarantee to catch.

use std::fs;
use std::io;
use std::path::Path;

use local_rag_projection::{
    DenseQuery, Hash32, PointId, ProjectionStore, ShardParams, head, manifest_hash,
};

use crate::corpus::{SeededDataset, point_ids};
use crate::report::{ConformanceCase, ConformanceReport};

/// A fixed synthetic projection-op id for the built head (deterministic UUID).
fn op_id() -> local_rag_core::identity::Uuid {
    local_rag_core::identity::uuidv7_from(1000, [0x40; 10])
}

/// Run the shared conformance suite for `store` against `dataset`, using `base`
/// as a scratch directory (each case gets its own sub-directory). Returns a
/// structured [`ConformanceReport`]; never panics on a case failure — a failure
/// is recorded as `passed: false`, so one broken backend cannot abort the spike.
pub fn run_conformance(
    store: &dyn ProjectionStore,
    dataset: &SeededDataset,
    base: &Path,
) -> io::Result<ConformanceReport> {
    let params = ShardParams {
        dimensions: dataset.dims,
    };
    let mut cases = Vec::new();

    cases.push(case("reopen_preserves_points_and_head", || {
        reopen_case(store, dataset, params, &base.join("reopen"))
    }));
    cases.push(case("empty_shard_head_is_none_not_error", || {
        empty_head_case(store, params, &base.join("empty"))
    }));
    cases.push(case("head_manifest_matches_recomputed", || {
        manifest_matches_case(store, dataset, params, &base.join("manifest"))
    }));
    cases.push(case("manifest_detects_equal_count_different_set", || {
        manifest_set_sensitivity_case(dataset)
    }));
    cases.push(case("on_disk_corruption_is_detected", || {
        corruption_case(store, dataset, params, &base.join("corrupt"))
    }));

    Ok(ConformanceReport::new(cases))
}

/// A one-line summary of what validate-on-open did with corruption, for the
/// report's `durability` metric (spec 14 §7). Reads the corruption case result.
pub fn durability_summary(report: &ConformanceReport) -> String {
    match report
        .cases
        .iter()
        .find(|c| c.name == "on_disk_corruption_is_detected")
    {
        Some(c) if c.passed => "validate-on-open: on-disk corruption detected".to_string(),
        Some(c) => format!("validate-on-open: NOT detected — {}", c.detail),
        None => "validate-on-open: not exercised".to_string(),
    }
}

/// Run one case closure, turning a `Result<String, String>` into a
/// [`ConformanceCase`]: `Ok(detail)` passes, `Err(detail)` fails.
fn case(name: &str, body: impl FnOnce() -> Result<String, String>) -> ConformanceCase {
    match body() {
        Ok(detail) => ConformanceCase {
            name: name.to_string(),
            passed: true,
            detail,
        },
        Err(detail) => ConformanceCase {
            name: name.to_string(),
            passed: false,
            detail,
        },
    }
}

/// Build a shard at `dir`: upsert every point, then write the head last (spec 05
/// §1/§5). Returns the built head's manifest hash for later comparison.
fn build(
    store: &dyn ProjectionStore,
    dataset: &SeededDataset,
    params: ShardParams,
    dir: &Path,
) -> Result<Hash32, String> {
    let shard = store.open(dir, params).map_err(|e| format!("open: {e}"))?;
    shard
        .upsert(&dataset.points)
        .map_err(|e| format!("upsert: {e}"))?;
    let ids = point_ids(dataset);
    let built = head(
        dataset.worktree_id,
        dataset.generation_id,
        dataset.model_space_id,
        op_id(),
        &ids,
    );
    shard
        .write_head(&built)
        .map_err(|e| format!("write_head: {e}"))?;
    Ok(built.manifest_hash)
}

fn reopen_case(
    store: &dyn ProjectionStore,
    dataset: &SeededDataset,
    params: ShardParams,
    dir: &Path,
) -> Result<String, String> {
    let manifest = build(store, dataset, params, dir)?;

    // Reopen from scratch — the whole point is that on-disk state survives.
    let shard = store
        .open(dir, params)
        .map_err(|e| format!("reopen: {e}"))?;
    let count = shard.point_count().map_err(|e| format!("count: {e}"))?;
    if count as usize != dataset.points.len() {
        return Err(format!(
            "point_count {count} != {} after reopen",
            dataset.points.len()
        ));
    }
    let head = shard
        .read_head()
        .map_err(|e| format!("read_head: {e}"))?
        .ok_or("head is None after reopen")?;
    if head.manifest_hash != manifest {
        return Err("reopened head manifest differs from the built one".to_string());
    }
    Ok(format!("{count} points and head survived a reopen"))
}

fn empty_head_case(
    store: &dyn ProjectionStore,
    params: ShardParams,
    dir: &Path,
) -> Result<String, String> {
    let shard = store.open(dir, params).map_err(|e| format!("open: {e}"))?;
    match shard.read_head() {
        Ok(None) => Ok("a shard with no head reads back as a clean None".to_string()),
        Ok(Some(_)) => Err("a never-written head must be None, got Some".to_string()),
        Err(e) => Err(format!("read_head on an empty shard errored: {e}")),
    }
}

fn manifest_matches_case(
    store: &dyn ProjectionStore,
    dataset: &SeededDataset,
    params: ShardParams,
    dir: &Path,
) -> Result<String, String> {
    build(store, dataset, params, dir)?;
    let shard = store
        .open(dir, params)
        .map_err(|e| format!("reopen: {e}"))?;
    let head = shard
        .read_head()
        .map_err(|e| format!("read_head: {e}"))?
        .ok_or("head is None")?;

    let recomputed = recompute_manifest(&*shard, dataset)?;
    if recomputed != head.manifest_hash {
        return Err("recomputed manifest != head.manifest_hash".to_string());
    }
    Ok("head manifest matches the value recomputed from actual point_ids".to_string())
}

/// Pure check (no disk): a point-id set that differs from the real set by exactly
/// one id — with the *same cardinality* — must hash differently. This is the
/// property a bare count comparison would miss (D-006); the manifest is the only
/// thing standing between "equal count, different content" and a silently wrong
/// index.
fn manifest_set_sensitivity_case(dataset: &SeededDataset) -> Result<String, String> {
    let mut ids = point_ids(dataset);
    if ids.len() < 2 {
        return Err("dataset too small to swap an id".to_string());
    }
    let truth = manifest_hash(
        &dataset.worktree_id,
        &dataset.generation_id,
        &dataset.model_space_id,
        &ids,
    );

    // Replace the last id with one not in the set — same count, different set.
    ids.pop();
    ids.push(PointId::from_hex(
        "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
    ));
    let swapped = manifest_hash(
        &dataset.worktree_id,
        &dataset.generation_id,
        &dataset.model_space_id,
        &ids,
    );

    if swapped == truth {
        return Err("equal count with a different id set hashed identically".to_string());
    }
    Ok("a same-count, different-set manifest hashes differently".to_string())
}

fn corruption_case(
    store: &dyn ProjectionStore,
    dataset: &SeededDataset,
    params: ShardParams,
    dir: &Path,
) -> Result<String, String> {
    let built_manifest = build(store, dataset, params, dir)?;
    corrupt_largest_regular_file(dir).map_err(|e| format!("corrupt: {e}"))?;

    // Reopen and decide whether the corruption is detectable.
    match store.open(dir, params) {
        // Unopenable is a legitimate detection (F12-class).
        Err(e) => Ok(format!("reopen after corruption errored (detected): {e}")),
        Ok(shard) => {
            let head = shard.read_head().map_err(|e| format!("read_head: {e}"))?;
            let Some(head) = head else {
                return Ok("head lost after corruption (detected)".to_string());
            };
            // Head still present: the manifest recomputed from the (now corrupt)
            // on-disk set must diverge from the committed one.
            let recomputed = recompute_manifest(&*shard, dataset)?;
            if recomputed == head.manifest_hash && head.manifest_hash == built_manifest {
                return Err(
                    "corruption left the manifest matching — a silently wrong index".to_string(),
                );
            }
            Ok(
                "recomputed manifest diverged from the head after corruption (detected)"
                    .to_string(),
            )
        }
    }
}

/// Recompute the manifest hash from a shard's *actual* points (spec 05 §4) — the
/// cache-actual source, never a stored expectation (the D-006 rule). Uses the
/// dataset only for the identifying tuple, which the head also binds.
fn recompute_manifest(
    shard: &dyn local_rag_projection::ShardHandle,
    dataset: &SeededDataset,
) -> Result<Hash32, String> {
    let ids: Vec<PointId> = shard
        .point_ids()
        .map_err(|e| format!("point_ids: {e}"))?
        .collect();
    Ok(manifest_hash(
        &dataset.worktree_id,
        &dataset.generation_id,
        &dataset.model_space_id,
        &ids,
    ))
}

/// Truncate the largest regular file in `dir` to half its length — a
/// backend-agnostic corruption of the shard's data. For a file-based backend the
/// largest file is the point data, so this either drops points (a count/manifest
/// mismatch) or breaks a record (an unopenable shard); both are detectable.
fn corrupt_largest_regular_file(dir: &Path) -> io::Result<()> {
    let mut largest: Option<(std::path::PathBuf, u64)> = None;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        if !meta.is_file() {
            continue;
        }
        let len = meta.len();
        if largest.as_ref().is_none_or(|(_, best)| len > *best) {
            largest = Some((entry.path(), len));
        }
    }
    let (path, len) = largest.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "shard directory has no regular file",
        )
    })?;
    let file = fs::OpenOptions::new().write(true).open(&path)?;
    // Halve it — for any non-trivial data file this removes real content.
    file.set_len(len / 2)?;
    Ok(())
}

/// A dense query built from a dataset's first query vector (used by the metric
/// probe, re-exported so the bin does not duplicate the shape).
pub fn first_query(dataset: &SeededDataset) -> Option<DenseQuery> {
    dataset.queries.first().cloned()
}
