//! The read-lock search skeleton (spec 09 §1, 06 §3, 02 §5/§6) — T09-03.
//!
//! [`SearchEngine::search_code`] is the pipeline verbatim from spec 09 §1:
//! resolve the request's worktree, take `L2.read` for the **whole** pipeline
//! (spec 06 §3 `[FIXED]`), resolve the active `(generation, model_space)`
//! tuple, validate the FTS view (spec 06 §4) and the dense shard, then
//! release and return a canonical outcome (spec 02 §6).
//!
//! T12-01 filled in the lexical leg: [`Stage::LexicalLeg`] now runs the real
//! BM25 query (`local_rag_store::lexical_leg`) against the active generation,
//! with the spec's default column weights, the `name_pattern` prefix filter and
//! the `max(limit·4, 50)` candidate depth (spec 09 §2/§4) — still inside the
//! same held `L2.read`, and **only** when the FTS view validated (an invalid
//! head means the leg does not run at all and the response is explicitly
//! `dense_only`, never a silently empty lexical result `[FIXED]`).
//!
//! T12-02 filled in the dense leg ([`Stage::DenseLeg`]): the active model
//! space's `code_raw` representation, a query vector from the injected
//! [`QueryEmbedder`], and the production brute-force backend.
//!
//! T12-03 closed the pipeline: legs are selected per
//! [`SearchMode`](local_rag_protocol::SearchMode) (spec 09 §5), their candidates
//! are fused by RRF ([`crate::fusion`], spec 09 §4), and the result is spec 09
//! §7's canonical [`SearchResponse`] — `results[]` with presentable metadata,
//! `generation {id, number}`, `degraded` and `diagnostics`.
//!
//! Deliberately out of scope here (owned by T12-04, not duplicated):
//! `source_blob`-derived output — `snippet` (always `None` today),
//! `get_file_context` and the cached per-generation overview — plus the
//! graph/parent-unit half of enrichment; [`Stage::Enrichment`] remains a marker
//! for where that lands.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use local_rag_core::identity::Uuid;
use local_rag_projection::{
    DenseQuery, RepresentationKind, ScoredPoint, ShardManager, expected_points,
    projection_kind_to_store, representation_key_for, required_code_kinds,
};
use local_rag_protocol::{
    DegradedMode, ErrorEnvelope, FileContext, GenerationRef, ProjectOverview, SearchMode,
    SearchResponse, SearchResult,
};
use local_rag_store::lock::ReadTimedOut;
use local_rag_store::{
    CacheDb, CacheOpenError, FtsAvailability, FtsOpenOutcome, FtsRebuildError, LexicalHit,
    LexicalQuery, OccurrenceMetadata, OpenError, RepresentationKey, RequestRoot, Resolution,
    StateDb, ValidationDepth, WorktreeLockRegistry, candidate_depth, generation_number,
    lexical_leg, occurrences_by_id, open_and_validate_fts, projection_state, resolve, rusqlite,
};

use crate::context::file_context;
use crate::fusion::{FusedHit, FusionWeights, rrf};
use crate::overview::{OverviewCache, compute as compute_overview};
use crate::snippet::cut_batch;

/// A provisional default for the bounded `L2.read` wait (spec 02 §6:
/// "search waits on L2.read (bounded); timeout → BUSY_RETRY"). No
/// `config.toml` field exists for this yet (spec 02 §3.1) — this is an
/// internal engineering constant, not calibrated against any benchmark.
/// Nearest precedent is `state.sqlite`'s own `busy_timeout`, which bounds a
/// different kind of wait.
pub const DEFAULT_L2_READ_WAIT_BUDGET: Duration = Duration::from_millis(2_000);

/// A pipeline execution stage, named so tests can observe which lock level is
/// held at each point ("instrumentation proves lock held in every leg" —
/// T09-03's acceptance criterion).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Reading the active `(generation, model_space)` tuple.
    ActiveTuple,
    /// Validating (and possibly synchronously rebuilding) the FTS view.
    FtsLeg,
    /// Acquiring the shard handle and running the dense query.
    DenseLeg,
    /// The lexical leg: the active-generation BM25 query (T12-01).
    LexicalLeg,
    /// Enrichment (stub — T12-04 owns the real parent-unit/graph work).
    Enrichment,
}

/// An injectable seam so tests can observe per-stage lock state — the same
/// "inject a trait object, fake it in tests" idiom as
/// `local_rag_projection::switch::VectorSource`/`local_rag_core::identity::UuidSource`.
/// Production callers use [`NoopObserver`].
pub trait StageObserver: Send + Sync {
    /// Called once per [`Stage`], in pipeline order, while `L2.read` is held.
    fn on_stage(&self, stage: Stage);
}

/// The production default: observes nothing.
#[derive(Debug, Default)]
pub struct NoopObserver;

impl StageObserver for NoopObserver {
    fn on_stage(&self, _stage: Stage) {}
}

