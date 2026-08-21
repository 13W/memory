//! `embedding_cache` repository, integrity validation, and the batched
//! `last_used_at` seam (spec 03 §1.2, §4.2, §4.4; 10 §2) — group 11, T11-02.
//!
//! `embedding_cache` is a rebuildable, independently validated view keyed by
//! `(subject_kind, subject_hash, representation_id)` (spec 03 §4.2). Unlike
//! [`super::text`]'s `normalized_text_cache` — whose primary key **is**
//! content-derived, so a corrupt row is caught by re-deriving it — this table's
//! key is derived from the *subject*, never from the cached vector bytes. A
//! bit-flip in `vector_f32` would leave the key intact and undetectable by
//! re-derivation, which is exactly why spec 03 §4.2 gives this table its own
//! stored `checksum` column: [`verify_cached_embedding`] recomputes it and
//! compares, mirroring [`super::text::verify_cached_text`]'s role but with a
//! real stored digest instead of an identity re-derivation.
//!
//! ## Subject hashing (spec 03 §1.2)
//!
//! [`SubjectKind`] mirrors the `subject_kind` CHECK values — a *different*
//! taxonomy from [`crate::registry::RepresentationKind`], which classifies the
//! representation, not the subject's storage shape (`code_raw`/`code_context`
//! both resolve to a `content_blob`/`occurrence_context` subject respectively;
//! `structural_description` would too, post-v0). The actual per-kind hash
//! constructors ([`local_rag_core::identity::domain::subject_content_blob`] and
//! friends) live in `local-rag-core`, since they are pure identity primitives
//! with no database dependency.
//!
//! ## Checksum is a plain integrity digest, not a spec 03 §1.2 identity hash
//!
//! `checksum` ("H over vector bytes", spec 03 §4.2) is computed via
//! [`local_rag_core::hash::sha256_hex`] — the same "stable namespacing /
//! drift-detection digest" family `Migration::checksum` already uses, **not** a
//! domain-separated BLAKE3 hash. Spec 03 §1.2's domain table is for *identity*
//! hashes (subject_hash, manifest hashes) that things are looked up or deduped
//! by; the vector-bytes checksum is pure corruption detection with no identity
//! role — `hash.rs`'s own module doc draws exactly this line. Recorded as an
//! as-built decision (spec 03 §4.2), not left ambiguous.
//!
//! ## Little-endian f32 vectors
//!
//! [`encode_vector_le`]/[`decode_vector_le`] are the wire format `vector_f32`
//! stores. `decode_vector_le` rejects a byte length not a multiple of 4 (the
//! "dimensions validation" half of the card); [`verify_cached_embedding`]
//! additionally checks the stored `dimensions` against the decoded vector's
//! length before checking the checksum (cheap before expensive, mirroring
//! [`super::validate::validate_fts_cheap`]/`validate_fts_strong`'s ordering).
//!
//! ## `last_used_at` batching seam
//!
//! [`BatchingLastUsedEmbeddings`]/[`flush_last_used_embeddings`] are the
//! composite-keyed analogue of [`super::text::BatchingLastUsed`]/
//! [`super::text::flush_last_used`] — a second, purpose-specific type rather
//! than a generic retrofit, since `embedding_cache`'s primary key is a
//! three-part [`EmbeddingKey`], not `normalized_text_cache`'s single `blob_id`.
//! Ships the seam only, not the flush cadence, exactly like its sibling.
//!
//! ## Eviction is a separate module
//!
//! Budget-LRU eviction with active/rebuild pins ([`crate::eviction`]) reads the
//! meta projection this module exposes ([`EmbeddingCacheMeta`],
//! [`all_embedding_meta`]) but lives in its own top-level module — it also
//! reads `state.sqlite`'s registry, which this module (a `cache.sqlite`-only
//! concern) never touches.

use std::collections::HashMap;
use std::sync::Mutex;

use rusqlite::{Connection, Error, OptionalExtension, Transaction, params, types::Type};

