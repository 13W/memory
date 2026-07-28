//! Candidate review operations (spec 03 §2.5, 04 §6, 08 §3/§5/§8): `propose`/
//! `edit`/`approve`/`reject` over `pending_memory_candidate`, on top of
//! [`super::candidate`]'s DDL/state-machine primitives (T14-01) and
//! [`super::op`]'s transactional op engine (T14-02–04). Candidate expiry
//! (spec 04 §6 `[SPEC]` default 30 days) is a batch sweep in
//! [`crate::housekeeping::run_candidate_expiry_sweep`], not here — it mirrors
//! this crate's other GC-style sweeps rather than living per-domain.
//!
//! # `proposed_operation` — a tagged JSON enum, not a caller-opaque blob
//!
//! Spec 08 §4's router op list is `{create, reinforce, supersede, resolve,
//! retract, noop, propose_candidate}`; `noop` materializes nothing and
//! `propose_candidate` cannot nest itself, so exactly five ops are ever
//! candidate-proposed — [`ProposedOperation`] has exactly those five
//! variants. `kind`/`scope_kind` are plain `String` fields, parsed via the
//! existing [`super::entry::MemoryKind::from_db`]/
//! [`super::entry::ScopeKind::from_db`] at [`approve_candidate`] time —
//! deliberately *not* adding `serde` derives to those T14-01 types, keeping
//! the JSON concern local to this file. `memory_id`(s) are embedded in the
//! payload, decided at propose time: the same "caller mints the id, never
//! inside the write path" discipline every other op already follows.
//!
//! # Materialization evidence comes from `candidate_evidence`, not a copy
//!
//! `candidate_evidence`'s own DDL comment already states the principle this
//! generalizes: "FK provenance, not embedded snapshots." It has no
//! `evidence_kind`/`session_id` columns (unlike `memory_evidence`), so
//! [`approve_candidate`] derives them by reading each linked
//! `observation_id`'s own `observation_envelope.evidence_kind`/`session_id`
//! (already durable from T13-04) and builds the underlying op's
//! `EvidenceInput` slice from that — no redundant storage, and "FK evidence"
//! (spec 08's own phrase for this task) is satisfied through the FK chain
//! that already exists rather than a new one.
//!
//! # Double-approval idempotence, cheapest layer first
//!
//! [`approve_candidate`] reads the candidate's current `review_state` before
//! doing anything else. Already `approved` → [`ApproveCandidateOutcome::
//! AlreadyApproved`] immediately: no JSON parsing, no op-engine call, no
//! re-materialization — the state machine's own self-transition-is-legal
//! convention *is* the idempotence guarantee here, cheaper than a round-trip
//! through `find_by_idempotency_key`. As defense-in-depth for a genuine
//! crash-and-retry mid-transaction, the underlying op call still carries a
//! deterministic `idempotency_key` (`"candidate:<candidate_id>"`) so even a
//! retry that reaches the op-engine layer resolves via T14-02's existing
//! replay mechanism. `reject`/`expire` on an already-terminal candidate stay
//! ordinary [`ReviewError::IllegalTransition`] rejections (matching
//! `CandidateState::check_transition`'s existing terminal-everything
//! semantics) — only `approve → approve` gets special no-op treatment, since
//! it is the only one spec 04 §6's card names.
//!
//! # "Conflicting edit" has no numeric version to check against
//!
//! `pending_memory_candidate` has no `entry_version`/`updated_at` column
//! (unlike `memory_entry`), and spec 11 §2's own `edit_memory_candidate(id,
//! patch)` signature (contrast `edit_memory(id, patch, expected_version)`)
//! confirms this is intentional. [`edit_candidate`]'s conflict check is
//! state-based: legal only while `review_state == pending`; editing an
//! already-approved/rejected/expired candidate is [`ReviewError::NotPending`].

use rusqlite::types::Type;
use rusqlite::{Connection, Error, Transaction, params};
use serde::{Deserialize, Serialize};

use super::audit::Actor;
use super::candidate::{
    CandidateState, CandidateTransitionError, IllegalCandidateTransition, NewCandidate,
    candidate_evidence_for, candidate_state, create_candidate, insert_candidate_evidence,
    transition_candidate,
};
use super::entry::{MemoryKind, ScopeKind};
use super::op::{
    CreateMemoryOp, EvidenceInput, MemoryOpError, MemoryOpOutcome, ReinforceMemoryOp,
    ResolveMemoryOp, RetractMemoryOp, SupersedeMemoryOp, apply_create, apply_reinforce,
    apply_resolve, apply_retract, apply_supersede,
};
use crate::observation::EvidenceKind;