/// Turns the request's query text into a vector **for a specific
/// representation** (spec 09 §3: "query embedding computed with the
/// representation of the active model space").
///
/// A seam rather than a direct dependency on `crates/embed`, following the same
/// "inject a trait object, fake it in tests" idiom as
/// `local_rag_projection::switch::VectorSource` and
/// `local_rag_core::identity::UuidSource`. Two reasons it earns its keep here:
/// the daemon (group 15) owns provider selection *and* the `data_policy` guard
/// that must run before any remote provider is considered (spec 12 §1) — policy
/// is not a search concern; and this crate stays free of an inference runtime,
/// so its tests remain deterministic and offline.
///
/// Implementations MUST honor `key`: embedding with a different model than the
/// one the shard's points were embedded with produces silently meaningless
/// neighbours, which is exactly the failure the six-field
/// [`RepresentationKey`] exists to make impossible.
pub trait QueryEmbedder: Send + Sync {
    /// Embed `query` under `key`, or explain why not.
    fn embed_query(
        &self,
        query: &str,
        key: &RepresentationKey,
    ) -> Result<Vec<f32>, QueryEmbedError>;
}

/// Why a query could not be embedded — no provider registered for the
/// representation, a provider failure, a policy refusal.
///
/// Never fatal to a search: the dense leg turns it into `degraded:
/// lexical_only` plus this text as the diagnostic (spec 02 §6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryEmbedError {
    /// Human-readable reason, surfaced verbatim in `diagnostics`.
    pub reason: String,
}

impl QueryEmbedError {
    /// Build an error from its reason.
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

impl fmt::Display for QueryEmbedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "query embedding failed: {}", self.reason)
    }
}

impl std::error::Error for QueryEmbedError {}

/// A [`QueryEmbedder`] that never produces a vector.
///
/// The honest default for a store with no embedding provider wired up yet
/// (every caller before group 15): the dense leg degrades to `lexical_only`
/// with this reason in `diagnostics`, rather than a silently empty dense leg
/// pretending to be a healthy hybrid search.
#[derive(Debug, Default)]
pub struct UnavailableEmbedder;

impl QueryEmbedder for UnavailableEmbedder {
    fn embed_query(
        &self,
        _query: &str,
        key: &RepresentationKey,
    ) -> Result<Vec<f32>, QueryEmbedError> {
        Err(QueryEmbedError::new(format!(
            "no embedding provider is configured for model {}",
            key.model_id
        )))
    }
}

/// One dense-leg candidate (spec 09 §3), in the same shape the lexical leg's
/// [`LexicalHit`] uses so T12-03 can fuse them without translating identities.
#[derive(Debug, Clone, PartialEq)]
pub struct DenseHit {
    /// The occurrence behind the matched projection point (spec 05 §3's
    /// derivation, inverted via `expected_points`).
    pub occurrence_id: String,
    /// 1-based position in this leg's result order — RRF's `rank_leg(d)`
    /// (spec 09 §4).
    pub rank: usize,
    /// The backend's similarity under the representation's own
    /// `distance_metric`, always "higher is closer". Comparable *within* a leg
    /// only; fusion uses `rank`.
    pub score: f32,
}

/// One `search_code` request: the explicit context to resolve (spec 02 §3.3)
/// plus the already-embedded dense query vector (query embedding is the
/// caller's concern — this crate does not embed).
#[derive(Debug, Clone, PartialEq)]
pub struct SearchRequest {
    /// The request's explicit worktree context.
    pub root: RequestRoot,
    /// The raw query text (spec 09 §1's `query`). Tokenized by the lexical leg
    /// and embedded by the dense one; never handed to FTS5 verbatim.
    pub query: String,
    /// Which legs to run (spec 09 §5). Defaults to
    /// [`SearchMode::Hybrid`](local_rag_protocol::SearchMode::Hybrid).
    pub mode: SearchMode,
    /// The caller's requested result count (spec 09 §1's `limit`). Both legs
    /// derive their candidate depth from it via
    /// `local_rag_store::candidate_depth` (§4); the *response* carries at most
    /// `limit` fused hits.
    pub limit: usize,
    /// Optional prefix filter on `local_name`/`qualified_name` (spec 09 §1's
    /// `name_pattern`).
    pub name_pattern: Option<String>,
    /// Why [`query`](Self::query) is not in the language the index is kept in,
    /// when it is not (`T21-19`, ADR-0011 §Decision 2).
    ///
    /// Already rendered, because its destination is `diagnostics` (spec 09 §7,
    /// "diagnostics always says why") rather than a field of its own:
    /// `degraded` is the typed marker for a **leg** that could not serve, and a
    /// query the translator could not reach is not that.
    ///
    /// The decision is made above this engine — a translation needs a
    /// generative model, and `crates/search` has no business holding one — so
    /// what arrives here is the outcome, not the question.
    pub query_degraded: Option<String>,
}

/// One pipeline run, as an instrumented caller sees it: the canonical
/// [`SearchResponse`] plus the internals a wire client has no business seeing.
///
/// [`SearchEngine::search_code`] returns only `response` — spec 09 §7's shape,
/// and nothing more. This fuller view exists for
/// [`search_code_instrumented`](SearchEngine::search_code_instrumented), whose
/// whole purpose is letting tests assert on things the wire deliberately omits:
/// the **model space** served (T09-04's "exactly one active generation/model
/// tuple" load test) and each leg's **raw** candidates before fusion merged
/// them (T12-01/T12-02's per-leg suites).
#[derive(Debug, Clone, PartialEq)]
pub struct PipelineSnapshot {
    /// The resolved worktree.
    pub worktree_id: String,
    /// The active model space served. Not a §7 field — a shard is per
    /// `(worktree, model_space)` (spec 05 §2), so this is what proves a
    /// response did not mix model spaces.
    pub model_space_id: String,
    /// The lexical leg's ranked candidates (spec 09 §2), best first, **before**
    /// fusion. Empty whenever the leg did not run — an invalid FTS view
    /// (`dense_only`), a mode that did not ask for it, or a query that reduces
    /// to no terms.
    pub lexical: Vec<LexicalHit>,
    /// The dense leg's ranked candidates (spec 09 §3), best first, **before**
    /// fusion. Empty whenever the leg did not run — `lexical_only`, a mode that
    /// did not ask for it, or a query with no text.
    pub dense: Vec<DenseHit>,
    /// The canonical response (spec 09 §7).
    pub response: SearchResponse,
}

