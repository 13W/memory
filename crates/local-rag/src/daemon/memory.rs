//! Memory-read context for the MCP status/memory tools (T15-04) — wires the
//! daemon's own already-open `state.sqlite`/`cache.sqlite` handles into spec
//! 08 §6's recall pipeline (`local_rag_memory::recall`) and the review-tool
//! store primitives (`local_rag_store::memory`), mirroring [`super::search`]'s
//! `SearchEngine` construction for the code-query tools.

use std::sync::Arc;

use local_rag_core::identity::UuidSource;
use local_rag_memory::recall::{BruteForceCosine, MemoryDenseBackend, QueryEmbedder};
use local_rag_store::{CacheDb, StateDb};

/// Everything a T15-04/T15-05 MCP tool adapter needs to read
/// `state.sqlite`/`cache.sqlite` directly — unlike
/// [`super::search::build_search_engine`]'s `SearchEngine`, there is no
/// shard manager or worktree lock here: memory recall holds no lock across
/// its pipeline (spec 08 §6's own as-built note), and the review-tool reads
/// (`list_memory`, `list_memory_candidates`, `inspect_memory_evidence`,
/// `stats`, `health`) are plain, single-connection SQL. `uuids` (T15-05)
/// mints fresh ids the daemon itself is responsible for: `remember`'s
/// `memory_id` and `give_feedback`'s `observation_id` — unlike the router
/// or a candidate proposer, which already mint their own ids before calling
/// into the store, the MCP layer *is* the caller here, so nothing upstream
/// supplies one.
pub struct MemoryContext {
    pub state: Arc<StateDb>,
    pub cache: Arc<CacheDb>,
    pub embedder: Arc<dyn QueryEmbedder>,
    pub dense_backend: Arc<dyn MemoryDenseBackend>,
    pub recall_token_budget: u32,
    pub uuids: Arc<dyn UuidSource + Send + Sync>,
}

/// Build the [`MemoryContext`] the MCP memory tools call.
///
/// `embedder` is `main.rs`'s `UnavailableEmbedder` today — the same "type
/// before backend, real provider is T15-07's job" precedent
/// [`super::search::build_search_engine`] already documents for code search's
/// own `query_embedder`; recall's dense leg correctly degrades
/// (`dense_degraded: Some(EmbedFailed(..))`) until then. `dense_backend` is
/// always [`BruteForceCosine`] — it has no availability gating of its own
/// (unlike the embedder, it is just cosine math over whatever vectors the
/// embedder did or didn't produce), so it is hardcoded here rather than
/// threaded through `StartOptions`, mirroring [`super::search::
/// NoRebuildVectorSource`]'s own unconditional construction.
pub fn build_memory_context(
    state: Arc<StateDb>,
    cache: Arc<CacheDb>,
    embedder: Arc<dyn QueryEmbedder>,
    recall_token_budget: u32,
    uuids: Arc<dyn UuidSource + Send + Sync>,
) -> Arc<MemoryContext> {
    Arc::new(MemoryContext {
        state,
        cache,
        embedder,
        dense_backend: Arc::new(BruteForceCosine),
        recall_token_budget,
        uuids,
    })
}
