//! Budget-LRU eviction of `embedding_cache` with active/rebuild pins (spec 03
//! §4.2 `[SPEC]`) — group 11, T11-02.
//!
//! Mirrors [`crate::retention`]'s mark-and-sweep split and
//! [`crate::housekeeping`]'s three-layer shape (pure predicate → thin DB reader
//! → batched, dry-run-capable entry point), applied to a byte-budget walk
//! instead of a state-predicate sweep:
//!
//! - [`rows_to_evict`] is the pure core: sort **unpinned** rows by
//!   `last_used_at` ascending, evict oldest-first until the retained set's
//!   total `byte_size` is at or under budget. Table-tested with no I/O.
//! - [`store_wide_embedding_pins`] is the cross-worktree pin reader — no
//!   existing reader answers "which subjects are protected" (confirmed:
//!   [`crate::registry::projection_state`] is strictly per-worktree), so this
//!   loops [`crate::registry::all_worktree_ids`] exactly like
//!   [`crate::retention`]'s own `store_wide_pinned` does for generations.
//! - [`run_embedding_cache_eviction`] is the batched, dry-run-capable entry
//!   point composing both.
//!
//! # Pin rule (spec 03 §4.2 `[SPEC]`: "rows pinned while referenced by an
//! active projection tuple or a running rebuild")
//!
//! A pinned `(generation_id, model_space_id)` tuple is read from **both** the
//! `active_*` and `target_*` columns of every worktree's
//! `worktree_projection_state` row (regardless of `status`):
//!
//! - `active_*` covers "active projection tuple" literally, and "a running
//!   rebuild" too — rebuild always retargets the **active** tuple, never
//!   `target` (spec 05 §7: "rebuild never changes *which* generation is
//!   active, only re-syncs the shard to match it").
//! - `target_*` is a deliberate, cheap, strictly-safer **superset** beyond the
//!   card's literal wording: an in-flight `switch()` reads `embedding_cache`
//!   for the target tuple's missing points *before* committing (spec 05 §5
//!   step 1), so evicting a target-only subject mid-switch would induce an
//!   avoidable `SwitchError::MissingVector`. Recorded explicitly as a
//!   conservative extension, the same way [`crate::retention`]'s module docs
//!   explicitly record "`failed` is never pinned by retention itself."
//!
//! # Resolving a pinned tuple to `embedding_cache` subject keys
//!
//! Only [`RepresentationKind::CodeRaw`] is resolved to a real
//! [`EmbeddingKey`] today, via [`crate::code::content_blob_ids_for_generation`]
//! and [`local_rag_core::identity::domain::subject_content_blob`].
//! `CodeContext`'s subject-hash format is `[OPEN]` (spec 09 §3: "content vs
//! context representation choice is decided by the benchmark") and `Memory`'s
//! backing table does not exist before group 14 — neither can produce a real
//! `embedding_cache` row yet, so skipping them here is a safe no-op, not a
//! misclassification (T11-02's scope-boundary decision, see the task's
//! evidence in `PROGRESS.md`).

use std::collections::BTreeSet;

use rusqlite::Connection;

use local_rag_core::config::StorageConfig;
use local_rag_core::identity::domain::subject_content_blob;

use crate::cache::{
    CacheDb, CacheOpenError, CacheWriteError, EmbeddingCacheMeta, EmbeddingKey, SubjectKind,
};
use crate::code::content_blob_ids_for_generation;
use crate::registry::{
    RepresentationKind, all_worktree_ids, model_space_required_representation_ids, projection_state,
};

/// The bounded cache-transaction batch size for a real eviction sweep (spec 03
/// §3 `[SPEC]`: "≤ 500 rows/tx"), mirroring [`crate::retention::SWEEP_BATCH_ROWS`].
pub const EVICTION_BATCH_ROWS: usize = 500;

/// The tuning parameter for budget-LRU eviction (the spec's
/// `embedding_cache_budget_mb`).
///
/// Kept separate from [`StorageConfig`] so the pure policy
/// ([`rows_to_evict`]) can be exercised with raw boundary values; build it from
/// config with [`EvictionParams::from_storage_config`], mirroring
/// [`crate::retention::RetentionParams::from_storage_config`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EvictionParams {
    /// The budget, in bytes.
    pub budget_bytes: i64,
}

impl EvictionParams {
    /// Read `embedding_cache_budget_mb` from the `[storage]` config (spec 02
    /// §3.1), MiB → bytes (saturating, so an absurd config can never overflow).
    pub fn from_storage_config(cfg: &StorageConfig) -> Self {
        let bytes = cfg.embedding_cache_budget_mb.saturating_mul(1024 * 1024);
        EvictionParams {
            budget_bytes: i64::try_from(bytes).unwrap_or(i64::MAX),
        }
    }
}

