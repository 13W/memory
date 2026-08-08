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
//!
//! [`code_query_embedder`]/[`memory_query_embedder`] are the daemon's own
//! production constructors for the two `StartOptions` fields, deferred behind
//! [`LazyQueryEmbedder`] so a model installed *after* the daemon started is
//! picked up without a restart (D-037).

use std::sync::{Arc, RwLock};

use local_rag_core::paths::StoreLayout;
use local_rag_embed::{EmbedRequest, Embedder};
use local_rag_memory::recall::UnavailableEmbedder as UnavailableMemoryEmbedder;
use local_rag_models::{DEFAULT_MODEL_ID, OnnxEmbedder, find, is_installed};
use local_rag_search::{QueryEmbedError, QueryEmbedder, UnavailableEmbedder};
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

/// What probing the store's on-disk model state found.
pub enum ProviderProbe<T: ?Sized> {
    /// No usable model on disk yet. Re-probed on the next query that needs a
    /// vector: `local-rag init --download-models` may still be running, or may
    /// not have been run at all when this daemon started.
    NotInstalled,
    /// A model is installed but the provider would not open (corrupt install,
    /// no ONNX Runtime on `PATH`/`ORT_DYLIB_PATH`, unregistered
    /// representation). Terminal for this process: reopening an ONNX session
    /// on every query is not a price a hot path can pay, and none of those
    /// causes clears itself.
    Unusable,
    /// A live provider.
    Ready(Arc<T>),
}

/// A `QueryEmbedder` that opens its real backend on the first query that
/// needs one, and keeps re-probing the store until one exists (D-037).
///
/// Both `search_code`'s and `recall`'s dense legs used to be resolved exactly
/// once, while `serve` was assembling `StartOptions`. A daemon started before
/// `local-rag init --download-models` therefore stayed `lexical_only` (and
/// `mode: "code"` stayed `INDEX_UNAVAILABLE`) for its whole lifetime, however
/// long after the model actually landed — only `local-rag restart` fixed it.
///
/// The probe runs on the dense path only, so nothing changed for a lexical
/// query, and it costs one `stat` of the model's `.ok` marker while no model
/// is installed — the same signal `local-rag init` writes and
/// `local_rag_embed::require_model_assets` reads. Once a provider is open, or
/// once one has proven unopenable, the fast path is a read lock and an `Arc`
/// clone: there is no background poll and no timer to configure.
pub struct LazyQueryEmbedder<T: ?Sized> {
    state: RwLock<ProviderProbe<T>>,
    unavailable: Arc<T>,
    probe: Box<dyn Fn() -> ProviderProbe<T> + Send + Sync>,
}

impl<T: ?Sized> LazyQueryEmbedder<T> {
    /// Wrap `probe` — called until it answers with something other than
    /// [`ProviderProbe::NotInstalled`] — falling back to `unavailable` (the
    /// relevant crate's `UnavailableEmbedder`) until then.
    pub fn new(
        unavailable: Arc<T>,
        probe: impl Fn() -> ProviderProbe<T> + Send + Sync + 'static,
    ) -> Self {
        Self {
            state: RwLock::new(ProviderProbe::NotInstalled),
            unavailable,
            probe: Box::new(probe),
        }
    }

    fn provider(&self) -> Arc<T> {
        {
            let state = self.state.read().expect("query-embedder state lock");
            match &*state {
                ProviderProbe::Ready(provider) => return Arc::clone(provider),
                ProviderProbe::Unusable => return Arc::clone(&self.unavailable),
                ProviderProbe::NotInstalled => {}
            }
        }
        let mut state = self.state.write().expect("query-embedder state lock");
        if matches!(*state, ProviderProbe::NotInstalled) {
            *state = (self.probe)();
        }
        match &*state {
            ProviderProbe::Ready(provider) => Arc::clone(provider),
            _ => Arc::clone(&self.unavailable),
        }
    }
}

impl QueryEmbedder for LazyQueryEmbedder<dyn QueryEmbedder> {
    fn embed_query(
        &self,
        query: &str,
        key: &RepresentationKey,
    ) -> Result<Vec<f32>, QueryEmbedError> {
        self.provider().embed_query(query, key)
    }
}