impl PipelineSnapshot {
    /// Whether the dense leg served this response (the inverse of
    /// `degraded == lexical_only`), for readers that care about the leg rather
    /// than the wire vocabulary.
    pub fn dense_served(&self) -> bool {
        self.response.degraded != Some(DegradedMode::LexicalOnly)
    }
}

/// An infrastructure-level failure: none of spec 02 §6's named error codes
/// describe it (a SQLite open/read failure, or a stored `worktree_id` that
/// fails to parse as a UUID — structurally should never happen, since every
/// `worktree_id` is minted via `UuidSource`).
#[derive(Debug)]
#[non_exhaustive]
pub enum SearchInfraError {
    /// Opening a `state.sqlite` read connection failed.
    StateOpen(OpenError),
    /// Reading `state.sqlite` failed.
    StateRead(rusqlite::Error),
    /// Opening a `cache.sqlite` read connection failed.
    CacheOpen(CacheOpenError),
    /// Running the lexical query against `cache.sqlite` failed.
    CacheRead(rusqlite::Error),
    /// The FTS validator failed at the infrastructure level.
    Fts(FtsRebuildError),
    /// A stored `worktree_id` did not parse as a UUID.
    CorruptWorktreeId(String),
    /// The active generation has no `generation` row — a corrupt store, since
    /// `worktree_projection_state.active_generation_id` is a foreign key into
    /// exactly that table.
    MissingGeneration(String),
}

impl fmt::Display for SearchInfraError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SearchInfraError::StateOpen(e) => write!(f, "search: state.sqlite open failed: {e}"),
            SearchInfraError::StateRead(e) => write!(f, "search: state.sqlite read failed: {e}"),
            SearchInfraError::CacheOpen(e) => write!(f, "search: cache.sqlite open failed: {e}"),
            SearchInfraError::CacheRead(e) => write!(f, "search: lexical query failed: {e}"),
            SearchInfraError::Fts(e) => write!(f, "search: FTS validation failed: {e}"),
            SearchInfraError::CorruptWorktreeId(id) => {
                write!(f, "search: worktree_id {id:?} is not a valid UUID")
            }
            SearchInfraError::MissingGeneration(id) => {
                write!(f, "search: active generation {id:?} has no generation row")
            }
        }
    }
}

impl std::error::Error for SearchInfraError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SearchInfraError::StateOpen(e) => Some(e),
            SearchInfraError::StateRead(e) => Some(e),
            SearchInfraError::CacheOpen(e) => Some(e),
            SearchInfraError::CacheRead(e) => Some(e),
            SearchInfraError::Fts(e) => Some(e),
            SearchInfraError::CorruptWorktreeId(_) | SearchInfraError::MissingGeneration(_) => None,
        }
    }
}

/// The hybrid code-search read path (spec 09 §1). Holds everything a
/// `search_code` call needs: the two SQLite handles, the L2 lock registry
/// (T09-01), and the L3 shard manager (T09-02).
pub struct SearchEngine {
    state: Arc<StateDb>,
    cache: Arc<CacheDb>,
    locks: Arc<WorktreeLockRegistry>,
    shards: Arc<ShardManager>,
    embedder: Arc<dyn QueryEmbedder>,
    /// Per-`(worktree, generation)` `project_overview` cache (spec 11 §2,
    /// T12-04).
    overviews: OverviewCache,
    read_wait_budget: Duration,
    /// The representation the dense leg searches over. `code_raw` in v0 (spec 09
    /// §3); see [`with_dense_kind`](Self::with_dense_kind).
    dense_kind: RepresentationKind,
    /// How much each leg's rank counts when the two are fused (spec 09 §4,
    /// D-018); see [`with_fusion_weights`](Self::with_fusion_weights).
    fusion_weights: FusionWeights,
}

impl SearchEngine {
    /// Assemble a search engine over already-open store handles, with no
    /// embedding provider — every dense leg degrades to `lexical_only` with an
    /// explicit reason ([`UnavailableEmbedder`]).
    pub fn new(
        state: Arc<StateDb>,
        cache: Arc<CacheDb>,
        locks: Arc<WorktreeLockRegistry>,
        shards: Arc<ShardManager>,
        read_wait_budget: Duration,
    ) -> Self {
        Self::with_embedder(
            state,
            cache,
            locks,
            shards,
            Arc::new(UnavailableEmbedder),
            read_wait_budget,
        )
    }

