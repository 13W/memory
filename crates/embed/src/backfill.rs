//! The resumable coverage backfill worker (spec 10 §3/§4 step 2) — T11-04.
//!
//! Spec 10 §4's migration recipe puts one worker between "register model space B"
//! and "B → projection_ready": *"Backfill worker embeds expected content into
//! `embedding_cache` under B's `representation_ids` (batch, resumable; coverage
//! tracks progress)"*. This is that worker.
//!
//! # Resumability without a journal
//!
//! There is no progress table and no checkpoint. Spec 10 §3 `[FIXED]` makes
//! coverage *"always recomputable from `state.sqlite` × `embedding_cache`"*, so
//! every run recomputes `missing = expected \ valid_cached` from the two
//! databases and continues from whatever is left. A crash — at any point, mid
//! batch or between batches — is healed by running again; nothing has to be
//! replayed, rolled back, or reconciled. This is the same model
//! [`local_rag_store::retention`]'s sweep uses, and the reason spec 02 §4.1's
//! daemon-start resume list does not mention backfill at all.
//!
//! # Ordering constraints this worker must respect
//!
//! * **Never one transaction across both databases** (spec 03 §1.4 `[FIXED]`):
//!   vectors land in `cache.sqlite` (L4b) and coverage lands in `state.sqlite`
//!   (L4a) as separate transactions. A crash between them is exactly why
//!   coverage is *advisory* and recomputed rather than incremented.
//! * **Never embed inside a write job** (spec 02 §5: "L4 queues are leaves").
//!   Embedding is the slow part (ADR-0004 measured ≈43 ms per snippet for the
//!   selected model); it runs between transactions, never inside one.
//! * **Bounded** (spec 02 §5 `[FIXED]`, 06 §5 `[SPEC: ≤ 500 rows/tx]`): work is
//!   chunked into an embedding batch and a write batch, so neither the provider
//!   call nor the write transaction grows with the size of the repository.
//!
//! # Reported numbers
//!
//! [`BackfillReport`] carries the recomputed [`Coverage`] plus what this run did
//! (`embedded`/`reused`/`repaired`/`failed`). `failed` is deliberately *not*
//! folded into `ready`: `Coverage::fully_covered` compares `ready >= expected`,
//! so a failed subject keeps the model space out of `projection_ready` instead of
//! silently promoting it (spec 04 §3).

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::Mutex;

use local_rag_core::config::DataPolicy;
use local_rag_core::identity::domain::subject_memory_entry;
use local_rag_store::rusqlite::Connection;
use local_rag_store::{
    CacheDb, CacheOpenError, CacheWriteError, Coverage, CoverageEntry, EmbeddingKey, ExternalPins,
    ModelSpaceState, RepresentationKind, RetentionParams, StateDb, SubjectSet, WriteError,
    context_subjects_for_generation, delete_embedding, derive_content_blob, expected_subject_keys,
    get_embedding, get_normalized_text, insert_embedding, model_space_required_kinds,
    model_space_required_representation_ids, occurrences_for_fts, pinned_generations,
    recompute_coverage, rusqlite, source_bytes, transition_model_space, verify_cached_embedding,
    verify_cached_text, write_model_space_coverage,
};

use crate::contract::{EmbedError, EmbedRequest};
use crate::pool::ProviderPool;

/// Subjects embedded per provider call (`[SPEC]`).
///
/// Small enough that one failed batch loses little work and a cancelled worker
/// stops promptly; large enough to amortize a provider call. Not a calibrated
/// number — O2 is still open, and no `config.toml` field exists for it (spec 02
/// §3.1), so it is recorded here the way T09-03 recorded its own uncalibrated
/// wait budget rather than invented as a threshold.
pub const DEFAULT_EMBED_BATCH: usize = 32;

/// Rows written per `cache.sqlite` transaction (`[SPEC: ≤ 500 rows/tx]`, spec 06
/// §5), mirroring `local_rag_store::EVICTION_BATCH_ROWS`.
pub const DEFAULT_WRITE_BATCH_ROWS: usize = 500;

