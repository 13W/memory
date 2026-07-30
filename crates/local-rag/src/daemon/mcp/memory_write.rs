//! The eight MCP memory-write tool adapters: `remember`/
//! `approve_memory_candidate`/`reject_memory_candidate`/
//! `edit_memory_candidate`/`edit_memory`/`retract_memory`/`merge_memories`/
//! `give_feedback` — T15-05. Same shape as [`super::memory`]'s read
//! adapters: parse args, call a domain function against [`MemoryContext`]'s
//! already-open `state.sqlite`, map the outcome into a [`CallToolResult`].
//! Kept in a sibling file, not appended to `memory.rs`, so that file's own
//! doc claim ("every tool here is read-only") stays true — every function
//! here opens a real write transaction (`ctx.state.writer().transaction`),
//! never `open_read`.

use serde::Serialize;
use serde_json::{Map, Value};

use local_rag_core::hash::sha256_hex;
use local_rag_memory::schema::Signal;
use local_rag_protocol::ErrorEnvelope;
use local_rag_store::{
    Actor, ApproveCandidateOutcome, CreateMemoryOp, EditMemoryOp, EvidenceKind,
    GLOBAL_SCOPE_OWNER_ID, MemoryKind, MemoryOpError, MemoryOpOutcome, MergeLoser, MergeMemoryOp,
    NewObservationEnvelope, ProposedOperation, RequestRoot, Resolution, RetractMemoryOp,
    ReviewError, ScopeKind, TrustLevel, apply_create, apply_edit, apply_merge, apply_retract,
    approve_candidate, edit_candidate, insert_envelope, reject_candidate, resolve,
};

use crate::daemon::memory::MemoryContext;

use super::content::{self, CallToolResult};
use super::memory::{infra_err, optional_enum};
use super::tools::{reject_unknown_keys, require_string};

// ---------------------------------------------------------------------------
// shared helpers
// ---------------------------------------------------------------------------

fn memory_op_error_envelope(e: &MemoryOpError) -> ErrorEnvelope {
    match e {
        MemoryOpError::UnknownMemory => ErrorEnvelope::unknown_memory(),
        MemoryOpError::OptimisticConflict { expected, actual } => {
            ErrorEnvelope::optimistic_conflict(*expected, *actual)
        }
        MemoryOpError::CanonicalKeyConflict => ErrorEnvelope::canonical_key_conflict(),
        MemoryOpError::InvalidGlobalScopeOwner => ErrorEnvelope::invalid_global_scope_owner(),
        MemoryOpError::IllegalTransition(illegal) => {
            ErrorEnvelope::illegal_memory_transition(illegal.to_string())
        }
        MemoryOpError::EntryTerminal => ErrorEnvelope::entry_terminal(),
        MemoryOpError::IncompatibleScope => ErrorEnvelope::incompatible_scope(),
        MemoryOpError::EmptyMergeSet => ErrorEnvelope::empty_merge_set(),
        MemoryOpError::ModelClaimOnlyProvenance => ErrorEnvelope::model_claim_only_provenance(),
    }
}

fn review_error_envelope(e: &ReviewError) -> ErrorEnvelope {
    match e {
        ReviewError::UnknownCandidate => ErrorEnvelope::unknown_candidate(),
        ReviewError::IllegalTransition(illegal) => {
            ErrorEnvelope::illegal_candidate_transition(illegal.to_string())
        }
        ReviewError::NotPending => ErrorEnvelope::candidate_not_pending(),
        ReviewError::InvalidProposedOperation(detail) => {
            ErrorEnvelope::invalid_proposed_operation(detail.clone())
        }
        ReviewError::Materialization(e) => memory_op_error_envelope(e),
    }
}

/// The JSON-RPC request `id` (string, number, or null — JSON-RPC 2.0 allows
/// all three) stringified for the `mcp:<session_id>:<request_id>` source
/// identity spec 11 §2 names for `give_feedback`, and reused by `remember`'s
/// own idempotency key (see `op.rs::CreateMemoryOp::idempotency_key`'s
/// doc for why).
fn request_id_string(id: &Value) -> String {
    match id {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Null => "null".to_string(),
        other => other.to_string(),
    }
}

