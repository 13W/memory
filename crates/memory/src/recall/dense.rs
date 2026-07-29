//! The recall pipeline's dense (cosine) leg (spec 08 §6) — T14-08.
//!
//! Spec 08 §6 asks for "brute-force cosine over `embedding_cache` memory
//! vectors … bounded cardinality … behind the relevance-backend trait — ANN
//! replaces brute-force ONLY on cardinality/latency metrics, not by default".
//! Two traits realize that:
//!
//! - [`QueryEmbedder`] — turn the recall query text into a vector, mirroring
//!   `local_rag_search::pipeline::QueryEmbedder`'s exact seam (inject a trait
//!   object, default to [`UnavailableEmbedder`], real impl wired at the
//!   daemon composition point only). A **new, independent** trait rather than
//!   a shared one: `crates/memory` depending on `crates/search` for a
//!   15-line trait would be backwards, and the two seams embed under
//!   different `RepresentationKind`s (`code_raw` vs. `memory`) — nothing
//!   about them should ever be the same object.
//! - [`MemoryDenseBackend`] — score a query vector against a bounded, already
//!   fetched candidate set. [`BruteForceCosine`] is the only impl this task
//!   ships, calling the exact same free function the production dense
//!   backend does — [`local_rag_projection::contract::similarity`] — so a
//!   future ANN swap only replaces this one trait impl, never the pipeline
//!   around it.
//!
//! # Why not `BruteForceProjectionStore`
//!
//! The production dense backend (`local_rag_projection::brute_force`,
//! ADR-0003) is disk-shard-shaped: `open()` takes a directory, every mutation
//! rewrites `points.bin`, and a [`local_rag_projection::contract::
//! ProjectionHead`] proves generation/model-space consistency on open. None
//! of that fits a transient, scope-unioned scan over whatever
//! `embedding_cache` rows a recall request's candidate set happens to touch —
//! there is no shard, no generation, and no persisted head to validate. The
//! *math* the shard calls (`similarity`) is exactly what "brute-force cosine"
//! means, so this module calls it directly against vectors read straight out
//! of `embedding_cache`.

use std::collections::BTreeMap;

use local_rag_core::identity::domain::subject_memory_entry;
use local_rag_projection::contract::{DistanceMetric, similarity};
use local_rag_store::rusqlite::Connection;
use local_rag_store::{
    RecallCandidate, RepresentationKey, SubjectKind, decode_vector_le, embeddings_for_subject_kind,
    verify_cached_embedding,
};

/// Why the dense leg produced no hits at all — a degradation, never an error
/// (spec 09 §3's "degradation, never an error" idiom, generalized to memory
/// recall: a store with no memory representation registered yet, or no
/// provider wired, must still serve the lexical leg).
#[derive(Debug, Clone, PartialEq)]
pub enum DenseLegUnavailable {
    /// No `memory`-kind representation is registered for the resolved model
    /// space.
    NoRepresentation,
    /// [`QueryEmbedder::embed_query`] refused.
    EmbedFailed(QueryEmbedError),
    /// The embedder returned a vector whose length disagrees with the
    /// representation's declared `dimensions`.
    DimensionMismatch { expected: u32, got: usize },
}

/// One dense-leg hit: the entry, its 1-based rank, and the raw similarity
/// score (diagnostics only — [`super::fusion::rrf`] fuses on rank, never on
/// this value, exactly like the code-search dense leg).
#[derive(Debug, Clone, PartialEq)]
pub struct DenseRecallHit {
    pub memory_id: String,
    pub rank: usize,
    pub score: f32,
}

/// Turn recall query text into a vector under `key` (spec 08 §6's "same
/// `representation_id` as the active memory representation").
///
/// Mirrors `local_rag_search::pipeline::QueryEmbedder` field-for-field on
/// purpose (same seam shape, same reason: keep this crate's tests offline
/// and deterministic, provider selection and the `data_policy` guard belong
/// to the daemon, group 15).
pub trait QueryEmbedder: Send + Sync {
    fn embed_query(
        &self,
        query: &str,
        key: &RepresentationKey,
    ) -> Result<Vec<f32>, QueryEmbedError>;
}

