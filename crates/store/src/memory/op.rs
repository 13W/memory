//! The shared transactional memory-op engine (spec 08 §3): `create`,
//! `reinforce`, `noop` (T14-02), plus `resolve`/`supersede`/`retract`/`edit`
//! (T14-03) — the atomic mutation+evidence+audit+idempotency contract every
//! memory operation follows. `merge_memories` (T14-04) composes the same
//! primitives this module establishes; this module does not implement it.
//!
//! Mirrors [`crate::observation::import::import_batch`]'s shape: each `apply_*`
//! is a **sync** `fn(&Transaction<'_>, ...)` that composes several sibling
//! low-level [`super::entry`]/[`super::evidence`]/[`super::audit`] primitives
//! inside a transaction the *caller* already opened (via
//! [`crate::StateWriter::transaction`]) — this module never opens one itself.
//!
//! # Lifecycle ops (T14-03)
//!
//! `apply_resolve`/`apply_retract` are thin wrappers over a shared private
//! `apply_state_transition` helper: read `(kind, state, entry_version)` once,
//! check `expected_version` then [`entry::MemoryState::check_transition`]
//! (spec 04 §5's kind-specific guard — reused directly rather than calling
//! [`entry::transition_memory_entry`], which has no notion of
//! `entry_version` and would re-read redundantly), then one combined
//! `UPDATE ... SET state=?, entry_version=?, updated_at=?`. Unlike the raw
//! `transition_*` primitives elsewhere in this crate, a legal *self*-transition
//! still bumps `entry_version` and writes an `audit_event` here — consistent
//! with `apply_reinforce`'s precedent (every applied op returns a real
//! `audit_id`), not a semantic claim that something changed.
//!
//! `apply_supersede` is the promotion op: create a **new** entry with
//! `supersedes_id` pointing at an existing one, and transition that existing
//! entry to `superseded` — one transaction, both pre-validated before either
//! write (mirroring `crates/projection/src/switch.rs::commit_switch`'s
//! "check both sides, then mutate" shape). The new entry is created first,
//! the old one retired second (spec 04 §5's own sentence order: "a new fact
//! entry... which transitions to superseded"). The response describes the
//! **new** entry only (mirrors `apply_create`'s return) — the old entry's
//! transition is a verified side effect, not threaded through the return
//! type. Only the new entry's `audit_event` carries the caller's
//! `idempotency_key`; the old entry's transition-audit row does not, so a
//! replay never needs a second, colliding key for the same
//! `(entity_kind, entity_id, entity_version)`.
//!
//! `apply_edit` is the one op allowed to change `text` (`apply_reinforce`
//! structurally cannot). It adds a guard neither `apply_create` nor
//! `apply_reinforce` needed: [`MemoryOpError::EntryTerminal`] rejects editing
//! an entry whose current state is terminal — this task's card is the one
//! that owns "kind/state guards" generally; nothing in spec 08 §3 forces this
//! specific rule, so it is an as-built decision (see spec 08 §3's own note).
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

use rusqlite::types::Type;
use rusqlite::{Error, OptionalExtension, Transaction, params};

use super::audit::{
    Actor, AuditEventRow, NewAuditEvent, find_by_idempotency_key, insert_audit_event,
};
use super::entry::{
    CreateMemoryEntryError, IllegalMemoryTransition, MemoryKind, MemoryState, NewMemoryEntry,
    ScopeKind, create_memory_entry,
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

/// A `resolve` request (spec 04 §5: `task`/`question` only, `active →
/// resolved`).
#[derive(Debug, Clone, Copy)]
pub struct ResolveMemoryOp<'a> {
    pub memory_id: &'a str,
    pub expected_version: i64,
    pub evidence: &'a [EvidenceInput<'a>],
    pub actor: Actor,
    pub idempotency_key: Option<&'a str>,
}

/// A `retract` request (spec 04 §5: `task`/`question`/`fact`-family, `active
/// → retracted`; not legal for `hypothesis` — `retracted` isn't in its state
/// set). Retract ≠ delete: the row survives for audit (spec 08 §3).
#[derive(Debug, Clone, Copy)]
pub struct RetractMemoryOp<'a> {
    pub memory_id: &'a str,
    pub expected_version: i64,
    pub evidence: &'a [EvidenceInput<'a>],
    pub actor: Actor,
    pub idempotency_key: Option<&'a str>,
}

