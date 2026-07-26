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
//! - [`store_wide_embedding_pins`] is the pin reader; since T11-04 it is a thin
//!   delegation to [`crate::subjects`], which owns the one definition of
//!   "expected subjects of a model space" that both this module and the backfill
//!   worker consume.
//! - [`run_embedding_cache_eviction`] is the batched, dry-run-capable entry
//!   point composing both.
//!
//! # Pin rule (spec 03 §4.2 `[SPEC]`: "rows pinned while referenced by an
//! active projection tuple or a running rebuild")
//!
//! The pinned set is **pin-root generations × protected model spaces**
//! ([`crate::subjects::protected_subject_keys`]). Two revisions to T11-02's
//! original tuple-only rule, both widening it:
//!
//! - **Generations** come from the retention pin roots (spec 06 §5) rather than
//!   from the `active_*`/`target_*` columns alone. Those columns are a subset:
//!   an active generation is `active`, an in-flight target is `building`/
//!   `projection_ready`, and all three are unconditional pin roots — so the
//!   original guarantees still hold, and `retiring` generations inside the `K`/`T`
//!   window are now covered too, matching what T11-04's backfill is required to
//!   embed. `target_*` remains covered for the reason T11-02 recorded: an
//!   in-flight `switch()` reads `embedding_cache` for the target tuple's missing
//!   points *before* committing (spec 05 §5 step 1), so evicting a target-only
//!   subject mid-switch would induce an avoidable `SwitchError::MissingVector`.
//! - **Model spaces** additionally include every space in `building` or
//!   `projection_ready`. A space being backfilled is referenced by **no**
//!   worktree yet (it enters `worktree_projection_state` only at switch time,
//!   spec 10 §4 step 4), so under the old rule its freshly written rows were the
//!   LRU's first victims — the worker and the evictor would fight over the same
//!   rows indefinitely. This is the pin the spec's own "a running rebuild"
//!   clause implies for a running *backfill*.
//!
//! Resolution of a kind to real subject keys (today `code_raw` only —
//! `code_context`'s format is `[OPEN]`, `memory`'s tables arrive in group 14)
//! also lives in [`crate::subjects`]; unresolvable kinds are simply absent from
//! the pin set here, which is safe because no such `embedding_cache` row can
//! exist either.

use std::collections::BTreeSet;

use rusqlite::Connection;

use local_rag_core::config::StorageConfig;

#[cfg(test)]
use crate::cache::SubjectKind;
use crate::cache::{CacheDb, CacheOpenError, CacheWriteError, EmbeddingCacheMeta, EmbeddingKey};
use crate::retention::{ExternalPins, RetentionParams};

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
    /// The retention window the pin roots are computed against (T11-04): the pin
    /// set is now defined over pin-root generations (spec 06 §5), so eviction
    /// needs the same `K`/`T` the GC uses. Both come from one `[storage]`
    /// section, so [`EvictionParams::from_storage_config`] fills both.
    pub retention: RetentionParams,
}

impl EvictionParams {
    /// Read `embedding_cache_budget_mb` (and, for the pin roots, `K`/`T`) from
    /// the `[storage]` config (spec 02 §3.1), MiB → bytes (saturating, so an
    /// absurd config can never overflow).
    pub fn from_storage_config(cfg: &StorageConfig) -> Self {
        let bytes = cfg.embedding_cache_budget_mb.saturating_mul(1024 * 1024);
        EvictionParams {
            budget_bytes: i64::try_from(bytes).unwrap_or(i64::MAX),
            retention: RetentionParams::from_storage_config(cfg),
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

/// Every `embedding_cache` subject key protected from eviction, unioned across
/// the whole store.
///
/// Delegates to [`crate::subjects::protected_subject_keys`] (T11-04), which owns
/// the definition shared with the backfill worker: pin-root generations (spec 06
/// §5) × model spaces that are either referenced by a worktree's projection state
/// **or** still `building`/`projection_ready`. The second half is what T11-02's
/// tuple-only rule could not express: a space being filled by a backfill is
/// referenced by no worktree yet, so its freshly written rows would otherwise be
/// the LRU's first victims — see the module docs above.
pub fn store_wide_embedding_pins(
    state: &Connection,
    params: &EvictionParams,
    now_ms: i64,
) -> rusqlite::Result<BTreeSet<EmbeddingKey>> {
    crate::subjects::protected_subject_keys(
        state,
        &params.retention,
        &ExternalPins::default(),
        now_ms,
    )
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
    now_ms: i64,
    dry_run: bool,
) -> Result<EvictionReport, EvictionError> {
    let cache_read = cache.open_read()?;
    let rows = crate::cache::all_embedding_meta(&cache_read)?;
    drop(cache_read);

    let pinned = store_wide_embedding_pins(state, params, now_ms)?;
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