/// The `embedding_cache.subject_kind` CHECK values (spec 03 §4.2). A different
/// taxonomy from [`crate::registry::RepresentationKind`] — see the module doc.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SubjectKind {
    /// A content-shared blob (`content_blob.blob_id`) — backs `code_raw`.
    ContentBlob,
    /// A per-occurrence context serialization — backs `code_context`.
    OccurrenceContext,
    /// A durable memory entry — backs `memory`.
    MemoryEntry,
}

impl SubjectKind {
    /// The stored `embedding_cache.subject_kind` value.
    pub fn as_str(self) -> &'static str {
        match self {
            SubjectKind::ContentBlob => "content_blob",
            SubjectKind::OccurrenceContext => "occurrence_context",
            SubjectKind::MemoryEntry => "memory_entry",
        }
    }

    /// Parse a stored value; `None` for anything the CHECK forbids.
    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "content_blob" => Some(SubjectKind::ContentBlob),
            "occurrence_context" => Some(SubjectKind::OccurrenceContext),
            "memory_entry" => Some(SubjectKind::MemoryEntry),
            _ => None,
        }
    }
}

/// The composite primary key of an `embedding_cache` row (spec 03 §4.2:
/// `PRIMARY KEY (subject_kind, subject_hash, representation_id)`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EmbeddingKey {
    /// The subject's storage-shape kind.
    pub subject_kind: SubjectKind,
    /// The domain-separated subject hash (spec 03 §1.2).
    pub subject_hash: String,
    /// The representation this cached vector was embedded under (logical FK →
    /// `state.representation`).
    pub representation_id: String,
}

/// Little-endian f32 byte encoding of `vector` (spec 03 §4.2 "little-endian
/// f32").
pub fn encode_vector_le(vector: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(vector.len() * 4);
    for v in vector {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

/// `bytes` is not a whole number of little-endian f32 values (its length is not
/// a multiple of 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VectorLengthError {
    /// The offending byte length.
    pub byte_len: usize,
}

impl std::fmt::Display for VectorLengthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "vector byte length {} is not a multiple of 4 (little-endian f32)",
            self.byte_len
        )
    }
}

impl std::error::Error for VectorLengthError {}

/// Decode little-endian f32 `bytes` back to a vector. `Err` if `bytes.len()` is
/// not a multiple of 4 — the "dimensions validation" half of the card.
pub fn decode_vector_le(bytes: &[u8]) -> Result<Vec<f32>, VectorLengthError> {
    if !bytes.len().is_multiple_of(4) {
        return Err(VectorLengthError {
            byte_len: bytes.len(),
        });
    }
    Ok(bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect())
}

/// A full `embedding_cache` row (spec 03 §4.2), keyed by an [`EmbeddingKey`]
/// supplied separately by the caller (not stored inline — see [`get_embedding`]).
#[derive(Debug, Clone, PartialEq)]
pub struct EmbeddingCacheRow {
    /// The declared vector dimensionality.
    pub dimensions: i64,
    /// The raw little-endian f32 bytes, as stored (not yet decoded — see
    /// [`decode_vector_le`]).
    pub vector_f32: Vec<u8>,
    /// `vector_f32.len()`, stored redundantly for cheap validation.
    pub byte_size: i64,
    /// `H` over `vector_f32` (`local_rag_core::hash::sha256_hex`) — the
    /// integrity checksum, not a spec 03 §1.2 identity hash.
    pub checksum: String,
    /// When the row was first cached (Unix ms).
    pub created_at: i64,
    /// When the row was last used (Unix ms); driven by the batching seam.
    pub last_used_at: i64,
}

/// The lightweight projection [`crate::eviction`]'s budget-LRU scan needs — no
/// vector bytes, so scanning the whole table never materializes gigabytes of
/// BLOBs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingCacheMeta {
    /// The row's primary key.
    pub key: EmbeddingKey,
    /// `vector_f32.len()` (bytes).
    pub byte_size: i64,
    /// When the row was last used (Unix ms).
    pub last_used_at: i64,
}

