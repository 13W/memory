//! The six MCP status/memory-read tool adapters: `stats`/`health`/`recall`/
//! `list_memory`/`list_memory_candidates`/`inspect_memory_evidence` — T15-04.
//! Same shape as [`super::code`]'s three adapters: parse args, call a domain
//! function against [`MemoryContext`]'s already-open connections, map the
//! outcome into a [`CallToolResult`]. Every tool here is read-only — none
//! ever opens a write transaction (`MemoryContext::state`/`cache` only ever
//! calls `open_read`/`writer().queue_capacity()`, never `writer().
//! transaction(..)`).

use serde::Serialize;
use serde_json::{Map, Value};

use local_rag_memory::recall as recall_pipeline;
use local_rag_protocol::ErrorEnvelope;
use local_rag_store::{
    CandidateState, MemoryEntryRow, MemoryKind, MemoryState, ProposedOperation, RequestRoot,
    Resolution, STUCK_RUN_ATTEMPT_THRESHOLD, ScopeKind, consolidation_run_counts, list_candidates,
    list_memory_entries_for_scope, memory_entry_counts, memory_evidence_for,
    observation_envelope_count, observations_applied_since, oldest_open_run_created_at,
    pending_candidate_counts, projection_state, resolve, store_instance_uuid,
    stuck_consolidation_runs, total_pending_backlog,
};

use crate::daemon::memory::MemoryContext;
use crate::daemon::mode::DaemonMode;

use super::content::{self, CallToolResult};
use super::tools::{
    DEFAULT_LIST_LIMIT, DEFAULT_RECALL_LIMIT, MAX_LIST_LIMIT, MAX_RECALL_LIMIT,
    reject_unknown_keys, require_string,
};

/// Every read here goes through this: an open failure or a `rusqlite::Error`
/// is `INDEX_UNAVAILABLE` (spec 02 §6's own name for "the index cannot serve
/// this request") — never a JSON-RPC `-32603`, mirroring `code.rs`'s
/// `infra_err` precedent for `SearchInfraError`, generalized to any
/// `Display` error these plain SQL reads can produce.
pub(super) fn infra_err(e: impl std::fmt::Display) -> CallToolResult {
    content::err(&ErrorEnvelope::index_unavailable(e.to_string()))
}

pub(super) fn optional_enum<T: Copy>(
    args: &Map<String, Value>,
    key: &str,
    parse: impl Fn(&str) -> Option<T>,
    allowed_desc: &str,
) -> Result<Option<T>, String> {
    match args.get(key) {
        None => Ok(None),
        Some(Value::String(s)) => parse(s)
            .map(Some)
            .ok_or_else(|| format!("{key} must be one of {allowed_desc}, got {s:?}")),
        Some(_) => Err(format!("{key} must be a string")),
    }
}

fn optional_limit(args: &Map<String, Value>, default: i64, max: i64) -> Result<i64, String> {
    match args.get("limit") {
        None => Ok(default),
        Some(Value::Number(n)) => n
            .as_i64()
            .filter(|v| (1..=max).contains(v))
            .ok_or_else(|| format!("limit must be an integer between 1 and {max}")),
        Some(_) => Err("limit must be an integer".to_string()),
    }
}

fn optional_offset(args: &Map<String, Value>) -> Result<i64, String> {
    match args.get("offset") {
        None => Ok(0),
        Some(Value::Number(n)) => n
            .as_i64()
            .filter(|v| *v >= 0)
            .ok_or_else(|| "offset must be a non-negative integer".to_string()),
        Some(_) => Err("offset must be an integer".to_string()),
    }
}

// ---------------------------------------------------------------------------
// recall
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct RecallResultEntryWire {
    memory_id: String,
    kind: String,
    state: String,
    confidence: f64,
    text: String,
}

impl From<recall_pipeline::RecallResultEntry> for RecallResultEntryWire {
    fn from(e: recall_pipeline::RecallResultEntry) -> Self {
        RecallResultEntryWire {
            memory_id: e.memory_id,
            kind: e.kind.as_str().to_string(),
            state: e.state.as_str().to_string(),
            confidence: e.confidence,
            text: e.text,
        }
    }
}