    /// [`new`](Self::new) with a real [`QueryEmbedder`] — the constructor the
    /// daemon uses once providers exist (group 15).
    pub fn with_embedder(
        state: Arc<StateDb>,
        cache: Arc<CacheDb>,
        locks: Arc<WorktreeLockRegistry>,
        shards: Arc<ShardManager>,
        embedder: Arc<dyn QueryEmbedder>,
        read_wait_budget: Duration,
    ) -> Self {
        Self {
            state,
            cache,
            locks,
            shards,
            embedder,
            overviews: OverviewCache::default(),
            read_wait_budget,
            dense_kind: RepresentationKind::CodeRaw,
            fusion_weights: FusionWeights::default(),
        }
    }

    /// Search the dense leg over `kind` instead of the `code_raw` default.
    ///
    /// Spec 09 §3 leaves the content-vs-context choice `[OPEN]`, "decided by the
    /// benchmark", and ships `code_raw` in v0. This seam is what lets a
    /// `code_context` shard *be* benchmarked without a second engine: the query
    /// is embedded under that kind's representation key and the point filter
    /// follows it, so both sides of the comparison move together. The default is
    /// unchanged, so no production path is affected until the `[OPEN]` closes in
    /// context's favour (D-016).
    #[must_use]
    pub fn with_dense_kind(mut self, kind: RepresentationKind) -> Self {
        self.dense_kind = kind;
        self
    }

    /// Fuse the legs with `weights` instead of [`FusionWeights::default`].
    ///
    /// The default is derived, not tuned (D-018), and this seam is what lets the
    /// benchmark *check* it against neighbouring policies without a second
    /// engine or a request field: weights are a property of how this deployment
    /// ranks, never of an individual query.
    #[must_use]
    pub fn with_fusion_weights(mut self, weights: FusionWeights) -> Self {
        self.fusion_weights = weights;
        self
    }

    /// Run one search (spec 09 §1). The outer [`Result`] is an infrastructure
    /// failure; the inner is the canonical domain outcome (spec 02 §6) —
    /// mirrors `local_rag_store::registry::attach`'s outer/inner idiom.
    pub async fn search_code(
        &self,
        request: SearchRequest,
        now_ms: i64,
    ) -> Result<Result<SearchResponse, ErrorEnvelope>, SearchInfraError> {
        Ok(self
            .search_code_instrumented(request, now_ms, &NoopObserver)
            .await?
            .map(|snapshot| snapshot.response))
    }

    /// [`search_code`](Self::search_code), plus a [`StageObserver`] callback
    /// at every pipeline stage — the seam tests use to prove `L2.read` is
    /// held throughout.
    pub async fn search_code_instrumented(
        &self,
        request: SearchRequest,
        now_ms: i64,
        observer: &dyn StageObserver,
    ) -> Result<Result<PipelineSnapshot, ErrorEnvelope>, SearchInfraError> {
        // Step 0 (spec 09 §5): `semantic` is the description leg — post-v0,
        // benchmark-gated `[FIXED]`. Refused before anything else happens: no
        // resolution, no lock, no leg.
        if request.mode == SearchMode::Semantic {
            return Ok(Err(ErrorEnvelope::unsupported_mode(request.mode)));
        }

        // Step 1 (spec 09 §1): resolve worktree from request context — before
        // any lock is taken.
        let worktree_id = {
            let conn = self
                .state
                .open_read()
                .map_err(SearchInfraError::StateOpen)?;
            match resolve(&conn, &request.root).map_err(SearchInfraError::StateRead)? {
                Resolution::Resolved { worktree_id, .. } => worktree_id,
                // No dedicated spec 02 §6 code exists for "resolvable only via
                // an explicit attach()"; folded into the same wire outcome as
                // an unknown worktree (both mean "code search cannot proceed
                // for this request").
                Resolution::GlobalOnly | Resolution::Ambiguous { .. } => {
                    return Ok(Err(ErrorEnvelope::worktree_not_indexed()));
                }
            }
        };

        // Step 2 (spec 09 §1 / 06 §3 [FIXED]): L2.read spans everything else,
        // with a bounded wait (spec 02 §6: "search waits on L2.read
        // (bounded); timeout → BUSY_RETRY").
        match self
            .locks
            .read_bounded(
                &worktree_id,
                self.read_wait_budget,
                self.run_locked(&worktree_id, &request, now_ms, observer),
            )
            .await
        {
            Ok(inner) => inner,
            Err(ReadTimedOut) => Ok(Err(ErrorEnvelope::busy_retry())),
        }
    }

    /// `get_file_context(path)` (spec 11 §2) — T12-04.
    ///
    /// Resolves the worktree before any lock, then reads the active
    /// generation's occurrences for `normalized_path` and their `source_blob`
    /// excerpts under `L2.read` (spec 06 §3), so list and snippets can never
    /// come from different generations.
    ///
    /// No `now_ms`, unlike `search_code`: nothing here validates or rebuilds a
    /// cache, so there is no clock-dependent decision to make.
    pub async fn get_file_context(
        &self,
        root: &RequestRoot,
        normalized_path: &str,
    ) -> Result<Result<FileContext, ErrorEnvelope>, SearchInfraError> {
        let Some(worktree_id) = self.resolve_worktree(root)? else {
            return Ok(Err(ErrorEnvelope::worktree_not_indexed()));
        };
        match self
            .locks
            .read_bounded(
                &worktree_id,
                self.read_wait_budget,
                self.file_context_locked(&worktree_id, normalized_path),
            )
            .await
        {
            Ok(inner) => inner,
            Err(ReadTimedOut) => Ok(Err(ErrorEnvelope::busy_retry())),
        }
    }