/// Batch sizes for one backfill run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackfillParams {
    /// Subjects per provider call.
    pub embed_batch: usize,
    /// Rows per cache write transaction.
    pub write_batch_rows: usize,
}

impl Default for BackfillParams {
    fn default() -> Self {
        BackfillParams {
            embed_batch: DEFAULT_EMBED_BATCH,
            write_batch_rows: DEFAULT_WRITE_BATCH_ROWS,
        }
    }
}

/// What one backfill run observed and did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackfillReport {
    /// Coverage recomputed from `state.sqlite` × `embedding_cache` at the end of
    /// the run (also written to `model_space.coverage`).
    pub coverage: Coverage,
    /// Subjects embedded and written by this run.
    pub embedded: u64,
    /// Subjects that already had a valid cached row (never re-embedded).
    pub reused: u64,
    /// Cached rows that failed [`verify_cached_embedding`], were deleted, and
    /// re-embedded (spec 03 §4.4: "mismatch → delete row, recompute lazily").
    pub repaired: u64,
    /// Subjects whose embedding attempt failed in this run.
    pub failed: u64,
    /// Subjects skipped because another run holds them in flight.
    pub deferred: u64,
    /// Cache write transactions committed.
    pub batches: u64,
}

/// A failure that aborts a backfill run.
#[derive(Debug)]
#[non_exhaustive]
pub enum BackfillError {
    /// A `required` representation kind has no computable subject —
    /// `structural_description` (post-v0, no serialization decided yet) is the
    /// only one left; `code_raw`/`code_context`/`memory` all have subject
    /// functions (`memory`'s landed in T14-08/D-013).
    ///
    /// Refusing is deliberate. Treating such a kind as "zero expected subjects"
    /// would make [`Coverage::fully_covered`] true for it and let the model space
    /// reach `projection_ready` without being covered — a silent degradation
    /// (spec 02 §6 `[FIXED]`: nothing degrades silently).
    UnsupportedRequiredKind {
        /// The kind that cannot be resolved to subjects yet.
        kind: RepresentationKind,
    },
    /// The model space does not exist.
    UnknownModelSpace {
        /// The id that was looked up.
        model_space_id: String,
    },
    /// A subject's normalized text could not be recovered from `state.sqlite`.
    MissingSource {
        /// The content blob whose text is unavailable.
        blob_id: String,
    },
    /// Recomputed text does not hash back to its `blob_id` (corrupt source).
    BlobMismatch {
        /// The expected blob id.
        blob_id: String,
        /// What the recomputed text hashed to.
        recomputed: String,
    },
    /// Reading `state.sqlite` or `cache.sqlite` failed.
    Sqlite(rusqlite::Error),
    /// The provider pool refused the run outright — a policy refusal, a missing
    /// provider, or absent model assets. Unlike a per-batch failure these cannot
    /// improve on the next batch, so the run stops instead of counting every
    /// remaining subject as failed.
    Embed(EmbedError),
    /// Opening a read connection to `state.sqlite` failed.
    StateOpen(local_rag_store::OpenError),
    /// Opening a read connection to `cache.sqlite` failed.
    CacheOpen(CacheOpenError),
    /// Writing vectors to `cache.sqlite` failed.
    CacheWrite(CacheWriteError),
    /// Writing coverage to `state.sqlite` failed.
    StateWrite(WriteError),
    /// A named failpoint fired between batches (test builds only); the rows
    /// already committed stay committed and the next run resumes from them.
    #[cfg(feature = "failpoints")]
    Interrupted,
}

