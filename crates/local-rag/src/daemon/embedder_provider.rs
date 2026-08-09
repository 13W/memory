//! The daemon's own ONNX sessions — at most two per process (`code_raw` +
//! `memory`, D-036), opened lazily on first use (D-037) and shared between
//! [`query_embedder`](super::query_embedder)'s adapters and (T20-05/T20-06)
//! indexing's backfill pool, instead of each side opening its own (T20-03).
//!
//! [`LazyProvider`] is the shared probe-and-latch mechanism `LazyQueryEmbedder`
//! used to own outright; it is now generic over the *policy* a caller wants on
//! a miss. `query_embedder`'s [`LazyQueryEmbedder`](super::query_embedder::LazyQueryEmbedder)
//! fails open to an `UnavailableEmbedder` (search/recall already have a tested
//! degraded path); [`LazyEmbedderProvider`] here fails honest with `None` — a
//! session provider has no meaningful placeholder `Arc<dyn Embedder>` to hand
//! back (`Embedder::key()` is not a `Result`, so a fabricated key would
//! silently poison whichever `RepresentationKey` a backfill caller wrote
//! vectors under).

use std::sync::{Arc, RwLock};

use local_rag_core::paths::StoreLayout;
use local_rag_embed::Embedder;
use local_rag_models::{DEFAULT_MODEL_ID, OnnxEmbedder, find, is_installed};

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

/// The shared lazy-probe-and-latch mechanism (D-037): call `probe` again only
/// while the state is still [`ProviderProbe::NotInstalled`], and freeze on the
/// first [`ProviderProbe::Ready`] or [`ProviderProbe::Unusable`]. Generic over
/// what a caller does with a non-`Ready` state — see the module doc.
pub struct LazyProvider<T: ?Sized> {
    state: RwLock<ProviderProbe<T>>,
    probe: Box<dyn Fn() -> ProviderProbe<T> + Send + Sync>,
}

impl<T: ?Sized> LazyProvider<T> {
    /// Wrap `probe`, starting `NotInstalled`.
    pub fn new(probe: impl Fn() -> ProviderProbe<T> + Send + Sync + 'static) -> Self {
        Self {
            state: RwLock::new(ProviderProbe::NotInstalled),
            probe: Box::new(probe),
        }
    }

    /// The current state, re-probing at most once per call and only while
    /// still `NotInstalled`. Once `Ready`/`Unusable`, this is a read lock and
    /// an `Arc` clone — no re-probe, no I/O.
    pub fn state(&self) -> ProviderProbe<T> {
        {
            let state = self.state.read().expect("lazy-provider state lock");
            match &*state {
                ProviderProbe::Ready(provider) => {
                    return ProviderProbe::Ready(Arc::clone(provider));
                }
                ProviderProbe::Unusable => return ProviderProbe::Unusable,
                ProviderProbe::NotInstalled => {}
            }
        }
        let mut state = self.state.write().expect("lazy-provider state lock");
        if matches!(*state, ProviderProbe::NotInstalled) {
            *state = (self.probe)();
        }
        match &*state {
            ProviderProbe::Ready(provider) => ProviderProbe::Ready(Arc::clone(provider)),
            ProviderProbe::Unusable => ProviderProbe::Unusable,
            ProviderProbe::NotInstalled => ProviderProbe::NotInstalled,
        }
    }

    /// [`Self::state`], narrowed to "is there a live provider right now".
    pub fn ready(&self) -> Option<Arc<T>> {
        match self.state() {
            ProviderProbe::Ready(provider) => Some(provider),
            ProviderProbe::Unusable | ProviderProbe::NotInstalled => None,
        }
    }
}

/// The daemon's single owner of its ONNX sessions — one [`LazyProvider`] per
/// `RepresentationKind` (`code_raw`, `memory`), probed independently so a
/// broken/missing model on one kind never blocks the other (D-036: one
/// physical model, two sessions, one per kind).
pub struct LazyEmbedderProvider {
    code: LazyProvider<dyn Embedder>,
    memory: LazyProvider<dyn Embedder>,
}

