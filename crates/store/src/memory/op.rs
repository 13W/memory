//! The shared transactional memory-op engine (spec 08 §3): `create`,
//! `reinforce`, `noop` — the atomic mutation+evidence+audit+idempotency
//! contract every memory operation follows. `resolve`/`supersede`/`retract`/
//! `edit` (T14-03) and `merge_memories` (T14-04) compose the same primitives
//! this module establishes; this module does not implement them.
//!
//! Mirrors [`crate::observation::import::import_batch`]'s shape: each `apply_*`
//! is a **sync** `fn(&Transaction<'_>, ...)` that composes several sibling
//! low-level [`super::entry`]/[`super::evidence`]/[`super::audit`] primitives
//! inside a transaction the *caller* already opened (via
//! [`crate::StateWriter::transaction`]) — this module never opens one itself.
//!
//! # Idempotency (spec 08 §3: "same `idempotency_key` ⇒ recognized as already
//! applied, returns the original result")
//!
//! For a router-originated op (`idempotency_key: Some(_)`), every `apply_*`
//! checks [`super::audit::find_by_idempotency_key`] **first**, inside the same
//! transaction as the rest of the operation. A hit short-circuits: nothing
//! else is read or written, and [`MemoryOpOutcome::Replayed`] is reconstructed
//! directly from the matching `audit_event` row. This is what makes the
//! "returns the original result" guarantee true by construction rather than
//! by re-deriving equal output on every retry.
//!
//! # `noop` writes nothing
//!
//! Spec 08 §4's op envelope is `{create, reinforce, supersede, resolve,
//! retract, noop, propose_candidate}` "with target/kind/text/scope/
//! canonical_key/confidence inputs" — read as those fields being populated
//! *as relevant to each op*. `noop` needs none of them: it is the router's
//! "considered, no action" acknowledgment, with no target at all. Recording it
//! as its own `audit_event` would need *some* `(entity_kind, entity_id,
//! entity_version)`, but attaching it to the examined entry's current
//! (unchanged) version breaks the very first time two independent
//! consolidation runs both examine the same still-unmodified entry and both
//! decide "no action" — `audit_event`'s `UNIQUE (entity_kind, entity_id,
//! entity_version)` would reject the second, legitimate decision as if it
//! were a duplicate. [`apply_noop`] sidesteps this by writing nothing at all:
//! redoing nothing on retry is still nothing, so it needs no
//! `idempotency_key` bookkeeping either.

use rusqlite::{OptionalExtension, Transaction, params};

use super::audit::{
    Actor, AuditEventRow, NewAuditEvent, find_by_idempotency_key, insert_audit_event,
};
use super::entry::{
    CreateMemoryEntryError, MemoryKind, NewMemoryEntry, ScopeKind, create_memory_entry,
};
use super::evidence::{NewMemoryEvidence, insert_memory_evidence};
use crate::observation::EvidenceKind;

/// `audit_event.entity_kind` this module writes — every op here targets a
/// `memory_entry` row (or, for `noop`, nothing at all).
const ENTITY_KIND_MEMORY_ENTRY: &str = "memory_entry";

/// One evidence link to attach when applying an operation — [`NewMemoryEvidence`]
/// minus `memory_id`, which the enclosing op already carries.
#[derive(Debug, Clone, Copy)]
pub struct EvidenceInput<'a> {
    pub observation_id: &'a str,
    pub evidence_kind: EvidenceKind,
    pub session_id: &'a str,
    pub agent_id: Option<&'a str>,
    pub commit_hash: Option<&'a str>,
}

/// A `create` request, mirroring [`NewMemoryEntry`] plus the evidence/audit
/// fields the op contract (spec 08 §3) requires alongside the mutation.
#[derive(Debug, Clone, Copy)]
pub struct CreateMemoryOp<'a> {
    pub memory_id: &'a str,
    pub kind: MemoryKind,
    pub text: &'a str,
    pub canonical_key: Option<&'a str>,
    pub scope_kind: ScopeKind,
    pub scope_owner_id: &'a str,
    pub confidence: f64,
    pub importance: f64,
    pub valid_from_tree: Option<&'a str>,
    pub last_verified_tree: Option<&'a str>,
    pub evidence: &'a [EvidenceInput<'a>],
    pub actor: Actor,
    /// `Some` for a router-originated op (spec 08 §3); `None` for a direct
    /// tool call (e.g. `remember`, 08 §5), which has no retry to recognize.
    pub idempotency_key: Option<&'a str>,
}

/// A `reinforce` request. Deliberately carries **no `text` field** — reinforce
/// can never edit text, a structural guarantee rather than a runtime check
/// (spec 08 §3 `[FIXED]`).
#[derive(Debug, Clone, Copy)]
pub struct ReinforceMemoryOp<'a> {
    pub memory_id: &'a str,
    pub expected_version: i64,
    /// `Some` to raise confidence to this value; `None` to leave it
    /// unchanged (evidence-only reinforcement).
    pub confidence: Option<f64>,
    pub evidence: &'a [EvidenceInput<'a>],
    pub actor: Actor,
    pub idempotency_key: Option<&'a str>,
}

