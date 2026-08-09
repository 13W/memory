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
//! `Arc<dyn Embedder>` handed out by [`super::embedder_provider::LazyEmbedderProvider`]
//! (T20-03) — the daemon's single owner of its `code_raw`/`memory` ONNX
//! sessions, shared with indexing's backfill pool — rather than opening their
//! own session.
//!
//! [`code_query_embedder`]/[`memory_query_embedder`] are the daemon's own
//! production constructors for the two `StartOptions` fields, deferred behind
//! [`LazyQueryEmbedder`] so a model installed *after* the daemon started is
//! picked up without a restart (D-037); [`LazyQueryEmbedder`] is now a thin
//! fail-open facade over [`super::embedder_provider::LazyProvider`]'s shared
//! probe-and-latch mechanism.

use std::sync::Arc;

use local_rag_embed::{EmbedRequest, Embedder};
use local_rag_memory::recall::UnavailableEmbedder as UnavailableMemoryEmbedder;
use local_rag_search::{QueryEmbedError, QueryEmbedder, UnavailableEmbedder};
use local_rag_store::RepresentationKey;

use super::embedder_provider::{LazyEmbedderProvider, LazyProvider, ProviderProbe};

/// Wraps an [`Embedder`] session (shared with indexing's backfill pool via
/// [`LazyEmbedderProvider`], T20-03) as a [`QueryEmbedder`]: one-text batch,
/// unwrapped back to a single vector. Refuses (rather than silently answering
/// under the wrong model) whenever the caller's requested `key` does not
/// exactly match the wrapped provider's own [`Embedder::key`] — the same
/// "MUST honor `key`" contract `QueryEmbedder`'s own trait doc already
/// states.
pub struct EmbedderQueryAdapter {
    embedder: Arc<dyn Embedder>,
}

impl EmbedderQueryAdapter {
    pub fn new(embedder: Arc<dyn Embedder>) -> Self {
        Self { embedder }
    }
}

impl QueryEmbedder for EmbedderQueryAdapter {
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
pub struct MemoryEmbedderQueryAdapter {
    embedder: Arc<dyn Embedder>,
}

impl MemoryEmbedderQueryAdapter {
    pub fn new(embedder: Arc<dyn Embedder>) -> Self {
        Self { embedder }
    }
}

impl local_rag_memory::recall::QueryEmbedder for MemoryEmbedderQueryAdapter {
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
///
/// A thin fail-open facade (T20-03) over [`LazyProvider`]'s shared
/// probe-and-latch mechanism — falls back to `unavailable` on anything but
/// `Ready`, instead of [`LazyEmbedderProvider`]'s fail-honest `None`.
pub struct LazyQueryEmbedder<T: ?Sized> {
    inner: LazyProvider<T>,
    unavailable: Arc<T>,
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
            inner: LazyProvider::new(probe),
            unavailable,
        }
    }

