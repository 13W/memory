//! `memory_entry`: the kind-specific confirmedness machine (spec 03 §2.5, 04 §5).
//!
//! `kind` is the entry's origin and is **immutable** — no function here ever
//! writes it after [`create_memory_entry`]; promotion (e.g. a confirmed
//! hypothesis becoming a fact) happens by creating a *new* row with
//! `supersedes_id` pointing at the old one (spec 04 §5 preamble), a composed
//! operation T14-03 owns. `state` selects among **three** legal transition sets
//! depending on `kind` — unlike every other state machine in this crate
//! (`GenerationState`, `WorktreeState`, `ModelSpaceState`), `memory_entry.state`
//! carries no SQL `CHECK` (the DDL comment says "kind-specific machine, doc 04
//! §5"), so legality is entirely a Rust-side guard and a corrupt/unknown stored
//! value surfaces as [`rusqlite::Error::FromSqlConversionFailure`], mirroring
//! [`registry::GenerationState`](crate::registry::GenerationState)'s idiom.

use rusqlite::types::Type;
use rusqlite::{Connection, Error, OptionalExtension, Transaction, params};

use super::GLOBAL_SCOPE_OWNER_ID;

/// `memory_entry.kind` (spec 03 §2.5 CHECK domain) — origin, immutable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryKind {
    Fact,
    Decision,
    Convention,
    Procedure,
    Task,
    Question,
    Hypothesis,
}

impl MemoryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MemoryKind::Fact => "fact",
            MemoryKind::Decision => "decision",
            MemoryKind::Convention => "convention",
            MemoryKind::Procedure => "procedure",
            MemoryKind::Task => "task",
            MemoryKind::Question => "question",
            MemoryKind::Hypothesis => "hypothesis",
        }
    }

    /// Parse a stored `memory_entry.kind` value; `None` for anything the CHECK
    /// constraint forbids.
    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "fact" => Some(MemoryKind::Fact),
            "decision" => Some(MemoryKind::Decision),
            "convention" => Some(MemoryKind::Convention),
            "procedure" => Some(MemoryKind::Procedure),
            "task" => Some(MemoryKind::Task),
            "question" => Some(MemoryKind::Question),
            "hypothesis" => Some(MemoryKind::Hypothesis),
            _ => None,
        }
    }
}

/// `memory_entry.state` (spec 04 §5) — the union of all kind-specific states.
/// Which transitions out of a given state are legal depends on the entry's
/// [`MemoryKind`] (see [`MemoryState::check_transition`]); not every state is
/// reachable by every kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryState {
    Active,
    Resolved,
    Retracted,
    Confirmed,
    Rejected,
    Superseded,
}

impl MemoryState {
    pub fn as_str(self) -> &'static str {
        match self {
            MemoryState::Active => "active",
            MemoryState::Resolved => "resolved",
            MemoryState::Retracted => "retracted",
            MemoryState::Confirmed => "confirmed",
            MemoryState::Rejected => "rejected",
            MemoryState::Superseded => "superseded",
        }
    }

    /// Parse a stored `memory_entry.state` value; `None` for anything outside
    /// the union domain (there is no SQL `CHECK` to lean on here — see the
    /// module doc).
    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "active" => Some(MemoryState::Active),
            "resolved" => Some(MemoryState::Resolved),
            "retracted" => Some(MemoryState::Retracted),
            "confirmed" => Some(MemoryState::Confirmed),
            "rejected" => Some(MemoryState::Rejected),
            "superseded" => Some(MemoryState::Superseded),
            _ => None,
        }
    }

    /// Terminal states are excluded from recall by default (spec 04 §5, 08 §6),
    /// though still queryable via review tools. `confirmed` is **not**
    /// terminal — a confirmed hypothesis stays recall-eligible as high-trust
    /// until it is superseded by a promoted fact.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            MemoryState::Resolved
                | MemoryState::Retracted
                | MemoryState::Rejected
                | MemoryState::Superseded
        )
    }

    /// Check whether `self → to` is legal for an entry of `kind` (spec 04 §5),
    /// returning a typed [`IllegalMemoryTransition`] otherwise. Pure — no I/O.
    ///
    /// Three disjoint machines, selected by `kind`:
    /// - `task`/`question`: `active → resolved | retracted`
    /// - `hypothesis`: `active → confirmed | rejected | superseded`, and
    ///   `confirmed → superseded` (D-020: promotion to a fact acts on an
    ///   already-confirmed hypothesis — spec 04 §5's own prose: "a confirmed
    ///   hypothesis stays... promotion to fact happens only via explicit
    ///   supersede — a new fact entry... pointing at the hypothesis, which
    ///   transitions to superseded." `confirmed → rejected`/`retracted` have
    ///   no textual basis and are deliberately not added.)
    /// - `fact`/`decision`/`convention`/`procedure`: `active → superseded | retracted`
    ///
    /// A self-transition (`X → X`) is always legal — an idempotent no-op, the
    /// same convention every state machine in this crate honors (spec 04
    /// preamble: "honor the request rather than coerce it"). A state that is
    /// legal for a *different* kind (e.g. `confirmed` for a `fact`) is illegal
    /// here — the machines are disjoint, not a shared superset.
    pub fn check_transition(
        self,
        kind: MemoryKind,
        to: MemoryState,
    ) -> Result<(), IllegalMemoryTransition> {
        use MemoryState::{Active, Confirmed, Rejected, Resolved, Retracted, Superseded};

        if self == to {
            return Ok(());
        }

        let legal = match kind {
            MemoryKind::Task | MemoryKind::Question => {
                matches!((self, to), (Active, Resolved) | (Active, Retracted))
            }
            MemoryKind::Hypothesis => matches!(
                (self, to),
                (Active, Confirmed)
                    | (Active, Rejected)
                    | (Active, Superseded)
                    | (Confirmed, Superseded)
            ),
            MemoryKind::Fact
            | MemoryKind::Decision
            | MemoryKind::Convention
            | MemoryKind::Procedure => {
                matches!((self, to), (Active, Superseded) | (Active, Retracted))
            }
        };

        if legal {
            Ok(())
        } else {
            Err(IllegalMemoryTransition {
                kind,
                from: self,
                to,
            })
        }
    }
}