#[derive(Debug, Serialize)]
struct RecallResult {
    scope: String,
    additional_context: String,
    entries: Vec<RecallResultEntryWire>,
    candidate_count: usize,
    truncated: bool,
    dense_degraded: Option<String>,
}

/// Human-readable degradation reason (T15-04, `[SPEC]` — no wire shape for
/// this exists yet; a plain string is enough for a diagnostic field, the
/// same precedent `ErrorEnvelope.details` already sets).
fn dense_degraded_label(d: &recall_pipeline::DenseLegUnavailable) -> String {
    match d {
        recall_pipeline::DenseLegUnavailable::NoRepresentation => "no_representation".to_string(),
        recall_pipeline::DenseLegUnavailable::EmbedFailed(e) => {
            format!("embed_failed: {}", e.reason)
        }
        recall_pipeline::DenseLegUnavailable::DimensionMismatch { expected, got } => {
            format!("dimension_mismatch: expected {expected} got {got}")
        }
    }
}

pub async fn recall(
    ctx: &MemoryContext,
    root: RequestRoot,
    args: &Map<String, Value>,
) -> Result<CallToolResult, String> {
    reject_unknown_keys(args, &["query", "limit"])?;
    // Deliberately not `require_string`: an absent/empty query is legal
    // termless recall (spec 08 §6) — the hook's own `SessionStart` case.
    let query = match args.get("query") {
        None => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(_) => return Err("query must be a string".to_string()),
    };
    let limit = optional_limit(args, DEFAULT_RECALL_LIMIT, MAX_RECALL_LIMIT)?;

    let state_read = match ctx.state.open_read() {
        Ok(c) => c,
        Err(e) => return Ok(infra_err(e)),
    };
    let cache_read = match ctx.cache.open_read() {
        Ok(c) => c,
        Err(e) => return Ok(infra_err(e)),
    };

    let request = recall_pipeline::RecallRequest {
        root,
        query: &query,
    };
    let outcome = match recall_pipeline::recall(
        &state_read,
        &cache_read,
        ctx.embedder.as_ref(),
        ctx.dense_backend.as_ref(),
        &request,
        ctx.recall_token_budget,
    ) {
        Ok(o) => o,
        Err(e) => return Ok(infra_err(e)),
    };

    let mut entries: Vec<RecallResultEntryWire> = outcome
        .entries
        .into_iter()
        .map(RecallResultEntryWire::from)
        .collect();
    entries.truncate(limit as usize);

    Ok(content::ok(&RecallResult {
        scope: outcome.scope_label,
        additional_context: outcome.additional_context,
        entries,
        candidate_count: outcome.candidate_count,
        truncated: outcome.truncated,
        dense_degraded: outcome.dense_degraded.as_ref().map(dense_degraded_label),
    }))
}

// ---------------------------------------------------------------------------
// list_memory
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct MemoryEntryWire {
    memory_id: String,
    kind: String,
    state: String,
    text: String,
    canonical_key: Option<String>,
    scope_kind: String,
    scope_owner_id: String,
    confidence: f64,
    importance: f64,
    valid_from_tree: Option<String>,
    last_verified_tree: Option<String>,
    supersedes_id: Option<String>,
    entry_version: i64,
    created_at: i64,
    updated_at: i64,
}