    fn provider(&self) -> Arc<T> {
        self.inner
            .ready()
            .unwrap_or_else(|| Arc::clone(&self.unavailable))
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

/// `search_code`'s dense-leg provider — its ONNX session comes from `provider`
/// ([`LazyEmbedderProvider`], T20-03), the same one indexing's backfill pool
/// uses, instead of opening one of its own. Re-checked per query until the
/// shared session appears (D-037).
///
/// A missing model, or one that fails to open, degrades to
/// [`UnavailableEmbedder`] rather than failing daemon startup: `search_code`
/// already has a tested, spec-correct `lexical_only` fallback for exactly this
/// case (`daemon::search::build_search_engine`'s own doc), and a store the
/// operator has not run `local-rag init --download-models` against yet must
/// still serve lexical search.
pub fn code_query_embedder(provider: &Arc<LazyEmbedderProvider>) -> Arc<dyn QueryEmbedder> {
    let provider = Arc::clone(provider);
    let unavailable: Arc<dyn QueryEmbedder> = Arc::new(UnavailableEmbedder);
    Arc::new(LazyQueryEmbedder::new(
        unavailable,
        move || match provider.code_probe() {
            ProviderProbe::Ready(embedder) => {
                ProviderProbe::Ready(Arc::new(EmbedderQueryAdapter::new(embedder)))
            }
            ProviderProbe::Unusable => ProviderProbe::Unusable,
            ProviderProbe::NotInstalled => ProviderProbe::NotInstalled,
        },
    ))
}

/// `recall`'s dense-leg provider (D-036) — the same shared-session shape as
/// [`code_query_embedder`], but reading `provider`'s `memory` leg and wrapped
/// in [`MemoryEmbedderQueryAdapter`] instead. A missing/uninstalled model, or
/// one `local-rag init` never ran against, degrades to
/// [`UnavailableMemoryEmbedder`] — `recall`'s own tested `dense_degraded:
/// NoRepresentation`/`EmbedFailed` fallback, not a daemon startup failure.
pub fn memory_query_embedder(
    provider: &Arc<LazyEmbedderProvider>,
) -> Arc<dyn local_rag_memory::recall::QueryEmbedder> {
    let provider = Arc::clone(provider);
    let unavailable: Arc<dyn local_rag_memory::recall::QueryEmbedder> =
        Arc::new(UnavailableMemoryEmbedder);
    Arc::new(LazyQueryEmbedder::new(
        unavailable,
        move || match provider.memory_probe() {
            ProviderProbe::Ready(embedder) => {
                ProviderProbe::Ready(Arc::new(MemoryEmbedderQueryAdapter::new(embedder)))
            }
            ProviderProbe::Unusable => ProviderProbe::Unusable,
            ProviderProbe::NotInstalled => ProviderProbe::NotInstalled,
        },
    ))
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
        let adapter = EmbedderQueryAdapter::new(Arc::new(FixedEmbedder {
            key: key("test-model"),
            vector: vec![1.0, 0.0, 0.0],
        }));
        let out = adapter
            .embed_query("hello", &key("test-model"))
            .expect("matching key succeeds");
        assert_eq!(out, vec![1.0, 0.0, 0.0]);
    }

    #[test]
    fn refuses_a_mismatched_key_rather_than_silently_answering() {
        let adapter = EmbedderQueryAdapter::new(Arc::new(FixedEmbedder {
            key: key("test-model"),
            vector: vec![1.0, 0.0, 0.0],
        }));
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

        let adapter = MemoryEmbedderQueryAdapter::new(Arc::new(FixedEmbedder {
            key: memory_key("test-model"),
            vector: vec![0.0, 1.0, 0.0],
        }));
        let out = MemoryQueryEmbedder::embed_query(&adapter, "hello", &memory_key("test-model"))
            .expect("matching key succeeds");
        assert_eq!(out, vec![0.0, 1.0, 0.0]);
    }

    #[test]
    fn memory_adapter_refuses_a_mismatched_key_rather_than_silently_answering() {
        use local_rag_memory::recall::QueryEmbedder as MemoryQueryEmbedder;

        let adapter = MemoryEmbedderQueryAdapter::new(Arc::new(FixedEmbedder {
            key: memory_key("test-model"),
            vector: vec![0.0, 1.0, 0.0],
        }));
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
        let provider = Arc::new(LazyEmbedderProvider::new(&layout));

        assert!(
            code_query_embedder(&provider)
                .embed_query("q", &key("test-model"))
                .is_err(),
            "an empty store must degrade, not panic or block"
        );
        assert!(
            MemoryQueryEmbedder::embed_query(
                memory_query_embedder(&provider).as_ref(),
                "q",
                &memory_key("test-model")
            )
            .is_err()
        );
    }

    // -----------------------------------------------------------------
    // T20-03: the query adapters and indexing's backfill pool must share the
    // same ONNX session per kind, not open one each.
    // -----------------------------------------------------------------

    #[test]
    fn the_query_adapters_and_the_backfill_share_one_session() {
        let opens = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = Arc::clone(&opens);
        let provider = Arc::new(LazyEmbedderProvider::with_probes(
            move || {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                ProviderProbe::Ready(Arc::new(FixedEmbedder {
                    key: key("m"),
                    vector: vec![1.0, 0.0, 0.0],
                }))
            },
            || ProviderProbe::NotInstalled,
        ));

        let query = code_query_embedder(&provider);
        for _ in 0..3 {
            query
                .embed_query("q", &key("m"))
                .expect("the query leg serves");
        }
        let backfill = provider
            .code()
            .expect("the backfill leg must see the same session");
        assert!(
            backfill
                .embed(EmbedRequest::new(
                    RepresentationKind::CodeRaw,
                    vec!["x".to_string()]
                ))
                .is_ok()
        );

        assert_eq!(
            opens.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "query adapters and the backfill pool must share one ONNX session per kind"
        );
    }
}