impl std::fmt::Display for BackfillError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackfillError::UnsupportedRequiredKind { kind } => write!(
                f,
                "required representation kind {} has no subject function yet; \
                 refusing to report it as covered",
                kind.as_str()
            ),
            BackfillError::UnknownModelSpace { model_space_id } => {
                write!(f, "unknown model space {model_space_id}")
            }
            BackfillError::MissingSource { blob_id } => {
                write!(
                    f,
                    "no source available to recompute text for blob {blob_id}"
                )
            }
            BackfillError::BlobMismatch {
                blob_id,
                recomputed,
            } => write!(
                f,
                "recomputed text for blob {blob_id} hashes to {recomputed}"
            ),
            BackfillError::Sqlite(e) => write!(f, "sqlite error during backfill: {e}"),
            BackfillError::Embed(e) => write!(f, "embedding refused: {e}"),
            BackfillError::StateOpen(e) => write!(f, "could not open state: {e}"),
            BackfillError::CacheOpen(e) => write!(f, "could not open cache: {e}"),
            BackfillError::CacheWrite(e) => write!(f, "could not write embeddings: {e}"),
            BackfillError::StateWrite(e) => write!(f, "could not write coverage: {e}"),
            #[cfg(feature = "failpoints")]
            BackfillError::Interrupted => write!(f, "backfill interrupted between batches"),
        }
    }
}

impl std::error::Error for BackfillError {}

impl From<rusqlite::Error> for BackfillError {
    fn from(e: rusqlite::Error) -> Self {
        BackfillError::Sqlite(e)
    }
}

impl From<CacheOpenError> for BackfillError {
    fn from(e: CacheOpenError) -> Self {
        BackfillError::CacheOpen(e)
    }
}

impl From<CacheWriteError> for BackfillError {
    fn from(e: CacheWriteError) -> Self {
        BackfillError::CacheWrite(e)
    }
}

impl From<WriteError> for BackfillError {
    fn from(e: WriteError) -> Self {
        BackfillError::StateWrite(e)
    }
}

/// Subjects currently being embedded, shared between concurrent runs.
///
/// Two runs over the same model space would otherwise each observe the same
/// subject as missing and embed it twice — correct (writes are idempotent) but
/// wasteful, and the waste is the expensive half. A run *reserves* the keys it is
/// about to embed; a concurrent run sees them reserved, counts them as
/// `deferred`, and moves on. Reservations are released when the run finishes with
/// them, so an interrupted run's keys are simply picked up by the next one.
///
/// A `std::sync::Mutex` is correct here precisely because the guard is never held
/// across an `.await` — reserve and release are both synchronous set operations.
#[derive(Debug, Default)]
pub struct InFlight(Mutex<HashSet<EmbeddingKey>>);

impl InFlight {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Reserve every key of `keys` that is not already reserved; returns the
    /// reserved subset in input order.
    fn reserve(&self, keys: &[EmbeddingKey]) -> Vec<EmbeddingKey> {
        let mut guard = self.0.lock().expect("in-flight registry poisoned");
        keys.iter()
            .filter(|key| guard.insert((*key).clone()))
            .cloned()
            .collect()
    }

    /// Release reservations.
    fn release(&self, keys: &[EmbeddingKey]) {
        let mut guard = self.0.lock().expect("in-flight registry poisoned");
        for key in keys {
            guard.remove(key);
        }
    }

    /// How many keys are reserved right now (diagnostics/tests).
    pub fn len(&self) -> usize {
        self.0.lock().expect("in-flight registry poisoned").len()
    }