fn parse_signal(s: &str) -> Option<Signal> {
    match s {
        "low" => Some(Signal::Low),
        "medium" => Some(Signal::Medium),
        "high" => Some(Signal::High),
        _ => None,
    }
}

fn require_enum<T: Copy>(
    args: &Map<String, Value>,
    key: &str,
    parse: impl Fn(&str) -> Option<T>,
    allowed_desc: &str,
) -> Result<T, String> {
    match args.get(key) {
        Some(Value::String(s)) => {
            parse(s).ok_or_else(|| format!("{key} must be one of {allowed_desc}, got {s:?}"))
        }
        Some(_) => Err(format!("{key} must be a string")),
        None => Err(format!("{key} is required")),
    }
}

fn require_i64(args: &Map<String, Value>, key: &str) -> Result<i64, String> {
    match args.get(key) {
        Some(Value::Number(n)) => n
            .as_i64()
            .ok_or_else(|| format!("{key} must be an integer")),
        Some(_) => Err(format!("{key} must be an integer")),
        None => Err(format!("{key} is required")),
    }
}

/// The single `(scope_kind, scope_owner_id)` a `remember` call writes into
/// (T15-05, `[SPEC]`). Defaults to `repository` when the request's worktree
/// resolves, else `global` — a durable memory is normally "about this
/// project," not the transient worktree checkout. An explicit
/// `repository`/`worktree` request while unresolved is `WORKTREE_NOT_INDEXED`
/// (the caller asked for a scope this request cannot supply), never silently
/// downgraded to `global`.
fn resolve_write_scope(
    resolution: &Resolution,
    requested: Option<ScopeKind>,
) -> Result<(ScopeKind, String), ErrorEnvelope> {
    let resolved = match resolution {
        Resolution::Resolved {
            repo_id,
            worktree_id,
        } => Some((repo_id.as_str(), worktree_id.as_str())),
        Resolution::GlobalOnly | Resolution::Ambiguous { .. } => None,
    };

    let scope_kind = requested.unwrap_or(if resolved.is_some() {
        ScopeKind::Repository
    } else {
        ScopeKind::Global
    });

    match scope_kind {
        ScopeKind::Global => Ok((ScopeKind::Global, GLOBAL_SCOPE_OWNER_ID.to_string())),
        ScopeKind::Repository => match resolved {
            Some((repo_id, _)) => Ok((ScopeKind::Repository, repo_id.to_string())),
            None => Err(ErrorEnvelope::worktree_not_indexed()),
        },
        ScopeKind::Worktree => match resolved {
            Some((_, worktree_id)) => Ok((ScopeKind::Worktree, worktree_id.to_string())),
            None => Err(ErrorEnvelope::worktree_not_indexed()),
        },
    }
}

/// The wire shape of a successful `create`/`edit`/`retract`/`merge`/
/// `approve` materialization (spec 08 §3: "Response carries the new
/// `entry_version` and `audit_id`"). `outcome` surfaces `MemoryOpOutcome`'s
/// `Applied`/`Replayed` distinction directly rather than hiding it — a
/// caller retrying after a timeout can tell "this is the result of my
/// original call" from "this just happened."
#[derive(Debug, Serialize)]
struct MemoryOpResultWire {
    memory_id: String,
    entry_version: i64,
    audit_id: i64,
    outcome: &'static str,
}

impl From<MemoryOpOutcome> for MemoryOpResultWire {
    fn from(outcome: MemoryOpOutcome) -> Self {
        let (result, outcome) = match outcome {
            MemoryOpOutcome::Applied(r) => (r, "applied"),
            MemoryOpOutcome::Replayed(r) => (r, "replayed"),
        };
        MemoryOpResultWire {
            memory_id: result.memory_id,
            entry_version: result.entry_version,
            audit_id: result.audit_id,
            outcome,
        }
    }
}