/// The materialized shape of a candidate's proposal (spec 08 §4's router op
/// envelope, restricted to the five ops that are ever candidate-proposed —
/// see the module doc). Wire format: `{"op": "create", ...fields}` (`serde`
/// internally tagged, `snake_case`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ProposedOperation {
    Create {
        memory_id: String,
        kind: String,
        text: String,
        canonical_key: Option<String>,
        scope_kind: String,
        scope_owner_id: String,
        confidence: f64,
        importance: f64,
        valid_from_tree: Option<String>,
        last_verified_tree: Option<String>,
    },
    Reinforce {
        memory_id: String,
        expected_version: i64,
        confidence: Option<f64>,
    },
    Resolve {
        memory_id: String,
        expected_version: i64,
    },
    Retract {
        memory_id: String,
        expected_version: i64,
    },
    Supersede {
        old_memory_id: String,
        old_expected_version: i64,
        new_memory_id: String,
        new_kind: String,
        new_text: String,
        new_canonical_key: Option<String>,
        new_scope_kind: String,
        new_scope_owner_id: String,
        new_confidence: f64,
        new_importance: f64,
        new_valid_from_tree: Option<String>,
        new_last_verified_tree: Option<String>,
    },
}

/// Why a candidate-review request was rejected at the domain level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewError {
    /// No `pending_memory_candidate` row has this id.
    UnknownCandidate,
    /// The candidate machine (spec 04 §6) forbids the requested transition —
    /// wraps [`IllegalCandidateTransition`].
    IllegalTransition(IllegalCandidateTransition),
    /// [`edit_candidate`] targets a candidate whose `review_state` is no
    /// longer `pending` — the "conflicting edit" case (see the module doc:
    /// candidates have no numeric version to check instead).
    NotPending,
    /// `proposed_operation` failed to parse as [`ProposedOperation`], or one
    /// of its `kind`/`scope_kind` strings is outside the CHECK domain.
    InvalidProposedOperation(String),
    /// The dispatched op itself was rejected by the op engine (spec 08 §3
    /// preconditions — optimistic conflict, canonical-key conflict, illegal
    /// transition, ...).
    Materialization(MemoryOpError),
}

impl std::fmt::Display for ReviewError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReviewError::UnknownCandidate => write!(f, "unknown candidate"),
            ReviewError::IllegalTransition(e) => write!(f, "{e}"),
            ReviewError::NotPending => write!(f, "candidate is no longer pending"),
            ReviewError::InvalidProposedOperation(detail) => {
                write!(f, "invalid proposed_operation: {detail}")
            }
            ReviewError::Materialization(e) => write!(f, "materialization failed: {e}"),
        }
    }
}

impl std::error::Error for ReviewError {}

/// The outcome of [`approve_candidate`]: freshly materialized, or recognized
/// as an already-approved candidate and short-circuited before touching the
/// op engine again (see the module doc's "double-approval idempotence").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApproveCandidateOutcome {
    Materialized(MemoryOpOutcome),
    AlreadyApproved,
}

/// One read-back `pending_memory_candidate` row (spec 03 §2.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateRow {
    pub candidate_id: String,
    pub proposed_operation: String,
    pub conflicts: Option<String>,
    pub review_state: CandidateState,
    pub created_at: i64,
}

/// Propose a candidate: insert the `pending_memory_candidate` row plus its
/// `candidate_evidence` FK links, in the caller's transaction. A duplicate
/// `candidate_id` surfaces as the natural `rusqlite::Error` (PRIMARY KEY) —
/// no special handling, mirroring this crate's default for a caller-minted,
/// already-unique id (e.g. `create_repository`).
pub fn propose_candidate(
    tx: &Transaction<'_>,
    candidate_id: &str,
    proposed_operation: &ProposedOperation,
    conflicts: &[&str],
    evidence_observation_ids: &[&str],
    now_ms: i64,
) -> rusqlite::Result<()> {
    let proposed_json =
        serde_json::to_string(proposed_operation).expect("ProposedOperation serializes infallibly");
    let conflicts_json = if conflicts.is_empty() {
        None
    } else {
        Some(serde_json::to_string(conflicts).expect("memory ids serialize infallibly"))
    };
    create_candidate(
        tx,
        &NewCandidate {
            candidate_id,
            proposed_operation: &proposed_json,
            conflicts: conflicts_json.as_deref(),
        },
        now_ms,
    )?;
    for observation_id in evidence_observation_ids {
        insert_candidate_evidence(tx, candidate_id, observation_id)?;
    }
    Ok(())
}

