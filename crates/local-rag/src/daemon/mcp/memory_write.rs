//! The eight MCP memory-write tool adapters: `remember`/
//! `approve_memory_candidate`/`reject_memory_candidate`/
//! `edit_memory_candidate`/`edit_memory`/`retract_memory`/`confirm_memory`/
//! `reject_memory`/`merge_memories`/
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
    Actor, ApproveCandidateOutcome, ConfirmMemoryOp, CreateMemoryOp, EditMemoryOp, EvidenceKind,
    GLOBAL_SCOPE_OWNER_ID, MemoryKind, MemoryOpError, MemoryOpOutcome, MergeLoser, MergeMemoryOp,
    NewObservationEnvelope, ProposedOperation, RejectMemoryOp, RequestRoot, Resolution,
    RetractMemoryOp, ReviewError, ScopeKind, TrustLevel, apply_confirm, apply_create, apply_edit,
    apply_merge, apply_reject, apply_retract, approve_candidate, edit_candidate,
    find_by_idempotency_key, insert_envelope, reject_candidate, resolve, upsert_normalization,
};

use crate::daemon::memory::MemoryContext;
use crate::daemon::normalization::boundary::OwnedNormalizationRow;

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

/// Why a `remember` call that asked for no particular scope ended up in
/// `global` instead of `repository` (D-064). The two states have different
/// remedies, so they are not collapsed into one marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScopeFallback {
    /// The request's root resolved to nothing this store knows.
    WorktreeNotIndexed,
    /// The root matched more than one detached worktree, and spec 04 §7
    /// forbids guessing between them.
    WorktreeAmbiguous,
}

impl ScopeFallback {
    /// Lowercase marker, matching spec 02 §6's own `degraded: "dense_only"`
    /// style rather than the `SCREAMING_CASE` of the error codes.
    fn as_str(self) -> &'static str {
        match self {
            ScopeFallback::WorktreeNotIndexed => "worktree_not_indexed",
            ScopeFallback::WorktreeAmbiguous => "worktree_ambiguous",
        }
    }

    fn hint(self) -> &'static str {
        match self {
            ScopeFallback::WorktreeNotIndexed => {
                "this worktree is not indexed, so the entry was stored machine-wide and every \
                 project's recall will see it; index it (`local-rag index <path>`) to get \
                 project-scoped memory, or pass scope explicitly to silence this"
            }
            ScopeFallback::WorktreeAmbiguous => {
                "this root matches more than one detached worktree, so the entry was stored \
                 machine-wide and every project's recall will see it; re-bind the worktree \
                 (`local-rag repo attach`) to get project-scoped memory"
            }
        }
    }
}

/// The scope a `remember` call actually wrote into, plus why — if the caller
/// asked for no scope and did not get the `repository` one it would have had
/// with an indexed worktree.
#[derive(Debug)]
struct WriteScope {
    kind: ScopeKind,
    owner_id: String,
    /// `None` on every non-degraded path: an explicit `scope` request is the
    /// caller's own choice, never a degradation, and a resolved worktree
    /// yields the normal `repository` default.
    fallback: Option<ScopeFallback>,
}