#[derive(Debug, Serialize)]
struct AlreadyApprovedWire {
    outcome: &'static str,
}

#[derive(Debug, Serialize)]
struct IdWire {
    id: String,
}

#[derive(Debug, Serialize)]
struct GiveFeedbackResult {
    source_event_id: String,
    /// `true` when this call's `dedup_key` already existed — a retried
    /// identical JSON-RPC call, not a new observation (the task card's own
    /// "feedback duplicate request" bullet).
    deduplicated: bool,
}

// ---------------------------------------------------------------------------
// remember
// ---------------------------------------------------------------------------

pub async fn remember(
    ctx: &MemoryContext,
    root: RequestRoot,
    args: &Map<String, Value>,
    session_id: &str,
    request_id: &Value,
    now_ms: i64,
) -> Result<CallToolResult, String> {
    reject_unknown_keys(
        args,
        &[
            "text",
            "kind",
            "scope",
            "canonical_key",
            "importance",
            "confirmed_by_user",
        ],
    )?;
    let text = require_string(args, "text")?;
    let kind = require_enum(
        args,
        "kind",
        MemoryKind::from_db,
        "fact/decision/convention/procedure/task/question/hypothesis",
    )?;
    let scope_requested = optional_enum(
        args,
        "scope",
        ScopeKind::from_db,
        "global/repository/worktree",
    )?;
    let canonical_key = match args.get("canonical_key") {
        None => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return Err("canonical_key must be a string".to_string()),
    };
    let importance_signal = optional_enum(args, "importance", parse_signal, "low/medium/high")?
        .unwrap_or(Signal::Medium);
    let confirmed_by_user = match args.get("confirmed_by_user") {
        None => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err("confirmed_by_user must be a boolean".to_string()),
    };

    let state_read = match ctx.state.open_read() {
        Ok(c) => c,
        Err(e) => return Ok(infra_err(e)),
    };
    let resolution = match resolve(&state_read, &root) {
        Ok(r) => r,
        Err(e) => return Ok(infra_err(e)),
    };
    drop(state_read);
    let (scope_kind, scope_owner_id) = match resolve_write_scope(&resolution, scope_requested) {
        Ok(v) => v,
        Err(envelope) => return Ok(content::err(&envelope)),
    };

    let memory_id = ctx.uuids.next_uuid().to_string();
    // Always Actor::User, never Actor::Router, regardless of
    // confirmed_by_user (T15-05, `[SPEC]`) -- see op.rs's own doc comment
    // on the model-claim-only-provenance backstop for why this is the
    // already-anticipated design, not a new interpretation.
    let confidence = if confirmed_by_user {
        Signal::High.confidence()
    } else {
        Signal::Medium.confidence()
    };
    let importance = importance_signal.importance();
    let idempotency_key = format!(
        "mcp-remember:{session_id}:{}",
        request_id_string(request_id)
    );

    let outcome = ctx
        .state
        .writer()
        .transaction(move |tx| {
            apply_create(
                tx,
                &CreateMemoryOp {
                    memory_id: &memory_id,
                    kind,
                    text: &text,
                    canonical_key: canonical_key.as_deref(),
                    scope_kind,
                    scope_owner_id: &scope_owner_id,
                    confidence,
                    importance,
                    valid_from_tree: None,
                    last_verified_tree: None,
                    evidence: &[],
                    actor: Actor::User,
                    idempotency_key: Some(&idempotency_key),
                },
                now_ms,
            )
        })
        .await;

    match outcome {
        Ok(Ok(outcome)) => Ok(content::ok(&MemoryOpResultWire::from(outcome))),
        Ok(Err(e)) => Ok(content::err(&memory_op_error_envelope(&e))),
        Err(e) => Ok(infra_err(e)),
    }
}

// ---------------------------------------------------------------------------
// approve_memory_candidate / reject_memory_candidate / edit_memory_candidate
// ---------------------------------------------------------------------------