impl From<MemoryEntryRow> for MemoryEntryWire {
    fn from(row: MemoryEntryRow) -> Self {
        MemoryEntryWire {
            memory_id: row.memory_id,
            kind: row.kind.as_str().to_string(),
            state: row.state.as_str().to_string(),
            text: row.text,
            canonical_key: row.canonical_key,
            scope_kind: row.scope_kind.as_str().to_string(),
            scope_owner_id: row.scope_owner_id,
            confidence: row.confidence,
            importance: row.importance,
            valid_from_tree: row.valid_from_tree,
            last_verified_tree: row.last_verified_tree,
            supersedes_id: row.supersedes_id,
            entry_version: row.entry_version,
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
}

#[derive(Debug, Serialize)]
struct ListMemoryResult {
    scope: String,
    entries: Vec<MemoryEntryWire>,
    has_more: bool,
}

pub async fn list_memory(
    ctx: &MemoryContext,
    root: RequestRoot,
    args: &Map<String, Value>,
) -> Result<CallToolResult, String> {
    reject_unknown_keys(args, &["kind", "state", "scope", "limit", "offset"])?;
    let kind_filter = optional_enum(
        args,
        "kind",
        MemoryKind::from_db,
        "fact/decision/convention/procedure/task/question/hypothesis",
    )?;
    // Deliberately no default exclusion of terminal states (spec 04 §5:
    // "remain queryable via review tools") — `state` narrows only when the
    // caller supplies it.
    let state_filter = optional_enum(
        args,
        "state",
        MemoryState::from_db,
        "active/resolved/retracted/confirmed/rejected/superseded",
    )?;
    let scope_filter = optional_enum(
        args,
        "scope",
        ScopeKind::from_db,
        "global/repository/worktree",
    )?;
    let limit = optional_limit(args, DEFAULT_LIST_LIMIT, MAX_LIST_LIMIT)?;
    let offset = optional_offset(args)?;

    let state_read = match ctx.state.open_read() {
        Ok(c) => c,
        Err(e) => return Ok(infra_err(e)),
    };

    // Spec 02 §6: an unresolved/unknown worktree is never an error for
    // memory tools — `resolve` degrades to `GlobalOnly` structurally, and
    // `scopes_for` folds that (and `Ambiguous`) to the global scope alone.
    let resolution = match resolve(&state_read, &root) {
        Ok(r) => r,
        Err(e) => return Ok(infra_err(e)),
    };
    let (scope_label, scopes) = recall_pipeline::scopes_for(&resolution);
    let scopes: Vec<(ScopeKind, String)> = match scope_filter {
        Some(wanted) => scopes.into_iter().filter(|(k, _)| *k == wanted).collect(),
        None => scopes,
    };

    let mut combined: Vec<MemoryEntryRow> = Vec::new();
    for (kind, owner) in &scopes {
        match list_memory_entries_for_scope(&state_read, *kind, owner, kind_filter, state_filter) {
            Ok(rows) => combined.extend(rows),
            Err(e) => return Ok(infra_err(e)),
        }
    }
    // Re-sort the union: each per-scope query is already (created_at,
    // memory_id) ordered, but the union across scopes is not.
    combined.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.memory_id.cmp(&b.memory_id))
    });

    let total = combined.len();
    let offset_usize = usize::try_from(offset).unwrap_or(usize::MAX);
    let limit_usize = limit as usize;
    let has_more = total > offset_usize.saturating_add(limit_usize);
    let entries: Vec<MemoryEntryWire> = combined
        .into_iter()
        .skip(offset_usize)
        .take(limit_usize)
        .map(MemoryEntryWire::from)
        .collect();

    Ok(content::ok(&ListMemoryResult {
        scope: scope_label,
        entries,
        has_more,
    }))
}

// ---------------------------------------------------------------------------
// list_memory_candidates (global-only: pending_memory_candidate has no
// scope column at all — the literal "global-only behavior where applicable"
// the task card names)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct CandidateWire {
    candidate_id: String,
    proposed_operation: ProposedOperation,
    conflicts: Vec<String>,
    review_state: String,
    created_at: i64,
}

#[derive(Debug, Serialize)]
struct ListCandidatesResult {
    candidates: Vec<CandidateWire>,
    has_more: bool,
}

