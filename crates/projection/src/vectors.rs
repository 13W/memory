//! The production [`VectorSource`]: `embedding_cache`-backed vector lookup — T11-05.
//!
//! Spec 05 §5 step 1 `[FIXED]`: *"vectors come from `embedding_cache`; unchanged
//! content is NOT re-embedded"*. T07-03 shipped the [`VectorSource`] seam for
//! exactly this and left the implementation to the task that needs a real
//! multi-model-space switch — this one.
//!
//! # Why the lookup needs two hops
//!
//! [`VectorSource::vector`] is keyed by `(occurrence_id, representation_kind)` —
//! the projection's vocabulary — while `embedding_cache` is keyed by
//! `(subject_kind, subject_hash, representation_id)` — the cache's. The bridge is
//! per kind, and only `code_raw` has one today:
//!
//! ```text
//! occurrence_id ──parsed_unit.blob_id──▶ blob_id ──H(subject/content_blob)──▶ subject_hash
//! representation_kind ──model_space_representation──▶ representation_id
//! ```
//!
//! `code_context` has one too (D-016) — its hash is over the rendered envelope,
//! so the bridge is `occurrence_id ──serialize(envelope)──▶ subject_hash` and no
//! two paths share a subject. `memory`'s tables arrive in group 14 — the same
//! boundary `local_rag_store::subjects` draws for the backfill worker and the
//! eviction pin rule, so all three agree on which subjects can exist at all.
//!
//! # Missing is `None`, never a guess
//!
//! A cache row that fails [`verify_cached_embedding`] is treated exactly like an
//! absent one. The callers turn that into `SwitchError::MissingVector` /
//! `RebuildError::MissingVector`, which is spec 05 §7's coverage guard: *"the
//! shard never goes `clean` with a partial expected set"*. Repairing such a row
//! is the backfill worker's job (T11-04), not this reader's — this type has no
//! "compute/embed" path by construction, matching the seam's own contract.
//!
//! # Cost
//!
//! The `occurrence_id → blob_id` map is loaded once per generation (one indexed
//! scan through `local_rag_store::content_blob_ids_for_generation`) and cached,
//! because a switch asks for every occurrence of the generation in a row. The
//! `code_context` map is cached the same way and for a stronger reason: building
//! it decompresses every source revision of the generation, which must happen
//! once per source, not once per point. Cache reads stay per-call: they are point
//! lookups on a `WITHOUT ROWID` primary key, and holding a long-lived cache
//! connection inside a `VectorSource` would outlive the read snapshot the caller
//! opened.

use std::collections::HashMap;
use std::sync::Mutex;

use local_rag_core::identity::Uuid;
use local_rag_core::identity::domain::subject_content_blob;
use local_rag_store::{
    CacheDb, EmbeddingKey, StateDb, SubjectKind, content_blob_ids_for_generation,
    context_subjects_for_generation, decode_vector_le, get_embedding,
    model_space_required_representation_ids, verify_cached_embedding,
};

use crate::contract::RepresentationKind;
use crate::switch::VectorSource;

/// A [`VectorSource`] that reads committed vectors out of `cache.sqlite`.
///
/// Scoped to one `(generation, model_space)` tuple — the same tuple the switch or
/// rebuild it feeds is targeting — so the representation ids and the occurrence
/// map are resolved once instead of per point.
pub struct CacheVectorSource<'a> {
    cache: &'a CacheDb,
    /// `representation_kind → representation_id` for the tuple's model space.
    representations: HashMap<RepresentationKind, String>,
    /// `occurrence_id → blob_id`, loaded lazily on first use.
    blobs: Mutex<Option<HashMap<String, String>>>,
    /// `occurrence_id → context subject_hash`, loaded lazily on first use.
    contexts: Mutex<Option<HashMap<String, String>>>,
    state: &'a StateDb,
    generation_id: String,
}

impl<'a> CacheVectorSource<'a> {
    /// Build a source for `(generation_id, model_space_id)`.
    ///
    /// Reads the model space's **required** representations up front (spec 05 §4:
    /// the expected point set is defined over exactly those), so a kind the space
    /// does not require resolves to `None` rather than to some other space's
    /// vectors.
    pub fn new(
        state: &'a StateDb,
        cache: &'a CacheDb,
        state_read: &local_rag_store::rusqlite::Connection,
        generation_id: &Uuid,
        model_space_id: &Uuid,
    ) -> local_rag_store::rusqlite::Result<Self> {
        let representations =
            model_space_required_representation_ids(state_read, &model_space_id.to_string())?
                .into_iter()
                .filter_map(|(kind, id)| store_kind_to_projection(kind).map(|k| (k, id)))
                .collect();
        Ok(CacheVectorSource {
            cache,
            representations,
            blobs: Mutex::new(None),
            contexts: Mutex::new(None),
            state,
            generation_id: generation_id.to_string(),
        })
    }

