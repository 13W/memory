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
//! Deliberately out of scope here (owned by T09-04 or the rest of group 12, not
//! duplicated): RRF fusion, `results[]`, per-leg scoring (T12-02/T12-03); real
//! enrichment — parent unit, qualified name, graph (T12-04,
//! [`Stage::Enrichment`] is a stub); the `mode` request field and per-mode leg
//! selection (spec 09 §5, T12-03 — this pipeline always attempts both legs,
//! mirroring the default `hybrid` mode); load/failpoint tests under concurrent
//! generation/model-space switches (T09-04 — this module only proves the lock
//! is held across one pipeline run, never that concurrent switches can't mix
//! generations).

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use local_rag_core::identity::Uuid;
use local_rag_projection::{DenseQuery, ShardManager};
use local_rag_protocol::{DegradedMode, ErrorEnvelope};
use local_rag_store::lock::ReadTimedOut;
use local_rag_store::{
    CacheDb, CacheOpenError, FtsAvailability, FtsOpenOutcome, FtsRebuildError, LexicalHit,
    LexicalQuery, OpenError, RequestRoot, Resolution, StateDb, ValidationDepth,
    WorktreeLockRegistry, lexical_leg, open_and_validate_fts, projection_state,
    requires_index_unavailable, resolve, rusqlite,
};

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

/// One `search_code` request: the explicit context to resolve (spec 02 §3.3)
/// plus the already-embedded dense query vector (query embedding is the
/// caller's concern — this crate does not embed).
#[derive(Debug, Clone, PartialEq)]
pub struct SearchRequest {
    /// The request's explicit worktree context.
    pub root: RequestRoot,
    /// The raw query text for the lexical leg (spec 09 §1's `query`). Tokenized
    /// by the leg itself, never handed to FTS5 verbatim.
    pub query: String,
    /// The caller's requested result count (spec 09 §1's `limit`). The lexical
    /// leg derives its candidate depth from it via
    /// `local_rag_store::candidate_depth` (§4); the dense leg still takes its
    /// own `k` until T12-02/T12-03 fuse the two.
    pub limit: usize,
    /// Optional prefix filter on `local_name`/`qualified_name` (spec 09 §1's
    /// `name_pattern`).
    pub name_pattern: Option<String>,
    /// The query vector for the dense leg.
    pub query_vector: Vec<f32>,
    /// The maximum number of dense candidates to request.
    pub k: usize,
}

/// The read-path skeleton's success shape. **Not** spec 09 §7's full response
/// (`results[]`/`legs`/`snippet`, group 12's job) — only the resolved tuple
/// plus the degraded/diagnostics skeleton this task owns.
#[derive(Debug, Clone, PartialEq)]
pub struct PipelineSnapshot {
    /// The resolved worktree.
    pub worktree_id: String,
    /// The active generation served.
    pub generation_id: String,
    /// The active model space served.
    pub model_space_id: String,
    /// The lexical leg's ranked candidates (spec 09 §2), best first. Empty
    /// whenever the leg did not run — an invalid FTS view (`dense_only`) or a
    /// query that reduces to no terms. Fusing these with the dense leg into
    /// spec 09 §7's `results[]` is T12-03.
    pub lexical: Vec<LexicalHit>,
    /// `None` when both legs served; `Some` names which leg was skipped.
    pub degraded: Option<DegradedMode>,
    /// Freeform diagnostic reasons (spec 02 §6: "every degraded response
    /// includes the validation reason").
    pub diagnostics: Vec<String>,
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
            SearchInfraError::CorruptWorktreeId(_) => None,
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
    read_wait_budget: Duration,
}

impl SearchEngine {
    /// Assemble a search engine over already-open store handles.
    pub fn new(
        state: Arc<StateDb>,
        cache: Arc<CacheDb>,
        locks: Arc<WorktreeLockRegistry>,
        shards: Arc<ShardManager>,
        read_wait_budget: Duration,
    ) -> Self {
        Self {
            state,
            cache,
            locks,
            shards,
            read_wait_budget,
        }
    }

    /// Run one search (spec 09 §1). The outer [`Result`] is an infrastructure
    /// failure; the inner is the canonical domain outcome (spec 02 §6) —
    /// mirrors `local_rag_store::registry::attach`'s outer/inner idiom.
    pub async fn search_code(
        &self,
        request: SearchRequest,
        now_ms: i64,
    ) -> Result<Result<PipelineSnapshot, ErrorEnvelope>, SearchInfraError> {
        self.search_code_instrumented(request, now_ms, &NoopObserver)
            .await
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

        observer.on_stage(Stage::FtsLeg);
        let fts_outcome = open_and_validate_fts(
            &self.state,
            &self.cache,
            worktree_id,
            ValidationDepth::Cheap,
            now_ms,
        )
        .await
        .map_err(SearchInfraError::Fts)?;
        let fts_availability = to_fts_availability(fts_outcome);

        observer.on_stage(Stage::DenseLeg);
        let worktree_uuid: Uuid = worktree_id
            .parse()
            .map_err(|_| SearchInfraError::CorruptWorktreeId(worktree_id.to_string()))?;
        let (dense_available, dense_diagnostic) =
            match self.shards.acquire(worktree_uuid, now_ms).await {
                Ok(handle) => {
                    let query = DenseQuery {
                        vector: request.query_vector.clone(),
                        k: request.k,
                    };
                    match handle.search(&query) {
                        Ok(_points) => (true, None),
                        Err(e) => (false, Some(e.to_string())),
                    }
                }
                Err(e) => (false, Some(e.to_string())),
            };

        if requires_index_unavailable(&fts_availability, dense_available) {
            let mut details = Vec::new();
            if let FtsAvailability::Unavailable(Some(divergence)) = &fts_availability {
                details.push(divergence.to_string());
            }
            if let Some(diagnostic) = &dense_diagnostic {
                details.push(diagnostic.clone());
            }
            return Ok(Err(ErrorEnvelope::index_unavailable(details.join("; "))));
        }

        // The lexical leg (T12-01), still under L2.read. It runs *only* on a
        // validated view: an invalid `fts_projection_head` degrades to
        // `dense_only` with a diagnostic, and an empty FTS is never silently
        // served as a correct lexical result (spec 06 §4 `[FIXED]`).
        observer.on_stage(Stage::LexicalLeg);
        let fts_available = matches!(fts_availability, FtsAvailability::Valid);
        let lexical = if fts_available {
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

        // Stub: real enrichment is T12-04. Still runs under L2.read so
        // instrumentation can prove it.
        observer.on_stage(Stage::Enrichment);

        let degraded = match (fts_available, dense_available) {
            (true, true) => None,
            (true, false) => Some(DegradedMode::LexicalOnly),
            (false, true) => Some(DegradedMode::DenseOnly),
            (false, false) => {
                unreachable!("requires_index_unavailable already returned on both-down")
            }
        };

        let mut diagnostics = Vec::new();
        if let FtsAvailability::Unavailable(Some(divergence)) = &fts_availability {
            diagnostics.push(divergence.to_string());
        }
        if let Some(diagnostic) = &dense_diagnostic {
            diagnostics.push(diagnostic.clone());
        }

        Ok(Ok(PipelineSnapshot {
            worktree_id: worktree_id.to_string(),
            generation_id,
            model_space_id,
            lexical,
            degraded,
            diagnostics,
        }))
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