impl local_rag_memory::recall::QueryEmbedder
    for LazyQueryEmbedder<dyn local_rag_memory::recall::QueryEmbedder>
{
    fn embed_query(
        &self,
        query: &str,
        key: &RepresentationKey,
    ) -> Result<Vec<f32>, local_rag_memory::recall::QueryEmbedError> {
        self.provider().embed_query(query, key)
    }
}

/// `search_code`'s dense-leg provider, gated on the same signal `cli::init`
/// uses — the default model's `.ok` marker — not on any flag or config
/// toggle, and re-checked per query until it appears (D-037).
///
/// A missing model, or one that fails to open, degrades to
/// [`UnavailableEmbedder`] rather than failing daemon startup: `search_code`
/// already has a tested, spec-correct `lexical_only` fallback for exactly this
/// case (`daemon::search::build_search_engine`'s own doc), and a store the
/// operator has not run `local-rag init --download-models` against yet must
/// still serve lexical search.
pub fn code_query_embedder(layout: &StoreLayout) -> Arc<dyn QueryEmbedder> {
    let layout = layout.clone();
    let unavailable: Arc<dyn QueryEmbedder> = Arc::new(UnavailableEmbedder);
    Arc::new(LazyQueryEmbedder::new(unavailable, move || {
        let Some(entry) = find(DEFAULT_MODEL_ID) else {
            return ProviderProbe::Unusable;
        };
        if !is_installed(&layout, entry.model_id) {
            return ProviderProbe::NotInstalled;
        }
        match OnnxEmbedder::open(&layout, entry) {
            Ok(embedder) => ProviderProbe::Ready(Arc::new(EmbedderQueryAdapter::new(embedder))),
            Err(e) => {
                tracing::warn!(
                    "local-rag: {} is installed but could not be opened ({e}); \
                     search_code will stay lexical_only until this is fixed",
                    entry.model_id
                );
                ProviderProbe::Unusable
            }
        }
    }))
}