/// Edit a candidate's `proposed_operation`/`conflicts` while it is still
/// `pending` (spec 11 §2 `edit_memory_candidate(id, patch)`). `None` leaves
/// that field unchanged. Editing a non-`pending` candidate is
/// [`ReviewError::NotPending`] — no mutation.
pub fn edit_candidate(
    tx: &Transaction<'_>,
    candidate_id: &str,
    new_proposed_operation: Option<&ProposedOperation>,
    new_conflicts: Option<&[&str]>,
) -> rusqlite::Result<Result<(), ReviewError>> {
    let Some(state) = candidate_state(tx, candidate_id)? else {
        return Ok(Err(ReviewError::UnknownCandidate));
    };
    if state != CandidateState::Pending {
        return Ok(Err(ReviewError::NotPending));
    }

    if let Some(op) = new_proposed_operation {
        let json = serde_json::to_string(op).expect("ProposedOperation serializes infallibly");
        tx.execute(
            "UPDATE pending_memory_candidate SET proposed_operation = ?2 WHERE candidate_id = ?1",
            params![candidate_id, json],
        )?;
    }
    if let Some(conflicts) = new_conflicts {
        let json = if conflicts.is_empty() {
            None
        } else {
            Some(serde_json::to_string(conflicts).expect("memory ids serialize infallibly"))
        };
        tx.execute(
            "UPDATE pending_memory_candidate SET conflicts = ?2 WHERE candidate_id = ?1",
            params![candidate_id, json],
        )?;
    }
    Ok(Ok(()))
}

/// Reject a candidate (spec 04 §6): `pending → rejected`. Never touches the
/// op engine — "rejected never materializes."
pub fn reject_candidate(
    tx: &Transaction<'_>,
    candidate_id: &str,
) -> rusqlite::Result<Result<(), ReviewError>> {
    match transition_candidate(tx, candidate_id, CandidateState::Rejected)? {
        Ok(()) => Ok(Ok(())),
        Err(CandidateTransitionError::UnknownCandidate) => Ok(Err(ReviewError::UnknownCandidate)),
        Err(CandidateTransitionError::Illegal(e)) => Ok(Err(ReviewError::IllegalTransition(e))),
    }
}

/// Approve a candidate (spec 04 §6): materialize `proposed_operation`
/// through the same transactional memory-op path as the router
/// (`actor='user'`, spec's own wording), then transition the candidate to
/// `approved` — one transaction. See the module doc for the double-approval
/// short-circuit and the FK-evidence derivation.
pub fn approve_candidate(
    tx: &Transaction<'_>,
    candidate_id: &str,
    now_ms: i64,
) -> rusqlite::Result<Result<ApproveCandidateOutcome, ReviewError>> {
    let Some((review_state, proposed_operation_json)) =
        candidate_state_and_proposal(tx, candidate_id)?
    else {
        return Ok(Err(ReviewError::UnknownCandidate));
    };

    if review_state == CandidateState::Approved {
        return Ok(Ok(ApproveCandidateOutcome::AlreadyApproved));
    }
    if let Err(illegal) = review_state.check_transition(CandidateState::Approved) {
        return Ok(Err(ReviewError::IllegalTransition(illegal)));
    }

    let proposed: ProposedOperation = match serde_json::from_str(&proposed_operation_json) {
        Ok(p) => p,
        Err(e) => return Ok(Err(ReviewError::InvalidProposedOperation(e.to_string()))),
    };

    let observation_ids = candidate_evidence_for(tx, candidate_id)?;
    let mut sourced = Vec::with_capacity(observation_ids.len());
    for observation_id in &observation_ids {
        if let Some((evidence_kind, session_id)) = observation_evidence_source(tx, observation_id)?
        {
            sourced.push((observation_id.clone(), evidence_kind, session_id));
        }
    }
    let evidence: Vec<EvidenceInput<'_>> = sourced
        .iter()
        .map(
            |(observation_id, evidence_kind, session_id)| EvidenceInput {
                observation_id,
                evidence_kind: *evidence_kind,
                session_id,
                agent_id: None,
                commit_hash: None,
            },
        )
        .collect();

    let idempotency_key = format!("candidate:{candidate_id}");
    let outcome = match materialize(tx, &proposed, &evidence, &idempotency_key, now_ms)? {
        Ok(outcome) => outcome,
        Err(e) => return Ok(Err(e)),
    };

    tx.execute(
        "UPDATE pending_memory_candidate SET review_state = ?2 WHERE candidate_id = ?1",
        params![candidate_id, CandidateState::Approved.as_str()],
    )?;

    Ok(Ok(ApproveCandidateOutcome::Materialized(outcome)))
}