    /// Whether nothing is reserved.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// One subject that needs embedding: its cache key and which representation it
/// belongs to. The text itself is resolved later, through the subject-hash →
/// source index, so the partition pass never touches `state.sqlite`.
#[derive(Debug, Clone)]
struct PendingSubject {
    key: EmbeddingKey,
    kind: RepresentationKind,
}

/// Run one backfill pass for `model_space_id`.
///
/// Reads the expected subject set (pin-root generations × the space's `required`
/// representations, `local_rag_store::subjects`), reuses every valid cached row,
/// embeds what is missing in bounded batches through `pool`, writes the vectors
/// in bounded `cache.sqlite` transactions, then recomputes and stores coverage in
/// its own `state.sqlite` transaction.
///
/// `policy` is the **effective** data policy (compute it with
/// `local_rag_store::effective_data_policy`); it is passed straight to the pool,
/// whose guard runs before provider selection.
///
/// Call again after a crash, a failure, or a partial run — the pass is
/// idempotent and picks up exactly what is still missing.
#[allow(clippy::too_many_arguments)]
pub async fn run_backfill(
    state_db: &StateDb,
    cache: &CacheDb,
    pool: &ProviderPool,
    policy: DataPolicy,
    model_space_id: &str,
    params: &BackfillParams,
    retention: &RetentionParams,
    in_flight: &InFlight,
    now_ms: i64,
) -> Result<BackfillReport, BackfillError> {
    let state_read = state_db.open_read().map_err(BackfillError::StateOpen)?;

    let expected = expected_set(&state_read, model_space_id, retention, now_ms)?;
    let representations = model_space_required_representation_ids(&state_read, model_space_id)?;

    let mut report = BackfillReport::default();

    // 1. Partition the expected set into "already valid" and "must embed",
    //    repairing corrupt rows on the way (spec 03 §4.4 step 4).
    let (pending, corrupt) = partition_expected(cache, &expected, &representations, &mut report)?;
    if !corrupt.is_empty() {
        let to_delete = corrupt.clone();
        cache
            .writer()
            .transaction(move |tx| {
                for key in &to_delete {
                    delete_embedding(tx, key)?;
                }
                Ok(())
            })
            .await?;
    }

    // 2. Reserve what this run will embed, so a concurrent run does not redo it.
    let pending_keys: Vec<EmbeddingKey> = pending.iter().map(|p| p.key.clone()).collect();
    let reserved: BTreeSet<EmbeddingKey> = in_flight.reserve(&pending_keys).into_iter().collect();
    report.deferred = (pending.len() - reserved.len()) as u64;
    let mine: Vec<PendingSubject> = pending
        .into_iter()
        .filter(|p| reserved.contains(&p.key))
        .collect();
    let reserved_keys: Vec<EmbeddingKey> = mine.iter().map(|p| p.key.clone()).collect();

    let outcome = embed_and_write(
        &state_read,
        cache,
        pool,
        policy,
        &mine,
        params,
        &expected,
        now_ms,
        &mut report,
    )
    .await;

    // Always release reservations, including on the error paths — an interrupted
    // run must not leave keys locked out of the next one.
    in_flight.release(&reserved_keys);
    outcome?;

    // 3. Recompute coverage from what is actually in the two databases and store
    //    it (its own state.sqlite transaction — never cross-database, 03 §1.4).
    report.coverage = write_coverage(
        state_db,
        &state_read,
        cache,
        model_space_id,
        &expected,
        &representations,
        report.failed,
        now_ms,
    )
    .await?;

    Ok(report)
}

/// Promote `model_space_id` to `projection_ready` if its stored coverage is
/// complete (spec 04 §3, 10 §4 step 3).
///
/// Thin by design: the completeness gate lives in
/// `transition_model_space`, which reads the **stored** `model_space.coverage`
/// and refuses with `IncompleteCoverage` when a required kind is short. Run
/// [`run_backfill`] first — it is what makes the stored value current.
pub async fn promote_if_covered(
    state_db: &StateDb,
    model_space_id: &str,
    now_ms: i64,
) -> Result<Result<(), local_rag_store::ModelSpaceTransitionError>, BackfillError> {
    let read = state_db
        .open_read()
        .map_err(|_| BackfillError::UnknownModelSpace {
            model_space_id: model_space_id.to_string(),
        })?;
    let required = model_space_required_kinds(&read, model_space_id)?;
    let id = model_space_id.to_string();
    let outcome = state_db
        .writer()
        .transaction(move |tx| {
            transition_model_space(tx, &id, ModelSpaceState::ProjectionReady, &required, now_ms)
        })
        .await?;
    Ok(outcome)
}

/// The expected subject set, refusing any `required` kind with no subject
/// function (see [`BackfillError::UnsupportedRequiredKind`]).
fn expected_set(
    state: &Connection,
    model_space_id: &str,
    retention: &RetentionParams,
    now_ms: i64,
) -> Result<SubjectSet, BackfillError> {
    let generations = pinned_generations(state, retention, &ExternalPins::default(), now_ms)?;
    let set = expected_subject_keys(state, model_space_id, &generations)?;
    if let Some(kind) = set.unsupported.iter().next() {
        return Err(BackfillError::UnsupportedRequiredKind { kind: *kind });
    }
    Ok(set)
}

/// Split the expected set into subjects that still need embedding and cached
/// rows that are corrupt (to delete, then re-embed).
fn partition_expected(
    cache: &CacheDb,
    expected: &SubjectSet,
    representations: &[(RepresentationKind, String)],
    report: &mut BackfillReport,
) -> Result<(Vec<PendingSubject>, Vec<EmbeddingKey>), BackfillError> {
    let by_representation: BTreeMap<&str, RepresentationKind> = representations
        .iter()
        .map(|(kind, id)| (id.as_str(), *kind))
        .collect();

    let read = cache.open_read()?;
    let mut pending = Vec::new();
    let mut corrupt = Vec::new();

    for key in &expected.keys {
        let kind = by_representation
            .get(key.representation_id.as_str())
            .copied()
            .unwrap_or(RepresentationKind::CodeRaw);

        match get_embedding(&read, key)? {
            Some(row) if verify_cached_embedding(&row).is_ok() => report.reused += 1,
            Some(_) => {
                report.repaired += 1;
                corrupt.push(key.clone());
                pending.push(PendingSubject {
                    key: key.clone(),
                    kind,
                });
            }
            None => pending.push(PendingSubject {
                key: key.clone(),
                kind,
            }),
        }
    }
    Ok((pending, corrupt))
}

/// Embed and persist `pending` in bounded batches.
#[allow(clippy::too_many_arguments)]
async fn embed_and_write(
    state_read: &Connection,
    cache: &CacheDb,
    pool: &ProviderPool,
    policy: DataPolicy,
    pending: &[PendingSubject],
    params: &BackfillParams,
    expected: &SubjectSet,
    now_ms: i64,
    report: &mut BackfillReport,
) -> Result<(), BackfillError> {
    if pending.is_empty() {
        return Ok(());
    }

    // subject_hash → (blob_id, source row) for every expected `code_raw` subject.
    let sources = blob_index(state_read, expected)?;
    // …and subject_hash → envelope for every `code_context` one. Built only when
    // some pending subject actually needs it: the pass reads every source byte of
    // the expected generations, which a `code_raw`-only run has no reason to pay.
    let contexts = if pending
        .iter()
        .any(|p| p.key.subject_kind == local_rag_store::SubjectKind::OccurrenceContext)
    {
        context_index(state_read, expected)?
    } else {
        BTreeMap::new()
    };
    // …and subject_hash → text for every `memory` subject, gated the same way
    // (a code-only run never reads `memory_entry` at all).
    let memories = if pending
        .iter()
        .any(|p| p.key.subject_kind == local_rag_store::SubjectKind::MemoryEntry)
    {
        memory_index(state_read)?
    } else {
        BTreeMap::new()
    };

    let mut written: Vec<(EmbeddingKey, i64, Vec<f32>)> = Vec::new();
    for chunk in pending.chunks(params.embed_batch.max(1)) {
        let mut texts = Vec::with_capacity(chunk.len());
        let mut keys = Vec::with_capacity(chunk.len());
        for subject in chunk {
            // Every expected key came from a generation in `expected` (or, for
            // `memory`, from the same table read twice), so its source must be
            // in the matching index; an absence means the two views disagree,
            // which is a defect, not a recoverable state.
            let text = match subject.key.subject_kind {
                local_rag_store::SubjectKind::OccurrenceContext => contexts
                    .get(subject.key.subject_hash.as_str())
                    .cloned()
                    .ok_or_else(|| BackfillError::MissingSource {
                        blob_id: subject.key.subject_hash.clone(),
                    })?,
                local_rag_store::SubjectKind::MemoryEntry => memories
                    .get(subject.key.subject_hash.as_str())
                    .cloned()
                    .ok_or_else(|| BackfillError::MissingSource {
                        blob_id: subject.key.subject_hash.clone(),
                    })?,
                local_rag_store::SubjectKind::ContentBlob => {
                    let source =
                        sources
                            .get(subject.key.subject_hash.as_str())
                            .ok_or_else(|| BackfillError::MissingSource {
                                blob_id: subject.key.subject_hash.clone(),
                            })?;
                    normalized_text(state_read, cache, source)?
                }
            };
            texts.push(text);
            keys.push(subject.key.clone());
        }

        // The slow call: outside every transaction (spec 02 §5, "L4 queues are
        // leaves").
        let kind = chunk[0].kind;
        match pool.embed(policy, EmbedRequest::new(kind, texts)) {
            Ok(vectors) => {
                for (key, vector) in keys.into_iter().zip(vectors) {
                    written.push((key, vector.dimensions() as i64, vector.into_inner()));
                }
            }
            Err(err) if is_fatal(&err) => return Err(BackfillError::Embed(err)),
            Err(_) => {
                // A failed batch is counted, not fatal: the run continues and the
                // shortfall keeps the model space out of `projection_ready`.
                report.failed += keys.len() as u64;
            }
        }

        while written.len() >= params.write_batch_rows.max(1) {
            let batch: Vec<_> = written.drain(..params.write_batch_rows.max(1)).collect();
            flush(cache, batch, now_ms, report).await?;
        }
    }

    if !written.is_empty() {
        flush(cache, written, now_ms, report).await?;
    }
    Ok(())
}

/// Commit one batch of vectors, then expose the named crash point.
async fn flush(
    cache: &CacheDb,
    batch: Vec<(EmbeddingKey, i64, Vec<f32>)>,
    now_ms: i64,
    report: &mut BackfillReport,
) -> Result<(), BackfillError> {
    let rows = batch.len() as u64;
    cache
        .writer()
        .transaction(move |tx| {
            for (key, dimensions, vector) in &batch {
                insert_embedding(tx, key, *dimensions, vector, now_ms)?;
            }
            Ok(())
        })
        .await?;
    report.embedded += rows;
    report.batches += 1;

    // Crash point *after* a non-empty batch commits — the resume test kills here
    // and asserts the next run continues from the committed rows (the same
    // placement `local_rag_store::retention`'s sweep uses).
    #[cfg(feature = "failpoints")]
    local_rag_test_support::fail_point!(
        "embed.backfill.between_batches",
        Err(BackfillError::Interrupted)
    );

    Ok(())
}

/// Recompute coverage from the live databases and store it.
#[allow(clippy::too_many_arguments)]
async fn write_coverage(
    state_db: &StateDb,
    state_read: &Connection,
    cache: &CacheDb,
    model_space_id: &str,
    expected: &SubjectSet,
    representations: &[(RepresentationKind, String)],
    failed: u64,
    now_ms: i64,
) -> Result<Coverage, BackfillError> {
    let expected_counts = expected.expected_per_kind(representations);
    let read = cache.open_read()?;

    let mut counts: BTreeMap<RepresentationKind, CoverageEntry> = BTreeMap::new();
    for (kind, representation_id) in representations {
        let mut entry = CoverageEntry {
            expected: expected_counts.get(kind).copied().unwrap_or_default(),
            ready: 0,
            failed: 0,
        };
        for key in expected
            .keys
            .iter()
            .filter(|k| &k.representation_id == representation_id)
        {
            if let Some(row) = get_embedding(&read, key)?
                && verify_cached_embedding(&row).is_ok()
            {
                entry.ready += 1;
            }
        }
        entry.failed = entry.expected.saturating_sub(entry.ready).min(failed);
        counts.insert(*kind, entry);
    }
    drop(read);

    let required = model_space_required_kinds(state_read, model_space_id)?;
    let coverage = recompute_coverage(&required, &counts);

    let (id, stored) = (model_space_id.to_string(), coverage.clone());
    state_db
        .writer()
        .transaction(move |tx| write_model_space_coverage(tx, &id, &stored, now_ms))
        .await?;
    Ok(coverage)
}

/// Whether a pool error means "stop the run" rather than "count this batch as
/// failed": policy refusals and missing providers cannot improve by trying the
/// next batch.
fn is_fatal(err: &EmbedError) -> bool {
    matches!(
        err,
        EmbedError::PolicyBlockedRemote { .. }
            | EmbedError::NoProvider { .. }
            | EmbedError::ModelAssetsMissing { .. }
    )
}

/// Resolve every expected subject's source row, keyed by subject hash.
/// `subject_hash → envelope` for every `code_context` subject of the expected
/// generations (D-016).
///
/// Unlike `code_raw`, whose text is recomputed per subject on demand, a context
/// envelope is produced by the same pass that computes its hash — the hash *is*
/// the hash of that text, so carrying it here costs nothing and removes any way
/// for the two to disagree.
fn context_index(
    state: &Connection,
    expected: &SubjectSet,
) -> Result<BTreeMap<String, String>, BackfillError> {
    let mut index: BTreeMap<String, String> = BTreeMap::new();
    for generation_id in &expected.generations {
        for subject in context_subjects_for_generation(state, generation_id)? {
            index
                .entry(subject.subject_hash)
                .or_insert(subject.serialization);
        }
    }
    Ok(index)
}

/// `subject_hash → text` for every `memory_entry` row (T14-08). Built only
/// when some pending subject actually needs it, mirroring [`context_index`]'s
/// own gating: a `code_raw`/`code_context`-only run has no reason to read the
/// memory table at all. Unlike [`blob_index`]/[`context_index`], this does not
/// filter by `expected.generations` — memory is not generation-scoped (see
/// `local_rag_store::subjects`'s own module doc) — so it simply re-derives
/// every row's hash the identical way `memory_entry_subject_keys` did when
/// building `expected` in the first place; the two independently agreeing is
/// what lets [`embed_and_write`]'s `.ok_or(MissingSource)` below catch a real
/// disagreement between the two views rather than assuming one.
fn memory_index(state: &Connection) -> Result<BTreeMap<String, String>, BackfillError> {
    Ok(local_rag_store::all_memory_entries_with_text(state)?
        .into_iter()
        .map(|(memory_id, text)| {
            let hash = subject_memory_entry(&memory_id, &text);
            (hash, text)
        })
        .collect())
}

fn blob_index(
    state: &Connection,
    expected: &SubjectSet,
) -> Result<BTreeMap<String, SubjectSource>, BackfillError> {
    let mut index: BTreeMap<String, SubjectSource> = BTreeMap::new();
    for generation_id in &expected.generations {
        for row in occurrences_for_fts(state, generation_id)? {
            let hash = local_rag_core::identity::domain::subject_content_blob(&row.blob_id);
            index.entry(hash).or_insert(SubjectSource {
                blob_id: row.blob_id,
                file_revision_id: row.file_revision_id,
                span_start: row.span_start,
                span_end: row.span_end,
                language: row.language,
            });
        }
    }
    Ok(index)
}

/// Everything needed to produce (or recompute) one subject's text.
#[derive(Debug, Clone)]
struct SubjectSource {
    blob_id: String,
    file_revision_id: String,
    span_start: i64,
    span_end: i64,
    language: String,
}

/// The normalized text of a subject: a verified cache hit, or a recompute from
/// the exact `source_blob` — the same recipe
/// `local_rag_store::cache::materialize_fts` uses (spec 06 §4).
fn normalized_text(
    state: &Connection,
    cache: &CacheDb,
    source: &SubjectSource,
) -> Result<String, BackfillError> {
    let read = cache.open_read()?;
    if let Some(hit) = get_normalized_text(&read, &source.blob_id)?
        && verify_cached_text(&source.blob_id, &source.language, &hit.normalized_text)
    {
        return Ok(hit.normalized_text);
    }
    drop(read);

    let bytes = source_bytes(state, &source.file_revision_id)?.ok_or_else(|| {
        BackfillError::MissingSource {
            blob_id: source.blob_id.clone(),
        }
    })?;
    let slice = bytes
        .get(source.span_start as usize..source.span_end as usize)
        .ok_or_else(|| BackfillError::MissingSource {
            blob_id: source.blob_id.clone(),
        })?;
    let text = std::str::from_utf8(slice).map_err(|_| BackfillError::MissingSource {
        blob_id: source.blob_id.clone(),
    })?;
    let derived = derive_content_blob(&source.language, text);
    if derived.blob_id != source.blob_id {
        return Err(BackfillError::BlobMismatch {
            blob_id: source.blob_id.clone(),
            recomputed: derived.blob_id,
        });
    }
    Ok(derived.normalized_text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(hash: &str) -> EmbeddingKey {
        EmbeddingKey {
            subject_kind: local_rag_store::SubjectKind::ContentBlob,
            subject_hash: hash.to_string(),
            representation_id: "r-1".to_string(),
        }
    }

    #[test]
    fn in_flight_reserves_each_key_once() {
        let registry = InFlight::new();
        let keys = vec![key("a"), key("b")];

        let first = registry.reserve(&keys);
        assert_eq!(first.len(), 2, "a fresh registry reserves everything");
        assert_eq!(registry.len(), 2);

        // A concurrent run sees them taken and gets nothing.
        assert!(registry.reserve(&keys).is_empty());

        registry.release(&first);
        assert!(registry.is_empty());
        // ... and after release they are available again (an interrupted run
        // must not lock its keys out of the next one).
        assert_eq!(registry.reserve(&keys).len(), 2);
    }

    #[test]
    fn in_flight_reserves_only_the_free_subset() {
        let registry = InFlight::new();
        registry.reserve(&[key("a")]);

        let reserved = registry.reserve(&[key("a"), key("b"), key("c")]);
        assert_eq!(
            reserved,
            vec![key("b"), key("c")],
            "an already-reserved key is skipped, the rest are taken, in order"
        );
    }

    #[test]
    fn only_unrecoverable_pool_errors_stop_the_run() {
        // These cannot improve by trying the next batch.
        assert!(is_fatal(&EmbedError::PolicyBlockedRemote {
            policy: DataPolicy::LocalOnly,
            blocked: vec!["hosted".to_string()],
        }));
        assert!(is_fatal(&EmbedError::NoProvider {
            kind: RepresentationKind::CodeRaw,
        }));
        assert!(is_fatal(&EmbedError::ModelAssetsMissing {
            model_id: "m".to_string(),
            expected_path: "/models/m".to_string(),
        }));

        // These are per-batch failures: counted, and the run continues.
        assert!(!is_fatal(&EmbedError::permanent("400")));
        assert!(!is_fatal(&EmbedError::retryable("500")));
        assert!(!is_fatal(&EmbedError::AllProvidersFailed {
            failures: Vec::new(),
        }));
    }

    #[test]
    fn defaults_are_the_documented_batch_sizes() {
        let p = BackfillParams::default();
        assert_eq!(p.embed_batch, DEFAULT_EMBED_BATCH);
        assert_eq!(p.write_batch_rows, DEFAULT_WRITE_BATCH_ROWS);
        assert_eq!(
            p.write_batch_rows,
            local_rag_store::EVICTION_BATCH_ROWS,
            "the write batch mirrors the store's own ≤ 500 rows/tx bound"
        );
    }
}