/// Insert an `embedding_cache` row, or touch `last_used_at` if the exact same
/// key already exists (spec 03 §4.2).
///
/// A conflicting key means the same subject was already embedded under the
/// same representation — the cached vector is necessarily identical (the
/// representation's model/version/dimensions/metric are fixed by
/// `representation_id`), so the conflict is a cache hit, and the only
/// meaningful update is bumping `last_used_at` — mirrors
/// [`super::text::insert_normalized_text`]'s identical reasoning.
pub fn insert_embedding(
    tx: &Transaction<'_>,
    key: &EmbeddingKey,
    dimensions: i64,
    vector: &[f32],
    now_ms: i64,
) -> rusqlite::Result<()> {
    let bytes = encode_vector_le(vector);
    let byte_size = i64::try_from(bytes.len()).unwrap_or(i64::MAX);
    let checksum = local_rag_core::hash::sha256_hex(&bytes);
    tx.execute(
        "INSERT INTO embedding_cache \
           (subject_kind, subject_hash, representation_id, dimensions, vector_f32, \
            byte_size, checksum, created_at, last_used_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8) \
         ON CONFLICT(subject_kind, subject_hash, representation_id) \
         DO UPDATE SET last_used_at = excluded.last_used_at",
        params![
            key.subject_kind.as_str(),
            key.subject_hash,
            key.representation_id,
            dimensions,
            bytes,
            byte_size,
            checksum,
            now_ms,
        ],
    )?;
    Ok(())
}

/// Read an `embedding_cache` row by its full key (spec 03 §4.2).
///
/// Returns `Ok(None)` only for a genuinely absent row (`QueryReturnedNoRows`).
/// Every other error — a transient `SQLITE_BUSY`/`BUSY_SNAPSHOT`, a missing
/// table — is **propagated**, never silently mapped to "absent" (the D-003
/// lesson, mirroring [`super::text::get_normalized_text`]).
pub fn get_embedding(
    conn: &Connection,
    key: &EmbeddingKey,
) -> rusqlite::Result<Option<EmbeddingCacheRow>> {
    conn.query_row(
        "SELECT dimensions, vector_f32, byte_size, checksum, created_at, last_used_at \
         FROM embedding_cache \
         WHERE subject_kind = ?1 AND subject_hash = ?2 AND representation_id = ?3",
        params![
            key.subject_kind.as_str(),
            key.subject_hash,
            key.representation_id
        ],
        |r| {
            Ok(EmbeddingCacheRow {
                dimensions: r.get(0)?,
                vector_f32: r.get(1)?,
                byte_size: r.get(2)?,
                checksum: r.get(3)?,
                created_at: r.get(4)?,
                last_used_at: r.get(5)?,
            })
        },
    )
    .optional()
}

/// Delete an `embedding_cache` row, returning whether one was removed.
///
/// Used both to evict a row that fails [`verify_cached_embedding`] (spec 03
/// §4.4 "mismatch → delete row, recompute lazily") and by budget-LRU eviction
/// ([`crate::eviction`]).
pub fn delete_embedding(tx: &Transaction<'_>, key: &EmbeddingKey) -> rusqlite::Result<bool> {
    let removed = tx.execute(
        "DELETE FROM embedding_cache \
         WHERE subject_kind = ?1 AND subject_hash = ?2 AND representation_id = ?3",
        params![
            key.subject_kind.as_str(),
            key.subject_hash,
            key.representation_id
        ],
    )?;
    Ok(removed > 0)
}

/// Delete every `embedding_cache` row for one subject, across **all**
/// representations, returning how many went.
///
/// [`delete_embedding`] needs the full three-field key because its callers —
/// integrity eviction and budget LRU — are always acting on one specific row.
/// A privacy purge is not: it is deleting a subject, and a subject may hold a
/// vector in every model space the store has ever had. Enumerating those
/// spaces at the call site would make the deletion's completeness depend on a
/// list the caller has no reason to know, so the filter drops the
/// representation instead (`D-074`).
pub fn delete_embeddings_for_subject(
    tx: &Transaction<'_>,
    subject_kind: SubjectKind,
    subject_hash: &str,
) -> rusqlite::Result<u64> {
    let removed = tx.execute(
        "DELETE FROM embedding_cache WHERE subject_kind = ?1 AND subject_hash = ?2",
        params![subject_kind.as_str(), subject_hash],
    )?;
    Ok(removed as u64)
}