    async fn file_context_locked(
        &self,
        worktree_id: &str,
        normalized_path: &str,
    ) -> Result<Result<FileContext, ErrorEnvelope>, SearchInfraError> {
        let conn = self
            .state
            .open_read()
            .map_err(SearchInfraError::StateOpen)?;
        let Some(generation_id) = active_generation(&conn, worktree_id)? else {
            return Ok(Err(ErrorEnvelope::worktree_not_indexed()));
        };
        file_context(&conn, &generation_id, normalized_path)
    }

    /// `project_overview()` (spec 11 §2) — T12-04.
    ///
    /// Derived from the active generation under `L2.read` and cached per
    /// `(worktree, generation)`; see [`crate::overview`] for what each field
    /// means and why the cache lives in memory.
    pub async fn project_overview(
        &self,
        root: &RequestRoot,
    ) -> Result<Result<Arc<ProjectOverview>, ErrorEnvelope>, SearchInfraError> {
        let Some(worktree_id) = self.resolve_worktree(root)? else {
            return Ok(Err(ErrorEnvelope::worktree_not_indexed()));
        };
        match self
            .locks
            .read_bounded(
                &worktree_id,
                self.read_wait_budget,
                self.project_overview_locked(&worktree_id),
            )
            .await
        {
            Ok(inner) => inner,
            Err(ReadTimedOut) => Ok(Err(ErrorEnvelope::busy_retry())),
        }
    }

    async fn project_overview_locked(
        &self,
        worktree_id: &str,
    ) -> Result<Result<Arc<ProjectOverview>, ErrorEnvelope>, SearchInfraError> {
        let conn = self
            .state
            .open_read()
            .map_err(SearchInfraError::StateOpen)?;
        let Some(generation_id) = active_generation(&conn, worktree_id)? else {
            return Ok(Err(ErrorEnvelope::worktree_not_indexed()));
        };
        if let Some(cached) = self.overviews.get(worktree_id, &generation_id) {
            return Ok(Ok(cached));
        }
        let computed = Arc::new(compute_overview(&conn, &generation_id)?);
        self.overviews
            .put(worktree_id, &generation_id, computed.clone());
        Ok(Ok(computed))
    }

    /// Resolve the request's worktree (spec 02 §3.3) — before any lock, exactly
    /// as [`search_code`](Self::search_code) does. `None` means the request does
    /// not name an indexed worktree.
    fn resolve_worktree(&self, root: &RequestRoot) -> Result<Option<String>, SearchInfraError> {
        let conn = self
            .state
            .open_read()
            .map_err(SearchInfraError::StateOpen)?;
        match resolve(&conn, root).map_err(SearchInfraError::StateRead)? {
            Resolution::Resolved { worktree_id, .. } => Ok(Some(worktree_id)),
            Resolution::GlobalOnly | Resolution::Ambiguous { .. } => Ok(None),
        }
    }

