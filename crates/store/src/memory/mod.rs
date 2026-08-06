//! Durable memory tables in `state.sqlite` (spec 03 §2.5 "Memory side" — the
//! seven tables T13-04 explicitly left for this task; see
//! [`crate::observation`]'s module doc for the scope boundary against the
//! observation-ledger subset it ships).
//!
//! This module owns the ninth numbered migration ([`SCHEMA_V9`]): `memory_entry`,
//! `memory_evidence`, `pending_memory_candidate`, `candidate_evidence`,
//! `processing_cursor`, `consolidation_run`, and `audit_event`. It ships exactly
//! what T05-01 shipped for the generation lifecycle — DDL plus a pure,
//! kind/state-aware `check_transition` and a guarded `transition_*`
//! read-then-write primitive per machine — and nothing beyond that:
//!
//! - **`memory_entry`** ([`entry`]): a *kind-specific* state machine (spec 04
//!   §5) — `kind` (origin, immutable) selects which of three legal transition
//!   sets `state` may move through. [`entry::MemoryState::check_transition`]
//!   takes both `kind` and `to`, unlike the single-machine `check_transition`s
//!   elsewhere in this crate (`GenerationState`, `WorktreeState`,
//!   `ModelSpaceState`), because `memory_entry.state` carries no SQL `CHECK` —
//!   the kind-conditional legality lives entirely in Rust (unlike
//!   `pending_memory_candidate.review_state`/`consolidation_run.state`, which
//!   do have one). [`entry::transition_memory_entry`] deliberately does **not**
//!   touch `entry_version`/`updated_at`: spec 04 §5 couples every version
//!   increment to a matching `audit_event`, and [`op`] composes that.
//! - **`memory_evidence`** ([`evidence`]): plain insert/read over an
//!   already-existing `memory_entry` and `observation_envelope` (T13-04) — no
//!   state machine.
//! - **`pending_memory_candidate`**/`candidate_evidence` ([`candidate`]): the
//!   review-candidate machine (spec 04 §6), all three transitions terminal.
//! - **`processing_cursor`**/`consolidation_run` ([`consolidation`]): the
//!   consolidation-run machine (spec 04 §4). T14-01 ships only the pure
//!   transition legality; lease acquisition/renewal timing against a clock is
//!   T14-06's runner.
//! - **`audit_event`** ([`audit`]): plain insert/read, plus
//!   [`audit::find_by_idempotency_key`] — the read [`op`] checks first on
//!   every operation to recognize an already-applied retry.
//!
//! T14-02 adds **[`op`]**, the shared transactional memory-op engine (spec 08
//! §3): [`op::apply_create`]/[`op::apply_reinforce`] compose the primitives
//! above into the atomic mutation+evidence+audit+idempotency contract every
//! memory operation follows, and [`op::apply_noop`] is the router's
//! zero-write "considered, no action" acknowledgment (see [`op`]'s own doc for
//! why `noop` writes nothing).
//!
//! T14-03 adds the lifecycle/edit ops in the same module:
//! [`op::apply_resolve`]/[`op::apply_retract`] (kind-specific state
//! transitions, spec 04 §5), [`op::apply_supersede`] (the promotion op —
//! create a new entry, retire the old one, one transaction; see D-020 below),
//! and [`op::apply_edit`] (the one op allowed to change `text`).
//!
//! T14-04 adds [`op::apply_merge`]: a survivor absorbs evidence from N ≥ 1
//! losers, each of which transitions to `superseded` with `supersedes_id`
//! pointing at the survivor — the first op setting that column on an
//! already-existing row rather than at `INSERT` time. See [`op`]'s own
//! module doc for the duplicate-evidence and scope-compatibility decisions.
//!
//! **D-020** (found while planning T14-03, `[SPEC]`): spec 04 §5's own prose
//! narrates promotion acting on an *already-confirmed* hypothesis, but
//! T14-01's shipped [`entry::MemoryState::check_transition`] only allowed
//! `active → superseded` for `Hypothesis`, leaving `confirmed` a dead end.
//! Fixed by adding exactly `confirmed → superseded` (see `check_transition`'s
//! own doc for the full rationale and what was deliberately *not* added).
//!
//! T14-05 adds **[`review`]**, the candidate review operations (spec 04 §6, 08
//! §3/§5/§8): `propose`/`edit`/`reject` are thin wrappers over [`candidate`]'s
//! machine, and `approve` deserializes the candidate's `proposed_operation`
//! JSON and dispatches to the matching [`op::apply_create`]/
//! [`op::apply_reinforce`]/[`op::apply_resolve`]/[`op::apply_retract`]/
//! [`op::apply_supersede`] with `actor=`[`audit::Actor::User`] — "the same
//! transactional memory-op path as the router" the spec asks for — deriving
//! evidence from `candidate_evidence`'s FK chain rather than storing it twice.
//! See [`review`]'s own module doc for the JSON schema, the double-approval
//! idempotence design, and why the "conflicting edit" check is state-based.
//!
//! T14-06 adds **[`runner`]**, the consolidation lease/cursor runner (spec 08
//! §4): [`consolidation::open_next_run`]/[`consolidation::retry_run`] (step
//! 1's bounded snapshot + lease acquisition, extended in [`consolidation`]
//! itself since T14-01 already owns the pure `RunState` machine) and
//! [`runner::run_once`] (steps 2–4: load the window, call a caller-supplied
//! generator *outside* any transaction while renewing the lease every 30s,
//! then apply the ordered op list in one short transaction via
//! [`runner::commit_apply_run`]). **T14-06 does not implement the router
//! itself** — [`runner::run_once`] is generic over any generator matching
//! its signature; T14-07 supplies the real local-generator implementation
//! against this same contract. See [`runner`]'s own module doc for the
//! atomicity fix this task's design centers on (a mid-batch op rejection
//! must roll back the *whole* apply, not just stop short) and the
//! lease-fencing/failed-routing as-built decisions.