pub async fn list_memory_candidates(
    ctx: &MemoryContext,
    args: &Map<String, Value>,
) -> Result<CallToolResult, String> {
    reject_unknown_keys(args, &["state", "limit", "offset"])?;
    let state_filter = optional_enum(
        args,
        "state",
        CandidateState::from_db,
        "pending/approved/rejected/expired",
    )?;
    let limit = optional_limit(args, DEFAULT_LIST_LIMIT, MAX_LIST_LIMIT)?;
    let offset = optional_offset(args)?;

    let state_read = match ctx.state.open_read() {
        Ok(c) => c,
        Err(e) => return Ok(infra_err(e)),
    };
    // Over-fetch by one to detect has_more without a second COUNT(*) query
    // (`list_candidates`'s own doc: "a caller wanting to detect 'more rows
    // exist' over-fetches by one and slices the extra row off itself").
    let mut rows = match list_candidates(&state_read, state_filter, limit + 1, offset) {
        Ok(r) => r,
        Err(e) => return Ok(infra_err(e)),
    };
    let has_more = rows.len() as i64 > limit;
    rows.truncate(limit as usize);

    let mut candidates = Vec::with_capacity(rows.len());
    for row in rows {
        let proposed_operation: ProposedOperation =
            match serde_json::from_str(&row.proposed_operation) {
                Ok(op) => op,
                Err(e) => {
                    return Ok(infra_err(format!(
                        "corrupt proposed_operation for candidate {}: {e}",
                        row.candidate_id
                    )));
                }
            };
        let conflicts: Vec<String> = match &row.conflicts {
            None => Vec::new(),
            Some(raw) => match serde_json::from_str(raw) {
                Ok(v) => v,
                Err(e) => {
                    return Ok(infra_err(format!(
                        "corrupt conflicts for candidate {}: {e}",
                        row.candidate_id
                    )));
                }
            },
        };
        candidates.push(CandidateWire {
            candidate_id: row.candidate_id,
            proposed_operation,
            conflicts,
            review_state: row.review_state.as_str().to_string(),
            created_at: row.created_at,
        });
    }

    Ok(content::ok(&ListCandidatesResult {
        candidates,
        has_more,
    }))
}

// ---------------------------------------------------------------------------
// inspect_memory_evidence
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct InspectEvidenceResult {
    memory_id: String,
    observation_ids: Vec<String>,
}

pub async fn inspect_memory_evidence(
    ctx: &MemoryContext,
    args: &Map<String, Value>,
) -> Result<CallToolResult, String> {
    reject_unknown_keys(args, &["memory_id"])?;
    let memory_id = require_string(args, "memory_id")?;

    let state_read = match ctx.state.open_read() {
        Ok(c) => c,
        Err(e) => return Ok(infra_err(e)),
    };
    // An unknown memory_id is an empty list, not an error -- the same
    // "domain answer, not a caller mistake" idiom `PATH_NOT_INDEXED` uses
    // elsewhere for "well-formed but names nothing".
    let observation_ids = match memory_evidence_for(&state_read, &memory_id) {
        Ok(ids) => ids,
        Err(e) => return Ok(infra_err(e)),
    };
    Ok(content::ok(&InspectEvidenceResult {
        memory_id,
        observation_ids,
    }))
}

// ---------------------------------------------------------------------------
// stats
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct MemoryCountWire {
    kind: String,
    state: String,
    count: i64,
}

impl From<local_rag_store::MemoryCountRow> for MemoryCountWire {
    fn from(r: local_rag_store::MemoryCountRow) -> Self {
        MemoryCountWire {
            kind: r.kind.as_str().to_string(),
            state: r.state.as_str().to_string(),
            count: r.count,
        }
    }
}

#[derive(Debug, Serialize)]
struct CandidateCountWire {
    state: String,
    count: i64,
}

impl From<local_rag_store::CandidateCountRow> for CandidateCountWire {
    fn from(r: local_rag_store::CandidateCountRow) -> Self {
        CandidateCountWire {
            state: r.state.as_str().to_string(),
            count: r.count,
        }
    }
}

#[derive(Debug, Serialize)]
struct MemoryStatsWire {
    entries_by_kind_state: Vec<MemoryCountWire>,
    pending_candidates_by_state: Vec<CandidateCountWire>,
}

/// D-049: `consolidation_run` count-by-state bucket, the run twin of
/// [`MemoryCountWire`]/[`CandidateCountWire`].
#[derive(Debug, Serialize)]
struct RunCountWire {
    state: String,
    count: i64,
}