impl LazyEmbedderProvider {
    /// The production constructor: the same on-disk gate `cli::init`/
    /// `indexing::finish_index_ctx` use — `find(DEFAULT_MODEL_ID)` →
    /// `is_installed(layout, ..)` → `OnnxEmbedder::open`/`open_for_memory` —
    /// but lazy (D-037) and fail-honest (a `tracing::warn!` on the
    /// installed-but-unopenable branch, same as `query_embedder`'s
    /// constructors, X-004).
    pub fn new(layout: &StoreLayout) -> Self {
        let code_layout = layout.clone();
        let memory_layout = layout.clone();
        Self {
            code: LazyProvider::new(move || probe_onnx(&code_layout, OnnxEmbedder::open, "code")),
            memory: LazyProvider::new(move || {
                probe_onnx(&memory_layout, OnnxEmbedder::open_for_memory, "memory")
            }),
        }
    }

    /// A test/alternative seam — the same pattern `LazyQueryEmbedder::new`
    /// already exposes publicly for injecting a probe without ONNX.
    pub fn with_probes(
        code: impl Fn() -> ProviderProbe<dyn Embedder> + Send + Sync + 'static,
        memory: impl Fn() -> ProviderProbe<dyn Embedder> + Send + Sync + 'static,
    ) -> Self {
        Self {
            code: LazyProvider::new(code),
            memory: LazyProvider::new(memory),
        }
    }

    /// The `code_raw` session, if one is currently open.
    pub fn code(&self) -> Option<Arc<dyn Embedder>> {
        self.code.ready()
    }

    /// The `memory` session, if one is currently open.
    pub fn memory(&self) -> Option<Arc<dyn Embedder>> {
        self.memory.ready()
    }

    /// [`Self::code`]'s state three-valued — `query_embedder`'s adapters need
    /// to tell `Unusable` apart from `NotInstalled` to latch a terminal
    /// failure instead of re-probing on every dense query.
    pub fn code_probe(&self) -> ProviderProbe<dyn Embedder> {
        self.code.state()
    }

    /// [`Self::memory`]'s state three-valued — see [`Self::code_probe`].
    pub fn memory_probe(&self) -> ProviderProbe<dyn Embedder> {
        self.memory.state()
    }
}

