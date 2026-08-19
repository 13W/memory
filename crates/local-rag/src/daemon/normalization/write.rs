//! Applying one entry's normalization: vector first, row second (ADR-0010
//! Decision 6) — T21-05.
//!
//! A translation **moves an entry's subject hash**. The vector sitting in
//! `cache.sqlite` was computed for the original text, so the moment the
//! normalization row says "ready", every reader starts asking the cache for a
//! hash that was never written. Nothing errors: the dense leg simply finds no
//! vector and returns nothing, which is exactly the silent failure D-067 cost
//! this project once already.
//!
//! Hence the order, and it is the whole point of this module:
//!
//! 1. translate (`local_rag_memory::normalize::translate`);
//! 2. embed the English text and write the vector under the **new** hash into
//!    `cache.sqlite`;
//! 3. only then commit the normalization row into `state.sqlite`.
//!
//! Two databases, therefore two transactions — spec 03 §1.4 `[FIXED]` forbids a
//! writable cross-database transaction, so atomicity across the pair is not
//! available and the order is what stands in for it. A crash between the two
//! leaves an unreferenced cache row, which is harmless by the
//! "`cache.sqlite` is fully rebuildable" invariant and is reclaimed by ordinary
//! eviction. The reverse order would leave a *referenced* hash with no vector,
//! which nothing reclaims and nothing reports.
//!
//! The named crash point [`FAILPOINT_AFTER_VECTOR`] sits exactly between the
//! two writes so that order is proven from the production path rather than
//! argued in this comment.
//!
//! # What this module does not decide
//!
//! Which entries to normalize, in what batches, and how to back off after a
//! failure is T21-06's. This function applies exactly one entry and returns
//! what happened.

use local_rag_core::config::DataPolicy;
use local_rag_core::hash::sha256_hex;
use local_rag_embed::{EmbedRequest, ProviderPool};
use local_rag_memory::normalize::detect::ScriptClass;
use local_rag_memory::normalize::translate::{
    TRANSLATOR_VERSION, TranslateError, TranslateRequest, Translation, translate,
};
use local_rag_store::{
    CURRENT_NORMALIZER_VERSION, CacheDb, EmbeddingKey, ModelSpaceState, NormalizationStatus,
    NormalizationView, NormalizationWrite, RepresentationKind, StateDb, SubjectKind, UpsertOutcome,
    decide_effective_text, insert_embedding, memory_entry_subject_hash, model_space_ids_in_states,
    model_space_required_representation_ids, representation_key,
};

/// The crash point between the `cache.sqlite` write and the `state.sqlite`
/// write — the one place where the ADR-0010 write order is observable.
#[cfg(feature = "failpoints")]
pub const FAILPOINT_AFTER_VECTOR: &str = "memory.normalization.after_vector";

/// One entry to normalize: its id and the text as it stands right now.
#[derive(Debug, Clone, Copy)]
pub struct NormalizationTarget<'a> {
    pub memory_id: &'a str,
    pub text: &'a str,
}

/// What [`apply_normalization`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NormalizationOutcome {
    /// The detector found nothing to translate. A `skipped` row was written and
    /// `cache.sqlite` was **not touched at all**: the effective text is still
    /// the original, so its hash — and its existing vector — are unchanged.
    Skipped { class: ScriptClass },
    /// A variant was embedded and then committed.
    Normalized {
        /// The subject hash the vector was written under — the hash every
        /// reader will now compute for this entry.
        subject_hash: String,
        /// How many `embedding_cache` rows were written (one per memory
        /// representation that could take this vector).
        vectors_written: usize,
    },
    /// The entry's text changed while this translation was in flight, so the
    /// conditional write refused it. Nothing was committed to `state.sqlite`;
    /// the vector that was already written is simply unreferenced.
    TextMoved,
}