mod audit;
mod candidate;
mod consolidation;
mod entry;
mod evidence;
mod op;
mod review;
mod runner;
mod stats;

pub use audit::{
    Actor, AuditEventRow, NewAuditEvent, find_by_idempotency_key, insert_audit_event,
    read_audit_events_for_entity,
};
pub use candidate::{
    CandidateState, CandidateTransitionError, IllegalCandidateTransition, NewCandidate,
    candidate_evidence_for, candidate_state, create_candidate, insert_candidate_evidence,
    pending_candidate_ages, transition_candidate,
};
pub use consolidation::{
    IllegalRunTransition, LEASE_DURATION_MS, LEASE_RENEW_INTERVAL_MS, NewConsolidationRun,
    RenewError, RunState, RunTransitionError, RunWindow, SnapshotOutcome, StaleRun, acquire_lease,
    consolidation_run_state, create_consolidation_run, lease_expired, open_next_run,
    pending_backlog, processing_cursor, renew_lease, retry_run, sessions_with_pending_backlog,
    stale_runs, transition_run, upsert_processing_cursor,
};
pub(crate) use entry::all_memory_entry_ids;
pub use entry::{
    CreateMemoryEntryError, IllegalMemoryTransition, MemoryEntryRow, MemoryEntrySummary,
    MemoryKind, MemoryState, MemoryTransitionError, NewMemoryEntry, RecallCandidate, ScopeKind,
    active_entries_for_scope, all_memory_entries_with_text, canonical_key_owner,
    create_memory_entry, list_memory_entries_for_scope, memory_entry_by_id, memory_entry_state,
    memory_entry_summary, recall_candidate_by_id, recall_candidates_for_scope,
    transition_memory_entry,
};
pub use evidence::{NewMemoryEvidence, insert_memory_evidence, memory_evidence_for};
pub use op::{
    CreateMemoryOp, EditMemoryOp, EvidenceInput, MemoryOpError, MemoryOpOutcome, MemoryOpResult,
    MergeLoser, MergeMemoryOp, ReinforceMemoryOp, ResolveMemoryOp, RetractMemoryOp,
    SupersedeMemoryOp, apply_create, apply_edit, apply_merge, apply_noop, apply_reinforce,
    apply_resolve, apply_retract, apply_supersede,
};
pub use review::{
    ApproveCandidateOutcome, CandidateRow, ProposedOperation, ReviewError, approve_candidate,
    edit_candidate, list_candidates, observation_evidence_source, propose_candidate,
    reject_candidate,
};
pub use runner::{
    ApplyReport, ConsolidationWindow, GeneratedOp, RunOutcome, RunOutcomeError, RunnerApplyError,
    RunnerError, WindowObservation, commit_apply_run, run_once,
};
pub use stats::{CandidateCountRow, MemoryCountRow, memory_entry_counts, pending_candidate_counts};