    /// The pipeline body, run while `L2.read` is held (spec 06 §3).
    async fn run_locked(
        &self,
        worktree_id: &str,
        request: &SearchRequest,
        now_ms: i64,
        observer: &dyn StageObserver,
    ) -> Result<Result<PipelineSnapshot, ErrorEnvelope>, SearchInfraError> {
        observer.on_stage(Stage::ActiveTuple);
        let row = {
            let conn = self
                .state
                .open_read()
                .map_err(SearchInfraError::StateOpen)?;
            projection_state(&conn, worktree_id).map_err(SearchInfraError::StateRead)?
        };
        let Some((generation_id, model_space_id)) = row.and_then(|r| {
            let generation_id = r.active_generation_id?;
            let model_space_id = r.active_model_space_id?;
            Some((generation_id, model_space_id))
        }) else {
            return Ok(Err(ErrorEnvelope::worktree_not_indexed()));
        };

        // Legs per mode (spec 09 §5). A leg the mode does not ask for is not
        // *degraded*, it is simply not run — and it is not run at all: no FTS
        // validation, no provider call, no shard open. `Semantic` never gets
        // here (rejected before the lock).
        let wants_lexical = request.mode.wants_lexical();
        let wants_dense = request.mode.wants_dense();

        observer.on_stage(Stage::FtsLeg);
        let fts_availability = if wants_lexical {
            let outcome = open_and_validate_fts(
                &self.state,
                &self.cache,
                worktree_id,
                ValidationDepth::Cheap,
                now_ms,
            )
            .await
            .map_err(SearchInfraError::Fts)?;
            Some(to_fts_availability(outcome))
        } else {
            None
        };

        observer.on_stage(Stage::DenseLeg);
        let dense_outcome = if wants_dense {
            let worktree_uuid: Uuid = worktree_id
                .parse()
                .map_err(|_| SearchInfraError::CorruptWorktreeId(worktree_id.to_string()))?;
            Some(
                self.dense_leg(
                    worktree_uuid,
                    &generation_id,
                    &model_space_id,
                    request,
                    now_ms,
                )
                .await?,
            )
        } else {
            None
        };

        // `Some(true)` served, `Some(false)` asked for but unavailable, `None`
        // not asked for.
        let lexical_served = fts_availability
            .as_ref()
            .map(|a| matches!(a, FtsAvailability::Valid));
        let dense_served = dense_outcome.as_ref().map(|o| o.available);

        let mut diagnostics = Vec::new();
        // First, because it explains the shape of everything after it: a query
        // that is not in the index's language reaches the dense leg and nothing
        // else, so a reader seeing thin results should see the reason before
        // the leg-level notes.
        if let Some(reason) = &request.query_degraded {
            diagnostics.push(reason.clone());
        }
        if let Some(FtsAvailability::Unavailable(Some(divergence))) = &fts_availability {
            diagnostics.push(divergence.to_string());
        }
        if let Some(diagnostic) = dense_outcome.as_ref().and_then(|o| o.diagnostic.as_ref()) {
            diagnostics.push(diagnostic.clone());
        }

        // No requested leg could serve ⇒ `INDEX_UNAVAILABLE` (spec 02 §6). This
        // generalizes `local_rag_store::requires_index_unavailable`'s
        // both-legs-down predicate over spec 09 §5's single-leg modes, where
        // "the other leg" the caller could have degraded onto was never
        // requested.
        if lexical_served != Some(true) && dense_served != Some(true) {
            return Ok(Err(ErrorEnvelope::index_unavailable(
                diagnostics.join("; "),
            )));
        }

        // The lexical leg (T12-01), still under L2.read. It runs *only* on a
        // validated view: an invalid `fts_projection_head` degrades to
        // `dense_only` with a diagnostic, and an empty FTS is never silently
        // served as a correct lexical result (spec 06 §4 `[FIXED]`).
        observer.on_stage(Stage::LexicalLeg);
        let lexical = if lexical_served == Some(true) {
            let conn = self
                .cache
                .open_read()
                .map_err(SearchInfraError::CacheOpen)?;
            lexical_leg(
                &conn,
                worktree_id,
                &generation_id,
                &LexicalQuery::new(
                    &request.query,
                    request.name_pattern.as_deref(),
                    request.limit,
                ),
            )
            .map_err(SearchInfraError::CacheRead)?
        } else {
            Vec::new()
        };
        let dense: Vec<DenseHit> = dense_outcome.map(|o| o.hits).unwrap_or_default();

        // `degraded` means "you got less than you asked for" (spec 09 §7), so a
        // single-leg mode whose one leg served is **not** degraded — nothing was
        // skipped. Only a mode that asked for both and got one reports which.
        let degraded = match (lexical_served, dense_served) {
            (Some(true), Some(false)) => Some(DegradedMode::LexicalOnly),
            (Some(false), Some(true)) => Some(DegradedMode::DenseOnly),
            _ => None,
        };

        // Enrichment is still a stub for the graph/parent-unit half (T12-04);
        // the presentable metadata spec 09 §7 lists is assembled below.
        observer.on_stage(Stage::Enrichment);

        let fused = rrf(&lexical, &dense, request.limit, self.fusion_weights);
        let response = self.build_response(&generation_id, fused, degraded, &mut diagnostics)?;

        Ok(Ok(PipelineSnapshot {
            worktree_id: worktree_id.to_string(),
            model_space_id,
            lexical,
            dense,
            response,
        }))
    }

    /// Turn fused hits into spec 09 §7's canonical response: presentable
    /// metadata per result, the generation reference, and the degraded /
    /// diagnostics envelope.
    fn build_response(
        &self,
        generation_id: &str,
        fused: Vec<FusedHit>,
        degraded: Option<DegradedMode>,
        diagnostics: &mut Vec<String>,
    ) -> Result<SearchResponse, SearchInfraError> {
        let conn = self
            .state
            .open_read()
            .map_err(SearchInfraError::StateOpen)?;

        let number = generation_number(&conn, generation_id)
            .map_err(SearchInfraError::StateRead)?
            .ok_or_else(|| SearchInfraError::MissingGeneration(generation_id.to_string()))?;

        let ids: Vec<&str> = fused.iter().map(|f| f.occurrence_id.as_str()).collect();
        let metadata =
            occurrences_by_id(&conn, generation_id, &ids).map_err(SearchInfraError::StateRead)?;
        let mut by_id: HashMap<&str, &OccurrenceMetadata> = metadata
            .iter()
            .map(|m| (m.occurrence_id.as_str(), m))
            .collect();

        let mut ordered: Vec<&OccurrenceMetadata> = Vec::with_capacity(fused.len());
        let mut kept: Vec<&FusedHit> = Vec::with_capacity(fused.len());
        let mut dropped = 0usize;
        for hit in &fused {
            // Structurally impossible under a held `L2.read` (a generation's
            // occurrence set is immutable once `projection_ready`, spec 03
            // §1.2), so this is a corruption signal rather than a normal miss —
            // reported, never papered over with empty fields.
            let Some(meta) = by_id.remove(hit.occurrence_id.as_str()) else {
                dropped += 1;
                continue;
            };
            ordered.push(meta);
            kept.push(hit);
        }
        if dropped > 0 {
            diagnostics.push(format!(
                "{dropped} fused hit(s) had no occurrence row in generation {generation_id} \
                 and were dropped"
            ));
        }

        // Snippets come from the stored `source_blob`, never the live file
        // (spec 09 §7 `[FIXED]`), batched one read per revision (T12-04).
        let (snippets, snippet_diagnostics) =
            cut_batch(&conn, &ordered).map_err(SearchInfraError::StateRead)?;
        diagnostics.extend(snippet_diagnostics);

        let results = kept
            .into_iter()
            .zip(ordered)
            .zip(snippets)
            .map(|((hit, meta), snippet)| SearchResult {
                occurrence_id: hit.occurrence_id.clone(),
                path: meta.normalized_path.clone(),
                name: meta.local_name.clone().unwrap_or_default(),
                qualified_name: meta.qualified_name.clone(),
                unit_kind: meta.unit_kind.as_str().to_string(),
                span: [meta.span_start, meta.span_end],
                language: meta.language.clone(),
                score: hit.score,
                legs: hit.legs,
                snippet,
            })
            .collect();

        Ok(SearchResponse {
            results,
            generation: GenerationRef {
                id: generation_id.to_string(),
                number,
            },
            degraded,
            diagnostics: std::mem::take(diagnostics),
        })
    }