/// Why [`QueryEmbedder::embed_query`] could not produce a vector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryEmbedError {
    pub reason: String,
}

/// The default [`QueryEmbedder`]: always refuses, with an explicit reason —
/// a store with no provider wired degrades visibly (dense leg unavailable,
/// lexical leg still serves) rather than silently returning a meaningless
/// vector.
#[derive(Debug, Clone, Copy, Default)]
pub struct UnavailableEmbedder;

impl QueryEmbedder for UnavailableEmbedder {
    fn embed_query(
        &self,
        _query: &str,
        _key: &RepresentationKey,
    ) -> Result<Vec<f32>, QueryEmbedError> {
        Err(QueryEmbedError {
            reason: "no query embedder wired".to_string(),
        })
    }
}

/// Score a query vector against a bounded candidate set (spec 08 §6's
/// "relevance-backend trait").
pub trait MemoryDenseBackend: Send + Sync {
    /// `candidates` is `(memory_id, vector)`, already bounded by the
    /// pipeline's cardinality guard. Returns hits ranked best-first —
    /// 1-based `rank` is the caller's responsibility to assign from this
    /// order (kept out of the trait so a future ANN backend need not know
    /// about ranking, only ordering).
    fn search(
        &self,
        metric: DistanceMetric,
        query: &[f32],
        candidates: &[(String, Vec<f32>)],
    ) -> Vec<(String, f32)>;
}

/// The v0 default: a linear scan calling the exact function the production
/// dense backend calls (see the module doc's "why not
/// `BruteForceProjectionStore`").
#[derive(Debug, Clone, Copy, Default)]
pub struct BruteForceCosine;

impl MemoryDenseBackend for BruteForceCosine {
    fn search(
        &self,
        metric: DistanceMetric,
        query: &[f32],
        candidates: &[(String, Vec<f32>)],
    ) -> Vec<(String, f32)> {
        let mut scored: Vec<(String, f32)> = candidates
            .iter()
            .map(|(memory_id, vector)| (memory_id.clone(), similarity(metric, query, vector)))
            .collect();
        // `(score desc, memory_id asc)` — the same tie-break idiom
        // `local_rag_projection::contract::rank_scored` uses, applied here
        // directly since this leg's candidates are keyed by `memory_id`, not
        // `PointId`.
        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        scored
    }
}

