//! `normalized_text_cache` repository and the `last_used_at` batching seam
//! (spec 03 §3, §4.2, §4.4; 06 §4) — T03-04.
//!
//! `normalized_text_cache` is a rebuildable, independently validated view of the
//! normalized text derived from `state.sqlite`'s `source_blob`. It is keyed by
//! `content_blob.blob_id`, which is itself `H(content_blob …)` over the normalized
//! text (spec 03 §1.2). There is deliberately **no checksum column**: a row is
//! validated by recomputing its identity from the stored text
//! ([`verify_cached_text`]) — a mismatch means the row is corrupt and must be
//! evicted ([`delete_normalized_text`]) and regenerated from `source_blob` (the
//! [`crate::code::derive_content_blob`] layer, spec 06 §4).
//!
//! Writes compose inside a [`CacheWriter::transaction`](crate::CacheWriter::transaction)
//! closure ([`insert_normalized_text`], [`delete_normalized_text`],
//! [`flush_last_used`]); reads take a read-only [`Connection`]
//! ([`get_normalized_text`]) and never swallow a transient error as "absent"
//! (the D-003 lesson): `QueryReturnedNoRows` maps to `None`, everything else —
//! including a transient `SQLITE_BUSY` — propagates so the caller can retry on a
//! fresh connection.
//!
//! ## `last_used_at` batching seam `[SPEC]`
//!
//! Spec 03 §3 requires `last_used_at` updates to be *batched* (flush ≤ every 5 s
//! or 500 rows) rather than written per read. This module ships the **seam**, not
//! the driver: [`LastUsedSink`] records "blob X was used at T" (deduping to the
//! latest timestamp), [`BatchingLastUsed`] accumulates in memory, and
//! [`flush_last_used`] applies a drained batch as one transaction. The actual
//! flush cadence (timer / row-count threshold) is wired by a later reconcile/
//! search task on top of this seam, mirroring the injected `Clock`/`UuidSource`
//! style used elsewhere.

use std::collections::HashMap;
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::code::{ALGO_VERSION, NORMALIZATION_VERSION, content_blob_id};

/// A row read from `normalized_text_cache` (spec 03 §4.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedTextRow {
    /// The normalized text (derived from `source_blob`).
    pub normalized_text: String,
    /// UTF-8 byte length of `normalized_text`.
    pub byte_size: i64,
    /// When the row was first cached (Unix ms).
    pub created_at: i64,
    /// When the row was last used (Unix ms); driven by the batching seam.
    pub last_used_at: i64,
}

/// Insert a `normalized_text_cache` row, or touch `last_used_at` if it already
/// exists (spec 03 §4.2).
///
/// The `blob_id` is content-derived, so a conflicting row holds *identical* text
/// — the conflict is a cache hit, and the only meaningful update is bumping
/// `last_used_at`. `created_at` and `last_used_at` are both set to `now_ms` on
/// first insert.
pub fn insert_normalized_text(
    tx: &Transaction<'_>,
    blob_id: &str,
    normalized_text: &str,
    byte_size: i64,
    now_ms: i64,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO normalized_text_cache \
           (blob_id, normalized_text, byte_size, created_at, last_used_at) \
         VALUES (?1, ?2, ?3, ?4, ?4) \
         ON CONFLICT(blob_id) DO UPDATE SET last_used_at = excluded.last_used_at",
        params![blob_id, normalized_text, byte_size, now_ms],
    )?;
    Ok(())
}

/// Read a `normalized_text_cache` row by `blob_id` (spec 03 §4.2).
///
/// Returns `Ok(None)` only for a genuinely absent row (`QueryReturnedNoRows`).
/// Every other error — a transient `SQLITE_BUSY`/`BUSY_SNAPSHOT` under contention,
/// a missing table after a rebuild — is **propagated**, never silently mapped to
/// "absent" (D-003). Callers retry a transient failure on a fresh read connection
/// (each [`CacheDb::open_read`](crate::CacheDb::open_read) takes a new WAL
/// snapshot, which clears `BUSY_SNAPSHOT` that `busy_timeout` cannot wait out).
pub fn get_normalized_text(
    conn: &Connection,
    blob_id: &str,
) -> rusqlite::Result<Option<NormalizedTextRow>> {
    conn.query_row(
        "SELECT normalized_text, byte_size, created_at, last_used_at \
         FROM normalized_text_cache WHERE blob_id = ?1",
        params![blob_id],
        |r| {
            Ok(NormalizedTextRow {
                normalized_text: r.get(0)?,
                byte_size: r.get(1)?,
                created_at: r.get(2)?,
                last_used_at: r.get(3)?,
            })
        },
    )
    .optional()
}