/// A rejected memory-entry transition (spec 04 §5): the machine for `kind`
/// forbids `from → to`. Carries `kind` because the same `(from, to)` pair can
/// be legal for one kind and illegal for another — the machines are disjoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IllegalMemoryTransition {
    pub kind: MemoryKind,
    pub from: MemoryState,
    pub to: MemoryState,
}

impl std::fmt::Display for IllegalMemoryTransition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "illegal memory transition ({}) {} → {}",
            self.kind.as_str(),
            self.from.as_str(),
            self.to.as_str()
        )
    }
}

impl std::error::Error for IllegalMemoryTransition {}

/// Why a [`transition_memory_entry`] request was rejected at the domain level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryTransitionError {
    /// No `memory_entry` row has this id.
    UnknownMemory,
    /// The kind-specific machine (spec 04 §5) forbids the requested transition.
    Illegal(IllegalMemoryTransition),
}

impl std::fmt::Display for MemoryTransitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MemoryTransitionError::UnknownMemory => write!(f, "unknown memory entry"),
            MemoryTransitionError::Illegal(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for MemoryTransitionError {}

/// `memory_entry.scope_kind` (spec 03 §2.5 CHECK domain).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeKind {
    Global,
    Repository,
    Worktree,
}

impl ScopeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ScopeKind::Global => "global",
            ScopeKind::Repository => "repository",
            ScopeKind::Worktree => "worktree",
        }
    }

    /// Parse a stored `memory_entry.scope_kind` value; `None` for anything the
    /// CHECK constraint forbids.
    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "global" => Some(ScopeKind::Global),
            "repository" => Some(ScopeKind::Repository),
            "worktree" => Some(ScopeKind::Worktree),
            _ => None,
        }
    }
}

/// A new `memory_entry` row (spec 03 §2.5), mirroring the DDL 1:1 apart from
/// `state` — every kind's machine starts at `active` (spec 04 §5), so
/// [`create_memory_entry`] fixes it rather than taking it as a parameter.
/// `memory_id` is caller-minted (UUIDv7); `entry_version` defaults to `1` in
/// the schema.
#[derive(Debug, Clone, Copy)]
pub struct NewMemoryEntry<'a> {
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
    pub supersedes_id: Option<&'a str>,
}

/// Why a [`create_memory_entry`] request was rejected at the domain level (as
/// opposed to an infrastructure/SQLite failure — e.g. a duplicate
/// `canonical_key` in the same scope, which bubbles up as the natural `UNIQUE`
/// constraint error over the partial `memory_canonical` index; no special
/// typed handling, mirroring `create_repository`'s `display_name` precedent).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateMemoryEntryError {
    /// `scope_kind = 'global'` with a `scope_owner_id` other than
    /// [`GLOBAL_SCOPE_OWNER_ID`] (spec 03 §2.5 `[SPEC]`: "global → fixed
    /// singleton UUID" — not a SQL `CHECK`, so this module enforces it).
    InvalidGlobalScopeOwner,
}