/// The dense leg, end to end: embed the query, bulk-read cached vectors for
/// `candidates`, score, rank. Every failure degrades
/// ([`DenseLegUnavailable`]) — never an `Err` a caller must decide whether to
/// propagate, matching `local_rag_search`'s own dense-leg convention.
///
/// `cache_read` is a `cache.sqlite` read connection; `candidates` is the
/// pipeline's already scope-unioned, cardinality-guarded set. A candidate
/// with no cached vector (never embedded yet, or a corrupt row) simply does
/// not appear in the output — this leg degrades per-entry, not as a whole,
/// exactly like a code occurrence with no dense point is just absent from
/// that leg's results.
#[allow(clippy::too_many_arguments)]
pub fn dense_leg(
    cache_read: &Connection,
    query: &str,
    representation: &RepresentationKey,
    representation_id: &str,
    embedder: &dyn QueryEmbedder,
    backend: &dyn MemoryDenseBackend,
    candidates: &[RecallCandidate],
    limit: usize,
) -> Result<Vec<DenseRecallHit>, DenseLegUnavailable> {
    if query.is_empty() || candidates.is_empty() {
        // A termless query or an empty candidate set is healthy, not a
        // failure — nothing to embed, no provider called, the leg is simply
        // empty (mirrors spec 09 §3's identical treatment of a termless
        // lexical/dense query).
        return Ok(Vec::new());
    }

    let vector = embedder
        .embed_query(query, representation)
        .map_err(DenseLegUnavailable::EmbedFailed)?;
    if vector.len() != representation.dimensions as usize {
        return Err(DenseLegUnavailable::DimensionMismatch {
            expected: representation.dimensions,
            got: vector.len(),
        });
    }

    let cap = i64::try_from(candidates.len()).unwrap_or(i64::MAX);
    let cached =
        embeddings_for_subject_kind(cache_read, SubjectKind::MemoryEntry, representation_id, cap)
            .map_err(|_| DenseLegUnavailable::NoRepresentation)?;
    let by_subject_hash: BTreeMap<&str, &local_rag_store::EmbeddingCacheRow> = cached
        .iter()
        .filter(|entry| verify_cached_embedding(&entry.row).is_ok())
        .map(|entry| (entry.subject_hash.as_str(), &entry.row))
        .collect();

    let scoreable: Vec<(String, Vec<f32>)> = candidates
        .iter()
        .filter_map(|c| {
            let hash = subject_memory_entry(&c.memory_id, &c.text);
            let row = by_subject_hash.get(hash.as_str())?;
            let decoded = decode_vector_le(&row.vector_f32).ok()?;
            Some((c.memory_id.clone(), decoded))
        })
        .collect();

    let ranked = backend.search(representation.distance_metric, &vector, &scoreable);
    Ok(ranked
        .into_iter()
        .take(limit)
        .enumerate()
        .map(|(i, (memory_id, score))| DenseRecallHit {
            memory_id,
            rank: i + 1,
            score,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(memory_id: &str, text: &str) -> RecallCandidate {
        RecallCandidate {
            memory_id: memory_id.to_string(),
            kind: local_rag_store::MemoryKind::Fact,
            state: local_rag_store::MemoryState::Active,
            text: text.to_string(),
            confidence: 0.5,
            created_at: 1000,
        }
    }

    #[test]
    fn brute_force_cosine_ranks_closer_vectors_first() {
        let backend = BruteForceCosine;
        let candidates = vec![
            ("far".to_string(), vec![0.0, 1.0]),
            ("close".to_string(), vec![1.0, 0.0]),
        ];
        let ranked = backend.search(DistanceMetric::Cosine, &[1.0, 0.0], &candidates);
        assert_eq!(ranked[0].0, "close");
        assert_eq!(ranked[1].0, "far");
    }

    #[test]
    fn brute_force_cosine_breaks_ties_by_memory_id() {
        let backend = BruteForceCosine;
        let candidates = vec![
            ("b".to_string(), vec![1.0, 0.0]),
            ("a".to_string(), vec![1.0, 0.0]),
        ];
        let ranked = backend.search(DistanceMetric::Cosine, &[1.0, 0.0], &candidates);
        assert_eq!(
            ranked.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
    }

    #[test]
    fn unavailable_embedder_always_refuses() {
        let key = RepresentationKey {
            kind: local_rag_store::RepresentationKind::Memory,
            representation_version: 1,
            normalization_version: 1,
            model_id: "m".to_string(),
            dimensions: 3,
            distance_metric: DistanceMetric::Cosine,
        };
        assert!(UnavailableEmbedder.embed_query("q", &key).is_err());
    }

    #[test]
    fn an_empty_query_skips_the_leg_without_calling_the_embedder() {
        struct Panicking;
        impl QueryEmbedder for Panicking {
            fn embed_query(
                &self,
                _q: &str,
                _k: &RepresentationKey,
            ) -> Result<Vec<f32>, QueryEmbedError> {
                panic!("must not be called for an empty query");
            }
        }

        let home = local_rag_test_support::TempHome::new().expect("home");
        let layout = local_rag_core::paths::StoreLayout::new(home.join("local-rag"));
        layout.ensure().expect("ensure");
        let cache = local_rag_store::CacheDb::open(layout.cache_db(), "store-uuid").expect("cache");
        let read = cache.open_read().expect("read");

        let key = RepresentationKey {
            kind: local_rag_store::RepresentationKind::Memory,
            representation_version: 1,
            normalization_version: 1,
            model_id: "m".to_string(),
            dimensions: 3,
            distance_metric: DistanceMetric::Cosine,
        };
        let hits = dense_leg(
            &read,
            "",
            &key,
            "repr-1",
            &Panicking,
            &BruteForceCosine,
            &[candidate("m1", "text")],
            10,
        )
        .expect("empty query is healthy, not an error");
        assert!(hits.is_empty());
    }
}