/// Compute which rows a budget-LRU eviction pass would remove (spec 03 §4.2:
/// "Eviction: LRU by `last_used_at` toward `embedding_cache_budget_mb`; rows
/// pinned ... are exempt").
///
/// Pure — no I/O. If the total `byte_size` of `rows` is already at or under
/// `budget_bytes`, evicts nothing. Otherwise walks **unpinned** rows ascending
/// by `last_used_at` (oldest first), evicting until the retained set is at or
/// under budget, or unpinned rows run out (pinned rows are exempt by
/// definition — the budget may still be exceeded afterward, which is
/// expected, not an error).
pub fn rows_to_evict(
    rows: &[EmbeddingCacheMeta],
    pinned: &BTreeSet<EmbeddingKey>,
    budget_bytes: i64,
) -> Vec<EmbeddingKey> {
    let total: i64 = rows.iter().map(|r| r.byte_size).sum();
    if total <= budget_bytes {
        return Vec::new();
    }
    let mut over = total - budget_bytes;

    let mut candidates: Vec<&EmbeddingCacheMeta> =
        rows.iter().filter(|r| !pinned.contains(&r.key)).collect();
    candidates.sort_by(|a, b| {
        a.last_used_at
            .cmp(&b.last_used_at)
            .then_with(|| a.key.cmp(&b.key))
    });

    let mut evict = Vec::new();
    for row in candidates {
        if over <= 0 {
            break;
        }
        evict.push(row.key.clone());
        over -= row.byte_size;
    }
    evict
}

/// Every `embedding_cache` subject key protected by an active or in-flight-target
/// projection tuple, unioned across all worktrees (see the module docs for the
/// pin rule and the `CodeRaw`-only resolution scope boundary).
pub fn store_wide_embedding_pins(state: &Connection) -> rusqlite::Result<BTreeSet<EmbeddingKey>> {
    let mut tuples: BTreeSet<(String, String)> = BTreeSet::new();
    for worktree_id in all_worktree_ids(state)? {
        let Some(row) = projection_state(state, &worktree_id)? else {
            continue;
        };
        for (generation_id, model_space_id) in [
            (row.active_generation_id, row.active_model_space_id),
            (row.target_generation_id, row.target_model_space_id),
        ] {
            if let (Some(generation_id), Some(model_space_id)) = (generation_id, model_space_id) {
                tuples.insert((generation_id, model_space_id));
            }
        }
    }

    let mut pinned = BTreeSet::new();
    for (generation_id, model_space_id) in tuples {
        let required = model_space_required_representation_ids(state, &model_space_id)?;
        let Some((_, representation_id)) = required
            .iter()
            .find(|(kind, _)| *kind == RepresentationKind::CodeRaw)
        else {
            // No `code_raw` representation registered/required for this model
            // space (today, before T11-03/T11-04 populate the registry) — a
            // safe no-op, not a missed pin: no such embedding_cache row could
            // exist yet either.
            continue;
        };
        for (_occurrence_id, blob_id) in content_blob_ids_for_generation(state, &generation_id)? {
            pinned.insert(EmbeddingKey {
                subject_kind: SubjectKind::ContentBlob,
                subject_hash: subject_content_blob(&blob_id),
                representation_id: representation_id.clone(),
            });
        }
    }
    Ok(pinned)
}

/// The outcome of an eviction pass — either the keys a real pass **removed**
/// or those a dry run **would** remove.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EvictionReport {
    /// Keys evicted (or, for a dry run, that would be evicted), in eviction
    /// (LRU) order.
    pub evicted: Vec<EmbeddingKey>,
    /// Rows left in the cache afterward (pinned, or the budget was already
    /// satisfied).
    pub retained: u64,
    /// Whether this was a dry run (nothing was actually deleted).
    pub dry_run: bool,
}

/// A failure from [`run_embedding_cache_eviction`].
#[derive(Debug)]
#[non_exhaustive]
pub enum EvictionError {
    /// Reading `state.sqlite` (pins) or `cache.sqlite` (rows) failed.
    Sqlite(rusqlite::Error),
    /// Opening a read connection to `cache.sqlite` failed.
    CacheOpen(CacheOpenError),
    /// Deleting evicted rows failed.
    CacheWrite(CacheWriteError),
}

impl From<rusqlite::Error> for EvictionError {
    fn from(e: rusqlite::Error) -> Self {
        EvictionError::Sqlite(e)
    }
}

impl From<CacheOpenError> for EvictionError {
    fn from(e: CacheOpenError) -> Self {
        EvictionError::CacheOpen(e)
    }
}