/// Delete a `normalized_text_cache` row, returning whether one was removed.
///
/// Used to evict a row whose text fails [`verify_cached_text`] (spec 03 §4.4
/// "mismatch → delete row, recompute lazily") before regenerating it.
pub fn delete_normalized_text(tx: &Transaction<'_>, blob_id: &str) -> rusqlite::Result<bool> {
    let removed = tx.execute(
        "DELETE FROM normalized_text_cache WHERE blob_id = ?1",
        params![blob_id],
    )?;
    Ok(removed > 0)
}

/// Whether `cached_text` reproduces `blob_id` — the cache's integrity check
/// (spec 03 §4.2/§4.4). Recomputes `H(content_blob …)` with the current
/// `algo_version`/`normalization_version` (every cached row was produced by this
/// binary, so those match by construction) and compares. `false` ⇒ the stored
/// text is corrupt; evict and regenerate from `source_blob`.
pub fn verify_cached_text(blob_id: &str, language: &str, cached_text: &str) -> bool {
    content_blob_id(ALGO_VERSION, language, NORMALIZATION_VERSION, cached_text) == blob_id
}

/// Records that a cached blob was used, for later batched `last_used_at` flushing
/// (spec 03 §3). The seam an eviction-aware reader writes to instead of issuing a
/// single-row `UPDATE` per read.
pub trait LastUsedSink: Send + Sync {
    /// Note that `blob_id` was used at `now_ms`.
    fn record_used(&self, blob_id: &str, now_ms: i64);
}

/// An in-memory [`LastUsedSink`] accumulator (spec 03 §3).
///
/// Deduplicates per `blob_id` to the *latest* timestamp, so a hot blob costs one
/// pending entry regardless of how often it is read. [`BatchingLastUsed::drain`]
/// hands the pending updates to [`flush_last_used`]; the flush *cadence* (≤ 5 s /
/// 500 rows) is a later task's concern — this type is only the buffer.
#[derive(Debug, Default)]
pub struct BatchingLastUsed {
    pending: Mutex<HashMap<String, i64>>,
}

impl BatchingLastUsed {
    /// A new, empty accumulator.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of distinct blobs with a pending update.
    pub fn len(&self) -> usize {
        self.pending.lock().expect("last-used lock").len()
    }

    /// Whether there are no pending updates.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Take and clear the pending updates as `(blob_id, last_used_at)` pairs.
    pub fn drain(&self) -> Vec<(String, i64)> {
        self.pending
            .lock()
            .expect("last-used lock")
            .drain()
            .collect()
    }
}

impl LastUsedSink for BatchingLastUsed {
    fn record_used(&self, blob_id: &str, now_ms: i64) {
        let mut pending = self.pending.lock().expect("last-used lock");
        pending
            .entry(blob_id.to_string())
            .and_modify(|ts| *ts = (*ts).max(now_ms))
            .or_insert(now_ms);
    }
}

/// Apply a drained batch of `last_used_at` updates in one transaction (spec 03
/// §3). Returns the number of rows actually updated (a blob evicted since it was
/// recorded simply matches no row). Missing rows are not an error.
pub fn flush_last_used(tx: &Transaction<'_>, updates: &[(String, i64)]) -> rusqlite::Result<usize> {
    let mut stmt =
        tx.prepare("UPDATE normalized_text_cache SET last_used_at = ?2 WHERE blob_id = ?1")?;
    let mut applied = 0;
    for (blob_id, last_used_at) in updates {
        applied += stmt.execute(params![blob_id, last_used_at])?;
    }
    Ok(applied)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_cached_text_matches_derivation() {
        let derived = crate::code::derive_content_blob("rust", "fn main() {}\n");
        assert!(verify_cached_text(
            &derived.blob_id,
            "rust",
            &derived.normalized_text
        ));
        // Wrong text or wrong language must not verify.
        assert!(!verify_cached_text(&derived.blob_id, "rust", "tampered"));
        assert!(!verify_cached_text(
            &derived.blob_id,
            "ruby",
            &derived.normalized_text
        ));
    }

    #[test]
    fn batching_dedups_to_latest_timestamp() {
        let sink = BatchingLastUsed::new();
        assert!(sink.is_empty());
        sink.record_used("blob-a", 100);
        sink.record_used("blob-a", 300); // later wins
        sink.record_used("blob-a", 200); // earlier ignored
        sink.record_used("blob-b", 50);
        assert_eq!(sink.len(), 2);

        let mut drained = sink.drain();
        drained.sort();
        assert_eq!(
            drained,
            vec![("blob-a".to_string(), 300), ("blob-b".to_string(), 50)]
        );
        // drain clears the buffer.
        assert!(sink.is_empty());
        assert!(sink.drain().is_empty());
    }
}