/// A `supersede` request: promote — create a **new** entry (any `kind`, e.g.
/// a confirmed `hypothesis` promoted to a `fact`) with `supersedes_id`
/// pointing at `old_memory_id`, and transition that existing entry to
/// `superseded` (spec 04 §5). `evidence` attaches to the **new** entry only
/// — the justification for the promotion, not the old entry's retirement.
#[derive(Debug, Clone, Copy)]
pub struct SupersedeMemoryOp<'a> {
    pub old_memory_id: &'a str,
    pub old_expected_version: i64,
    pub new_memory_id: &'a str,
    pub new_kind: MemoryKind,
    pub new_text: &'a str,
    pub new_canonical_key: Option<&'a str>,
    pub new_scope_kind: ScopeKind,
    pub new_scope_owner_id: &'a str,
    pub new_confidence: f64,
    pub new_importance: f64,
    pub new_valid_from_tree: Option<&'a str>,
    pub new_last_verified_tree: Option<&'a str>,
    pub evidence: &'a [EvidenceInput<'a>],
    pub actor: Actor,
    pub idempotency_key: Option<&'a str>,
}

/// An `edit` request — the one op allowed to change `text` (spec 08 §3).
/// `actor` distinguishes a user-edit from a router-edit, per the spec's own
/// explicit callout.
#[derive(Debug, Clone, Copy)]
pub struct EditMemoryOp<'a> {
    pub memory_id: &'a str,
    pub expected_version: i64,
    /// `Some` to replace the text; `None` to leave it unchanged (an
    /// importance-only edit).
    pub text: Option<&'a str>,
    pub importance: Option<f64>,
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
    /// `resolve`/`retract`/`supersede`'s kind-specific state-machine guard
    /// (spec 04 §5) forbids the requested transition — wraps
    /// [`entry::MemoryState::check_transition`](super::entry::MemoryState::check_transition)'s
    /// own typed rejection.
    IllegalTransition(IllegalMemoryTransition),
    /// `edit` targets an entry whose current state is terminal (spec 04 §5 /
    /// 08 §6) — an as-built guard this task owns (see the module doc).
    EntryTerminal,
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
            MemoryOpError::IllegalTransition(e) => write!(f, "{e}"),
            MemoryOpError::EntryTerminal => {
                write!(f, "cannot edit a terminal memory entry")
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

/// Read `(kind, state, entry_version)` for `memory_id`, if it exists. Shared
/// by every op below that needs the `expected_version`/kind-guard
/// preconditions — a corrupt stored `kind`/`state` surfaces as a typed
/// [`rusqlite::Error::FromSqlConversionFailure`], mirroring
/// [`super::entry::memory_entry_state`]'s own idiom.
fn read_kind_state_version(
    tx: &Transaction<'_>,
    memory_id: &str,
) -> rusqlite::Result<Option<(MemoryKind, MemoryState, i64)>> {
    tx.query_row(
        "SELECT kind, state, entry_version FROM memory_entry WHERE memory_id = ?1",
        params![memory_id],
        |r| {
            let raw_kind: String = r.get(0)?;
            let raw_state: String = r.get(1)?;
            let kind = MemoryKind::from_db(&raw_kind).ok_or_else(|| {
                Error::FromSqlConversionFailure(
                    0,
                    Type::Text,
                    format!("invalid memory_entry.kind {raw_kind:?}").into(),
                )
            })?;
            let state = MemoryState::from_db(&raw_state).ok_or_else(|| {
                Error::FromSqlConversionFailure(
                    1,
                    Type::Text,
                    format!("invalid memory_entry.state {raw_state:?}").into(),
                )
            })?;
            let version: i64 = r.get(2)?;
            Ok((kind, state, version))
        },
    )
    .optional()
}

/// Shared by `apply_create` and `apply_supersede`'s new-entry half: the typed
/// canonical-key scope-uniqueness pre-check + [`create_memory_entry`] call.
/// `supersedes_id` is `None` for a plain `create`, `Some(old_id)` for a
/// promotion.
#[allow(clippy::too_many_arguments)]
fn create_new_entry(
    tx: &Transaction<'_>,
    memory_id: &str,
    kind: MemoryKind,
    text: &str,
    canonical_key: Option<&str>,
    scope_kind: ScopeKind,
    scope_owner_id: &str,
    confidence: f64,
    importance: f64,
    valid_from_tree: Option<&str>,
    last_verified_tree: Option<&str>,
    supersedes_id: Option<&str>,
    now_ms: i64,
) -> rusqlite::Result<Result<(), MemoryOpError>> {
    if let Some(key) = canonical_key {
        let conflict: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM memory_entry \
                 WHERE scope_kind = ?1 AND scope_owner_id = ?2 AND canonical_key = ?3",
                params![scope_kind.as_str(), scope_owner_id, key],
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
            supersedes_id,
        },
        now_ms,
    )?;
    if let Err(CreateMemoryEntryError::InvalidGlobalScopeOwner) = create_result {
        return Ok(Err(MemoryOpError::InvalidGlobalScopeOwner));
    }
    Ok(Ok(()))
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

    if let Err(e) = create_new_entry(
        tx,
        request.memory_id,
        request.kind,
        request.text,
        request.canonical_key,
        request.scope_kind,
        request.scope_owner_id,
        request.confidence,
        request.importance,
        request.valid_from_tree,
        request.last_verified_tree,
        None,
        now_ms,
    )? {
        return Ok(Err(e));
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

/// Shared by `apply_resolve`/`apply_retract`/`apply_supersede`'s old-entry
/// half: idempotency-key replay, `expected_version`, the kind-specific
/// [`MemoryState::check_transition`] guard, then one combined
/// `UPDATE ... SET state=?, entry_version=?, updated_at=?` — see the module
/// doc for why `transition_memory_entry` isn't reused here.
#[allow(clippy::too_many_arguments)]
fn apply_state_transition(
    tx: &Transaction<'_>,
    memory_id: &str,
    expected_version: i64,
    to: MemoryState,
    op_name: &str,
    actor: Actor,
    idempotency_key: Option<&str>,
    evidence: &[EvidenceInput<'_>],
    now_ms: i64,
) -> rusqlite::Result<Result<MemoryOpOutcome, MemoryOpError>> {
    if let Some(key) = idempotency_key
        && let Some(existing) = find_by_idempotency_key(tx, key)?
    {
        return Ok(Ok(MemoryOpOutcome::Replayed(result_from_row(&existing))));
    }

    let Some((kind, state, current_version)) = read_kind_state_version(tx, memory_id)? else {
        return Ok(Err(MemoryOpError::UnknownMemory));
    };
    if current_version != expected_version {
        return Ok(Err(MemoryOpError::OptimisticConflict {
            expected: expected_version,
            actual: current_version,
        }));
    }
    if let Err(illegal) = state.check_transition(kind, to) {
        return Ok(Err(MemoryOpError::IllegalTransition(illegal)));
    }

    let new_version = current_version + 1;
    tx.execute(
        "UPDATE memory_entry SET state = ?2, entry_version = ?3, updated_at = ?4 \
         WHERE memory_id = ?1",
        params![memory_id, to.as_str(), new_version, now_ms],
    )?;

    insert_evidence_rows(tx, memory_id, evidence)?;

    let audit_id = insert_audit_event(
        tx,
        &NewAuditEvent {
            entity_kind: ENTITY_KIND_MEMORY_ENTRY,
            entity_id: memory_id,
            entity_version: new_version,
            op: op_name,
            actor,
            idempotency_key,
            payload: None,
        },
        now_ms,
    )?;

    Ok(Ok(MemoryOpOutcome::Applied(MemoryOpResult {
        memory_id: memory_id.to_string(),
        entry_version: new_version,
        audit_id,
    })))
}

/// Apply a `resolve` (spec 04 §5): `task`/`question` `active → resolved`.
/// Illegal for any other kind — the same typed
/// [`MemoryOpError::IllegalTransition`] every kind-guard violation surfaces.
pub fn apply_resolve(
    tx: &Transaction<'_>,
    request: &ResolveMemoryOp<'_>,
    now_ms: i64,
) -> rusqlite::Result<Result<MemoryOpOutcome, MemoryOpError>> {
    apply_state_transition(
        tx,
        request.memory_id,
        request.expected_version,
        MemoryState::Resolved,
        "resolve",
        request.actor,
        request.idempotency_key,
        request.evidence,
        now_ms,
    )
}

/// Apply a `retract` (spec 04 §5): `task`/`question`/`fact`-family `active →
/// retracted`. Retract ≠ delete — the row survives with `state='retracted'`
/// for audit (spec 08 §3); only an explicit privacy `purge` (12 §5, a later
/// task) hard-removes.
pub fn apply_retract(
    tx: &Transaction<'_>,
    request: &RetractMemoryOp<'_>,
    now_ms: i64,
) -> rusqlite::Result<Result<MemoryOpOutcome, MemoryOpError>> {
    apply_state_transition(
        tx,
        request.memory_id,
        request.expected_version,
        MemoryState::Retracted,
        "retract",
        request.actor,
        request.idempotency_key,
        request.evidence,
        now_ms,
    )
}

/// Apply a `supersede` (spec 04 §5, promotion): create the **new** entry
/// first, then retire the **old** one — both pre-validated before either
/// write (see the module doc for the ordering rationale and the
/// idempotency-key placement). Returns the **new** entry's result; the old
/// entry's transition to `superseded` is a verified side effect of the same
/// transaction.
pub fn apply_supersede(
    tx: &Transaction<'_>,
    request: &SupersedeMemoryOp<'_>,
    now_ms: i64,
) -> rusqlite::Result<Result<MemoryOpOutcome, MemoryOpError>> {
    if let Some(key) = request.idempotency_key
        && let Some(existing) = find_by_idempotency_key(tx, key)?
    {
        return Ok(Ok(MemoryOpOutcome::Replayed(result_from_row(&existing))));
    }

    let Some((old_kind, old_state, old_current_version)) =
        read_kind_state_version(tx, request.old_memory_id)?
    else {
        return Ok(Err(MemoryOpError::UnknownMemory));
    };
    if old_current_version != request.old_expected_version {
        return Ok(Err(MemoryOpError::OptimisticConflict {
            expected: request.old_expected_version,
            actual: old_current_version,
        }));
    }
    if let Err(illegal) = old_state.check_transition(old_kind, MemoryState::Superseded) {
        return Ok(Err(MemoryOpError::IllegalTransition(illegal)));
    }

    if let Err(e) = create_new_entry(
        tx,
        request.new_memory_id,
        request.new_kind,
        request.new_text,
        request.new_canonical_key,
        request.new_scope_kind,
        request.new_scope_owner_id,
        request.new_confidence,
        request.new_importance,
        request.new_valid_from_tree,
        request.new_last_verified_tree,
        Some(request.old_memory_id),
        now_ms,
    )? {
        return Ok(Err(e));
    }

    insert_evidence_rows(tx, request.new_memory_id, request.evidence)?;

    let old_new_version = old_current_version + 1;
    tx.execute(
        "UPDATE memory_entry SET state = ?2, entry_version = ?3, updated_at = ?4 \
         WHERE memory_id = ?1",
        params![
            request.old_memory_id,
            MemoryState::Superseded.as_str(),
            old_new_version,
            now_ms
        ],
    )?;

    // Only the new entry's row carries the caller's idempotency_key — see the
    // module doc for why the old entry's transition row must not.
    let new_audit_id = insert_audit_event(
        tx,
        &NewAuditEvent {
            entity_kind: ENTITY_KIND_MEMORY_ENTRY,
            entity_id: request.new_memory_id,
            entity_version: 1,
            op: "supersede",
            actor: request.actor,
            idempotency_key: request.idempotency_key,
            payload: None,
        },
        now_ms,
    )?;
    insert_audit_event(
        tx,
        &NewAuditEvent {
            entity_kind: ENTITY_KIND_MEMORY_ENTRY,
            entity_id: request.old_memory_id,
            entity_version: old_new_version,
            op: "supersede",
            actor: request.actor,
            idempotency_key: None,
            payload: None,
        },
        now_ms,
    )?;

    Ok(Ok(MemoryOpOutcome::Applied(MemoryOpResult {
        memory_id: request.new_memory_id.to_string(),
        entry_version: 1,
        audit_id: new_audit_id,
    })))
}

/// Apply an `edit` (spec 08 §3): the one op allowed to change `text`; also
/// accepts an optional `importance` change. Rejects editing a terminal entry
/// ([`MemoryOpError::EntryTerminal`] — an as-built guard, see the module
/// doc), unlike `apply_reinforce`, which this task's card does not ask to
/// guard by state.
pub fn apply_edit(
    tx: &Transaction<'_>,
    request: &EditMemoryOp<'_>,
    now_ms: i64,
) -> rusqlite::Result<Result<MemoryOpOutcome, MemoryOpError>> {
    if let Some(key) = request.idempotency_key
        && let Some(existing) = find_by_idempotency_key(tx, key)?
    {
        return Ok(Ok(MemoryOpOutcome::Replayed(result_from_row(&existing))));
    }

    let Some((_kind, state, current_version)) = read_kind_state_version(tx, request.memory_id)?
    else {
        return Ok(Err(MemoryOpError::UnknownMemory));
    };
    if current_version != request.expected_version {
        return Ok(Err(MemoryOpError::OptimisticConflict {
            expected: request.expected_version,
            actual: current_version,
        }));
    }
    if state.is_terminal() {
        return Ok(Err(MemoryOpError::EntryTerminal));
    }

    let new_version = current_version + 1;
    tx.execute(
        "UPDATE memory_entry \
         SET text = COALESCE(?2, text), importance = COALESCE(?3, importance), \
             entry_version = ?4, updated_at = ?5 \
         WHERE memory_id = ?1",
        params![
            request.memory_id,
            request.text,
            request.importance,
            new_version,
            now_ms
        ],
    )?;

    let audit_id = insert_audit_event(
        tx,
        &NewAuditEvent {
            entity_kind: ENTITY_KIND_MEMORY_ENTRY,
            entity_id: request.memory_id,
            entity_version: new_version,
            op: "edit",
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