    /// The content blob an occurrence's `code_raw` subject hashes from.
    fn blob_id(&self, occurrence_id: &str) -> Option<String> {
        let mut guard = self.blobs.lock().expect("vector-source blob map poisoned");
        if guard.is_none() {
            let read = self.state.open_read().ok()?;
            let rows = content_blob_ids_for_generation(&read, &self.generation_id).ok()?;
            *guard = Some(rows.into_iter().collect());
        }
        guard
            .as_ref()
            .and_then(|map| map.get(occurrence_id))
            .cloned()
    }

    /// The `code_context` subject hash of an occurrence (D-016).
    ///
    /// Recomputed from `state.sqlite` rather than read back from a table: the
    /// envelope is a *representation*, not stored content, so the hash exists
    /// only as a function of the generation's rows. Recomputing it here is also
    /// what makes a stale cache row detectable — it simply fails to match.
    fn context_hash(&self, occurrence_id: &str) -> Option<String> {
        let mut guard = self
            .contexts
            .lock()
            .expect("vector-source context map poisoned");
        if guard.is_none() {
            let read = self.state.open_read().ok()?;
            let rows = context_subjects_for_generation(&read, &self.generation_id).ok()?;
            *guard = Some(
                rows.into_iter()
                    .map(|s| (s.occurrence_id, s.subject_hash))
                    .collect(),
            );
        }
        guard
            .as_ref()
            .and_then(|map| map.get(occurrence_id))
            .cloned()
    }
}

impl VectorSource for CacheVectorSource<'_> {
    fn vector(&self, occurrence_id: &str, kind: RepresentationKind) -> Option<Vec<f32>> {
        let representation_id = self.representations.get(&kind)?;
        // Memory and structural-description subjects are not computable yet (see
        // the module doc).
        let (subject_kind, subject_hash) = match kind {
            RepresentationKind::CodeRaw => (
                SubjectKind::ContentBlob,
                subject_content_blob(&self.blob_id(occurrence_id)?),
            ),
            RepresentationKind::CodeContext => (
                SubjectKind::OccurrenceContext,
                self.context_hash(occurrence_id)?,
            ),
            RepresentationKind::StructuralDescription | RepresentationKind::Memory => return None,
        };

        let key = EmbeddingKey {
            subject_kind,
            subject_hash,
            representation_id: representation_id.clone(),
        };
        let read = self.cache.open_read().ok()?;
        let row = get_embedding(&read, &key).ok()??;
        // A corrupt row is "missing", not "usable": the caller's MissingVector is
        // the coverage guard, and the backfill worker owns the repair.
        verify_cached_embedding(&row).ok()?;
        decode_vector_le(&row.vector_f32).ok()
    }
}

/// Map the store's representation-kind enum onto the projection's own.
///
/// The two are structurally identical but deliberately distinct Rust types —
/// `local-rag-store` has no dependency on `local-rag-projection` (T11-01's note in
/// spec 05 §5) — so every crossing is explicit.
pub(crate) fn store_kind_to_projection(
    kind: local_rag_store::RepresentationKind,
) -> Option<RepresentationKind> {
    match kind {
        local_rag_store::RepresentationKind::CodeRaw => Some(RepresentationKind::CodeRaw),
        local_rag_store::RepresentationKind::CodeContext => Some(RepresentationKind::CodeContext),
        local_rag_store::RepresentationKind::StructuralDescription => {
            Some(RepresentationKind::StructuralDescription)
        }
        local_rag_store::RepresentationKind::Memory => Some(RepresentationKind::Memory),
    }
}

/// The inverse of [`store_kind_to_projection`] — total, since every projection
/// kind exists in the store's enum.
///
/// Needed by any caller that starts from the projection's vocabulary (a shard's
/// point kinds) and has to ask the registry about it — the search pipeline's
/// dense leg, which resolves the searched kind's `RepresentationKey` (D-016).
pub fn projection_kind_to_store(kind: RepresentationKind) -> local_rag_store::RepresentationKind {
    match kind {
        RepresentationKind::CodeRaw => local_rag_store::RepresentationKind::CodeRaw,
        RepresentationKind::CodeContext => local_rag_store::RepresentationKind::CodeContext,
        RepresentationKind::StructuralDescription => {
            local_rag_store::RepresentationKind::StructuralDescription
        }
        RepresentationKind::Memory => local_rag_store::RepresentationKind::Memory,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_mapping_is_total_and_token_preserving() {
        for (store, projection) in [
            (
                local_rag_store::RepresentationKind::CodeRaw,
                RepresentationKind::CodeRaw,
            ),
            (
                local_rag_store::RepresentationKind::CodeContext,
                RepresentationKind::CodeContext,
            ),
            (
                local_rag_store::RepresentationKind::StructuralDescription,
                RepresentationKind::StructuralDescription,
            ),
            (
                local_rag_store::RepresentationKind::Memory,
                RepresentationKind::Memory,
            ),
        ] {
            assert_eq!(store_kind_to_projection(store), Some(projection));
            assert_eq!(projection_kind_to_store(projection), store);
            // The two enums must keep agreeing on the stored token, since both
            // sides key rows by it.
            assert_eq!(store.as_str(), projection.as_str());
        }
    }
}