    /// The dense leg (spec 09 §3) — T12-02.
    ///
    /// Every failure here is a **degradation**, never an error: the caller turns
    /// `available == false` into `degraded: lexical_only` plus the diagnostic
    /// (spec 02 §6), exactly as an unopenable shard already did. The infra
    /// `Result` is reserved for "the store itself would not answer".
    async fn dense_leg(
        &self,
        worktree_id: Uuid,
        generation_id: &str,
        model_space_id: &str,
        request: &SearchRequest,
        now_ms: i64,
    ) -> Result<DenseOutcome, SearchInfraError> {
        let (Ok(generation_uuid), Ok(model_space_uuid)) = (
            generation_id.parse::<Uuid>(),
            model_space_id.parse::<Uuid>(),
        ) else {
            // Structurally unreachable (both are minted through `UuidSource`),
            // but this crate cannot enforce that, so it degrades rather than
            // panicking.
            return Ok(DenseOutcome::unavailable(format!(
                "active tuple has a non-UUID id: generation {generation_id:?}, \
                 model space {model_space_id:?}"
            )));
        };

        // The query's representation IS the active model space's representation
        // of the searched kind (spec 09 §3): the same `model_id`/`dimensions`/
        // `distance_metric` its points were embedded and its shard was opened
        // with. Resolved under the same `L2.read` as the tuple it belongs to.
        let (key, required_kinds) = {
            let conn = self
                .state
                .open_read()
                .map_err(SearchInfraError::StateOpen)?;
            let key = match representation_key_for(
                &conn,
                &model_space_uuid,
                projection_kind_to_store(self.dense_kind),
            ) {
                Ok(key) => key,
                Err(e) => return Ok(DenseOutcome::unavailable(e.to_string())),
            };
            let kinds = match required_code_kinds(&conn, &model_space_uuid) {
                Ok(kinds) => kinds,
                Err(e) => return Ok(DenseOutcome::unavailable(e.to_string())),
            };
            (key, kinds)
        };

        // A query with no text has nothing to embed. That is an empty leg, not
        // an unavailable one — mirroring the lexical leg's own termless case, so
        // an empty query never reports itself as a degraded search.
        if request.query.trim().is_empty() {
            return Ok(DenseOutcome::empty());
        }

        let vector = match self.embedder.embed_query(&request.query, &key) {
            Ok(vector) => vector,
            Err(e) => return Ok(DenseOutcome::unavailable(e.to_string())),
        };
        if vector.len() != key.dimensions as usize {
            return Ok(DenseOutcome::unavailable(format!(
                "query embedding has {} dimensions, representation {} expects {}",
                vector.len(),
                key.model_id,
                key.dimensions
            )));
        }

        // The shard holds a point per (occurrence × required representation
        // kind), but the dense leg reads exactly one of them — `code_raw` unless
        // overridden (spec 09 §3; the content-vs-context choice is `[OPEN]`,
        // decided by the benchmark). The backend
        // cannot filter by kind — brute-force has no payload predicate at all
        // (ADR-0003 records `filtered_hnsw_available = false`) — so the leg
        // over-fetches by the number of required kinds and filters below.
        // Without the factor, other kinds' points would eat the depth budget
        // before the filter ever saw them.
        let depth = candidate_depth(request.limit);
        let over_fetch = depth.saturating_mul(required_kinds.len().max(1));

        let handle = match self.shards.acquire(worktree_id, now_ms).await {
            Ok(handle) => handle,
            Err(e) => return Ok(DenseOutcome::unavailable(e.to_string())),
        };

        // `point_id` is a one-way digest (spec 05 §3), so the occurrence behind
        // a hit is recovered by re-deriving the expected set for the active
        // tuple — the very function the switch already uses to decide what
        // belongs in this shard (`expected_points`), which also carries each
        // point's `representation_kind` and is therefore the kind filter as well
        // as the reverse map.
        let by_point: HashMap<String, String> = {
            let conn = self
                .state
                .open_read()
                .map_err(SearchInfraError::StateOpen)?;
            match expected_points(&conn, &worktree_id, &generation_uuid, &model_space_uuid) {
                Ok(points) => points
                    .into_iter()
                    .filter(|p| p.representation_kind == self.dense_kind)
                    .map(|p| (p.point_id.as_str().to_string(), p.occurrence_id))
                    .collect(),
                Err(e) => return Ok(DenseOutcome::unavailable(e.to_string())),
            }
        };

        let scored = match handle.search(&DenseQuery {
            vector: vector.clone(),
            k: over_fetch,
        }) {
            Ok(scored) => scored,
            Err(e) => return Ok(DenseOutcome::unavailable(e.to_string())),
        };
        let window_was_full = scored.len() >= over_fetch;
        let mut hits = to_dense_hits(scored, &by_point, depth);

        // Over-fetching by the kind count is a heuristic, not a proof: if the
        // window came back full *and* still yielded fewer than `depth` `code_raw`
        // hits, the truncation may have hidden some behind other kinds' points.
        // Ask once more for the whole shard — for a linear-scan backend that is
        // the same scan, only a larger sort — so the leg's depth is guaranteed
        // rather than probabilistic. Bounded at exactly two backend calls.
        if window_was_full && hits.len() < depth {
            let total = match handle.point_count() {
                Ok(total) => total as usize,
                Err(e) => return Ok(DenseOutcome::unavailable(e.to_string())),
            };
            if total > over_fetch {
                match handle.search(&DenseQuery { vector, k: total }) {
                    Ok(scored) => hits = to_dense_hits(scored, &by_point, depth),
                    Err(e) => return Ok(DenseOutcome::unavailable(e.to_string())),
                }
            }
        }

        Ok(DenseOutcome {
            hits,
            available: true,
            diagnostic: None,
        })
    }
}