pub async fn approve_memory_candidate(
    ctx: &MemoryContext,
    args: &Map<String, Value>,
    now_ms: i64,
) -> Result<CallToolResult, String> {
    reject_unknown_keys(args, &["id"])?;
    let id = require_string(args, "id")?;

    let outcome = ctx
        .state
        .writer()
        .transaction(move |tx| approve_candidate(tx, &id, now_ms))
        .await;
    match outcome {
        Ok(Ok(ApproveCandidateOutcome::Materialized(op_outcome))) => {
            Ok(content::ok(&MemoryOpResultWire::from(op_outcome)))
        }
        Ok(Ok(ApproveCandidateOutcome::AlreadyApproved)) => Ok(content::ok(&AlreadyApprovedWire {
            outcome: "already_approved",
        })),
        Ok(Err(e)) => Ok(content::err(&review_error_envelope(&e))),
        Err(e) => Ok(infra_err(e)),
    }
}

pub async fn reject_memory_candidate(
    ctx: &MemoryContext,
    args: &Map<String, Value>,
) -> Result<CallToolResult, String> {
    reject_unknown_keys(args, &["id"])?;
    let id = require_string(args, "id")?;
    let id_for_wire = id.clone();

    let outcome = ctx
        .state
        .writer()
        .transaction(move |tx| reject_candidate(tx, &id))
        .await;
    match outcome {
        Ok(Ok(())) => Ok(content::ok(&IdWire { id: id_for_wire })),
        Ok(Err(e)) => Ok(content::err(&review_error_envelope(&e))),
        Err(e) => Ok(infra_err(e)),
    }
}

pub async fn edit_memory_candidate(
    ctx: &MemoryContext,
    args: &Map<String, Value>,
) -> Result<CallToolResult, String> {
    reject_unknown_keys(args, &["id", "patch"])?;
    let id = require_string(args, "id")?;
    let id_for_wire = id.clone();

    let patch = match args.get("patch") {
        Some(Value::Object(m)) => m,
        Some(_) => return Err("patch must be an object".to_string()),
        None => return Err("patch is required".to_string()),
    };
    for key in patch.keys() {
        if key != "proposed_operation" && key != "conflicts" {
            return Err(format!("unknown patch field: {key}"));
        }
    }
    let proposed_operation: Option<ProposedOperation> = match patch.get("proposed_operation") {
        None => None,
        Some(v) => Some(
            serde_json::from_value(v.clone())
                .map_err(|e| format!("patch.proposed_operation: {e}"))?,
        ),
    };
    let conflicts: Option<Vec<String>> = match patch.get("conflicts") {
        None => None,
        Some(Value::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    Value::String(s) => out.push(s.clone()),
                    _ => return Err("patch.conflicts must be an array of strings".to_string()),
                }
            }
            Some(out)
        }
        Some(_) => return Err("patch.conflicts must be an array".to_string()),
    };
    if proposed_operation.is_none() && conflicts.is_none() {
        return Err("patch must set at least one of proposed_operation/conflicts".to_string());
    }

    let outcome = ctx
        .state
        .writer()
        .transaction(move |tx| {
            let conflict_refs: Option<Vec<&str>> = conflicts
                .as_ref()
                .map(|c| c.iter().map(String::as_str).collect());
            edit_candidate(
                tx,
                &id,
                proposed_operation.as_ref(),
                conflict_refs.as_deref(),
            )
        })
        .await;
    match outcome {
        Ok(Ok(())) => Ok(content::ok(&IdWire { id: id_for_wire })),
        Ok(Err(e)) => Ok(content::err(&review_error_envelope(&e))),
        Err(e) => Ok(infra_err(e)),
    }
}

// ---------------------------------------------------------------------------
// edit_memory / retract_memory / merge_memories
// ---------------------------------------------------------------------------

