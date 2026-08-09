//! `SearchEngine` construction for the MCP code-query tools (T15-03) — wires
//! the daemon's own already-open `state.sqlite`/`cache.sqlite` handles into
//! spec 09's pipeline (`local_rag_search::SearchEngine`, fully built by
//! T12-04).

use std::sync::Arc;

use local_rag_core::identity::UuidSource;
use local_rag_core::paths::StoreLayout;
use local_rag_embed::LOCAL_BOOTSTRAP_DIMENSIONS;
use local_rag_projection::{
    BruteForceProjectionStore, RepresentationKind, ShardManager, ShardParams, VectorSource,
};
use local_rag_search::{DEFAULT_L2_READ_WAIT_BUDGET, QueryEmbedder, SearchEngine};
use local_rag_store::{CacheDb, StateDb, WorktreeLockRegistry};

/// A [`VectorSource`] that never has a vector — the MCP query path must
/// never trigger indexing work (this card's own "no synchronous indexing
/// call" test), and a shard `acquire` that needs a rebuild is exactly the
/// case a real `VectorSource` would otherwise be consulted for.
///
/// This is **not** a stand-in for "no real `VectorSource` exists" — one
/// does: [`local_rag_projection::CacheVectorSource`] is the production
/// reader, already used by the switch/rebuild paths (T11-04/T11-05). It
/// cannot serve here: it borrows `&StateDb`/`&CacheDb` and is scoped to one
/// `(generation_id, model_space_id)` tuple *at construction time*, while the
/// `ShardManager` built by [`build_search_engine`] is a single,
/// daemon-lifetime `Arc` serving every worktree/generation/model-space this
/// daemon will ever see — built once at startup, long before any particular
/// request names a generation. Generalizing `CacheVectorSource` to that
/// shape (owned `Arc`s, resolved per call instead of at construction) is
/// real new capability nobody has asked for yet; the indexing/rebuild path
/// stays exclusively T15-07's (and T11-04's backfill worker's) job.
///
/// The consequence is exactly spec 05's own guardrail, not a shortcut around
/// it: "validate on every open, rebuild on doubt" still runs in full on
/// every `acquire` — only the *repair* step is unreachable from the MCP
/// path. A shard a real indexing run already filled opens and serves
/// normally; one that would need re-deriving vectors degrades to
/// `lexical_only` (`SearchEngine`'s own `AcquireError` handling) instead of
/// doing indexing work here.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoRebuildVectorSource;

impl VectorSource for NoRebuildVectorSource {
    fn vector(&self, _occurrence_id: &str, _kind: RepresentationKind) -> Option<Vec<f32>> {
        None
    }
}

/// Build the `SearchEngine` the MCP code-query tools call.
///
/// `max_open_shards` comes from `config.daemon.max_open_shards`; `embedder`
/// is `daemon::query_embedder::code_query_embedder`'s result (T15-07,
/// T20-03): a real `EmbedderQueryAdapter` reading a session off the shared
/// `LazyEmbedderProvider` when the default model is installed and opens
/// cleanly, `UnavailableEmbedder` otherwise (spec-correctly producing
/// `degraded: "lexical_only"` with an explicit reason) — the same "type
/// before backend" precedent `RequestHandler` itself already set.
/// `ShardParams::with_dimensions(LOCAL_BOOTSTRAP_DIMENSIONS)` is only a
/// bootstrap fallback for a worktree with no active model space at all —
/// `ShardManager::acquire` resolves the real dimensions from the worktree's
/// own active space for every other case, and a worktree with no active
/// space returns `WORKTREE_NOT_INDEXED` before ever reaching the dense leg,
/// so this fallback value is otherwise unobservable.
pub fn build_search_engine(
    state: Arc<StateDb>,
    cache: Arc<CacheDb>,
    layout: StoreLayout,
    uuids: Arc<dyn UuidSource + Send + Sync>,
    embedder: Arc<dyn QueryEmbedder>,
    max_open_shards: u32,
) -> Arc<SearchEngine> {
    let shards = Arc::new(ShardManager::new(
        Arc::clone(&state),
        Arc::new(BruteForceProjectionStore::new()),
        layout,
        ShardParams::with_dimensions(LOCAL_BOOTSTRAP_DIMENSIONS as usize),
        Arc::new(NoRebuildVectorSource),
        uuids,
        max_open_shards,
    ));
    let locks = Arc::new(WorktreeLockRegistry::new());
    Arc::new(SearchEngine::with_embedder(
        state,
        cache,
        locks,
        shards,
        embedder,
        DEFAULT_L2_READ_WAIT_BUDGET,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_rebuild_vector_source_never_has_a_vector() {
        let source = NoRebuildVectorSource;
        assert_eq!(source.vector("occ-1", RepresentationKind::CodeRaw), None);
        assert_eq!(
            source.vector("occ-1", RepresentationKind::CodeContext),
            None
        );
        assert_eq!(
            source.vector("occ-1", RepresentationKind::StructuralDescription),
            None
        );
        assert_eq!(source.vector("occ-1", RepresentationKind::Memory), None);
    }
}