impl std::fmt::Display for CreateMemoryEntryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CreateMemoryEntryError::InvalidGlobalScopeOwner => write!(
                f,
                "scope_kind='global' requires scope_owner_id={GLOBAL_SCOPE_OWNER_ID}"
            ),
        }
    }
}

impl std::error::Error for CreateMemoryEntryError {}

/// Insert a `memory_entry` row, born `active` (spec 04 §5: every kind's
/// machine starts there). `updated_at` is seeded to `created_at`.
///
/// A duplicate `canonical_key` within the same scope surfaces as the natural
/// `UNIQUE` constraint error over the partial `memory_canonical` index — no
/// idempotent-converge/dedup-skip semantics here; resolving such a conflict
/// (via `reinforce`/`supersede`) is T14-02's transactional op engine, not this
/// primitive.
pub fn create_memory_entry(
    tx: &Transaction<'_>,
    row: &NewMemoryEntry<'_>,
    now_ms: i64,
) -> rusqlite::Result<Result<(), CreateMemoryEntryError>> {
    if row.scope_kind == ScopeKind::Global && row.scope_owner_id != GLOBAL_SCOPE_OWNER_ID {
        return Ok(Err(CreateMemoryEntryError::InvalidGlobalScopeOwner));
    }
    tx.execute(
        "INSERT INTO memory_entry \
           (memory_id, kind, state, text, canonical_key, scope_kind, scope_owner_id, \
            confidence, importance, valid_from_tree, last_verified_tree, supersedes_id, \
            created_at, updated_at) \
         VALUES (?1, ?2, 'active', ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?12)",
        params![
            row.memory_id,
            row.kind.as_str(),
            row.text,
            row.canonical_key,
            row.scope_kind.as_str(),
            row.scope_owner_id,
            row.confidence,
            row.importance,
            row.valid_from_tree,
            row.last_verified_tree,
            row.supersedes_id,
            now_ms,
        ],
    )?;
    Ok(Ok(()))
}

/// Transition `memory_id` to state `to`, enforcing the kind-specific machine
/// (spec 04 §5). Mirrors
/// [`transition_generation`](crate::registry::transition_generation): the
/// nested result separates infrastructure failure (outer, rolls back) from
/// domain rejection (inner, **no mutation**).
///
/// Deliberately does **not** touch `entry_version`/`updated_at` — spec 04 §5
/// couples every version increment to a matching `audit_event` in the same
/// tx, which is T14-02's transactional op engine to compose, not this
/// primitive's job (see the module doc).
pub fn transition_memory_entry(
    tx: &Transaction<'_>,
    memory_id: &str,
    to: MemoryState,
) -> rusqlite::Result<Result<(), MemoryTransitionError>> {
    let row: Option<(MemoryKind, MemoryState)> = tx
        .query_row(
            "SELECT kind, state FROM memory_entry WHERE memory_id = ?1",
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
                Ok((kind, state))
            },
        )
        .optional()?;

    let Some((kind, from)) = row else {
        return Ok(Err(MemoryTransitionError::UnknownMemory));
    };

    if let Err(illegal) = from.check_transition(kind, to) {
        return Ok(Err(MemoryTransitionError::Illegal(illegal)));
    }

    if from != to {
        tx.execute(
            "UPDATE memory_entry SET state = ?2 WHERE memory_id = ?1",
            params![memory_id, to.as_str()],
        )?;
    }
    Ok(Ok(()))
}

/// The entry's `(kind, state)`, if it exists (spec 03 §2.5).
///
/// A stored value outside either domain (corruption) surfaces as
/// [`rusqlite::Error::FromSqlConversionFailure`], never a silent default.
pub fn memory_entry_state(
    conn: &Connection,
    memory_id: &str,
) -> rusqlite::Result<Option<(MemoryKind, MemoryState)>> {
    conn.query_row(
        "SELECT kind, state FROM memory_entry WHERE memory_id = ?1",
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
            Ok((kind, state))
        },
    )
    .optional()
}

/// One recall-eligible `memory_entry` row (T14-07): just enough for the
/// router's own conflict lookup ([`active_entries_for_scope`]) to decide
/// whether a `reinforce`/`supersede` has a real target, without pulling in
/// T14-08's scored relevance pipeline. Deliberately not `EntryVersion`/
/// `entry_version`-named alone — every field a caller needs to both *pick* a
/// candidate and *cite* it in an op's `expected_version`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryEntrySummary {
    pub memory_id: String,
    pub kind: MemoryKind,
    pub state: MemoryState,
    pub text: String,
    pub scope_kind: ScopeKind,
    pub scope_owner_id: String,
    pub canonical_key: Option<String>,
    pub entry_version: i64,
}