/// Why [`apply_normalization`] could not finish.
#[derive(Debug)]
#[non_exhaustive]
pub enum NormalizationError {
    /// The translation itself failed — classify it with
    /// `local_rag_memory::normalize::translate::classify_translate_failure`.
    Translate(TranslateError),
    /// Reading `state.sqlite` failed.
    StateRead(local_rag_store::OpenError),
    /// A `state.sqlite` query failed.
    Sqlite(local_rag_store::rusqlite::Error),
    /// Embedding the translated text failed.
    Embed(local_rag_embed::EmbedError),
    /// A `cache.sqlite` write failed.
    CacheWrite(local_rag_store::CacheWriteError),
    /// Opening `cache.sqlite` failed.
    CacheOpen(local_rag_store::CacheOpenError),
    /// A `state.sqlite` write failed.
    StateWrite(local_rag_store::WriteError),
    /// The store has memory representations, but not one of them could take
    /// the vector that was produced — so committing the row would declare the
    /// entry normalized with no vector under its new hash, which is the exact
    /// window this module exists to close.
    NoUsableRepresentation {
        registered: usize,
        vector_dimensions: usize,
    },
    /// The named crash point fired (test builds only).
    #[cfg(feature = "failpoints")]
    FailpointInjected,
}

impl std::fmt::Display for NormalizationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NormalizationError::Translate(e) => write!(f, "{e}"),
            NormalizationError::StateRead(e) => write!(f, "could not open state.sqlite: {e}"),
            NormalizationError::Sqlite(e) => write!(f, "state read failed: {e}"),
            NormalizationError::Embed(e) => write!(f, "embedding the translation failed: {e}"),
            NormalizationError::CacheWrite(e) => write!(f, "cache write failed: {e}"),
            NormalizationError::CacheOpen(e) => write!(f, "could not open cache.sqlite: {e}"),
            NormalizationError::StateWrite(e) => write!(f, "state write failed: {e}"),
            NormalizationError::NoUsableRepresentation {
                registered,
                vector_dimensions,
            } => write!(
                f,
                "none of the {registered} registered memory representation(s) accepts a \
                 {vector_dimensions}-dimensional vector — the embedder does not match the \
                 registry, so nothing may be declared normalized"
            ),
            #[cfg(feature = "failpoints")]
            NormalizationError::FailpointInjected => write!(f, "failpoint fired"),
        }
    }
}

impl std::error::Error for NormalizationError {}

/// Translate one entry and commit the result in the only safe order.
#[allow(clippy::too_many_arguments)]
pub async fn apply_normalization(
    state_db: &StateDb,
    cache: &CacheDb,
    generators: &local_rag_embed::GeneratorPool,
    embedders: &ProviderPool,
    policy: DataPolicy,
    target: NormalizationTarget<'_>,
    now_ms: i64,
) -> Result<NormalizationOutcome, NormalizationError> {
    let source_sha = sha256_hex(target.text.as_bytes());

    let translation = translate(
        generators,
        policy,
        TranslateRequest {
            memory_id: target.memory_id,
            text: target.text,
        },
    )
    .map_err(NormalizationError::Translate)?;

    let english = match translation {
        // Nothing to translate: record why, touch no vector. The entry keeps
        // the hash it already has, which is the hash its cached vector is
        // under — this path must never write to `cache.sqlite`.
        Translation::Passthrough { class } => {
            let outcome = write_row(
                state_db,
                RowDraft::passthrough(target.memory_id, target.text, class),
                now_ms,
            )
            .await?;
            return Ok(match outcome {
                UpsertOutcome::Written => NormalizationOutcome::Skipped { class },
                _ => NormalizationOutcome::TextMoved,
            });
        }
        Translation::Translated { english } => english,
    };

    // The hash every reader will compute once the row below lands — derived by
    // the *same* decision function they use (T21-02), never re-implemented
    // here.
    let effective = decide_effective_text(
        target.memory_id,
        target.text,
        Some(NormalizationView {
            status: Some(NormalizationStatus::Ready),
            source_text_sha256: &source_sha,
            normalized_text: Some(&english),
        }),
    );
    let subject_hash = memory_entry_subject_hash(&effective);

    // Step 2: the vector, under the new hash, before anything claims the entry
    // is normalized.
    let vectors_written = write_vectors(
        state_db,
        cache,
        embedders,
        policy,
        &subject_hash,
        &english,
        now_ms,
    )
    .await?;

    #[cfg(feature = "failpoints")]
    local_rag_test_support::fail_point!(
        FAILPOINT_AFTER_VECTOR,
        Err(NormalizationError::FailpointInjected)
    );

    // Step 3: the row. Its own guard re-reads the entry's text inside the
    // transaction, so an `edit` that landed while this translation was running
    // makes the write refuse rather than overwrite.
    let outcome = write_row(
        state_db,
        RowDraft::ready(target.memory_id, &source_sha, &english),
        now_ms,
    )
    .await?;

    Ok(match outcome {
        UpsertOutcome::Written => NormalizationOutcome::Normalized {
            subject_hash,
            vectors_written,
        },
        _ => NormalizationOutcome::TextMoved,
    })
}