impl From<local_rag_store::RunCountRow> for RunCountWire {
    fn from(r: local_rag_store::RunCountRow) -> Self {
        RunCountWire {
            state: r.state.as_str().to_string(),
            count: r.count,
        }
    }
}

/// D-071: one consolidation run that needs a human — retried without
/// converging, or dead-lettered on the running build. The MCP twin of
/// `cli::stats`'s own block; both surfaces read the same store primitive.
#[derive(Debug, Serialize)]
struct StuckRunWire {
    run_id: String,
    session_id: String,
    attempt_count: i64,
    dead_lettered: bool,
    last_failure_kind: Option<String>,
    last_failure_reason: Option<String>,
    from_received_seq: i64,
    to_received_seq: i64,
}

impl From<local_rag_store::StuckRunRow> for StuckRunWire {
    fn from(r: local_rag_store::StuckRunRow) -> Self {
        StuckRunWire {
            run_id: r.run_id,
            session_id: r.session_id,
            attempt_count: r.attempt_count,
            dead_lettered: r.dead_lettered,
            last_failure_kind: r.last_failure_kind,
            last_failure_reason: r.last_failure_reason,
            from_received_seq: r.from_received_seq,
            to_received_seq: r.to_received_seq,
        }
    }
}

/// D-049: the observations pillar (`01-overview.md` §5-9), previously
/// unreported by `stats()` entirely.
#[derive(Debug, Serialize)]
struct ObservationStatsWire {
    total: i64,
}

/// D-049: consolidation backlog + a best-effort progress/ETA estimate.
/// `progress_pct`/`eta_seconds` are honestly `null` when unmeasurable (empty
/// store, zero throughput, zero backlog) rather than a fabricated number.
#[derive(Debug, Serialize)]
struct ConsolidationStatsWire {
    runs_by_state: Vec<RunCountWire>,
    pending_backlog_total: i64,
    progress_pct: Option<f64>,
    throughput_observations_per_min: f64,
    eta_seconds: Option<i64>,
    oldest_pending_run_created_at: Option<i64>,
    /// D-071: empty on a healthy store; every entry is a run no amount of
    /// waiting will fix on its own.
    stuck_runs: Vec<StuckRunWire>,
}

#[derive(Debug, Serialize)]
struct WriteQueueWire {
    capacity: usize,
    available: usize,
}

#[derive(Debug, Serialize)]
struct WriteQueuesWire {
    state: WriteQueueWire,
    cache: WriteQueueWire,
}

#[derive(Debug, Serialize)]
struct WorktreeStatsWire {
    repo_id: String,
    worktree_id: String,
    active_generation_id: Option<String>,
    active_model_space_id: Option<String>,
    projection_status: Option<String>,
    projection_last_error: Option<String>,
}

/// One tool's `tools/call` count (T19-05, spec 11 §2).
#[derive(Debug, Serialize)]
struct ToolCallCountWire {
    name: String,
    count: u64,
}

impl From<(String, u64)> for ToolCallCountWire {
    fn from((name, count): (String, u64)) -> Self {
        ToolCallCountWire { name, count }
    }
}

/// `tools/call` counts by tool name (T19-05, spec 11 §2) — `session` is
/// this connection's own counts (cleared when it closes), `since_daemon_start`
/// is a running total that survives every other session's disconnect but not
/// a daemon restart (in-memory only, `[SPEC]` deliberately not persisted).
/// Both are sorted by tool name for deterministic JSON.
#[derive(Debug, Serialize)]
struct ToolCallCountsWire {
    session: Vec<ToolCallCountWire>,
    since_daemon_start: Vec<ToolCallCountWire>,
}

#[derive(Debug, Serialize)]
struct StatsResult {
    memory: MemoryStatsWire,
    observations: ObservationStatsWire,
    consolidation: ConsolidationStatsWire,
    scope: String,
    worktree: Option<WorktreeStatsWire>,
    store_instance_uuid: Option<String>,
    write_queues: WriteQueuesWire,
    tool_calls: ToolCallCountsWire,
}