fn read_summary_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryEntrySummary> {
    let raw_kind: String = r.get(1)?;
    let kind = MemoryKind::from_db(&raw_kind).ok_or_else(|| {
        Error::FromSqlConversionFailure(
            1,
            Type::Text,
            format!("invalid memory_entry.kind {raw_kind:?}").into(),
        )
    })?;
    let raw_state: String = r.get(2)?;
    let state = MemoryState::from_db(&raw_state).ok_or_else(|| {
        Error::FromSqlConversionFailure(
            2,
            Type::Text,
            format!("invalid memory_entry.state {raw_state:?}").into(),
        )
    })?;
    let raw_scope_kind: String = r.get(4)?;
    let scope_kind = ScopeKind::from_db(&raw_scope_kind).ok_or_else(|| {
        Error::FromSqlConversionFailure(
            4,
            Type::Text,
            format!("invalid memory_entry.scope_kind {raw_scope_kind:?}").into(),
        )
    })?;
    Ok(MemoryEntrySummary {
        memory_id: r.get(0)?,
        kind,
        state,
        text: r.get(3)?,
        scope_kind,
        scope_owner_id: r.get(5)?,
        canonical_key: r.get(6)?,
        entry_version: r.get(7)?,
    })
}

const SUMMARY_COLUMNS: &str =
    "memory_id, kind, state, text, scope_kind, scope_owner_id, canonical_key, entry_version";