/// The single `(scope_kind, scope_owner_id)` a `remember` call writes into
/// (T15-05, `[SPEC]`). Defaults to `repository` when the request's worktree
/// resolves, else `global` — a durable memory is normally "about this
/// project," not the transient worktree checkout. An explicit
/// `repository`/`worktree` request while unresolved is `WORKTREE_NOT_INDEXED`
/// (the caller asked for a scope this request cannot supply), never silently
/// downgraded to `global`.
///
/// D-064: the *implicit* `global` fallback is not an error — spec 02 §6's own
/// table says memory tools work in repo/global scope for an unknown worktree —
/// but it is a degradation, and the same section's `[FIXED]` line ("Degradation
/// is always explicit in responses; nothing degrades silently") makes reporting
/// it mandatory, hence [`WriteScope::fallback`].
fn resolve_write_scope(
    resolution: &Resolution,
    requested: Option<ScopeKind>,
) -> Result<WriteScope, ErrorEnvelope> {
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

    let fallback = match (requested, resolution) {
        // Only an *implicit* global is a degradation.
        (None, Resolution::GlobalOnly) => Some(ScopeFallback::WorktreeNotIndexed),
        (None, Resolution::Ambiguous { .. }) => Some(ScopeFallback::WorktreeAmbiguous),
        _ => None,
    };

    match scope_kind {
        ScopeKind::Global => Ok(WriteScope {
            kind: ScopeKind::Global,
            owner_id: GLOBAL_SCOPE_OWNER_ID.to_string(),
            fallback,
        }),
        ScopeKind::Repository => match resolved {
            Some((repo_id, _)) => Ok(WriteScope {
                kind: ScopeKind::Repository,
                owner_id: repo_id.to_string(),
                fallback: None,
            }),
            None => Err(ErrorEnvelope::worktree_not_indexed()),
        },
        ScopeKind::Worktree => match resolved {
            Some((_, worktree_id)) => Ok(WriteScope {
                kind: ScopeKind::Worktree,
                owner_id: worktree_id.to_string(),
                fallback: None,
            }),
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

/// `remember`'s own wire shape: [`MemoryOpResultWire`]'s fields plus where the
/// entry actually landed (D-064). `scope` is always present — a caller that
/// never passed one should not have to infer it — and `degraded`/`hint` appear
/// only when an implicit `repository` became `global`, which spec 02 §6
/// `[FIXED]` requires be explicit in the response.
#[derive(Debug, Serialize)]
struct RememberResultWire {
    memory_id: String,
    entry_version: i64,
    audit_id: i64,
    outcome: &'static str,
    scope: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    degraded: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<&'static str>,
}

impl RememberResultWire {
    fn new(outcome: MemoryOpOutcome, scope: &WriteScope) -> Self {
        let op = MemoryOpResultWire::from(outcome);
        RememberResultWire {
            memory_id: op.memory_id,
            entry_version: op.entry_version,
            audit_id: op.audit_id,
            outcome: op.outcome,
            scope: scope.kind.as_str(),
            degraded: scope.fallback.map(ScopeFallback::as_str),
            hint: scope.fallback.map(ScopeFallback::hint),
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
    let write_scope = match resolve_write_scope(&resolution, scope_requested) {
        Ok(v) => v,
        Err(envelope) => return Ok(content::err(&envelope)),
    };
    let (scope_kind, scope_owner_id) = (write_scope.kind, write_scope.owner_id.clone());

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

    // T21-14, ADR-0011 §Decision 2: the canon is decided **before** the store
    // sees the text. A replay is checked first and skips translation entirely —
    // `apply_create` would recognise the idempotency key anyway and return the
    // original result, so translating again would spend a second of local GPU
    // to reproduce an answer nobody will store.
    let already_applied = match ctx.state.open_read() {
        Ok(read) => find_by_idempotency_key(&read, &idempotency_key)
            .ok()
            .flatten()
            .is_some(),
        Err(e) => return Ok(infra_err(e)),
    };
    let decided = if already_applied {
        None
    } else {
        Some(ctx.translator().decide(&memory_id, &text).await)
    };
    let canon = decided
        .as_ref()
        .map(|d| d.canon(&text).to_string())
        .unwrap_or_else(|| text.clone());
    let row = decided.map(|d| OwnedNormalizationRow::for_canon(&memory_id, &canon, d));

    let outcome = ctx
        .state
        .writer()
        .transaction(move |tx| {
            let applied = apply_create(
                tx,
                &CreateMemoryOp {
                    memory_id: &memory_id,
                    kind,
                    text: &canon,
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
            )?;
            // Same transaction as the create, so an entry can never exist with
            // an English canon and no record of what the author wrote.
            if applied.is_ok()
                && let Some(row) = &row
            {
                upsert_normalization(tx, &row.as_write(), now_ms)?;
            }
            Ok(applied)
        })
        .await;

    match outcome {
        Ok(Ok(outcome)) => Ok(content::ok(&RememberResultWire::new(outcome, &write_scope))),
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
// edit_memory / retract_memory / confirm_memory / reject_memory / merge_memories
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

    // T21-14: an edit that supplies new text is the same boundary as a create,
    // so it gets the same treatment. An importance-only edit decides nothing —
    // the canon is untouched, and `apply_edit` leaves the existing row alone.
    let decided = match text.as_deref() {
        Some(incoming) => Some(ctx.translator().decide(&id, incoming).await),
        None => None,
    };
    let canon = decided
        .as_ref()
        .zip(text.as_deref())
        .map(|(d, incoming)| d.canon(incoming).to_string());
    let row = decided
        .zip(canon.as_deref())
        .map(|(d, canon)| OwnedNormalizationRow::for_canon(&id, canon, d));

    let outcome = ctx
        .state
        .writer()
        .transaction(move |tx| {
            let applied = apply_edit(
                tx,
                &EditMemoryOp {
                    memory_id: &id,
                    expected_version,
                    text: canon.as_deref(),
                    importance,
                    actor: Actor::User,
                    idempotency_key: None,
                },
                now_ms,
            )?;
            // `apply_edit` drops the old row when the text moves (T21-07); this
            // writes the new one in the same transaction, so the window where
            // an entry has a fresh canon and no record of its origin does not
            // exist.
            if applied.is_ok()
                && let Some(row) = &row
            {
                upsert_normalization(tx, &row.as_write(), now_ms)?;
            }
            Ok(applied)
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

/// `confirm_memory` (D-079): spec 04 §5's `hypothesis` `active → confirmed`.
/// Same argument shape and same optimistic guard as `retract_memory` — the
/// two differ only in the state they reach and in whether recall keeps
/// showing the entry afterwards.
pub async fn confirm_memory(
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
            apply_confirm(
                tx,
                &ConfirmMemoryOp {
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

/// `reject_memory` (D-079): spec 04 §5's `hypothesis` `active → rejected`.
/// Not to be confused with `reject_memory_candidate`, which moves a *pending
/// candidate* (04 §6) and never touches a durable entry.
pub async fn reject_memory(
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
            apply_reject(
                tx,
                &RejectMemoryOp {
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

#[cfg(test)]
mod boundary_tests {
    //! The write boundary end to end through the real handlers (T21-14).
    //!
    //! These live inside the module because `mcp::memory_write` is private to
    //! `mcp` — widening it to `pub` so an integration test could reach in would
    //! trade a real encapsulation boundary for test convenience. Everything
    //! here builds a `MemoryContext` by hand with a scripted generator, which
    //! is the one thing the live-daemon harness in
    //! `tests/mcp_memory_write_tools.rs` cannot do.

    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use local_rag_core::config::DataPolicy;
    use local_rag_core::identity::SystemUuidV7;
    use local_rag_core::paths::StoreLayout;
    use local_rag_embed::{
        FinishReason, GenError, GenRequest, GenResponse, Generator, GeneratorEntry, GeneratorPool,
    };
    use local_rag_memory::recall::{BruteForceCosine, UnavailableEmbedder};
    use local_rag_store::{
        CacheDb, NormalizationStatus, StateDb, memory_entry_by_id, normalization_for,
    };
    use local_rag_test_support::TempHome;
    use serde_json::json;

    use super::*;
    use crate::daemon::memory::MemoryContext;

    const RU: &str = "мы решили всегда запускать тесты перед коммитом";
    const EN: &str = "we always run the tests before committing";
    const STORE_UUID: &str = "01a00000-0000-7000-8000-00000000fffd";

    /// A generator that counts calls and answers from a script, so "the
    /// detector short-circuited" is provable rather than assumed.
    #[derive(Clone)]
    struct ScriptedGenerator {
        calls: Arc<AtomicUsize>,
        answer: Option<String>,
    }

    impl ScriptedGenerator {
        fn translating(english: &str) -> Self {
            ScriptedGenerator {
                calls: Arc::new(AtomicUsize::new(0)),
                answer: Some(english.to_string()),
            }
        }

        fn refusing() -> Self {
            ScriptedGenerator {
                calls: Arc::new(AtomicUsize::new(0)),
                answer: None,
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn pool(&self) -> Arc<GeneratorPool> {
            Arc::new(GeneratorPool::new(vec![GeneratorEntry::local(
                "scripted",
                Arc::new(self.clone()),
            )]))
        }
    }

    impl Generator for ScriptedGenerator {
        fn generate(&self, _req: GenRequest) -> Result<GenResponse, GenError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match &self.answer {
                Some(english) => Ok(GenResponse {
                    text: json!({ "en": english }).to_string(),
                    finish_reason: FinishReason::Stop,
                    tokens_generated: None,
                }),
                None => Ok(GenResponse {
                    text: "I cannot do that".to_string(),
                    finish_reason: FinishReason::Stop,
                    tokens_generated: None,
                }),
            }
        }
    }

    struct Fixture {
        _home: TempHome,
        ctx: MemoryContext,
        state: Arc<StateDb>,
    }

    fn fixture(generators: Option<Arc<GeneratorPool>>) -> Fixture {
        let home = TempHome::new().expect("temp home");
        let layout = StoreLayout::new(home.join("local-rag"));
        layout.ensure().expect("ensure store tree");
        let state = Arc::new(StateDb::open(layout.state_db()).expect("open state.sqlite"));
        let cache = Arc::new(CacheDb::open(layout.cache_db(), STORE_UUID).expect("open cache"));
        let ctx = MemoryContext {
            state: Arc::clone(&state),
            cache,
            embedder: Arc::new(UnavailableEmbedder),
            dense_backend: Arc::new(BruteForceCosine),
            recall_token_budget: 1500,
            uuids: Arc::new(SystemUuidV7),
            generators,
            generator_model_id: "scripted-model".to_string(),
            data_policy: DataPolicy::LocalOnly,
        };
        Fixture {
            _home: home,
            ctx,
            state,
        }
    }

    fn remember_args(text: &str) -> Map<String, Value> {
        let mut args = Map::new();
        args.insert("text".to_string(), json!(text));
        args.insert("kind".to_string(), json!("fact"));
        args.insert("scope".to_string(), json!("global"));
        args
    }

    async fn call_remember(fx: &Fixture, text: &str, request_id: &str) -> Value {
        let args = remember_args(text);
        let raw = remember(
            &fx.ctx,
            RequestRoot::default(),
            &args,
            "sess-1",
            &json!(request_id),
            1_000,
        )
        .await
        .expect("handler ran");
        serde_json::to_value(&raw).expect("serializable")
    }

    fn stored_text(fx: &Fixture, memory_id: &str) -> String {
        let read = fx.state.open_read().expect("read");
        memory_entry_by_id(&read, memory_id)
            .expect("read entry")
            .expect("entry exists")
            .text
    }

    /// ADR-0010 Decision 8, still in force and now on the request path: an
    /// English note must not reach the generator at all.
    #[tokio::test]
    async fn english_text_costs_zero_inference() {
        let generator = ScriptedGenerator::translating("SHOULD NOT BE USED");
        let fx = fixture(Some(generator.pool()));

        call_remember(&fx, EN, "req-1").await;

        assert_eq!(generator.calls(), 0, "the detector answered on its own");
        let read = fx.state.open_read().expect("read");
        let id: String = read
            .query_row("SELECT memory_id FROM memory_entry", [], |r| r.get(0))
            .expect("one entry");
        assert_eq!(stored_text(&fx, &id), EN);
        let row = normalization_for(&read, &id)
            .expect("read row")
            .expect("row exists");
        assert_eq!(row.status, NormalizationStatus::English);
        assert_eq!(row.source_text, None, "there is no other text to keep");
    }

    /// The canon is English and the author's own words survive beside it.
    #[tokio::test]
    async fn russian_text_is_stored_english_with_the_original_as_provenance() {
        let generator = ScriptedGenerator::translating(EN);
        let fx = fixture(Some(generator.pool()));

        call_remember(&fx, RU, "req-1").await;

        assert_eq!(generator.calls(), 1);
        let read = fx.state.open_read().expect("read");
        let id: String = read
            .query_row("SELECT memory_id FROM memory_entry", [], |r| r.get(0))
            .expect("one entry");
        assert_eq!(stored_text(&fx, &id), EN, "the canon is English");
        let row = normalization_for(&read, &id)
            .expect("read row")
            .expect("row exists");
        assert_eq!(row.status, NormalizationStatus::Translated);
        assert_eq!(
            row.source_text.as_deref(),
            Some(RU),
            "the owner must still be able to read what they wrote",
        );
        assert_eq!(row.normalizer_model_id.as_deref(), Some("scripted-model"));
    }

    /// ADR-0011 §Decision 3: a refusal costs the author nothing.
    #[tokio::test]
    async fn a_refused_translation_keeps_the_note_and_records_why() {
        let generator = ScriptedGenerator::refusing();
        let fx = fixture(Some(generator.pool()));

        call_remember(&fx, RU, "req-1").await;

        assert_eq!(generator.calls(), 1);
        let read = fx.state.open_read().expect("read");
        let id: String = read
            .query_row("SELECT memory_id FROM memory_entry", [], |r| r.get(0))
            .expect("the note was stored anyway");
        assert_eq!(
            stored_text(&fx, &id),
            RU,
            "no half-translation, no lost note — the author's text stands",
        );
        let row = normalization_for(&read, &id)
            .expect("read row")
            .expect("row exists");
        assert_eq!(row.status, NormalizationStatus::Failed);
        assert!(row.last_error.is_some(), "the refusal is recorded");
        assert_eq!(row.attempt_count, 1, "a real refusal spends an attempt");
    }

    /// A daemon whose model is not installed keeps accepting notes, and the
    /// missing model does not cost the entry one of its attempts.
    #[tokio::test]
    async fn no_installed_model_is_a_refusal_not_a_failure_of_the_entry() {
        let fx = fixture(None);

        call_remember(&fx, RU, "req-1").await;

        let read = fx.state.open_read().expect("read");
        let id: String = read
            .query_row("SELECT memory_id FROM memory_entry", [], |r| r.get(0))
            .expect("the note was stored anyway");
        assert_eq!(stored_text(&fx, &id), RU);
        let row = normalization_for(&read, &id)
            .expect("read row")
            .expect("row exists");
        assert_eq!(row.status, NormalizationStatus::Failed);
        assert_eq!(
            row.attempt_count, 0,
            "a missing model is the environment's fault, not the entry's, so the sweep \
             must retry it the moment one appears",
        );
    }

    /// An edit is the same boundary as a create: new text becomes an English
    /// canon, and the row that described the *old* text is replaced rather than
    /// left behind.
    #[tokio::test]
    async fn editing_with_russian_text_installs_an_english_canon_and_replaces_the_row() {
        let generator = ScriptedGenerator::translating(EN);
        let fx = fixture(Some(generator.pool()));

        // Start from an English entry, so there is an `english` row to replace.
        call_remember(&fx, "the first version of this note", "req-1").await;
        let read = fx.state.open_read().expect("read");
        let (id, version): (String, i64) = read
            .query_row(
                "SELECT memory_id, entry_version FROM memory_entry",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("one entry");
        drop(read);
        assert_eq!(generator.calls(), 0);

        let mut args = Map::new();
        args.insert("id".to_string(), json!(id));
        args.insert("expected_version".to_string(), json!(version));
        args.insert("patch".to_string(), json!({ "text": RU }));
        edit_memory(&fx.ctx, &args, 2_000)
            .await
            .expect("handler ran");

        assert_eq!(generator.calls(), 1);
        assert_eq!(stored_text(&fx, &id), EN, "the new canon is English");
        let read = fx.state.open_read().expect("read");
        let row = normalization_for(&read, &id)
            .expect("read row")
            .expect("the row was replaced, not dropped");
        assert_eq!(row.status, NormalizationStatus::Translated);
        assert_eq!(row.source_text.as_deref(), Some(RU));
    }

    /// A replayed `remember` is recognised before the translator is asked, so a
    /// retry does not spend a second of local GPU reproducing an answer that
    /// will be discarded.
    #[tokio::test]
    async fn a_replay_does_not_translate_twice() {
        let generator = ScriptedGenerator::translating(EN);
        let fx = fixture(Some(generator.pool()));

        call_remember(&fx, RU, "req-1").await;
        assert_eq!(generator.calls(), 1);

        call_remember(&fx, RU, "req-1").await;
        assert_eq!(
            generator.calls(),
            1,
            "the idempotency key was checked before the translator",
        );
        let read = fx.state.open_read().expect("read");
        let entries: i64 = read
            .query_row("SELECT COUNT(*) FROM memory_entry", [], |r| r.get(0))
            .expect("count");
        assert_eq!(entries, 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use local_rag_store::{Candidate, WorktreeKind};

    fn resolved() -> Resolution {
        Resolution::Resolved {
            repo_id: "repo-1".to_string(),
            worktree_id: "wt-1".to_string(),
        }
    }

    fn ambiguous() -> Resolution {
        Resolution::Ambiguous {
            candidates: vec![
                Candidate {
                    repo_id: "repo-1".to_string(),
                    worktree_id: "wt-1".to_string(),
                    kind: WorktreeKind::Linked,
                },
                Candidate {
                    repo_id: "repo-1".to_string(),
                    worktree_id: "wt-2".to_string(),
                    kind: WorktreeKind::Linked,
                },
            ],
        }
    }

    #[test]
    fn a_resolved_root_defaults_to_repository_without_degrading() {
        let scope = resolve_write_scope(&resolved(), None).expect("resolved roots never error");
        assert_eq!(scope.kind, ScopeKind::Repository);
        assert_eq!(scope.owner_id, "repo-1");
        assert_eq!(scope.fallback, None);
    }

    /// D-064: the implicit `global` is still written (spec 02 §6's table says
    /// memory tools work in repo/global scope) but is now reported.
    #[test]
    fn an_unresolved_root_falls_back_to_global_and_says_so() {
        let scope = resolve_write_scope(&Resolution::GlobalOnly, None)
            .expect("the fallback is not an error");
        assert_eq!(scope.kind, ScopeKind::Global);
        assert_eq!(scope.owner_id, GLOBAL_SCOPE_OWNER_ID);
        assert_eq!(scope.fallback, Some(ScopeFallback::WorktreeNotIndexed));
    }

    /// An ambiguous root has its own remedy (`repo attach`, spec 04 §7), so it
    /// is a distinct marker rather than the same "not indexed" string.
    #[test]
    fn an_ambiguous_root_falls_back_with_its_own_marker() {
        let scope = resolve_write_scope(&ambiguous(), None).expect("the fallback is not an error");
        assert_eq!(scope.kind, ScopeKind::Global);
        assert_eq!(scope.fallback, Some(ScopeFallback::WorktreeAmbiguous));
        assert_ne!(
            ScopeFallback::WorktreeAmbiguous.hint(),
            ScopeFallback::WorktreeNotIndexed.hint()
        );
    }

    /// An explicitly requested `global` is the caller's own choice — nothing
    /// degraded, so nothing to report.
    #[test]
    fn an_explicit_global_request_is_never_a_degradation() {
        for resolution in [resolved(), Resolution::GlobalOnly, ambiguous()] {
            let scope = resolve_write_scope(&resolution, Some(ScopeKind::Global))
                .expect("global is always satisfiable");
            assert_eq!(scope.kind, ScopeKind::Global);
            assert_eq!(scope.fallback, None, "{resolution:?}");
        }
    }

    #[test]
    fn an_explicit_repository_or_worktree_request_while_unresolved_still_errors() {
        for requested in [ScopeKind::Repository, ScopeKind::Worktree] {
            let err = resolve_write_scope(&Resolution::GlobalOnly, Some(requested))
                .expect_err("the caller asked for a scope this request cannot supply");
            assert_eq!(err.code.as_str(), "WORKTREE_NOT_INDEXED", "{requested:?}");
        }
    }
}