/// `recall`'s dense-leg provider (D-036) — the same disk-state gate and
/// fail-open-to-degraded shape as [`code_query_embedder`], but opened under
/// the `memory` representation key (`OnnxEmbedder::open_for_memory`) and
/// wrapped in [`MemoryEmbedderQueryAdapter`] instead. A missing/uninstalled
/// model, or one `local-rag init` never ran against, degrades to
/// [`UnavailableMemoryEmbedder`] — `recall`'s own tested `dense_degraded:
/// NoRepresentation`/`EmbedFailed` fallback, not a daemon startup failure.
pub fn memory_query_embedder(
    layout: &StoreLayout,
) -> Arc<dyn local_rag_memory::recall::QueryEmbedder> {
    let layout = layout.clone();
    let unavailable: Arc<dyn local_rag_memory::recall::QueryEmbedder> =
        Arc::new(UnavailableMemoryEmbedder);
    Arc::new(LazyQueryEmbedder::new(unavailable, move || {
        let Some(entry) = find(DEFAULT_MODEL_ID) else {
            return ProviderProbe::Unusable;
        };
        if !is_installed(&layout, entry.model_id) {
            return ProviderProbe::NotInstalled;
        }
        match OnnxEmbedder::open_for_memory(&layout, entry) {
            Ok(embedder) => {
                ProviderProbe::Ready(Arc::new(MemoryEmbedderQueryAdapter::new(embedder)))
            }
            Err(e) => {
                tracing::warn!(
                    "local-rag: {} is installed but could not be opened for its memory \
                     representation ({e}); recall will stay dense-degraded until this is fixed",
                    entry.model_id
                );
                ProviderProbe::Unusable
            }
        }
    }))
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

    // -----------------------------------------------------------------
    // D-037: a model installed after the daemon started must be picked up
    // without a restart.
    // -----------------------------------------------------------------

    /// A `QueryEmbedder` answering a fixed vector regardless of `key` —
    /// enough to stand in for "a real provider opened", with no ONNX
    /// Runtime, no weights and no inference involved.
    struct AlwaysEmbeds;

    impl QueryEmbedder for AlwaysEmbeds {
        fn embed_query(
            &self,
            _query: &str,
            _key: &RepresentationKey,
        ) -> Result<Vec<f32>, QueryEmbedError> {
            Ok(vec![1.0, 0.0, 0.0])
        }
    }

    impl local_rag_memory::recall::QueryEmbedder for AlwaysEmbeds {
        fn embed_query(
            &self,
            _query: &str,
            _key: &RepresentationKey,
        ) -> Result<Vec<f32>, local_rag_memory::recall::QueryEmbedError> {
            Ok(vec![1.0, 0.0, 0.0])
        }
    }

    #[test]
    fn a_provider_that_appears_after_construction_is_picked_up_without_a_restart() {
        let installed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let seen = Arc::clone(&installed);
        let unavailable: Arc<dyn QueryEmbedder> = Arc::new(UnavailableEmbedder);
        let lazy = LazyQueryEmbedder::new(unavailable, move || {
            if seen.load(std::sync::atomic::Ordering::SeqCst) {
                ProviderProbe::Ready(Arc::new(AlwaysEmbeds))
            } else {
                ProviderProbe::NotInstalled
            }
        });

        assert!(
            lazy.embed_query("q", &key("test-model")).is_err(),
            "nothing installed yet: the dense leg must degrade"
        );

        installed.store(true, std::sync::atomic::Ordering::SeqCst);

        assert_eq!(
            lazy.embed_query("q", &key("test-model"))
                .expect("the newly installed provider must serve"),
            vec![1.0, 0.0, 0.0]
        );
    }

    #[test]
    fn the_memory_side_picks_up_a_late_provider_too() {
        use local_rag_memory::recall::QueryEmbedder as MemoryQueryEmbedder;

        let installed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let seen = Arc::clone(&installed);
        let unavailable: Arc<dyn MemoryQueryEmbedder> = Arc::new(UnavailableMemoryEmbedder);
        let lazy = LazyQueryEmbedder::new(unavailable, move || {
            if seen.load(std::sync::atomic::Ordering::SeqCst) {
                ProviderProbe::Ready(Arc::new(AlwaysEmbeds))
            } else {
                ProviderProbe::NotInstalled
            }
        });

        assert!(
            MemoryQueryEmbedder::embed_query(&lazy, "q", &memory_key("test-model")).is_err(),
            "nothing installed yet: recall's dense leg must degrade"
        );

        installed.store(true, std::sync::atomic::Ordering::SeqCst);

        assert_eq!(
            MemoryQueryEmbedder::embed_query(&lazy, "q", &memory_key("test-model"))
                .expect("the newly installed provider must serve"),
            vec![1.0, 0.0, 0.0]
        );
    }

    #[test]
    fn an_open_provider_is_never_reprobed_and_an_unopenable_one_is_never_retried() {
        let probes = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let counted = Arc::clone(&probes);
        let unavailable: Arc<dyn QueryEmbedder> = Arc::new(UnavailableEmbedder);
        let ready = LazyQueryEmbedder::new(unavailable, move || {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            ProviderProbe::Ready(Arc::new(AlwaysEmbeds))
        });
        for _ in 0..3 {
            ready.embed_query("q", &key("test-model")).expect("serves");
        }
        assert_eq!(
            probes.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "an already-open provider must not be re-probed per query"
        );

        probes.store(0, std::sync::atomic::Ordering::SeqCst);
        let counted = Arc::clone(&probes);
        let unavailable: Arc<dyn QueryEmbedder> = Arc::new(UnavailableEmbedder);
        let broken = LazyQueryEmbedder::new(unavailable, move || {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            ProviderProbe::Unusable
        });
        for _ in 0..3 {
            assert!(broken.embed_query("q", &key("test-model")).is_err());
        }
        assert_eq!(
            probes.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a provider that would not open must not be reopened per query"
        );
    }

    #[test]
    fn the_production_constructors_degrade_when_no_model_is_installed() {
        use local_rag_memory::recall::QueryEmbedder as MemoryQueryEmbedder;
        use local_rag_test_support::TempHome;

        let home = TempHome::new().expect("temp home");
        let layout = local_rag_core::paths::StoreLayout::new(home.join("local-rag"));
        layout.ensure().expect("ensure store tree");

        assert!(
            code_query_embedder(&layout)
                .embed_query("q", &key("test-model"))
                .is_err(),
            "an empty store must degrade, not panic or block"
        );
        assert!(
            MemoryQueryEmbedder::embed_query(
                memory_query_embedder(&layout).as_ref(),
                "q",
                &memory_key("test-model")
            )
            .is_err()
        );
    }
}