/// Every recall-eligible entry in `(scope_kind, scope_owner_id)`, optionally
/// narrowed to one `canonical_key` — T14-07's own minimal conflict lookup
/// (spec 08 §4 step 3's "candidate conflict set", the part
/// `local_rag_store::memory::runner`'s own doc reserves for T14-08's scored
/// pipeline; this is the plain, unscored version a router can use *before*
/// that pipeline exists). "Recall-eligible" mirrors spec 08 §6's own recall
/// filter: `!state.is_terminal()` — a `resolved`/`retracted`/`rejected`/
/// `superseded` row is never offered as a `reinforce`/`supersede` target.
pub fn active_entries_for_scope(
    conn: &Connection,
    scope_kind: ScopeKind,
    scope_owner_id: &str,
    canonical_key_filter: Option<&str>,
) -> rusqlite::Result<Vec<MemoryEntrySummary>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {SUMMARY_COLUMNS} FROM memory_entry \
         WHERE scope_kind = ?1 AND scope_owner_id = ?2 \
           AND (?3 IS NULL OR canonical_key = ?3) \
           AND state NOT IN ('resolved', 'retracted', 'rejected', 'superseded') \
         ORDER BY memory_id"
    ))?;
    let rows = stmt
        .query_map(
            params![scope_kind.as_str(), scope_owner_id, canonical_key_filter],
            read_summary_row,
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// The recall-eligible entry in `(scope_kind, scope_owner_id)` whose text is
/// **exactly** `text`, if there is one — `D-078`'s deduplication lookup.
///
/// The router creates an entry per window and cannot be relied on to notice
/// that it already made this exact claim: the candidate set it is shown is
/// capped ([`MAX_PROMPT_CANDIDATES`](crate::MAX_PROMPT_CANDIDATES)-equivalent
/// on the caller's side). When this reader was written that cap kept the
/// **oldest** entries, so past it the router was structurally blind to its own
/// recent output — on the owner's store that produced **136** copies of one
/// sentence, over half of the durable memory. D-080 changed which entries
/// survive the cap (related first, then newest); this lookup still stands,
/// because the cap itself did not go away. `canonical_key` cannot catch
/// this: the uniqueness index is partial on non-null keys, and the router
/// leaves the key null.
///
/// Exact text, not similarity, and deliberately so. "Is this the same claim?"
/// asked loosely is a judgement call that belongs to the model and to review;
/// asked as byte equality it is a fact, and a fact is what a mechanical guard
/// can act on without ever silently discarding something a human meant to
/// keep. Near-duplicates stay — they are exactly the case where a wrong answer
/// costs information.
///
/// Recall-eligibility mirrors [`active_entries_for_scope`]: a terminal entry
/// does not block a new one, because an author who retracted a claim and then
/// stated it again means the new one. The tie-break on a store that already
/// has duplicates is the oldest (`memory_id` is UUIDv7), so the answer does
/// not move under a caller.
pub fn active_entry_with_text(
    conn: &Connection,
    scope_kind: ScopeKind,
    scope_owner_id: &str,
    text: &str,
) -> rusqlite::Result<Option<MemoryEntrySummary>> {
    conn.query_row(
        &format!(
            "SELECT {SUMMARY_COLUMNS} FROM memory_entry \
             WHERE scope_kind = ?1 AND scope_owner_id = ?2 AND text = ?3 \
               AND state NOT IN ('resolved', 'retracted', 'rejected', 'superseded') \
             ORDER BY memory_id LIMIT 1"
        ),
        params![scope_kind.as_str(), scope_owner_id, text],
        read_summary_row,
    )
    .optional()
}

/// One entry's summary by id, regardless of state (unlike
/// [`active_entries_for_scope`], which only lists recall-eligible rows) — for
/// re-resolving a specific `memory_id` a generator named directly (T14-07's
/// `local_rag_memory::recall`), where a terminal state is itself meaningful
/// information (e.g. "this was already superseded"), not something to filter
/// out silently.
pub fn memory_entry_summary(
    conn: &Connection,
    memory_id: &str,
) -> rusqlite::Result<Option<MemoryEntrySummary>> {
    conn.query_row(
        &format!("SELECT {SUMMARY_COLUMNS} FROM memory_entry WHERE memory_id = ?1"),
        params![memory_id],
        read_summary_row,
    )
    .optional()
}

/// One recall-eligible `memory_entry` row, scored-pipeline shape (spec 08 §6,
/// T14-08). Unlike [`MemoryEntrySummary`] (T14-07's plain conflict-lookup
/// shape), this carries `confidence`/`created_at`: the formatter needs
/// `confidence` for the `additionalContext` `c=` field (11 §5) and
/// `created_at` for the deterministic tie-break (`score desc, created_at
/// desc, memory_id`) — neither is read by any existing caller, so adding them
/// to `MemoryEntrySummary` would widen every query that type already serves
/// for no benefit to those callers.
#[derive(Debug, Clone, PartialEq)]
pub struct RecallCandidate {
    pub memory_id: String,
    pub kind: MemoryKind,
    pub state: MemoryState,
    /// The entry's canonical text — what recall renders, what the lexical leg
    /// scores, what the dense leg embeds, and what the token budget is spent
    /// on. One text, read by every leg: ADR-0011 made English the canon, so the
    /// second, dense-only text T21-02 introduced no longer exists.
    pub text: String,
    pub confidence: f64,
    pub created_at: i64,
}

fn read_recall_candidate_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<RecallCandidate> {
    let raw_kind: String = r.get(1)?;
    let kind = MemoryKind::from_db(&raw_kind).ok_or_else(|| {
        Error::FromSqlConversionFailure(
            1,
            Type::Text,
            format!("invalid memory_entry.kind {raw_kind:?}").into(),
        )
    })?;
    let raw_state: String = r.get(2)?;
    let state = MemoryState::from_db(&raw_state).ok_or_else(|| {
        Error::FromSqlConversionFailure(
            2,
            Type::Text,
            format!("invalid memory_entry.state {raw_state:?}").into(),
        )
    })?;
    Ok(RecallCandidate {
        memory_id: r.get(0)?,
        kind,
        state,
        text: r.get(3)?,
        confidence: r.get(4)?,
        created_at: r.get(5)?,
    })
}

/// Every recall-eligible entry in `(scope_kind, scope_owner_id)` — spec 08
/// §6's candidate-set step, one scope at a time; a caller unions `Global`,
/// `Repository(repo_id)` and `Worktree(worktree_id)` (T14-08's own scope
/// resolution) the same way [`active_entries_for_scope`] already does for
/// T14-07's narrower conflict lookup. "Recall-eligible" is the identical
/// `!state.is_terminal()` filter — a `resolved`/`retracted`/`rejected`/
/// `superseded` row is excluded here for the same reason it is excluded
/// there (spec 04 §5, 08 §6).
pub fn recall_candidates_for_scope(
    conn: &Connection,
    scope_kind: ScopeKind,
    scope_owner_id: &str,
) -> rusqlite::Result<Vec<RecallCandidate>> {
    let mut stmt = conn.prepare(
        "SELECT e.memory_id, e.kind, e.state, e.text, e.confidence, e.created_at \
         FROM memory_entry e \
         WHERE e.scope_kind = ?1 AND e.scope_owner_id = ?2 \
           AND e.state NOT IN ('resolved', 'retracted', 'rejected', 'superseded') \
         ORDER BY e.memory_id",
    )?;
    let rows = stmt
        .query_map(
            params![scope_kind.as_str(), scope_owner_id],
            read_recall_candidate_row,
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// One entry's [`RecallCandidate`] projection by id, **regardless of
/// state** — unlike [`recall_candidates_for_scope`], which only lists
/// recall-eligible rows. For the recall pipeline's lifecycle re-check
/// (spec 08 §6's second, post-fusion filter box): between the initial
/// candidate-set read and the final budget-fill, a concurrent
/// `retract`/`supersede` is possible (memory recall holds no lock spanning
/// the whole pipeline, unlike code search's `L2.read`), so the pipeline
/// re-fetches each short-listed entry fresh, immediately before formatting,
/// and itself decides whether the *current* state is still eligible — the
/// same "terminal state is itself meaningful information, not something to
/// filter out silently" reasoning [`memory_entry_summary`] already states.
pub fn recall_candidate_by_id(
    conn: &Connection,
    memory_id: &str,
) -> rusqlite::Result<Option<RecallCandidate>> {
    conn.query_row(
        "SELECT e.memory_id, e.kind, e.state, e.text, e.confidence, e.created_at \
         FROM memory_entry e \
         WHERE e.memory_id = ?1",
        params![memory_id],
        read_recall_candidate_row,
    )
    .optional()
}

/// Every `memory_entry` row's `(memory_id, text)`, regardless of scope or
/// state — the raw material [`crate::subjects::memory_entry_subject_keys`]
/// hashes into subject keys and the backfill worker
/// (`local_rag_embed::backfill`) embeds, the same "one crate owns the SQL"
/// discipline every other cross-crate memory reader here follows.
///
/// T21-13 restored the plain form T21-02 had replaced with an effective-text
/// projection: under ADR-0011 an entry has exactly one text, so there is
/// nothing left for two readers to disagree about.
pub fn all_memory_entries_with_text(conn: &Connection) -> rusqlite::Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare("SELECT memory_id, text FROM memory_entry")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    rows.collect()
}

/// Which `memory_id`, if any, already owns `canonical_key` in `(scope_kind,
/// scope_owner_id)` — regardless of `state` (T14-07). Mirrors [`super::op`]'s
/// own private `create_new_entry` pre-check exactly (the `memory_canonical`
/// unique index has no state filter — a `superseded`/`retracted` row still
/// occupies its key), so a router-side caller (`local_rag_memory::guard`) can
/// predict a `create`/`supersede` canonical-key conflict *before* submitting
/// the op, instead of discovering it only when [`super::op::apply_create`]/
/// [`super::op::apply_supersede`] rejects it — which would otherwise roll
/// back the whole consolidation batch (`super::runner`'s own atomicity
/// guarantee) and reproduce the identical rejection on every deterministic
/// retry. [`active_entries_for_scope`] cannot serve this purpose: it
/// deliberately excludes terminal states, exactly the states this check must
/// still see.
pub fn canonical_key_owner(
    conn: &Connection,
    scope_kind: ScopeKind,
    scope_owner_id: &str,
    canonical_key: &str,
) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT memory_id FROM memory_entry \
         WHERE scope_kind = ?1 AND scope_owner_id = ?2 AND canonical_key = ?3",
        params![scope_kind.as_str(), scope_owner_id, canonical_key],
        |r| r.get(0),
    )
    .optional()
}