/// What a successful `create`/`reinforce` produced (spec 08 §3: "Response
/// carries the new `entry_version` and `audit_id`").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryOpResult {
    pub memory_id: String,
    pub entry_version: i64,
    pub audit_id: i64,
}

/// The outcome of a `create`/`reinforce` call: freshly applied, or recognized
/// as an already-applied retry and replayed from the recorded
/// `idempotency_key` (spec 08 §3). Mirrors this crate's `Reused`/`Created`
/// outcome-enum family (e.g. `code::revision::RevisionOutcome`) — a caller
/// that only cares about the resulting state can match on the inner
/// [`MemoryOpResult`] either way; a caller (or test) that cares whether a new
/// mutation actually happened matches on the variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryOpOutcome {
    Applied(MemoryOpResult),
    Replayed(MemoryOpResult),
}

/// Why a `create`/`reinforce` request was rejected at the domain level (as
/// opposed to an infrastructure/SQLite failure, which surfaces as the outer
/// [`rusqlite::Error`] and rolls the transaction back).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryOpError {
    /// `reinforce` targets a `memory_id` with no `memory_entry` row.
    UnknownMemory,
    /// `reinforce`'s `expected_version` does not match the entry's current
    /// `entry_version` — optimistic concurrency (spec 08 §3).
    OptimisticConflict { expected: i64, actual: i64 },
    /// `create`'s `canonical_key` already exists in the same
    /// `(scope_kind, scope_owner_id)` — spec 08 §3's "scope uniqueness"
    /// precondition, surfaced as a typed error (per the spec's own wording)
    /// rather than the raw `UNIQUE` constraint violation
    /// [`create_memory_entry`](super::entry::create_memory_entry) itself
    /// leaves unwrapped.
    CanonicalKeyConflict,
    /// `create`'s `scope_kind = 'global'` with a non-singleton
    /// `scope_owner_id` (spec 03 §2.5 `[SPEC]`) — wraps
    /// [`CreateMemoryEntryError::InvalidGlobalScopeOwner`].
    InvalidGlobalScopeOwner,
}

impl std::fmt::Display for MemoryOpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MemoryOpError::UnknownMemory => write!(f, "unknown memory entry"),
            MemoryOpError::OptimisticConflict { expected, actual } => write!(
                f,
                "optimistic conflict: expected entry_version {expected}, found {actual}"
            ),
            MemoryOpError::CanonicalKeyConflict => {
                write!(f, "canonical_key already exists in this scope")
            }
            MemoryOpError::InvalidGlobalScopeOwner => {
                write!(f, "scope_kind='global' requires the singleton scope owner")
            }
        }
    }
}

impl std::error::Error for MemoryOpError {}

/// Reconstruct the result a prior `create`/`reinforce` produced from its
/// `audit_event` row — what makes an idempotency-key replay return the
/// *original* result rather than a freshly (re)computed one.
fn result_from_row(row: &AuditEventRow) -> MemoryOpResult {
    MemoryOpResult {
        memory_id: row.entity_id.clone(),
        entry_version: row.entity_version,
        audit_id: row.audit_id,
    }
}