/// The worktree's active generation, or `None` when it has never been indexed
/// (spec 04 §2's `worktree_projection_state`).
fn active_generation(
    conn: &local_rag_store::rusqlite::Connection,
    worktree_id: &str,
) -> Result<Option<String>, SearchInfraError> {
    Ok(projection_state(conn, worktree_id)
        .map_err(SearchInfraError::StateRead)?
        .and_then(|row| row.active_generation_id))
}

/// Keep the `code_raw` hits (those present in `by_point`), map them to their
/// occurrences, cut at `depth` and number them from 1 — the backend already
/// returned them best-first (spec 05 §1's ranking contract).
fn to_dense_hits(
    scored: Vec<ScoredPoint>,
    by_point: &HashMap<String, String>,
    depth: usize,
) -> Vec<DenseHit> {
    scored
        .into_iter()
        .filter_map(|s| {
            by_point
                .get(s.point_id.as_str())
                .map(|occurrence_id| (occurrence_id.clone(), s.score))
        })
        .take(depth)
        .enumerate()
        .map(|(i, (occurrence_id, score))| DenseHit {
            occurrence_id,
            rank: i + 1,
            score,
        })
        .collect()
}

/// The dense leg's result plus whether the leg itself was serviceable.
struct DenseOutcome {
    hits: Vec<DenseHit>,
    available: bool,
    diagnostic: Option<String>,
}

impl DenseOutcome {
    /// The leg ran and found nothing — still a healthy leg.
    fn empty() -> Self {
        Self {
            hits: Vec::new(),
            available: true,
            diagnostic: None,
        }
    }

    /// The leg could not run; the caller degrades to `lexical_only` and reports
    /// `why`.
    fn unavailable(why: impl Into<String>) -> Self {
        Self {
            hits: Vec::new(),
            available: false,
            diagnostic: Some(why.into()),
        }
    }
}

/// Map the FTS validator's outcome to the availability the RRF/degraded
/// decision needs (spec 02 §6).
fn to_fts_availability(outcome: FtsOpenOutcome) -> FtsAvailability {
    match outcome {
        // Structurally shouldn't happen once an active tuple was confirmed
        // (commit_switch sets `worktree.current_generation_id` and
        // `worktree_projection_state.active_*` in the same transaction) —
        // treated as unavailable rather than panicking, since this crate
        // cannot enforce that invariant itself.
        FtsOpenOutcome::NoActiveGeneration => FtsAvailability::Unavailable(None),
        FtsOpenOutcome::Valid | FtsOpenOutcome::Rebuilt(_) => FtsAvailability::Valid,
        FtsOpenOutcome::DeferredBackground { divergence, .. } => {
            FtsAvailability::Unavailable(Some(divergence))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use local_rag_store::{FtsDivergence, FtsMaterializeOutcome};

    #[test]
    fn fts_outcome_maps_to_availability() {
        assert_eq!(
            to_fts_availability(FtsOpenOutcome::NoActiveGeneration),
            FtsAvailability::Unavailable(None)
        );
        assert_eq!(
            to_fts_availability(FtsOpenOutcome::Valid),
            FtsAvailability::Valid
        );
        assert_eq!(
            to_fts_availability(FtsOpenOutcome::Rebuilt(FtsMaterializeOutcome {
                occurrence_count: 1,
                manifest_hash: "h".to_string(),
            })),
            FtsAvailability::Valid
        );
        assert_eq!(
            to_fts_availability(FtsOpenOutcome::DeferredBackground {
                divergence: FtsDivergence::HeadMissing,
                occurrence_count_estimate: 5001,
            }),
            FtsAvailability::Unavailable(Some(FtsDivergence::HeadMissing))
        );
    }

    #[test]
    fn degraded_mode_matches_leg_availability() {
        fn degraded(fts_available: bool, dense_available: bool) -> Option<DegradedMode> {
            match (fts_available, dense_available) {
                (true, true) => None,
                (true, false) => Some(DegradedMode::LexicalOnly),
                (false, true) => Some(DegradedMode::DenseOnly),
                (false, false) => unreachable!(),
            }
        }
        assert_eq!(degraded(true, true), None);
        assert_eq!(degraded(true, false), Some(DegradedMode::LexicalOnly));
        assert_eq!(degraded(false, true), Some(DegradedMode::DenseOnly));
    }
}