/// Delete every `embedding_cache` row belonging to a memory entry, returning
/// how many went — `purge --all`'s half of `D-074`.
///
/// Deliberately a wholesale delete by kind rather than a loop over the hashes
/// of the entries being purged. `purge --all` removes every memory entry, so
/// the two are equivalent for rows that have a live subject; they differ for
/// rows that no longer do, and there the wholesale form is the correct one.
/// An orphan is exactly what must not survive the operation whose whole
/// purpose is that nothing derived from the purged text remains.
pub fn delete_all_memory_embeddings(tx: &Transaction<'_>) -> rusqlite::Result<u64> {
    let removed = tx.execute(
        "DELETE FROM embedding_cache WHERE subject_kind = ?1",
        params![SubjectKind::MemoryEntry.as_str()],
    )?;
    Ok(removed as u64)
}

/// One row of a bulk [`embeddings_for_subjects`] read: the row's
/// `subject_hash` (the other two key fields are the read's own filter, so
/// repeating them per row would be redundant) alongside its full vector
/// payload, undecoded/unverified (same contract as [`get_embedding`] — the
/// caller decodes via [`decode_vector_le`] and verifies via
/// [`verify_cached_embedding`] before trusting the bytes, matching every
/// other consumer of this cache).
#[derive(Debug, Clone, PartialEq)]
pub struct EmbeddingCacheEntry {
    /// The domain-separated subject hash (spec 03 §1.2).
    pub subject_hash: String,
    /// The row's vector payload and integrity fields.
    pub row: EmbeddingCacheRow,
}

/// How many `subject_hash` placeholders one statement of
/// [`embeddings_for_subjects`] may bind.
///
/// SQLite's own ceiling here is `SQLITE_MAX_VARIABLE_NUMBER`, which the
/// bundled build compiles at 32 766 — but that value is a build-time knob
/// (`libsqlite3-sys` reads it from the environment) and was 999 in every
/// SQLite before 3.32, so 999 is the portable floor rather than a measured
/// limit. Chunking at the floor is what lets this reader stay correct without
/// knowing its caller's own bound: recall's candidate set is capped by
/// `MAX_RECALL_CANDIDATES` (20 000) in `crates/memory`, a constant this crate
/// cannot see, and the next caller need not have a cap at all.
///
/// Deliberately **not** [`crate::EVICTION_BATCH_ROWS`] (500) or
/// `DEFAULT_WRITE_BATCH_ROWS`: those bound the rows a *write* transaction may
/// carry (spec 06 §5), a different axis entirely. Borrowing the number would
/// invent a lineage this constant does not have.
pub const EMBEDDING_SUBJECT_CHUNK: usize = 999;

const _: () = assert!(
    EMBEDDING_SUBJECT_CHUNK > 0 && EMBEDDING_SUBJECT_CHUNK <= 999,
    "the chunk must stay inside SQLite's portable parameter floor"
);

