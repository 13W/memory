//! `cache.sqlite` open/validation policy and the single bounded writer.
//!
//! [`CacheDb::open`] applies the cache connection pragmas (spec 03 §4), validates
//! the store binding, rebuilds the cache if it is missing/incompatible/corrupt
//! (03 §4.4), and spawns the one writer task that owns the sole writable cache
//! connection (02 §5 L4b — physically separate from state's L4a). Callers never
//! touch that connection directly; they submit short transactions through
//! [`CacheWriter::transaction`]. Reads go through [`CacheDb::open_read`].
//!
//! The cache is bound to a store by `store_instance_uuid`, which the **caller**
//! supplies (the authoritative value comes from the state store's
//! `store_settings` at daemon startup, 02 §4.1). This layer neither generates nor
//! seeds that UUID — the UUIDv7 generator and its seeding land in later tasks.

mod embedding;
mod fts;
mod fts_query;
mod open;
mod text;
mod validate;
mod writer;

pub use embedding::{
    BatchingLastUsedEmbeddings, EmbeddingCacheEntry, EmbeddingCacheMeta, EmbeddingCacheRow,
    EmbeddingDivergence, EmbeddingKey, LastUsedSinkEmbedding, SubjectKind, VectorLengthError,
    all_embedding_meta, decode_vector_le, delete_embedding, embeddings_for_subject_kind,
    encode_vector_le, flush_last_used_embeddings, get_embedding, insert_embedding,
    verify_cached_embedding,
};
pub use fts::{
    FtsMaterializeError, FtsMaterializeOutcome, FtsProjectionHeadRow, LEXICAL_SCHEMA_VERSION,
    TOKENIZER_VERSION, fts_doc_occurrence_count, fts_doc_occurrence_ids, fts_manifest_hash,
    materialize_fts, read_fts_projection_head, tokenize_identifier, tokenize_path,
    tokenize_qualified_name, tokenize_signature,
};
pub use fts_query::{
    BM25_DEFAULT_WEIGHTS, LexicalHit, LexicalQuery, MIN_CANDIDATE_DEPTH, candidate_depth,
    document_frequencies, fts_match_expression, fts_match_expression_from_terms,
    indexed_document_count, lexical_leg, query_fts, selective_terms,
};
pub use open::{CACHE_SCHEMA_VERSION, CacheOpenError, CacheOpenOutcome};
pub use text::{
    BatchingLastUsed, LastUsedSink, NormalizedTextRow, delete_normalized_text, flush_last_used,
    get_normalized_text, insert_normalized_text, verify_cached_text,
};
pub use validate::{
    FTS_SYNC_REBUILD_OCCURRENCE_THRESHOLD, FtsAvailability, FtsDivergence, FtsOpenOutcome,
    FtsRebuildError, ValidationDepth, open_and_validate_fts, requires_index_unavailable,
    should_rebuild_synchronously, validate_fts_cheap, validate_fts_strong,
};
pub use writer::{CacheWriteError, CacheWriter};

use std::path::{Path, PathBuf};

use rusqlite::Connection;

/// A handle to the rebuildable `cache.sqlite` store.
///
/// Owns the [`CacheWriter`] (the single write path) and hands out read-only
/// connections. The physical writer runs on a dedicated OS thread that owns the
/// only writable [`Connection`]; that thread stays alive until every
/// [`CacheWriter`] handle is dropped. As with state, a killed writer only ever
/// loses an uncommitted transaction — and for the cache even a lost *committed*
/// write is recoverable, since the whole store is rebuildable from state.
///
/// Dropping a `CacheDb` closes the queue but does **not** wait for the writer
/// thread to finish closing its connection. Use [`CacheDb::close`] when the next
/// step depends on this instance having fully let go of the files (D-009).
#[derive(Debug)]
pub struct CacheDb {
    path: PathBuf,
    writer: CacheWriter,
    outcome: CacheOpenOutcome,
    join: Option<std::thread::JoinHandle<()>>,
}

impl CacheDb {
    /// Open (creating/rebuilding as needed) `cache.sqlite` at `path`, bound to
    /// `store_instance_uuid`, with the default write-queue capacity
    /// ([`crate::DEFAULT_WRITE_QUEUE_CAPACITY`]).
    pub fn open(
        path: impl Into<PathBuf>,
        store_instance_uuid: &str,
    ) -> Result<Self, CacheOpenError> {
        Self::open_with_capacity(
            path,
            store_instance_uuid,
            crate::DEFAULT_WRITE_QUEUE_CAPACITY,
        )
    }

    /// Open `cache.sqlite` bound to `store_instance_uuid` with an explicit
    /// write-queue `capacity`.
    ///
    /// Validates the binding and rebuilds an incompatible/corrupt cache before
    /// the writer exists (open → validate/recreate → serve): the writer thread
    /// only ever receives a connection to an already-bound cache. A small
    /// capacity makes backpressure easy to exercise in tests; the daemon uses
    /// [`CacheDb::open`].
    pub fn open_with_capacity(
        path: impl Into<PathBuf>,
        store_instance_uuid: &str,
        capacity: usize,
    ) -> Result<Self, CacheOpenError> {
        let path = path.into();
        let (conn, outcome) =
            open::open_and_bind(&path, store_instance_uuid, crate::clock::system_now_ms())?;
        let (writer, join) =
            writer::CacheWriter::spawn(conn, capacity).map_err(CacheOpenError::Spawn)?;
        Ok(Self {
            path,
            writer,
            outcome,
            join: Some(join),
        })
    }

    /// Close the write path and **wait** for the writer thread to drop its
    /// connection.
    ///
    /// Ordinary `drop` is asynchronous: it closes the queue and lets the writer
    /// thread finish on its own, which is correct for a process that is going
    /// away (spec 02 §4.3 leaves graceful drain to the daemon, T15). It is *not*
    /// enough when something else immediately re-opens the very same path: SQLite
    /// checkpoints and unlinks `-wal`/`-shm` as the last connection closes, so a
    /// reader of the newly opened instance can observe the previous instance's
    /// teardown as a short read (`SQLITE_IOERR_SHORT_READ`) — a real, reproducible
    /// race found by D-009.
    ///
    /// Blocks until every [`CacheWriter`] clone handed out by this instance has
    /// been dropped; if a clone is still alive, the queue never closes and this
    /// call waits for it.
    pub fn close(mut self) {
        let join = self.join.take();
        // Drop this instance (and with it the writer handle it owns) so the queue
        // closes, then wait for the thread to exit.
        drop(self);
        if let Some(join) = join {
            // A panicking writer thread is already reported by its own unwind; the
            // caller of `close` only needs the "it is finished" guarantee.
            let _ = join.join();
        }
    }

    /// The single write path into `cache.sqlite`.
    pub fn writer(&self) -> &CacheWriter {
        &self.writer
    }

    /// Open a fresh **read-only** connection to `cache.sqlite`.
    ///
    /// The connection is opened `SQLITE_OPEN_READ_ONLY`, so any write attempt
    /// fails with `SQLITE_READONLY`. This is the cache read leg of the search
    /// pipeline (spec 02 §5); read-only cross-DB `ATTACH` is permitted (03 §1.4).
    pub fn open_read(&self) -> Result<Connection, CacheOpenError> {
        open::open_cache_read_only(&self.path)
    }

    /// Whether this cache was created, reused, or recreated at open time
    /// (spec 03 §4.4).
    pub fn outcome(&self) -> CacheOpenOutcome {
        self.outcome
    }

    /// The path to the underlying `cache.sqlite` file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}