impl From<CacheWriteError> for EvictionError {
    fn from(e: CacheWriteError) -> Self {
        EvictionError::CacheWrite(e)
    }
}

impl std::fmt::Display for EvictionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EvictionError::Sqlite(e) => write!(f, "sqlite error during eviction: {e}"),
            EvictionError::CacheOpen(e) => write!(f, "could not open cache for eviction: {e}"),
            EvictionError::CacheWrite(e) => write!(f, "could not delete evicted rows: {e}"),
        }
    }
}

impl std::error::Error for EvictionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            EvictionError::Sqlite(e) => Some(e),
            EvictionError::CacheOpen(e) => Some(e),
            EvictionError::CacheWrite(e) => Some(e),
        }
    }
}

/// Run one budget-LRU eviction pass over `cache`'s `embedding_cache` table,
/// pinning subjects live-referenced through `state` (spec 03 §4.2).
///
/// Reads live rows and live pins, computes the evict-list via the pure
/// [`rows_to_evict`], then — unless `dry_run` — deletes them in batches of
/// [`EVICTION_BATCH_ROWS`] through [`crate::CacheWriter::transaction`],
/// mirroring [`crate::retention::run_sweep`]'s batching discipline. `dry_run`
/// is threaded through as a parameter, not a separate code path, matching
/// [`crate::housekeeping`]'s reports.
pub async fn run_embedding_cache_eviction(
    cache: &CacheDb,
    state: &Connection,
    params: &EvictionParams,
    dry_run: bool,
) -> Result<EvictionReport, EvictionError> {
    let cache_read = cache.open_read()?;
    let rows = crate::cache::all_embedding_meta(&cache_read)?;
    drop(cache_read);

    let pinned = store_wide_embedding_pins(state)?;
    let evicted = rows_to_evict(&rows, &pinned, params.budget_bytes);
    let retained = rows.len() as u64 - evicted.len() as u64;

    if !dry_run {
        for chunk in evicted.chunks(EVICTION_BATCH_ROWS) {
            let chunk = chunk.to_vec();
            cache
                .writer()
                .transaction(move |tx| {
                    for key in &chunk {
                        crate::cache::delete_embedding(tx, key)?;
                    }
                    Ok(())
                })
                .await?;
        }
    }

    Ok(EvictionReport {
        evicted,
        retained,
        dry_run,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(
        kind: SubjectKind,
        hash: &str,
        repr: &str,
        byte_size: i64,
        last_used_at: i64,
    ) -> EmbeddingCacheMeta {
        EmbeddingCacheMeta {
            key: EmbeddingKey {
                subject_kind: kind,
                subject_hash: hash.to_string(),
                representation_id: repr.to_string(),
            },
            byte_size,
            last_used_at,
        }
    }

    #[test]
    fn under_budget_evicts_nothing() {
        let rows = vec![meta(SubjectKind::ContentBlob, "a", "r1", 100, 10)];
        let evict = rows_to_evict(&rows, &BTreeSet::new(), 1000);
        assert!(evict.is_empty());
    }

    #[test]
    fn over_budget_evicts_oldest_first_until_under_budget() {
        let rows = vec![
            meta(SubjectKind::ContentBlob, "oldest", "r1", 40, 100),
            meta(SubjectKind::ContentBlob, "middle", "r1", 40, 200),
            meta(SubjectKind::ContentBlob, "newest", "r1", 40, 300),
        ];
        // total = 120, budget = 80 -> must free 40 bytes -> evict exactly "oldest".
        let evict = rows_to_evict(&rows, &BTreeSet::new(), 80);
        assert_eq!(evict, vec![rows[0].key.clone()]);
    }

    #[test]
    fn pinned_rows_are_never_evicted_even_over_budget() {
        let rows = vec![
            meta(SubjectKind::ContentBlob, "oldest-pinned", "r1", 40, 100),
            meta(SubjectKind::ContentBlob, "newer-unpinned", "r1", 40, 200),
        ];
        let mut pinned = BTreeSet::new();
        pinned.insert(rows[0].key.clone());
        // budget = 0 -> would need to evict everything, but the pinned row is
        // exempt; only the unpinned one is evicted, leaving the cache over
        // budget (expected, not an error).
        let evict = rows_to_evict(&rows, &pinned, 0);
        assert_eq!(evict, vec![rows[1].key.clone()]);
    }

    #[test]
    fn eviction_stops_when_unpinned_rows_run_out() {
        let rows = vec![meta(SubjectKind::ContentBlob, "only", "r1", 100, 100)];
        let mut pinned = BTreeSet::new();
        pinned.insert(rows[0].key.clone());
        let evict = rows_to_evict(&rows, &pinned, 0);
        assert!(evict.is_empty(), "the only row is pinned, nothing to evict");
    }
}