/// Window (D-049, `[SPEC]`-chosen, not measured — mirrors `cli::stats`'s own
/// identical constant; the two `stats` implementations are independently
/// duplicated, not shared, per this module's own precedent) over which
/// recently-`applied` consolidation runs are summed to estimate
/// throughput/ETA.
const CONSOLIDATION_THROUGHPUT_WINDOW_MS: i64 = 5 * 60 * 1000;

/// The current wall-clock time as Unix milliseconds — mirrors
/// `consolidation_trigger::system_now_ms`/`lifecycle::system_now_ms`/
/// `worktree_task::system_now_ms` exactly, this project's established
/// convention of each call site carrying its own trivial copy rather than a
/// shared helper.
fn system_now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub async fn stats(
    ctx: &MemoryContext,
    root: RequestRoot,
    args: &Map<String, Value>,
    session_id: &str,
    tool_calls: &crate::daemon::tool_calls::ToolCallCounters,
) -> Result<CallToolResult, String> {
    reject_unknown_keys(args, &[])?;

    let state_read = match ctx.state.open_read() {
        Ok(c) => c,
        Err(e) => return Ok(infra_err(e)),
    };

    // Store-wide totals (not scope-filtered) -- "counts per pillar" is a
    // store-wide health figure (spec 11 §2); only the `worktree` block below
    // is resolved-scope-specific.
    let entries_by_kind_state = match memory_entry_counts(&state_read) {
        Ok(v) => v,
        Err(e) => return Ok(infra_err(e)),
    };
    let pending_candidates_by_state = match pending_candidate_counts(&state_read) {
        Ok(v) => v,
        Err(e) => return Ok(infra_err(e)),
    };

    // D-049: the observations pillar (`01-overview.md` §5-9) and the
    // consolidation backlog/progress -- store-wide, same as the memory
    // counts above, previously unreported by `stats()` entirely.
    let observations_total = match observation_envelope_count(&state_read) {
        Ok(v) => v,
        Err(e) => return Ok(infra_err(e)),
    };
    let runs_by_state = match consolidation_run_counts(&state_read) {
        Ok(v) => v,
        Err(e) => return Ok(infra_err(e)),
    };
    let pending_backlog_total = match total_pending_backlog(&state_read) {
        Ok(v) => v,
        Err(e) => return Ok(infra_err(e)),
    };
    let now_ms = system_now_ms();
    let applied_recently = match observations_applied_since(
        &state_read,
        now_ms - CONSOLIDATION_THROUGHPUT_WINDOW_MS,
    ) {
        Ok(v) => v,
        Err(e) => return Ok(infra_err(e)),
    };
    let oldest_pending_run_created_at = match oldest_open_run_created_at(&state_read) {
        Ok(v) => v,
        Err(e) => return Ok(infra_err(e)),
    };
    // D-071: the retry bookkeeping D-050 has been recording all along.
    let stuck_runs = match stuck_consolidation_runs(
        &state_read,
        local_rag_core::BUILD_ID,
        STUCK_RUN_ATTEMPT_THRESHOLD,
    ) {
        Ok(v) => v,
        Err(e) => return Ok(infra_err(e)),
    };
    let throughput_observations_per_min =
        applied_recently as f64 / (CONSOLIDATION_THROUGHPUT_WINDOW_MS as f64 / 60_000.0);
    let progress_pct = if observations_total > 0 {
        Some(
            (observations_total - pending_backlog_total) as f64 / observations_total as f64 * 100.0,
        )
    } else {
        None
    };
    let eta_seconds = if throughput_observations_per_min > 0.0 && pending_backlog_total > 0 {
        Some((pending_backlog_total as f64 / throughput_observations_per_min * 60.0) as i64)
    } else {
        None
    };

    let resolution = match resolve(&state_read, &root) {
        Ok(r) => r,
        Err(e) => return Ok(infra_err(e)),
    };
    let (scope_label, _scopes) = recall_pipeline::scopes_for(&resolution);

    let worktree = match &resolution {
        Resolution::Resolved {
            repo_id,
            worktree_id,
        } => {
            let projection = match projection_state(&state_read, worktree_id) {
                Ok(p) => p,
                Err(e) => return Ok(infra_err(e)),
            };
            Some(WorktreeStatsWire {
                repo_id: repo_id.clone(),
                worktree_id: worktree_id.clone(),
                active_generation_id: projection
                    .as_ref()
                    .and_then(|p| p.active_generation_id.clone()),
                active_model_space_id: projection
                    .as_ref()
                    .and_then(|p| p.active_model_space_id.clone()),
                projection_status: projection.as_ref().map(|p| p.status.as_str().to_string()),
                projection_last_error: projection.as_ref().and_then(|p| p.last_error.clone()),
            })
        }
        Resolution::GlobalOnly | Resolution::Ambiguous { .. } => None,
    };

    let store_instance_uuid_value = match store_instance_uuid(&state_read) {
        Ok(v) => v,
        Err(e) => return Ok(infra_err(e)),
    };

    Ok(content::ok(&StatsResult {
        memory: MemoryStatsWire {
            entries_by_kind_state: entries_by_kind_state
                .into_iter()
                .map(MemoryCountWire::from)
                .collect(),
            pending_candidates_by_state: pending_candidates_by_state
                .into_iter()
                .map(CandidateCountWire::from)
                .collect(),
        },
        observations: ObservationStatsWire {
            total: observations_total,
        },
        consolidation: ConsolidationStatsWire {
            runs_by_state: runs_by_state.into_iter().map(RunCountWire::from).collect(),
            pending_backlog_total,
            progress_pct,
            throughput_observations_per_min,
            eta_seconds,
            oldest_pending_run_created_at,
            stuck_runs: stuck_runs.into_iter().map(StuckRunWire::from).collect(),
        },
        scope: scope_label,
        worktree,
        store_instance_uuid: store_instance_uuid_value,
        write_queues: WriteQueuesWire {
            state: WriteQueueWire {
                capacity: ctx.state.writer().queue_capacity(),
                available: ctx.state.writer().available_slots(),
            },
            cache: WriteQueueWire {
                capacity: ctx.cache.writer().queue_capacity(),
                available: ctx.cache.writer().available_slots(),
            },
        },
        tool_calls: ToolCallCountsWire {
            session: tool_calls
                .session_snapshot(session_id)
                .into_iter()
                .map(ToolCallCountWire::from)
                .collect(),
            since_daemon_start: tool_calls
                .aggregate_snapshot()
                .into_iter()
                .map(ToolCallCountWire::from)
                .collect(),
        },
    }))
}

