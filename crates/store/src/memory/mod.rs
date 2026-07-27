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
//!   increment to a matching `audit_event`, and composing mutation + evidence +
//!   audit + the `expected_version` optimistic-concurrency precondition into one
//!   contract is T14-02's "transactional memory-op engine", not this task's.
//! - **`memory_evidence`** ([`evidence`]): plain insert/read over an
//!   already-existing `memory_entry` and `observation_envelope` (T13-04) — no
//!   state machine.
//! - **`pending_memory_candidate`**/`candidate_evidence` ([`candidate`]): the
//!   review-candidate machine (spec 04 §6), all three transitions terminal.
//! - **`processing_cursor`**/`consolidation_run` ([`consolidation`]): the
//!   consolidation-run machine (spec 04 §4). T14-01 ships only the pure
//!   transition legality; lease acquisition/renewal timing against a clock is
//!   T14-06's runner.
//! - **`audit_event`** ([`audit`]): plain insert/read; the atomic
//!   evidence+audit+idempotency operation contract is T14-02's.

mod audit;
mod candidate;
mod consolidation;
mod entry;
mod evidence;

pub use audit::{
    Actor, AuditEventRow, NewAuditEvent, insert_audit_event, read_audit_events_for_entity,
};
pub use candidate::{
    CandidateState, CandidateTransitionError, IllegalCandidateTransition, NewCandidate,
    candidate_state, create_candidate, insert_candidate_evidence, transition_candidate,
};
pub use consolidation::{
    IllegalRunTransition, NewConsolidationRun, RunState, RunTransitionError,
    consolidation_run_state, create_consolidation_run, processing_cursor, transition_run,
    upsert_processing_cursor,
};
pub use entry::{
    CreateMemoryEntryError, IllegalMemoryTransition, MemoryKind, MemoryState,
    MemoryTransitionError, NewMemoryEntry, ScopeKind, create_memory_entry, memory_entry_state,
    transition_memory_entry,
};
pub use evidence::{NewMemoryEvidence, insert_memory_evidence, memory_evidence_for};

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
