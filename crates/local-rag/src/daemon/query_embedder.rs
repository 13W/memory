//! Adapts a `local_rag_embed::Embedder` (indexing-time, batch) into
//! `local_rag_search::QueryEmbedder` (search-time, single-query) — the "real
//! provider" `daemon::search::build_search_engine`'s own doc comment already
//! named as T15-07's job, replacing `local_rag_search::UnavailableEmbedder`.
//!
//! Only `local_rag_search::QueryEmbedder` is adapted here, not
//! `local_rag_memory::recall::QueryEmbedder` (the structurally identical
//! sibling trait `recall`'s dense leg uses) — see this module's own doc on
//! [`EmbedderQueryAdapter`] for why: no `memory`-kind `RepresentationKey` has
//! ever been registered in production (D-013 assigns that specifically to
//! group 14, not T15-07, and group 14 closed without doing it — confirmed by
//! grep, every `set_model_space_representation(.., RepresentationKind::
//! Memory, ..)` call site in this workspace is a test or `xtask bench`).
//! Wiring a real provider for a key nothing has ever registered would mean
//! inventing that key's fields here — a real risk of silently mismatched
//! vectors, which is worse than `recall`'s current honest, visible
//! degradation. This gap is called out as-built rather than silently worked
//! around; closing it is out of this task's own scope.

use local_rag_embed::{EmbedRequest, Embedder};
use local_rag_search::{QueryEmbedError, QueryEmbedder};
use local_rag_store::RepresentationKey;

/// Wraps any [`Embedder`] as a [`QueryEmbedder`]: one-text batch, unwrapped
/// back to a single vector. Refuses (rather than silently answering under
/// the wrong model) whenever the caller's requested `key` does not exactly
/// match the wrapped provider's own [`Embedder::key`] — the same "MUST honor
/// `key`" contract `QueryEmbedder`'s own trait doc already states.
pub struct EmbedderQueryAdapter<E> {
    embedder: E,
}

impl<E: Embedder> EmbedderQueryAdapter<E> {
    pub fn new(embedder: E) -> Self {
        Self { embedder }
    }
}

impl<E: Embedder> QueryEmbedder for EmbedderQueryAdapter<E> {
    fn embed_query(
        &self,
        query: &str,
        key: &RepresentationKey,
    ) -> Result<Vec<f32>, QueryEmbedError> {
        let own_key = self.embedder.key();
        if &own_key != key {
            return Err(QueryEmbedError::new(format!(
                "provider's own representation key ({own_key:?}) does not match the requested key ({key:?})"
            )));
        }
        let mut vectors = self
            .embedder
            .embed(EmbedRequest::new(key.kind, vec![query.to_string()]))
            .map_err(|e| QueryEmbedError::new(e.to_string()))?;
        let vector = vectors
            .pop()
            .ok_or_else(|| QueryEmbedError::new("embedder returned no vector for the query"))?;
        Ok(vector.as_slice().to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use local_rag_embed::EmbedError;
    use local_rag_store::{DistanceMetric, RepresentationKind};

    struct FixedEmbedder {
        key: RepresentationKey,
        vector: Vec<f32>,
    }

    impl Embedder for FixedEmbedder {
        fn embed(&self, req: EmbedRequest) -> Result<Vec<local_rag_embed::Vector>, EmbedError> {
            Ok(req
                .texts
                .iter()
                .map(|_| local_rag_embed::Vector::new(self.vector.clone()))
                .collect())
        }

        fn key(&self) -> RepresentationKey {
            self.key.clone()
        }
    }

    fn key(model_id: &str) -> RepresentationKey {
        RepresentationKey {
            kind: RepresentationKind::CodeRaw,
            representation_version: 1,
            normalization_version: 1,
            model_id: model_id.to_string(),
            dimensions: 3,
            distance_metric: DistanceMetric::Cosine,
        }
    }

    #[test]
    fn embeds_the_query_when_the_key_matches() {
        let adapter = EmbedderQueryAdapter::new(FixedEmbedder {
            key: key("test-model"),
            vector: vec![1.0, 0.0, 0.0],
        });
        let out = adapter
            .embed_query("hello", &key("test-model"))
            .expect("matching key succeeds");
        assert_eq!(out, vec![1.0, 0.0, 0.0]);
    }

    #[test]
    fn refuses_a_mismatched_key_rather_than_silently_answering() {
        let adapter = EmbedderQueryAdapter::new(FixedEmbedder {
            key: key("test-model"),
            vector: vec![1.0, 0.0, 0.0],
        });
        let err = adapter
            .embed_query("hello", &key("some-other-model"))
            .expect_err("mismatched key must refuse");
        assert!(err.reason.contains("does not match"), "{}", err.reason);
    }
}