/// The `embedding_cache` rows for exactly the given `subject_hashes` under one
/// `(subject_kind, representation_id)`.
///
/// Returns them **in the caller's order**; a hash with no cached row is simply
/// absent (never an `Err`, never a positional `None`), and a repeated hash
/// yields one entry, at its first position. An empty request reads nothing at
/// all. Rows come back undecoded and unverified — the caller decodes via
/// [`decode_vector_le`] and verifies via [`verify_cached_embedding`], the same
/// contract [`get_embedding`] has. `last_used_at` is not touched (the LRU
/// bump lives in [`flush_last_used_embeddings`]).
///
/// D-067 replaced the earlier `embeddings_for_subject_kind(conn, kind,
/// representation_id, limit)`, which selected *every* row of the kind under
/// `ORDER BY subject_hash LIMIT ?`. Its one production caller — memory
/// recall's dense leg — passed its candidate count as that limit, so as soon
/// as the cache held more rows of the kind than the request had candidates
/// (other scopes, terminal entries, hashes left stale by earlier
/// `edit`/`supersede`), the read was truncated at an arbitrary point of hash
/// order and the candidates inside the cut silently lost their vectors.
/// Asking for the keys you actually want removes the failure mode instead of
/// documenting it.
///
/// The `IN (…)` list is built rather than bound through a carray/JSON
/// extension, keeping this on the same plain-SQL footing as
/// [`crate::occurrences_by_id`], and chunked at [`EMBEDDING_SUBJECT_CHUNK`]
/// so the statement's parameter count stays inside SQLite's portable floor
/// whatever the caller's own bound is.
pub fn embeddings_for_subjects(
    conn: &Connection,
    subject_kind: SubjectKind,
    representation_id: &str,
    subject_hashes: &[&str],
) -> rusqlite::Result<Vec<EmbeddingCacheEntry>> {
    if subject_hashes.is_empty() {
        return Ok(Vec::new());
    }
    let kind = subject_kind.as_str();
    let mut found: HashMap<String, EmbeddingCacheEntry> =
        HashMap::with_capacity(subject_hashes.len());
    for chunk in subject_hashes.chunks(EMBEDDING_SUBJECT_CHUNK) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(",");
        let mut stmt = conn.prepare(&format!(
            "SELECT subject_hash, dimensions, vector_f32, byte_size, checksum, created_at, \
                    last_used_at \
             FROM embedding_cache \
             WHERE subject_kind = ?1 AND representation_id = ?2 \
               AND subject_hash IN ({placeholders})"
        ))?;
        let mut bound: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(chunk.len() + 2);
        bound.push(&kind);
        bound.push(&representation_id);
        for hash in chunk {
            bound.push(hash);
        }
        let rows = stmt
            .query_map(bound.as_slice(), |r| {
                Ok(EmbeddingCacheEntry {
                    subject_hash: r.get(0)?,
                    row: EmbeddingCacheRow {
                        dimensions: r.get(1)?,
                        vector_f32: r.get(2)?,
                        byte_size: r.get(3)?,
                        checksum: r.get(4)?,
                        created_at: r.get(5)?,
                        last_used_at: r.get(6)?,
                    },
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for entry in rows {
            found.insert(entry.subject_hash.clone(), entry);
        }
    }
    Ok(subject_hashes
        .iter()
        .filter_map(|hash| found.remove(*hash))
        .collect())
}

/// Every `embedding_cache` row's meta projection, for the eviction scan
/// ([`crate::eviction::rows_to_evict`]).
///
/// A stored `subject_kind` outside the CHECK domain (corruption) surfaces as
/// [`rusqlite::Error::FromSqlConversionFailure`], never a silent default.
pub fn all_embedding_meta(conn: &Connection) -> rusqlite::Result<Vec<EmbeddingCacheMeta>> {
    let mut stmt = conn.prepare(
        "SELECT subject_kind, subject_hash, representation_id, byte_size, last_used_at \
         FROM embedding_cache",
    )?;
    let rows = stmt
        .query_map([], |r| {
            let raw_kind: String = r.get(0)?;
            let subject_kind = SubjectKind::from_db(&raw_kind).ok_or_else(|| {
                Error::FromSqlConversionFailure(
                    0,
                    Type::Text,
                    format!("invalid embedding_cache.subject_kind {raw_kind:?}").into(),
                )
            })?;
            Ok(EmbeddingCacheMeta {
                key: EmbeddingKey {
                    subject_kind,
                    subject_hash: r.get(1)?,
                    representation_id: r.get(2)?,
                },
                byte_size: r.get(3)?,
                last_used_at: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Why [`verify_cached_embedding`] rejected a row (spec 03 §4.2/§4.4). The card
/// calls out "checksum/dimension" as distinct failure modes worth
/// distinguishing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddingDivergence {
    /// The stored `dimensions`/`byte_size` do not match the actual
    /// `vector_f32` length.
    DimensionMismatch,
    /// The recomputed checksum over `vector_f32` does not match the stored
    /// `checksum`.
    ChecksumMismatch,
}

impl std::fmt::Display for EmbeddingDivergence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EmbeddingDivergence::DimensionMismatch => {
                write!(f, "stored dimensions/byte_size do not match vector_f32")
            }
            EmbeddingDivergence::ChecksumMismatch => {
                write!(f, "stored checksum does not match the recomputed digest")
            }
        }
    }
}

impl std::error::Error for EmbeddingDivergence {}

/// Validate `row`'s integrity (spec 03 §4.2/§4.4 step 4): "`embedding_cache`
/// rows are trusted per-row via `checksum` on read; mismatch → delete row,
/// recompute lazily."
///
/// Checks dimensions first (cheap: a length comparison) before recomputing the
/// checksum (a full hash over the vector bytes) — cheap before expensive,
/// mirroring [`super::validate::validate_fts_cheap`]/`validate_fts_strong`'s
/// established ordering.
pub fn verify_cached_embedding(row: &EmbeddingCacheRow) -> Result<(), EmbeddingDivergence> {
    let expected_len = row.dimensions.saturating_mul(4);
    if row.dimensions < 0
        || row.byte_size < 0
        || i64::try_from(row.vector_f32.len()).unwrap_or(i64::MAX) != row.byte_size
        || row.byte_size != expected_len
    {
        return Err(EmbeddingDivergence::DimensionMismatch);
    }
    if local_rag_core::hash::sha256_hex(&row.vector_f32) != row.checksum {
        return Err(EmbeddingDivergence::ChecksumMismatch);
    }
    Ok(())
}

/// Records that a cached embedding was used, for later batched `last_used_at`
/// flushing (spec 03 §3) — the [`EmbeddingKey`]-composite-keyed analogue of
/// [`super::text::LastUsedSink`].
pub trait LastUsedSinkEmbedding: Send + Sync {
    /// Note that `key` was used at `now_ms`.
    fn record_used(&self, key: &EmbeddingKey, now_ms: i64);
}

/// An in-memory [`LastUsedSinkEmbedding`] accumulator (spec 03 §3) — the
/// composite-keyed analogue of [`super::text::BatchingLastUsed`]. Deduplicates
/// per [`EmbeddingKey`] to the *latest* timestamp; the flush *cadence* is a
/// later task's concern, exactly like its sibling.
#[derive(Debug, Default)]
pub struct BatchingLastUsedEmbeddings {
    pending: Mutex<HashMap<EmbeddingKey, i64>>,
}

impl BatchingLastUsedEmbeddings {
    /// A new, empty accumulator.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of distinct keys with a pending update.
    pub fn len(&self) -> usize {
        self.pending.lock().expect("last-used lock").len()
    }

    /// Whether there are no pending updates.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Take and clear the pending updates as `(key, last_used_at)` pairs.
    pub fn drain(&self) -> Vec<(EmbeddingKey, i64)> {
        self.pending
            .lock()
            .expect("last-used lock")
            .drain()
            .collect()
    }
}

impl LastUsedSinkEmbedding for BatchingLastUsedEmbeddings {
    fn record_used(&self, key: &EmbeddingKey, now_ms: i64) {
        let mut pending = self.pending.lock().expect("last-used lock");
        pending
            .entry(key.clone())
            .and_modify(|ts| *ts = (*ts).max(now_ms))
            .or_insert(now_ms);
    }
}

/// Apply a drained batch of `last_used_at` updates in one transaction (spec 03
/// §3). Returns the number of rows actually updated (a key evicted since it was
/// recorded simply matches no row — not an error, mirrors
/// [`super::text::flush_last_used`]).
pub fn flush_last_used_embeddings(
    tx: &Transaction<'_>,
    updates: &[(EmbeddingKey, i64)],
) -> rusqlite::Result<usize> {
    let mut stmt = tx.prepare(
        "UPDATE embedding_cache SET last_used_at = ?4 \
         WHERE subject_kind = ?1 AND subject_hash = ?2 AND representation_id = ?3",
    )?;
    let mut applied = 0;
    for (key, last_used_at) in updates {
        applied += stmt.execute(params![
            key.subject_kind.as_str(),
            key.subject_hash,
            key.representation_id,
            last_used_at
        ])?;
    }
    Ok(applied)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(kind: SubjectKind, hash: &str, repr: &str) -> EmbeddingKey {
        EmbeddingKey {
            subject_kind: kind,
            subject_hash: hash.to_string(),
            representation_id: repr.to_string(),
        }
    }

    #[test]
    fn subject_kind_round_trips() {
        for kind in [
            SubjectKind::ContentBlob,
            SubjectKind::OccurrenceContext,
            SubjectKind::MemoryEntry,
        ] {
            assert_eq!(SubjectKind::from_db(kind.as_str()), Some(kind));
        }
        assert_eq!(SubjectKind::from_db("bogus"), None);
    }

    #[test]
    fn vector_codec_round_trips() {
        let original = vec![1.0f32, -2.5, 0.0, f32::MIN_POSITIVE, 3.75];
        let bytes = encode_vector_le(&original);
        assert_eq!(bytes.len(), original.len() * 4);
        let decoded = decode_vector_le(&bytes).expect("decode");
        assert_eq!(decoded, original);
    }

    #[test]
    fn decode_rejects_byte_length_not_a_multiple_of_four() {
        let err = decode_vector_le(&[0u8, 1, 2]).expect_err("must reject");
        assert_eq!(err.byte_len, 3);
    }

    #[test]
    fn verify_cached_embedding_accepts_a_freshly_encoded_row() {
        let vector = vec![1.0f32, 2.0, 3.0];
        let bytes = encode_vector_le(&vector);
        let row = EmbeddingCacheRow {
            dimensions: 3,
            byte_size: i64::try_from(bytes.len()).unwrap(),
            checksum: local_rag_core::hash::sha256_hex(&bytes),
            vector_f32: bytes,
            created_at: 1000,
            last_used_at: 1000,
        };
        assert_eq!(verify_cached_embedding(&row), Ok(()));
    }

    #[test]
    fn verify_cached_embedding_catches_dimension_mismatch() {
        let vector = vec![1.0f32, 2.0, 3.0];
        let bytes = encode_vector_le(&vector);
        let row = EmbeddingCacheRow {
            dimensions: 4, // wrong: only 3 f32s stored
            byte_size: i64::try_from(bytes.len()).unwrap(),
            checksum: local_rag_core::hash::sha256_hex(&bytes),
            vector_f32: bytes,
            created_at: 1000,
            last_used_at: 1000,
        };
        assert_eq!(
            verify_cached_embedding(&row),
            Err(EmbeddingDivergence::DimensionMismatch)
        );
    }

    #[test]
    fn verify_cached_embedding_catches_checksum_mismatch() {
        let vector = vec![1.0f32, 2.0, 3.0];
        let mut bytes = encode_vector_le(&vector);
        let checksum = local_rag_core::hash::sha256_hex(&bytes);
        bytes[0] ^= 0xFF; // tamper after computing the checksum
        let row = EmbeddingCacheRow {
            dimensions: 3,
            byte_size: i64::try_from(bytes.len()).unwrap(),
            checksum,
            vector_f32: bytes,
            created_at: 1000,
            last_used_at: 1000,
        };
        assert_eq!(
            verify_cached_embedding(&row),
            Err(EmbeddingDivergence::ChecksumMismatch)
        );
    }

    #[test]
    fn batching_dedups_to_latest_timestamp_per_composite_key() {
        let sink = BatchingLastUsedEmbeddings::new();
        assert!(sink.is_empty());
        let a = key(SubjectKind::ContentBlob, "hash-a", "repr-1");
        let b = key(SubjectKind::MemoryEntry, "hash-b", "repr-2");
        sink.record_used(&a, 100);
        sink.record_used(&a, 300); // later wins
        sink.record_used(&a, 200); // earlier ignored
        sink.record_used(&b, 50);
        assert_eq!(sink.len(), 2);

        let mut drained = sink.drain();
        drained.sort_by(|x, y| x.0.cmp(&y.0));
        assert_eq!(drained, vec![(a, 300), (b, 50)]);
        assert!(sink.is_empty());
        assert!(sink.drain().is_empty());
    }
}