/// One full `memory_entry` row (spec 03 §2.5) — every column, unlike
/// [`MemoryEntrySummary`]'s narrower conflict-lookup shape. Built for
/// `list_memory` (11 §2, T15-04): a reviewer needs `confidence`/`importance`/
/// timestamps/`supersedes_id`, none of which any existing caller (the
/// router's conflict lookup, the recall pipeline) reads — the same reasoning
/// [`RecallCandidate`] already gives for not reusing `MemoryEntrySummary`
/// either, generalized to the review tool's own full-row need.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryEntryRow {
    pub memory_id: String,
    pub kind: MemoryKind,
    pub state: MemoryState,
    pub text: String,
    pub canonical_key: Option<String>,
    pub scope_kind: ScopeKind,
    pub scope_owner_id: String,
    pub confidence: f64,
    pub importance: f64,
    pub valid_from_tree: Option<String>,
    pub last_verified_tree: Option<String>,
    pub supersedes_id: Option<String>,
    pub entry_version: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

const FULL_ROW_COLUMNS: &str = "memory_id, kind, state, text, canonical_key, scope_kind, \
     scope_owner_id, confidence, importance, valid_from_tree, last_verified_tree, \
     supersedes_id, entry_version, created_at, updated_at";

fn read_full_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryEntryRow> {
    let raw_kind: String = r.get(1)?;
    let kind = MemoryKind::from_db(&raw_kind).ok_or_else(|| {
        Error::FromSqlConversionFailure(
            1,
            Type::Text,
            format!("invalid memory_entry.kind {raw_kind:?}").into(),
        )
    })?;
    let raw_state: String = r.get(2)?;
    let state = MemoryState::from_db(&raw_state).ok_or_else(|| {
        Error::FromSqlConversionFailure(
            2,
            Type::Text,
            format!("invalid memory_entry.state {raw_state:?}").into(),
        )
    })?;
    let raw_scope_kind: String = r.get(5)?;
    let scope_kind = ScopeKind::from_db(&raw_scope_kind).ok_or_else(|| {
        Error::FromSqlConversionFailure(
            5,
            Type::Text,
            format!("invalid memory_entry.scope_kind {raw_scope_kind:?}").into(),
        )
    })?;
    Ok(MemoryEntryRow {
        memory_id: r.get(0)?,
        kind,
        state,
        text: r.get(3)?,
        canonical_key: r.get(4)?,
        scope_kind,
        scope_owner_id: r.get(6)?,
        confidence: r.get(7)?,
        importance: r.get(8)?,
        valid_from_tree: r.get(9)?,
        last_verified_tree: r.get(10)?,
        supersedes_id: r.get(11)?,
        entry_version: r.get(12)?,
        created_at: r.get(13)?,
        updated_at: r.get(14)?,
    })
}