/// Embed `english` and write it under `subject_hash` for every memory
/// representation this store still has.
///
/// The pool selects a provider by representation **kind**, not by
/// `representation_id` (see `embed_and_write` in `local_rag_embed::backfill`,
/// which does the same), so one embedding call serves the kind. A row is then
/// written for each memory representation whose declared `dimensions` match the
/// vector actually produced — a representation of a different width is provably
/// not describing this vector, and writing it there would poison the cache with
/// a row that `verify_cached_embedding` would later have to repair.
async fn write_vectors(
    state_db: &StateDb,
    cache: &CacheDb,
    embedders: &ProviderPool,
    policy: DataPolicy,
    subject_hash: &str,
    english: &str,
    now_ms: i64,
) -> Result<usize, NormalizationError> {
    let representation_ids = memory_representation_ids(state_db)?;
    let representation_count = representation_ids.len();
    if representation_ids.is_empty() {
        // No space registered the kind: there is no key to write under, and no
        // reader is looking for one either. Not an error.
        return Ok(0);
    }

    let mut vectors = embedders
        .embed(
            policy,
            EmbedRequest::new(RepresentationKind::Memory, vec![english.to_string()]),
        )
        .map_err(NormalizationError::Embed)?;
    let Some(vector) = vectors.pop() else {
        return Ok(0);
    };
    let dimensions = vector.dimensions() as i64;
    let values = vector.into_inner();

    let read = state_db
        .open_read()
        .map_err(NormalizationError::StateRead)?;
    let mut rows: Vec<EmbeddingKey> = Vec::new();
    for representation_id in representation_ids {
        let Some(key) =
            representation_key(&read, &representation_id).map_err(NormalizationError::Sqlite)?
        else {
            continue;
        };
        if i64::from(key.dimensions) != dimensions {
            continue;
        }
        rows.push(EmbeddingKey {
            subject_kind: SubjectKind::MemoryEntry,
            subject_hash: subject_hash.to_string(),
            representation_id,
        });
    }
    if rows.is_empty() {
        // There *are* representations, but none of them describes this vector.
        // The pool selects a provider by kind, so this means the configured
        // embedder and the registry disagree about width — writing the row now
        // would leave the entry normalized with no vector under its new hash,
        // and no reader would report it. Refuse instead.
        return Err(NormalizationError::NoUsableRepresentation {
            registered: representation_count,
            vector_dimensions: dimensions as usize,
        });
    }

    let written = rows.len();
    let batch = rows;
    cache
        .writer()
        .transaction(move |tx| {
            for key in &batch {
                insert_embedding(tx, key, dimensions, &values, now_ms)?;
            }
            Ok(())
        })
        .await
        .map_err(NormalizationError::CacheWrite)?;
    Ok(written)
}

/// Every memory `representation_id` registered by a model space that still
/// exists — a deleted space's rows are already evictable (spec 10 §4 step 6),
/// so writing for it would be work nothing will ever read.
fn memory_representation_ids(state_db: &StateDb) -> Result<Vec<String>, NormalizationError> {
    let read = state_db
        .open_read()
        .map_err(NormalizationError::StateRead)?;
    let spaces = model_space_ids_in_states(
        &read,
        &[
            ModelSpaceState::Active,
            ModelSpaceState::Building,
            ModelSpaceState::ProjectionReady,
            ModelSpaceState::Retiring,
        ],
    )
    .map_err(NormalizationError::Sqlite)?;

    let mut ids: Vec<String> = Vec::new();
    for space in spaces {
        let representations = model_space_required_representation_ids(&read, &space)
            .map_err(NormalizationError::Sqlite)?;
        for (kind, representation_id) in representations {
            if kind == RepresentationKind::Memory && !ids.contains(&representation_id) {
                ids.push(representation_id);
            }
        }
    }
    Ok(ids)
}