/// Version-9 migration DDL: the durable-memory tables (spec 03 §2.5, the
/// `memory_entry`/`memory_evidence`/`pending_memory_candidate`/
/// `candidate_evidence`/`processing_cursor`/`consolidation_run`/`audit_event`
/// subset T13-04 left for this task). Referenced by [`crate::migrate::ALL`] as
/// migration version 9.
///
/// **Frozen once shipped.** Like the earlier `SCHEMA_V*` constants, the
/// checksum is the SHA-256 of this text (see
/// [`crate::migrate::Migration::checksum`]); any edit trips
/// [`ChecksumDrift`](crate::migrate::MigrationError::ChecksumDrift) on an
/// existing store. Future schema changes are new numbered migrations.
pub(crate) const SCHEMA_V9: &str = "\
CREATE TABLE memory_entry (
  memory_id        TEXT PRIMARY KEY,
  kind             TEXT NOT NULL CHECK
    (kind IN ('fact','decision','convention','procedure','task','question','hypothesis')),
  state            TEXT NOT NULL,
  text             TEXT NOT NULL,
  canonical_key    TEXT,
  scope_kind       TEXT NOT NULL CHECK (scope_kind IN ('global','repository','worktree')),
  scope_owner_id   TEXT NOT NULL,
  confidence       REAL NOT NULL CHECK (confidence BETWEEN 0.0 AND 1.0),
  importance       REAL NOT NULL CHECK (importance BETWEEN 0.0 AND 1.0),
  valid_from_tree  TEXT,
  last_verified_tree TEXT,
  supersedes_id    TEXT REFERENCES memory_entry(memory_id),
  entry_version    INTEGER NOT NULL DEFAULT 1,
  created_at       INTEGER NOT NULL,
  updated_at       INTEGER NOT NULL
);
CREATE UNIQUE INDEX memory_canonical
  ON memory_entry(scope_kind, scope_owner_id, canonical_key)
  WHERE canonical_key IS NOT NULL;

CREATE TABLE memory_evidence (
  memory_id       TEXT NOT NULL REFERENCES memory_entry(memory_id),
  observation_id  TEXT NOT NULL REFERENCES observation_envelope(observation_id),
  evidence_kind   TEXT NOT NULL CHECK
    (evidence_kind IN ('user_statement','tool_result','test_result','code_state','model_claim')),
  session_id      TEXT NOT NULL,
  agent_id        TEXT,
  commit_hash     TEXT,
  PRIMARY KEY (memory_id, observation_id)
);

CREATE TABLE pending_memory_candidate (
  candidate_id        TEXT PRIMARY KEY,
  proposed_operation  TEXT NOT NULL,
  conflicts           TEXT,
  review_state        TEXT NOT NULL CHECK
    (review_state IN ('pending','approved','rejected','expired')),
  created_at          INTEGER NOT NULL
);

CREATE TABLE candidate_evidence (
  candidate_id    TEXT NOT NULL REFERENCES pending_memory_candidate(candidate_id) ON DELETE CASCADE,
  observation_id  TEXT NOT NULL REFERENCES observation_envelope(observation_id),
  PRIMARY KEY (candidate_id, observation_id)
);

CREATE TABLE processing_cursor (
  session_id                       TEXT PRIMARY KEY,
  last_consolidated_received_seq   INTEGER NOT NULL
);

CREATE TABLE consolidation_run (
  run_id             TEXT PRIMARY KEY,
  session_id         TEXT NOT NULL,
  from_received_seq  INTEGER NOT NULL,
  to_received_seq    INTEGER NOT NULL,
  router_version     TEXT NOT NULL,
  state              TEXT NOT NULL CHECK (state IN ('pending','running','applied','failed')),
  lease_until        INTEGER,
  created_at         INTEGER NOT NULL,
  updated_at         INTEGER NOT NULL
);
CREATE INDEX consolidation_by_session ON consolidation_run(session_id, state);

CREATE TABLE audit_event (
  audit_id         INTEGER PRIMARY KEY AUTOINCREMENT,
  entity_kind      TEXT NOT NULL,
  entity_id        TEXT NOT NULL,
  entity_version   INTEGER NOT NULL,
  op               TEXT NOT NULL,
  actor            TEXT NOT NULL CHECK (actor IN ('user','router','system')),
  idempotency_key  TEXT UNIQUE,
  payload          TEXT,
  created_at       INTEGER NOT NULL,
  UNIQUE (entity_kind, entity_id, entity_version)
);
";

/// The global singleton `memory_entry.scope_owner_id` for `scope_kind='global'`
/// (spec 03 §2.5 `[SPEC]`). Distinct from
/// [`registry::DEFAULT_MODEL_SPACE_ID`](crate::registry::DEFAULT_MODEL_SPACE_ID) —
/// the spec happens to assign both the same literal UUID, but they identify
/// unrelated rows in unrelated tables; conflating the two symbols would
/// misattribute the coincidence.
pub const GLOBAL_SCOPE_OWNER_ID: &str = "00000000-0000-7000-8000-000000000001";