pub async fn edit_memory(
    ctx: &MemoryContext,
    args: &Map<String, Value>,
    now_ms: i64,
) -> Result<CallToolResult, String> {
    reject_unknown_keys(args, &["id", "expected_version", "patch"])?;
    let id = require_string(args, "id")?;
    let expected_version = require_i64(args, "expected_version")?;

    let patch = match args.get("patch") {
        Some(Value::Object(m)) => m,
        Some(_) => return Err("patch must be an object".to_string()),
        None => return Err("patch is required".to_string()),
    };
    for key in patch.keys() {
        if key != "text" && key != "importance" {
            return Err(format!("unknown patch field: {key}"));
        }
    }
    let text = match patch.get("text") {
        None => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return Err("patch.text must be a string".to_string()),
    };
    let importance = match patch.get("importance") {
        None => None,
        Some(Value::Number(n)) => Some(
            n.as_f64()
                .filter(|v| (0.0..=1.0).contains(v))
                .ok_or_else(|| "patch.importance must be a number between 0 and 1".to_string())?,
        ),
        Some(_) => return Err("patch.importance must be a number".to_string()),
    };
    if text.is_none() && importance.is_none() {
        return Err("patch must set at least one of text/importance".to_string());
    }

    let outcome = ctx
        .state
        .writer()
        .transaction(move |tx| {
            apply_edit(
                tx,
                &EditMemoryOp {
                    memory_id: &id,
                    expected_version,
                    text: text.as_deref(),
                    importance,
                    actor: Actor::User,
                    idempotency_key: None,
                },
                now_ms,
            )
        })
        .await;
    match outcome {
        Ok(Ok(outcome)) => Ok(content::ok(&MemoryOpResultWire::from(outcome))),
        Ok(Err(e)) => Ok(content::err(&memory_op_error_envelope(&e))),
        Err(e) => Ok(infra_err(e)),
    }
}

pub async fn retract_memory(
    ctx: &MemoryContext,
    args: &Map<String, Value>,
    now_ms: i64,
) -> Result<CallToolResult, String> {
    reject_unknown_keys(args, &["id", "expected_version"])?;
    let id = require_string(args, "id")?;
    let expected_version = require_i64(args, "expected_version")?;

    let outcome = ctx
        .state
        .writer()
        .transaction(move |tx| {
            apply_retract(
                tx,
                &RetractMemoryOp {
                    memory_id: &id,
                    expected_version,
                    evidence: &[],
                    actor: Actor::User,
                    idempotency_key: None,
                },
                now_ms,
            )
        })
        .await;
    match outcome {
        Ok(Ok(outcome)) => Ok(content::ok(&MemoryOpResultWire::from(outcome))),
        Ok(Err(e)) => Ok(content::err(&memory_op_error_envelope(&e))),
        Err(e) => Ok(infra_err(e)),
    }
}

pub async fn merge_memories(
    ctx: &MemoryContext,
    args: &Map<String, Value>,
    now_ms: i64,
) -> Result<CallToolResult, String> {
    reject_unknown_keys(args, &["ids", "survivor_id"])?;

    let ids_value = match args.get("ids") {
        Some(Value::Array(items)) if items.len() >= 2 => items,
        Some(Value::Array(_)) => return Err("ids must have at least 2 entries".to_string()),
        Some(_) => return Err("ids must be an array".to_string()),
        None => return Err("ids is required".to_string()),
    };
    let mut entries: Vec<(String, i64)> = Vec::with_capacity(ids_value.len());
    for item in ids_value {
        let Value::Object(obj) = item else {
            return Err("each ids entry must be an object".to_string());
        };
        let memory_id = match obj.get("memory_id") {
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            _ => return Err("each ids entry needs a non-empty memory_id string".to_string()),
        };
        let expected_version = match obj.get("expected_version") {
            Some(Value::Number(n)) => n
                .as_i64()
                .ok_or_else(|| "expected_version must be an integer".to_string())?,
            _ => return Err("each ids entry needs an integer expected_version".to_string()),
        };
        entries.push((memory_id, expected_version));
    }

    let survivor_id = require_string(args, "survivor_id")?;
    let Some(survivor_pos) = entries.iter().position(|(id, _)| id == &survivor_id) else {
        return Err(format!(
            "survivor_id {survivor_id:?} must be present in ids"
        ));
    };
    let survivor_expected_version = entries[survivor_pos].1;
    let losers: Vec<(String, i64)> = entries
        .into_iter()
        .enumerate()
        .filter(|(i, _)| *i != survivor_pos)
        .map(|(_, e)| e)
        .collect();

    let outcome = ctx
        .state
        .writer()
        .transaction(move |tx| {
            let loser_structs: Vec<MergeLoser<'_>> = losers
                .iter()
                .map(|(id, expected_version)| MergeLoser {
                    memory_id: id,
                    expected_version: *expected_version,
                })
                .collect();
            apply_merge(
                tx,
                &MergeMemoryOp {
                    survivor_id: &survivor_id,
                    survivor_expected_version,
                    losers: &loser_structs,
                    actor: Actor::User,
                    idempotency_key: None,
                },
                now_ms,
            )
        })
        .await;
    match outcome {
        Ok(Ok(outcome)) => Ok(content::ok(&MemoryOpResultWire::from(outcome))),
        Ok(Err(e)) => Ok(content::err(&memory_op_error_envelope(&e))),
        Err(e) => Ok(infra_err(e)),
    }
}