/// One normalization row, owned — the single description both the
/// single-entry path and the tick's passthrough batch write through, so the
/// two can never disagree about what a row looks like.
#[derive(Debug, Clone)]
pub struct RowDraft {
    memory_id: String,
    status: NormalizationStatus,
    source_text_sha256: String,
    normalized_text: Option<String>,
    source_language: Option<String>,
    attempt_count: i64,
    last_error: Option<String>,
    next_attempt_at: Option<i64>,
}

impl RowDraft {
    /// Nothing to translate: the detector's own answer, recorded.
    pub fn passthrough(memory_id: &str, text: &str, class: ScriptClass) -> Self {
        RowDraft {
            memory_id: memory_id.to_string(),
            status: NormalizationStatus::Skipped,
            source_text_sha256: sha256_hex(text.as_bytes()),
            normalized_text: None,
            source_language: Some(script_label(class).to_string()),
            attempt_count: 0,
            last_error: None,
            next_attempt_at: None,
        }
    }

    /// A validated English variant, for text whose hash is `source_sha`.
    pub fn ready(memory_id: &str, source_sha: &str, english: &str) -> Self {
        RowDraft {
            memory_id: memory_id.to_string(),
            status: NormalizationStatus::Ready,
            source_text_sha256: source_sha.to_string(),
            normalized_text: Some(english.to_string()),
            source_language: Some(script_label(ScriptClass::NonLatin).to_string()),
            attempt_count: 1,
            last_error: None,
            next_attempt_at: None,
        }
    }

    /// A recorded failure. `attempt_count`/`next_attempt_at` are the caller's
    /// retry bookkeeping — see the worker's own classification.
    pub fn failure(
        memory_id: &str,
        source_sha: &str,
        attempt_count: i64,
        last_error: &str,
        next_attempt_at: Option<i64>,
    ) -> Self {
        RowDraft {
            memory_id: memory_id.to_string(),
            status: NormalizationStatus::Failed,
            source_text_sha256: source_sha.to_string(),
            normalized_text: None,
            source_language: None,
            attempt_count,
            last_error: Some(last_error.to_string()),
            next_attempt_at,
        }
    }

    fn as_write(&self) -> NormalizationWrite<'_> {
        NormalizationWrite {
            memory_id: &self.memory_id,
            status: self.status,
            source_text_sha256: &self.source_text_sha256,
            normalized_text: self.normalized_text.as_deref(),
            source_language: self.source_language.as_deref(),
            normalizer_model_id: None,
            prompt_version: Some(TRANSLATOR_VERSION),
            normalizer_version: CURRENT_NORMALIZER_VERSION,
            attempt_count: self.attempt_count,
            last_error: self.last_error.as_deref(),
            next_attempt_at: self.next_attempt_at,
        }
    }
}

fn script_label(class: ScriptClass) -> &'static str {
    match class {
        ScriptClass::English => "latin",
        ScriptClass::NonLatin => "non-latin",
        ScriptClass::Undetermined => "undetermined",
    }
}

/// Commit `drafts` in **one** `state.sqlite` transaction, returning each row's
/// outcome in order.
///
/// The tick's passthrough batch is the reason this takes a slice: an
/// all-English store would otherwise pay one transaction per entry for work
/// that costs no inference at all. Each row still carries its own conditional
/// guard, so one entry whose text moved refuses on its own without touching
/// the rest.
pub async fn write_rows(
    state_db: &StateDb,
    drafts: Vec<RowDraft>,
    now_ms: i64,
) -> Result<Vec<UpsertOutcome>, NormalizationError> {
    if drafts.is_empty() {
        return Ok(Vec::new());
    }
    state_db
        .writer()
        .transaction(move |tx| {
            let mut outcomes = Vec::with_capacity(drafts.len());
            for draft in &drafts {
                outcomes.push(local_rag_store::upsert_normalization(
                    tx,
                    &draft.as_write(),
                    now_ms,
                )?);
            }
            Ok(outcomes)
        })
        .await
        .map_err(NormalizationError::StateWrite)
}

/// Commit exactly one row — the single-entry path's thin wrapper over
/// [`write_rows`].
async fn write_row(
    state_db: &StateDb,
    draft: RowDraft,
    now_ms: i64,
) -> Result<UpsertOutcome, NormalizationError> {
    Ok(write_rows(state_db, vec![draft], now_ms)
        .await?
        .pop()
        .expect("one draft in, one outcome out"))
}