/// Dispatch `proposed` to the matching `op::apply_*`, `actor=User` (spec 04
/// §6). Parses `kind`/`scope_kind` strings here, surfacing a bad value as
/// [`ReviewError::InvalidProposedOperation`] before any write.
fn materialize(
    tx: &Transaction<'_>,
    proposed: &ProposedOperation,
    evidence: &[EvidenceInput<'_>],
    idempotency_key: &str,
    now_ms: i64,
) -> rusqlite::Result<Result<MemoryOpOutcome, ReviewError>> {
    let outcome = match proposed {
        ProposedOperation::Create {
            memory_id,
            kind,
            text,
            canonical_key,
            scope_kind,
            scope_owner_id,
            confidence,
            importance,
            valid_from_tree,
            last_verified_tree,
        } => {
            let Some(kind) = MemoryKind::from_db(kind) else {
                return Ok(Err(ReviewError::InvalidProposedOperation(format!(
                    "invalid kind {kind:?}"
                ))));
            };
            let Some(scope_kind) = ScopeKind::from_db(scope_kind) else {
                return Ok(Err(ReviewError::InvalidProposedOperation(format!(
                    "invalid scope_kind {scope_kind:?}"
                ))));
            };
            apply_create(
                tx,
                &CreateMemoryOp {
                    memory_id,
                    kind,
                    text,
                    canonical_key: canonical_key.as_deref(),
                    scope_kind,
                    scope_owner_id,
                    confidence: *confidence,
                    importance: *importance,
                    valid_from_tree: valid_from_tree.as_deref(),
                    last_verified_tree: last_verified_tree.as_deref(),
                    evidence,
                    actor: Actor::User,
                    idempotency_key: Some(idempotency_key),
                },
                now_ms,
            )?
        }
        ProposedOperation::Reinforce {
            memory_id,
            expected_version,
            confidence,
        } => apply_reinforce(
            tx,
            &ReinforceMemoryOp {
                memory_id,
                expected_version: *expected_version,
                confidence: *confidence,
                evidence,
                actor: Actor::User,
                idempotency_key: Some(idempotency_key),
            },
            now_ms,
        )?,
        ProposedOperation::Resolve {
            memory_id,
            expected_version,
        } => apply_resolve(
            tx,
            &ResolveMemoryOp {
                memory_id,
                expected_version: *expected_version,
                evidence,
                actor: Actor::User,
                idempotency_key: Some(idempotency_key),
            },
            now_ms,
        )?,
        ProposedOperation::Retract {
            memory_id,
            expected_version,
        } => apply_retract(
            tx,
            &RetractMemoryOp {
                memory_id,
                expected_version: *expected_version,
                evidence,
                actor: Actor::User,
                idempotency_key: Some(idempotency_key),
            },
            now_ms,
        )?,
        ProposedOperation::Supersede {
            old_memory_id,
            old_expected_version,
            new_memory_id,
            new_kind,
            new_text,
            new_canonical_key,
            new_scope_kind,
            new_scope_owner_id,
            new_confidence,
            new_importance,
            new_valid_from_tree,
            new_last_verified_tree,
        } => {
            let Some(new_kind) = MemoryKind::from_db(new_kind) else {
                return Ok(Err(ReviewError::InvalidProposedOperation(format!(
                    "invalid kind {new_kind:?}"
                ))));
            };
            let Some(new_scope_kind) = ScopeKind::from_db(new_scope_kind) else {
                return Ok(Err(ReviewError::InvalidProposedOperation(format!(
                    "invalid scope_kind {new_scope_kind:?}"
                ))));
            };
            apply_supersede(
                tx,
                &SupersedeMemoryOp {
                    old_memory_id,
                    old_expected_version: *old_expected_version,
                    new_memory_id,
                    new_kind,
                    new_text,
                    new_canonical_key: new_canonical_key.as_deref(),
                    new_scope_kind,
                    new_scope_owner_id,
                    new_confidence: *new_confidence,
                    new_importance: *new_importance,
                    new_valid_from_tree: new_valid_from_tree.as_deref(),
                    new_last_verified_tree: new_last_verified_tree.as_deref(),
                    evidence,
                    actor: Actor::User,
                    idempotency_key: Some(idempotency_key),
                },
                now_ms,
            )?
        }
    };
    Ok(outcome.map_err(ReviewError::Materialization))
}

/// `(review_state, proposed_operation)` for `candidate_id`, if it exists.
fn candidate_state_and_proposal(
    tx: &Transaction<'_>,
    candidate_id: &str,
) -> rusqlite::Result<Option<(CandidateState, String)>> {
    use rusqlite::OptionalExtension;
    tx.query_row(
        "SELECT review_state, proposed_operation FROM pending_memory_candidate \
         WHERE candidate_id = ?1",
        params![candidate_id],
        |r| {
            let raw: String = r.get(0)?;
            let state = CandidateState::from_db(&raw).ok_or_else(|| {
                Error::FromSqlConversionFailure(
                    0,
                    Type::Text,
                    format!("invalid pending_memory_candidate.review_state {raw:?}").into(),
                )
            })?;
            let proposed_operation: String = r.get(1)?;
            Ok((state, proposed_operation))
        },
    )
    .optional()
}

/// An `observation_envelope`'s own `(evidence_kind, session_id)`, if it
/// exists — what [`approve_candidate`] derives the materializing op's
/// evidence from (see the module doc).
fn observation_evidence_source(
    tx: &Transaction<'_>,
    observation_id: &str,
) -> rusqlite::Result<Option<(EvidenceKind, String)>> {
    use rusqlite::OptionalExtension;
    tx.query_row(
        "SELECT evidence_kind, session_id FROM observation_envelope WHERE observation_id = ?1",
        params![observation_id],
        |r| {
            let raw: String = r.get(0)?;
            let kind = EvidenceKind::from_db(&raw).ok_or_else(|| {
                Error::FromSqlConversionFailure(
                    0,
                    Type::Text,
                    format!("invalid observation_envelope.evidence_kind {raw:?}").into(),
                )
            })?;
            let session_id: String = r.get(1)?;
            Ok((kind, session_id))
        },
    )
    .optional()
}

/// List candidates, optionally filtered to one `review_state`, ascending by
/// `created_at` (spec 03 §2.5; the card's "list exposes version/provenance"
/// — with no numeric version on this table, `review_state`/`created_at` are
/// its own staleness signal; pair with
/// [`candidate_evidence_for`](super::candidate::candidate_evidence_for) for
/// provenance).
pub fn list_candidates(
    conn: &Connection,
    review_state_filter: Option<CandidateState>,
) -> rusqlite::Result<Vec<CandidateRow>> {
    let mut stmt = conn.prepare(
        "SELECT candidate_id, proposed_operation, conflicts, review_state, created_at \
         FROM pending_memory_candidate \
         WHERE ?1 IS NULL OR review_state = ?1 \
         ORDER BY created_at, candidate_id",
    )?;
    let filter = review_state_filter.map(CandidateState::as_str);
    let rows = stmt.query_map(params![filter], |r| {
        let raw: String = r.get(3)?;
        let review_state = CandidateState::from_db(&raw).ok_or_else(|| {
            Error::FromSqlConversionFailure(
                3,
                Type::Text,
                format!("invalid pending_memory_candidate.review_state {raw:?}").into(),
            )
        })?;
        Ok(CandidateRow {
            candidate_id: r.get(0)?,
            proposed_operation: r.get(1)?,
            conflicts: r.get(2)?,
            review_state,
            created_at: r.get(4)?,
        })
    })?;
    rows.collect()
}