fn insert_evidence_rows(
    tx: &Transaction<'_>,
    memory_id: &str,
    evidence: &[EvidenceInput<'_>],
) -> rusqlite::Result<()> {
    for e in evidence {
        insert_memory_evidence(
            tx,
            &NewMemoryEvidence {
                memory_id,
                observation_id: e.observation_id,
                evidence_kind: e.evidence_kind,
                session_id: e.session_id,
                agent_id: e.agent_id,
                commit_hash: e.commit_hash,
            },
        )?;
    }
    Ok(())
}

/// Apply a `create` (spec 08 §3): mint a `memory_entry` (born `active`,
/// `entry_version = 1`), attach its initial evidence, and write the
/// `audit_event` — one transaction, or nothing at all on domain rejection.
///
/// Precondition order: idempotency-key replay first; then the typed
/// `canonical_key` scope-uniqueness check (a pre-check `SELECT`, not a caught
/// constraint violation, so a conflict never touches the table); then
/// [`create_memory_entry`]'s own global-scope-owner guard. A conflict on
/// either aborts with **no mutation**, matching this crate's
/// read-then-write idiom.
pub fn apply_create(
    tx: &Transaction<'_>,
    request: &CreateMemoryOp<'_>,
    now_ms: i64,
) -> rusqlite::Result<Result<MemoryOpOutcome, MemoryOpError>> {
    if let Some(key) = request.idempotency_key
        && let Some(existing) = find_by_idempotency_key(tx, key)?
    {
        return Ok(Ok(MemoryOpOutcome::Replayed(result_from_row(&existing))));
    }

    if let Some(canonical_key) = request.canonical_key {
        let conflict: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM memory_entry \
                 WHERE scope_kind = ?1 AND scope_owner_id = ?2 AND canonical_key = ?3",
                params![
                    request.scope_kind.as_str(),
                    request.scope_owner_id,
                    canonical_key
                ],
                |r| r.get(0),
            )
            .optional()?;
        if conflict.is_some() {
            return Ok(Err(MemoryOpError::CanonicalKeyConflict));
        }
    }

    let create_result = create_memory_entry(
        tx,
        &NewMemoryEntry {
            memory_id: request.memory_id,
            kind: request.kind,
            text: request.text,
            canonical_key: request.canonical_key,
            scope_kind: request.scope_kind,
            scope_owner_id: request.scope_owner_id,
            confidence: request.confidence,
            importance: request.importance,
            valid_from_tree: request.valid_from_tree,
            last_verified_tree: request.last_verified_tree,
            supersedes_id: None,
        },
        now_ms,
    )?;
    if let Err(CreateMemoryEntryError::InvalidGlobalScopeOwner) = create_result {
        return Ok(Err(MemoryOpError::InvalidGlobalScopeOwner));
    }

    insert_evidence_rows(tx, request.memory_id, request.evidence)?;

    #[cfg(feature = "failpoints")]
    local_rag_test_support::fail_point!(
        "memory.op.create.before_audit",
        Err(rusqlite::Error::ToSqlConversionFailure(
            "failpoint: memory.op.create.before_audit".into()
        ))
    );

    let audit_id = insert_audit_event(
        tx,
        &NewAuditEvent {
            entity_kind: ENTITY_KIND_MEMORY_ENTRY,
            entity_id: request.memory_id,
            entity_version: 1,
            op: "create",
            actor: request.actor,
            idempotency_key: request.idempotency_key,
            payload: None,
        },
        now_ms,
    )?;

    Ok(Ok(MemoryOpOutcome::Applied(MemoryOpResult {
        memory_id: request.memory_id.to_string(),
        entry_version: 1,
        audit_id,
    })))
}

/// Apply a `reinforce` (spec 08 §3): add evidence to an **existing** entry
/// and optionally raise `confidence`; **never touches `text`** (no such field
/// exists on [`ReinforceMemoryOp`]). Every successful apply bumps
/// `entry_version` — even when `confidence` is unchanged, adding evidence is
/// itself a real mutation whose `audit_event` needs a version to attach to.
///
/// Precondition order: idempotency-key replay first; then `expected_version`
/// optimistic concurrency (spec 08 §3) — an unknown `memory_id` or a stale
/// version aborts with **no mutation**. Does not check the entry's `kind`/
/// `state` (out of scope for this task — see the module's owning task card).
pub fn apply_reinforce(
    tx: &Transaction<'_>,
    request: &ReinforceMemoryOp<'_>,
    now_ms: i64,
) -> rusqlite::Result<Result<MemoryOpOutcome, MemoryOpError>> {
    if let Some(key) = request.idempotency_key
        && let Some(existing) = find_by_idempotency_key(tx, key)?
    {
        return Ok(Ok(MemoryOpOutcome::Replayed(result_from_row(&existing))));
    }

    let current_version: Option<i64> = tx
        .query_row(
            "SELECT entry_version FROM memory_entry WHERE memory_id = ?1",
            params![request.memory_id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(current_version) = current_version else {
        return Ok(Err(MemoryOpError::UnknownMemory));
    };
    if current_version != request.expected_version {
        return Ok(Err(MemoryOpError::OptimisticConflict {
            expected: request.expected_version,
            actual: current_version,
        }));
    }

    let new_version = current_version + 1;
    tx.execute(
        "UPDATE memory_entry \
         SET confidence = COALESCE(?2, confidence), entry_version = ?3, updated_at = ?4 \
         WHERE memory_id = ?1",
        params![request.memory_id, request.confidence, new_version, now_ms],
    )?;

    insert_evidence_rows(tx, request.memory_id, request.evidence)?;

    #[cfg(feature = "failpoints")]
    local_rag_test_support::fail_point!(
        "memory.op.reinforce.before_audit",
        Err(rusqlite::Error::ToSqlConversionFailure(
            "failpoint: memory.op.reinforce.before_audit".into()
        ))
    );

    let audit_id = insert_audit_event(
        tx,
        &NewAuditEvent {
            entity_kind: ENTITY_KIND_MEMORY_ENTRY,
            entity_id: request.memory_id,
            entity_version: new_version,
            op: "reinforce",
            actor: request.actor,
            idempotency_key: request.idempotency_key,
            payload: None,
        },
        now_ms,
    )?;

    Ok(Ok(MemoryOpOutcome::Applied(MemoryOpResult {
        memory_id: request.memory_id.to_string(),
        entry_version: new_version,
        audit_id,
    })))
}

/// Apply a `noop` (spec 08 §3/§4): the router's "considered, no action"
/// acknowledgment. Takes no transaction and touches no table — see the
/// module doc for why this is a zero-write design rather than its own
/// `audit_event`. Exists as a real function (not "callers just skip it") so
/// a future op-dispatch loop (T14-06) can call every op kind uniformly.
pub fn apply_noop() {}