/// Every `memory_entry` row in `(scope_kind, scope_owner_id)`, **including
/// terminal states** — unlike [`active_entries_for_scope`]/
/// [`recall_candidates_for_scope`], which both exclude them for recall
/// purposes. Spec 04 §5: terminal states "remain queryable via review
/// tools," and `list_memory` (spec 11 §2, T15-04) is exactly that review
/// tool. `kind_filter`/`state_filter` are both optional narrowing, the same
/// `?N IS NULL OR ...` idiom [`active_entries_for_scope`] already uses for
/// `canonical_key_filter`. No `LIMIT`/`OFFSET` here: a caller unions
/// multiple scopes (global ∪ repository ∪ worktree) in Rust first — the same
/// per-scope-then-union pattern `local_rag_memory::recall::pipeline` already
/// follows — so pagination has to slice the union, not any one scope's rows.
pub fn list_memory_entries_for_scope(
    conn: &Connection,
    scope_kind: ScopeKind,
    scope_owner_id: &str,
    kind_filter: Option<MemoryKind>,
    state_filter: Option<MemoryState>,
) -> rusqlite::Result<Vec<MemoryEntryRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {FULL_ROW_COLUMNS} FROM memory_entry \
         WHERE scope_kind = ?1 AND scope_owner_id = ?2 \
           AND (?3 IS NULL OR kind = ?3) \
           AND (?4 IS NULL OR state = ?4) \
         ORDER BY created_at, memory_id"
    ))?;
    let rows = stmt
        .query_map(
            params![
                scope_kind.as_str(),
                scope_owner_id,
                kind_filter.map(MemoryKind::as_str),
                state_filter.map(MemoryState::as_str),
            ],
            read_full_row,
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// One full `memory_entry` row by id, regardless of state — `inspect memory
/// <id>`/`export`/`purge memory <id>`'s own read (11 §6, 12 §3, T16-02), the
/// full-row sibling of [`memory_entry_summary`]'s narrower shape.
pub fn memory_entry_by_id(
    conn: &Connection,
    memory_id: &str,
) -> rusqlite::Result<Option<MemoryEntryRow>> {
    conn.query_row(
        &format!("SELECT {FULL_ROW_COLUMNS} FROM memory_entry WHERE memory_id = ?1"),
        params![memory_id],
        read_full_row,
    )
    .optional()
}

/// Every `memory_id` in the store, regardless of scope or state, ascending —
/// `purge --all`'s own enumeration seam (T16-02), the memory-side analog of
/// `crate::observation::all_session_ids`.
pub(crate) fn all_memory_entry_ids(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT memory_id FROM memory_entry ORDER BY memory_id")?;
    let ids = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_kind_round_trips() {
        for kind in [
            MemoryKind::Fact,
            MemoryKind::Decision,
            MemoryKind::Convention,
            MemoryKind::Procedure,
            MemoryKind::Task,
            MemoryKind::Question,
            MemoryKind::Hypothesis,
        ] {
            assert_eq!(MemoryKind::from_db(kind.as_str()), Some(kind));
        }
        assert_eq!(MemoryKind::from_db("bogus"), None);
    }

    #[test]
    fn memory_state_round_trips() {
        for state in [
            MemoryState::Active,
            MemoryState::Resolved,
            MemoryState::Retracted,
            MemoryState::Confirmed,
            MemoryState::Rejected,
            MemoryState::Superseded,
        ] {
            assert_eq!(MemoryState::from_db(state.as_str()), Some(state));
        }
        assert_eq!(MemoryState::from_db("bogus"), None);
    }

    #[test]
    fn terminal_states_match_spec_04_5_and_08_6() {
        // Terminal: resolved, retracted, rejected, superseded. NOT terminal:
        // active, confirmed (a confirmed hypothesis stays recall-eligible).
        let terminal = [
            MemoryState::Resolved,
            MemoryState::Retracted,
            MemoryState::Rejected,
            MemoryState::Superseded,
        ];
        let not_terminal = [MemoryState::Active, MemoryState::Confirmed];
        for s in terminal {
            assert!(s.is_terminal(), "{s:?} must be terminal");
        }
        for s in not_terminal {
            assert!(!s.is_terminal(), "{s:?} must NOT be terminal");
        }
    }

    /// Exhaustive over every `(kind, from, to)` triple: exactly the spec 04 §5
    /// rows are legal, every self-transition is an idempotent legal no-op, and
    /// everything else — including a state legal only for a *different* kind —
    /// is illegal.
    #[test]
    fn check_transition_covers_the_whole_matrix() {
        use MemoryState::{Active, Confirmed, Rejected, Resolved, Retracted, Superseded};

        let all_kinds = [
            MemoryKind::Fact,
            MemoryKind::Decision,
            MemoryKind::Convention,
            MemoryKind::Procedure,
            MemoryKind::Task,
            MemoryKind::Question,
            MemoryKind::Hypothesis,
        ];
        let all_states = [Active, Resolved, Retracted, Confirmed, Rejected, Superseded];

        let legal_for = |kind: MemoryKind| -> Vec<(MemoryState, MemoryState)> {
            match kind {
                MemoryKind::Task | MemoryKind::Question => {
                    vec![(Active, Resolved), (Active, Retracted)]
                }
                MemoryKind::Hypothesis => vec![
                    (Active, Confirmed),
                    (Active, Rejected),
                    (Active, Superseded),
                    (Confirmed, Superseded), // D-020: promotion acts on a confirmed hypothesis
                ],
                MemoryKind::Fact
                | MemoryKind::Decision
                | MemoryKind::Convention
                | MemoryKind::Procedure => {
                    vec![(Active, Superseded), (Active, Retracted)]
                }
            }
        };

        for kind in all_kinds {
            let legal = legal_for(kind);

            for (from, to) in legal.iter().copied() {
                assert_eq!(
                    from.check_transition(kind, to),
                    Ok(()),
                    "({kind:?}) {from:?} → {to:?} legal"
                );
            }

            for s in all_states {
                assert_eq!(
                    s.check_transition(kind, s),
                    Ok(()),
                    "({kind:?}) {s:?} → {s:?} idempotent"
                );
            }

            for from in all_states {
                for to in all_states {
                    if from == to || legal.contains(&(from, to)) {
                        continue;
                    }
                    assert_eq!(
                        from.check_transition(kind, to),
                        Err(IllegalMemoryTransition { kind, from, to }),
                        "({kind:?}) {from:?} → {to:?} illegal"
                    );
                }
            }
        }
    }

    /// A store whose `memory_entry.kind`/`.state` somehow holds a value outside
    /// their domains (corruption) must surface a typed conversion error, not a
    /// silent default. A minimal constraint-free table injects the bad value.
    #[test]
    fn memory_entry_state_rejects_corrupt_enum() {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(
            "CREATE TABLE memory_entry (memory_id TEXT, kind TEXT, state TEXT);\n\
             INSERT INTO memory_entry VALUES ('m-bad-kind', 'zombie', 'active');\n\
             INSERT INTO memory_entry VALUES ('m-bad-state', 'fact', 'zombie');",
        )
        .expect("seed corrupt rows");

        let bad_kind = memory_entry_state(&conn, "m-bad-kind");
        assert!(
            matches!(
                bad_kind,
                Err(Error::FromSqlConversionFailure(0, Type::Text, _))
            ),
            "corrupt kind → typed conversion failure, got {bad_kind:?}",
        );

        let bad_state = memory_entry_state(&conn, "m-bad-state");
        assert!(
            matches!(
                bad_state,
                Err(Error::FromSqlConversionFailure(1, Type::Text, _))
            ),
            "corrupt state → typed conversion failure, got {bad_state:?}",
        );

        assert_eq!(
            memory_entry_state(&conn, "missing").expect("read"),
            None,
            "an absent id is a clean None"
        );
    }

    #[test]
    fn scope_kind_round_trips() {
        for kind in [
            ScopeKind::Global,
            ScopeKind::Repository,
            ScopeKind::Worktree,
        ] {
            assert_eq!(ScopeKind::from_db(kind.as_str()), Some(kind));
        }
        assert_eq!(ScopeKind::from_db("bogus"), None);
    }
}
