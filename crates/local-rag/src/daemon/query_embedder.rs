//! Adapts a `local_rag_embed::Embedder` (indexing-time, batch) into either
//! search's or memory's single-query `QueryEmbedder` trait — the "real
//! provider" `daemon::search::build_search_engine`'s own doc comment already
//! named as T15-07's job, replacing `local_rag_search::UnavailableEmbedder`,
//! and (D-036) `local_rag_memory::recall::UnavailableEmbedder`'s memory-side
//! counterpart.
//!
//! Two adapters, not one generic over both traits: `local_rag_search::
//! QueryEmbedder` and `local_rag_memory::recall::QueryEmbedder` are
//! structurally identical but nominally distinct crate-local traits (`recall`
//! stays independent of `crates/search` for a 15-line trait — see
//! `local_rag_memory::recall::dense`'s own module doc), so [`EmbedderQueryAdapter`]
//! and [`MemoryEmbedderQueryAdapter`] each implement one. Both wrap an
//! `Embedder` opened under the matching `RepresentationKind` (`OnnxEmbedder::
//! open`/`open_for_memory`, D-036) — one physical model, two sessions, one
//! per `kind`, since `Embedder::key()` is a single-key contract.

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

/// [`EmbedderQueryAdapter`]'s memory-side twin (D-036): implements
/// `local_rag_memory::recall::QueryEmbedder` instead of
/// `local_rag_search::QueryEmbedder` — same key-match-or-refuse contract,
/// different (nominally distinct) trait/error type.
pub struct MemoryEmbedderQueryAdapter<E> {
    embedder: E,
}

impl<E: Embedder> MemoryEmbedderQueryAdapter<E> {
    pub fn new(embedder: E) -> Self {
        Self { embedder }
    }
}

impl<E: Embedder> local_rag_memory::recall::QueryEmbedder for MemoryEmbedderQueryAdapter<E> {
    fn embed_query(
        &self,
        query: &str,
        key: &RepresentationKey,
    ) -> Result<Vec<f32>, local_rag_memory::recall::QueryEmbedError> {
        let own_key = self.embedder.key();
        if &own_key != key {
            return Err(local_rag_memory::recall::QueryEmbedError {
                reason: format!(
                    "provider's own representation key ({own_key:?}) does not match the requested key ({key:?})"
                ),
            });
        }
        let mut vectors = self
            .embedder
            .embed(EmbedRequest::new(key.kind, vec![query.to_string()]))
            .map_err(|e| local_rag_memory::recall::QueryEmbedError {
                reason: e.to_string(),
            })?;
        let vector = vectors
            .pop()
            .ok_or_else(|| local_rag_memory::recall::QueryEmbedError {
                reason: "embedder returned no vector for the query".to_string(),
            })?;
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

    fn memory_key(model_id: &str) -> RepresentationKey {
        RepresentationKey {
            kind: RepresentationKind::Memory,
            representation_version: 1,
            normalization_version: 1,
            model_id: model_id.to_string(),
            dimensions: 3,
            distance_metric: DistanceMetric::Cosine,
        }
    }

    #[test]
    fn memory_adapter_embeds_the_query_when_the_key_matches() {
        use local_rag_memory::recall::QueryEmbedder as MemoryQueryEmbedder;

        let adapter = MemoryEmbedderQueryAdapter::new(FixedEmbedder {
            key: memory_key("test-model"),
            vector: vec![0.0, 1.0, 0.0],
        });
        let out = MemoryQueryEmbedder::embed_query(&adapter, "hello", &memory_key("test-model"))
            .expect("matching key succeeds");
        assert_eq!(out, vec![0.0, 1.0, 0.0]);
    }

    #[test]
    fn memory_adapter_refuses_a_mismatched_key_rather_than_silently_answering() {
        use local_rag_memory::recall::QueryEmbedder as MemoryQueryEmbedder;

        let adapter = MemoryEmbedderQueryAdapter::new(FixedEmbedder {
            key: memory_key("test-model"),
            vector: vec![0.0, 1.0, 0.0],
        });
        let err =
            MemoryQueryEmbedder::embed_query(&adapter, "hello", &memory_key("some-other-model"))
                .expect_err("mismatched key must refuse");
        assert!(err.reason.contains("does not match"), "{}", err.reason);
    }
}