/// Shared probe body for one kind: disk-state gate, then `open`, logging a
/// warning (X-004) on the installed-but-unopenable branch.
fn probe_onnx(
    layout: &StoreLayout,
    open: impl FnOnce(
        &StoreLayout,
        &'static local_rag_models::ModelCatalogEntry,
    ) -> Result<OnnxEmbedder, local_rag_models::OnnxError>,
    leg: &'static str,
) -> ProviderProbe<dyn Embedder> {
    let Some(entry) = find(DEFAULT_MODEL_ID) else {
        return ProviderProbe::Unusable;
    };
    if !is_installed(layout, entry.model_id) {
        return ProviderProbe::NotInstalled;
    }
    match open(layout, entry) {
        Ok(embedder) => ProviderProbe::Ready(Arc::new(embedder)),
        Err(e) => {
            tracing::warn!(
                "local-rag: {} is installed but could not be opened for its {leg} representation ({e}); \
                 this session will stay unavailable until this is fixed",
                entry.model_id
            );
            ProviderProbe::Unusable
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use local_rag_embed::{EmbedError, EmbedRequest, Vector};
    use local_rag_store::{DistanceMetric, RepresentationKey, RepresentationKind};
    use local_rag_test_support::TempHome;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};

    struct FixedEmbedder {
        key: RepresentationKey,
        vector: Vec<f32>,
    }

    impl Embedder for FixedEmbedder {
        fn embed(&self, req: EmbedRequest) -> Result<Vec<Vector>, EmbedError> {
            Ok(req
                .texts
                .iter()
                .map(|_| Vector::new(self.vector.clone()))
                .collect())
        }

        fn key(&self) -> RepresentationKey {
            self.key.clone()
        }
    }

    fn key(kind: RepresentationKind, model_id: &str) -> RepresentationKey {
        RepresentationKey {
            kind,
            representation_version: 1,
            normalization_version: 1,
            model_id: model_id.to_string(),
            dimensions: 3,
            distance_metric: DistanceMetric::Cosine,
        }
    }

    #[test]
    fn each_kind_opens_its_session_exactly_once() {
        let code_opens = Arc::new(AtomicUsize::new(0));
        let memory_opens = Arc::new(AtomicUsize::new(0));
        let (c, m) = (Arc::clone(&code_opens), Arc::clone(&memory_opens));

        let provider = LazyEmbedderProvider::with_probes(
            move || {
                c.fetch_add(1, SeqCst);
                ProviderProbe::Ready(Arc::new(FixedEmbedder {
                    key: key(RepresentationKind::CodeRaw, "m"),
                    vector: vec![1.0, 0.0, 0.0],
                }))
            },
            move || {
                m.fetch_add(1, SeqCst);
                ProviderProbe::Ready(Arc::new(FixedEmbedder {
                    key: key(RepresentationKind::Memory, "m"),
                    vector: vec![0.0, 1.0, 0.0],
                }))
            },
        );

        let first = provider.code().expect("ready");
        for _ in 0..3 {
            let again = provider.code().expect("ready");
            assert!(
                Arc::ptr_eq(&first, &again),
                "the *same* session must be handed back, not just any open one"
            );
        }
        for _ in 0..3 {
            provider.memory().expect("ready");
        }

        assert_eq!(code_opens.load(SeqCst), 1);
        assert_eq!(memory_opens.load(SeqCst), 1);
    }

    #[test]
    fn a_kind_that_is_not_installed_yet_is_probed_again_until_it_appears() {
        let installed = Arc::new(AtomicBool::new(false));
        let opens = Arc::new(AtomicUsize::new(0));
        let (seen, counted) = (Arc::clone(&installed), Arc::clone(&opens));

        let provider = LazyEmbedderProvider::with_probes(
            move || {
                counted.fetch_add(1, SeqCst);
                if seen.load(SeqCst) {
                    ProviderProbe::Ready(Arc::new(FixedEmbedder {
                        key: key(RepresentationKind::CodeRaw, "m"),
                        vector: vec![1.0, 0.0, 0.0],
                    }))
                } else {
                    ProviderProbe::NotInstalled
                }
            },
            || ProviderProbe::NotInstalled,
        );

        assert!(provider.code().is_none());
        installed.store(true, SeqCst);
        assert!(provider.code().is_some());
        assert!(provider.code().is_some());
        assert_eq!(opens.load(SeqCst), 2, "latched after the first Ready");
    }

    #[test]
    fn a_kind_that_will_not_open_is_never_reopened() {
        let opens = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&opens);

        let provider = LazyEmbedderProvider::with_probes(
            move || {
                counted.fetch_add(1, SeqCst);
                ProviderProbe::Unusable
            },
            || ProviderProbe::NotInstalled,
        );

        for _ in 0..3 {
            assert!(provider.code().is_none());
        }
        assert_eq!(
            opens.load(SeqCst),
            1,
            "an unopenable provider must not be retried"
        );
    }

    #[test]
    fn the_two_kinds_probe_independently() {
        let provider = LazyEmbedderProvider::with_probes(
            || {
                ProviderProbe::Ready(Arc::new(FixedEmbedder {
                    key: key(RepresentationKind::CodeRaw, "m"),
                    vector: vec![1.0, 0.0, 0.0],
                }))
            },
            || ProviderProbe::NotInstalled,
        );

        assert!(provider.code().is_some());
        assert!(provider.memory().is_none());
    }

    #[test]
    fn the_production_constructor_answers_none_on_an_empty_store() {
        let home = TempHome::new().expect("temp home");
        let layout = StoreLayout::new(home.join("local-rag"));
        layout.ensure().expect("ensure store tree");

        let provider = LazyEmbedderProvider::new(&layout);
        assert!(provider.code().is_none());
        assert!(provider.memory().is_none());
    }
}