// ---------------------------------------------------------------------------
// give_feedback
// ---------------------------------------------------------------------------

pub async fn give_feedback(
    ctx: &MemoryContext,
    root: RequestRoot,
    args: &Map<String, Value>,
    session_id: &str,
    request_id: &Value,
    now_ms: i64,
) -> Result<CallToolResult, String> {
    reject_unknown_keys(args, &["text"])?;
    let text = require_string(args, "text")?;

    let state_read = match ctx.state.open_read() {
        Ok(c) => c,
        Err(e) => return Ok(infra_err(e)),
    };
    let resolution = match resolve(&state_read, &root) {
        Ok(r) => r,
        Err(e) => return Ok(infra_err(e)),
    };
    drop(state_read);
    let (repo_id, worktree_id) = match &resolution {
        Resolution::Resolved {
            repo_id,
            worktree_id,
        } => (Some(repo_id.clone()), Some(worktree_id.clone())),
        Resolution::GlobalOnly | Resolution::Ambiguous { .. } => (None, None),
    };

    let observation_id = ctx.uuids.next_uuid().to_string();
    // spec 11 §2's literal source identity: `mcp:<session_id>:<request_id>`
    // -- used as both `source_event_id` and `dedup_key`, so a retried
    // identical JSON-RPC call (same id) reproduces the same key and
    // `insert_envelope` reports it already exists (idempotent success, not
    // an error) rather than inserting a duplicate row.
    let source_event_id = format!("mcp:{session_id}:{}", request_id_string(request_id));
    let payload_hash = sha256_hex(text.as_bytes());
    let session_id_owned = session_id.to_string();
    let source_event_id_for_wire = source_event_id.clone();

    let received_seq = ctx
        .state
        .writer()
        .transaction(move |tx| {
            insert_envelope(
                tx,
                &NewObservationEnvelope {
                    observation_id: &observation_id,
                    source_event_id: &source_event_id,
                    dedup_key: Some(&source_event_id),
                    payload_hash: &payload_hash,
                    event_type: "McpFeedback",
                    evidence_kind: EvidenceKind::UserStatement.as_str(),
                    trust: TrustLevel::Normal.as_str(),
                    source_timestamp: Some(now_ms),
                    repo_id: repo_id.as_deref(),
                    worktree_id: worktree_id.as_deref(),
                    session_id: &session_id_owned,
                    agent_id: None,
                    turn_id: None,
                    batch_id: None,
                    commit_hash: None,
                    short_evidence_excerpt: None,
                    redaction_version: None,
                },
            )
        })
        .await;

    match received_seq {
        Ok(inner) => Ok(content::ok(&GiveFeedbackResult {
            source_event_id: source_event_id_for_wire,
            deduplicated: inner.is_none(),
        })),
        Err(e) => Ok(infra_err(e)),
    }
}