// ---------------------------------------------------------------------------
// health
// ---------------------------------------------------------------------------

/// `[SPEC]`: `health()` is only ever reachable in `DaemonMode::Normal` — the
/// same `tools/call` short-circuit that gates every other tool refuses the
/// whole call while `DaemonMode::MigrationOnly` (`dispatch::route_tools_
/// call`), before any tool-specific code runs. A store genuinely
/// mid-migration is diagnosable earlier, over a different channel: the
/// handshake `WELCOME`'s own `mode` field (spec 02 §4.2), or the CLI's
/// `local-rag status` (T15-01). This tool's `daemon_mode` field is included
/// for the contract's own "daemon/version/store status" completeness, not
/// because it is ever observed as anything but `"normal"` today.
#[derive(Debug, Serialize)]
struct HealthResult {
    daemon_mode: String,
    daemon_version: String,
    store_instance_uuid: Option<String>,
}

pub async fn health(
    ctx: &MemoryContext,
    mode: &DaemonMode,
    args: &Map<String, Value>,
) -> Result<CallToolResult, String> {
    reject_unknown_keys(args, &[])?;
    let state_read = match ctx.state.open_read() {
        Ok(c) => c,
        Err(e) => return Ok(infra_err(e)),
    };
    let store_instance_uuid_value = match store_instance_uuid(&state_read) {
        Ok(v) => v,
        Err(e) => return Ok(infra_err(e)),
    };
    Ok(content::ok(&HealthResult {
        daemon_mode: mode.as_str().to_string(),
        daemon_version: local_rag_core::VERSION.to_string(),
        store_instance_uuid: store_instance_uuid_value,
    }))
}
