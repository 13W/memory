//! The generation lifecycle over the `generation` table (spec 03 §2.1, machine in
//! spec 04 §1) — group 05.
//!
//! The `generation` table itself is created by [`SCHEMA_V2`](super::SCHEMA_V2)
//! (T02-03), which shipped only the worktree composite-FK seam
//! ([`set_current_generation`](super::set_current_generation)). This module adds
//! the group-05 lifecycle:
//!
//! - **Allocation** ([`allocate_generation`]): a new generation for a worktree is
//!   born in state `building` with a per-worktree monotone `generation_number`
//!   (`MAX(number) + 1`). `UNIQUE (worktree_id, generation_number)` (spec 03 §2.1)
//!   is the structural guard against two generations sharing a number; it holds
//!   even under concurrency because the single global writer (spec 03 §3)
//!   serializes the read-compute-write. `generation_id`/`now_ms` are caller-minted
//!   (never a clock or entropy source in the write path, as the registry
//!   primitives do).
//! - **The state machine** ([`GenerationState`] + [`check_transition`] +
//!   [`transition_generation`]): `building → projection_ready → active → retiring`
//!   plus `building|projection_ready → failed`, mirroring the
//!   [`WorktreeState`](super::WorktreeState) idiom (pure check, guarded
//!   read-then-write in one transaction, no mutation on rejection, corrupt stored
//!   enum → a typed [`rusqlite::Error::FromSqlConversionFailure`]).
//! - **Routable readers** ([`active_generations`], [`generation_state`]):
//!   `active_generations` returns only `state = 'active'` rows, so `retiring` and
//!   `failed` are **never** consulted for routing (spec 04 §1 `[FIXED]`).
//!
//! Out of scope here (later group-05 / group-07 tasks): the generation *builder*
//! and structural sharing (T05-03), the reconcile scheduler/triggers (T05-04),
//! and the projection switch that atomically retires N and activates N+1 in the
//! same transaction as `worktree_projection_state` (05 §5). The "exactly one
//! `active` per worktree" app invariant (spec 04 §1) is therefore upheld
//! *procedurally* by that future switch's ordering — this module exposes the
//! primitives and the [`active_generations`] reader that make the invariant
//! testable; it does not add a structural constraint (the schema is frozen).

use rusqlite::types::Type;
use rusqlite::{Connection, Error, OptionalExtension, Transaction, params};

/// The lifecycle state of a generation (spec 03 §2.1 `generation.state`, machine
/// in spec 04 §1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenerationState {
    /// Membership, occurrences, and FTS inputs are being assembled.
    Building,
    /// The build is complete and structurally valid; ready to become the
    /// projected generation (the projection switch is a later task).
    ProjectionReady,
    /// The one generation search routes to for this worktree.
    Active,
    /// Superseded by a newer active generation; retained for GC/audit only and
    /// **never** consulted for routing (spec 04 §1 `[FIXED]`).
    Retiring,
    /// A build/switch error terminated this generation; retained until GC like
    /// `retiring`, never routed.
    Failed,
}

impl GenerationState {
    /// The stored `generation.state` value.
    pub fn as_str(self) -> &'static str {
        match self {
            GenerationState::Building => "building",
            GenerationState::ProjectionReady => "projection_ready",
            GenerationState::Active => "active",
            GenerationState::Retiring => "retiring",
            GenerationState::Failed => "failed",
        }
    }

    /// Parse a stored `generation.state` value; `None` for anything the CHECK
    /// constraint forbids.
    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "building" => Some(GenerationState::Building),
            "projection_ready" => Some(GenerationState::ProjectionReady),
            "active" => Some(GenerationState::Active),
            "retiring" => Some(GenerationState::Retiring),
            "failed" => Some(GenerationState::Failed),
            _ => None,
        }
    }

    /// Check whether `self → to` is a legal transition (spec 04 §1), returning a
    /// typed [`IllegalGenerationTransition`] otherwise. Pure — no I/O.
    ///
    /// The machine is
    /// `building → projection_ready → active → retiring`, plus
    /// `building → failed` and `projection_ready → failed` (the "error in
    /// reconcile/switch" edge — a *build* fails from `building`, a *switch* fails
    /// from `projection_ready`; the already-serving `active` generation is never
    /// routed to `failed`, T05-05 keeps it live). `retiring` and `failed` are
    /// terminal — they leave only by GC row-deletion (group 06), which is not a
    /// state transition. A self-transition (`X → X`) is an idempotent no-op and is
    /// legal: honoring the request rather than coercing it (spec 04 preamble)
    /// keeps a crash/retry that re-requests the current state safe.
    pub fn check_transition(self, to: GenerationState) -> Result<(), IllegalGenerationTransition> {
        use GenerationState::{Active, Building, Failed, ProjectionReady, Retiring};
        let legal = match (self, to) {
            (a, b) if a == b => true,
            (Building, ProjectionReady) => true,
            (Building, Failed) => true,
            (ProjectionReady, Active) => true,
            (ProjectionReady, Failed) => true,
            (Active, Retiring) => true,
            _ => false,
        };
        if legal {
            Ok(())
        } else {
            Err(IllegalGenerationTransition { from: self, to })
        }
    }
}

/// A rejected generation state transition (spec 04 §1): the machine forbids
/// `from → to`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IllegalGenerationTransition {
    /// The generation's current state.
    pub from: GenerationState,
    /// The requested (illegal) target state.
    pub to: GenerationState,
}

impl std::fmt::Display for IllegalGenerationTransition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "illegal generation transition {} → {}",
            self.from.as_str(),
            self.to.as_str()
        )
    }
}

impl std::error::Error for IllegalGenerationTransition {}

/// Why a [`transition_generation`] request was rejected at the domain level (as
/// opposed to an infrastructure/SQLite failure, which surfaces as the outer
/// [`rusqlite::Error`] and rolls the transaction back).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenerationTransitionError {
    /// No `generation` row has this id.
    UnknownGeneration,
    /// The state machine (spec 04 §1) forbids the requested transition.
    Illegal(IllegalGenerationTransition),
}

impl std::fmt::Display for GenerationTransitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GenerationTransitionError::UnknownGeneration => write!(f, "unknown generation"),
            GenerationTransitionError::Illegal(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for GenerationTransitionError {}

/// Allocate a new generation for `worktree_id`, born in state `building` (spec 03
/// §2.1, 04 §1: `∅ → building`).
///
/// The `generation_number` is the per-worktree monotone `MAX(number) + 1` (the
/// first generation is `1`), computed over **all** of the worktree's generations —
/// `retiring`/`failed` rows keep their numbers until GC, so numbers are never
/// reused. `UNIQUE (worktree_id, generation_number)` is the structural backstop:
/// even though the single global writer (spec 03 §3) serializes the
/// read-compute-write so no two allocations ever truly race, the constraint would
/// catch any future implementation that computed the number outside the
/// transaction. Returns the allocated number.
///
/// `generation_id` (a caller-minted UUIDv7, never path-derived, spec 01 §5) and
/// `now_ms` are supplied by the caller, keeping the clock and entropy out of the
/// write path (as [`create_worktree`](super::create_worktree) does). An unknown
/// `worktree_id` is rejected by the `generation.worktree_id` foreign key (the
/// transaction rolls back); a duplicate `generation_id` by the primary key.
pub fn allocate_generation(
    tx: &Transaction<'_>,
    worktree_id: &str,
    generation_id: &str,
    now_ms: i64,
) -> rusqlite::Result<i64> {
    let number: i64 = tx.query_row(
        "SELECT COALESCE(MAX(generation_number), 0) + 1 FROM generation WHERE worktree_id = ?1",
        params![worktree_id],
        |r| r.get(0),
    )?;
    tx.execute(
        "INSERT INTO generation \
           (generation_id, worktree_id, generation_number, state, created_at) \
         VALUES (?1, ?2, ?3, 'building', ?4)",
        params![generation_id, worktree_id, number, now_ms],
    )?;
    Ok(number)
}

/// Transition `generation_id` to state `to`, enforcing the state machine
/// (spec 04 §1). Mirrors
/// [`transition_worktree_state`](super::transition_worktree_state).
///
/// The nested result separates infrastructure failure from domain rejection:
///
/// - the outer [`rusqlite::Result`] is `Err` only on a SQLite failure (the
///   transaction rolls back so the caller can retry);
/// - the inner [`Result`] is the domain outcome:
///   - `Err(UnknownGeneration)` when no row has this id → **no mutation**;
///   - `Err(Illegal(..))` for a forbidden transition → **no mutation** (the
///     illegality is detected before any write, so the enclosing transaction
///     commits a no-op);
///   - `Ok(())` for a legal transition to a different state → the row's `state`
///     is updated;
///   - `Ok(())` for a legal self-transition (`X → X`) → a no-op success.
///
/// A stored `state` outside the CHECK domain (corruption) surfaces as the outer
/// [`rusqlite::Error::FromSqlConversionFailure`], never a silent default (the same
/// idiom the worktree machine uses).
pub fn transition_generation(
    tx: &Transaction<'_>,
    generation_id: &str,
    to: GenerationState,
) -> rusqlite::Result<Result<(), GenerationTransitionError>> {
    let from: Option<GenerationState> = tx
        .query_row(
            "SELECT state FROM generation WHERE generation_id = ?1",
            params![generation_id],
            |r| {
                let raw: String = r.get(0)?;
                GenerationState::from_db(&raw).ok_or_else(|| {
                    Error::FromSqlConversionFailure(
                        0,
                        Type::Text,
                        format!("invalid generation.state {raw:?}").into(),
                    )
                })
            },
        )
        .optional()?;

    let Some(from) = from else {
        return Ok(Err(GenerationTransitionError::UnknownGeneration));
    };

    if let Err(illegal) = from.check_transition(to) {
        return Ok(Err(GenerationTransitionError::Illegal(illegal)));
    }

    if from != to {
        tx.execute(
            "UPDATE generation SET state = ?2 WHERE generation_id = ?1",
            params![generation_id, to.as_str()],
        )?;
    }
    Ok(Ok(()))
}

/// The generation's current lifecycle state, if it exists (spec 03 §2.1).
///
/// A stored value outside the CHECK domain (corruption) surfaces as
/// [`rusqlite::Error::FromSqlConversionFailure`], never a silent default.
pub fn generation_state(
    conn: &Connection,
    generation_id: &str,
) -> rusqlite::Result<Option<GenerationState>> {
    conn.query_row(
        "SELECT state FROM generation WHERE generation_id = ?1",
        params![generation_id],
        |r| {
            let raw: String = r.get(0)?;
            GenerationState::from_db(&raw).ok_or_else(|| {
                Error::FromSqlConversionFailure(
                    0,
                    Type::Text,
                    format!("invalid generation.state {raw:?}").into(),
                )
            })
        },
    )
    .optional()
}

/// The generation's per-worktree monotone `generation_number`, if it exists
/// (spec 03 §2.1) — T12-03.
///
/// Read by the search response's `generation: {id, number}` field (spec 09 §7).
/// Until now the number was only ever *written* (`allocate_generation`) or
/// scanned per worktree (retention, §5); a search knows its generation id and
/// needs exactly this one column.
pub fn generation_number(conn: &Connection, generation_id: &str) -> rusqlite::Result<Option<i64>> {
    conn.query_row(
        "SELECT generation_number FROM generation WHERE generation_id = ?1",
        params![generation_id],
        |r| r.get(0),
    )
    .optional()
}

/// The worktree's `active` generations, ascending by `generation_number` (spec 03
/// §2.1, 04 §1).
///
/// Only `state = 'active'` rows are returned — `retiring` and `failed` generations
/// are **never** consulted for routing (spec 04 §1 `[FIXED]`). The app invariant
/// is that this returns at most one id per worktree; the invariant is upheld
/// procedurally by the projection switch (a later task), and this reader is what
/// makes it observable/assertable.
pub fn active_generations(conn: &Connection, worktree_id: &str) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT generation_id FROM generation \
         WHERE worktree_id = ?1 AND state = 'active' \
         ORDER BY generation_number",
    )?;
    let ids = stmt
        .query_map(params![worktree_id], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(ids)
}

/// One full `generation` row (spec 03 §2.1, all five columns) — `inspect
/// generation <id>`'s own read (11 §6, T16-02). No evidence/audit concept
/// applies to a generation the way it does to a memory entry, so unlike
/// [`crate::memory::entry::MemoryEntryRow`]'s privacy-module wrapper, this
/// type is consumed as-is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenerationRow {
    pub generation_id: String,
    pub worktree_id: String,
    pub generation_number: i64,
    pub state: GenerationState,
    pub created_at: i64,
}

pub fn generation_row(
    conn: &Connection,
    generation_id: &str,
) -> rusqlite::Result<Option<GenerationRow>> {
    conn.query_row(
        "SELECT generation_id, worktree_id, generation_number, state, created_at \
         FROM generation WHERE generation_id = ?1",
        params![generation_id],
        |r| {
            let raw_state: String = r.get(3)?;
            let state = GenerationState::from_db(&raw_state).ok_or_else(|| {
                Error::FromSqlConversionFailure(
                    3,
                    Type::Text,
                    format!("invalid generation.state {raw_state:?}").into(),
                )
            })?;
            Ok(GenerationRow {
                generation_id: r.get(0)?,
                worktree_id: r.get(1)?,
                generation_number: r.get(2)?,
                state,
                created_at: r.get(4)?,
            })
        },
    )
    .optional()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_state_round_trips() {
        for state in [
            GenerationState::Building,
            GenerationState::ProjectionReady,
            GenerationState::Active,
            GenerationState::Retiring,
            GenerationState::Failed,
        ] {
            assert_eq!(GenerationState::from_db(state.as_str()), Some(state));
        }
        assert_eq!(GenerationState::from_db("bogus"), None);
    }

    #[test]
    fn check_transition_covers_the_whole_matrix() {
        use GenerationState::{Active, Building, Failed, ProjectionReady, Retiring};
        let all = [Building, ProjectionReady, Active, Retiring, Failed];

        // Legal directed transitions (spec 04 §1).
        let legal = [
            (Building, ProjectionReady),
            (Building, Failed),
            (ProjectionReady, Active),
            (ProjectionReady, Failed),
            (Active, Retiring),
        ];
        for (from, to) in legal {
            assert_eq!(from.check_transition(to), Ok(()), "{from:?} → {to:?} legal");
        }

        // Self-transitions are idempotent no-ops (legal) for every state.
        for s in all {
            assert_eq!(s.check_transition(s), Ok(()), "{s:?} → {s:?} idempotent");
        }

        // Everything else (excluding self-transitions) is illegal — in particular
        // `active → failed` (no trigger in spec 04 §1) and `building → active`
        // (must pass through `projection_ready`), plus every move out of the
        // terminal `retiring`/`failed`.
        for from in all {
            for to in all {
                if from == to || legal.contains(&(from, to)) {
                    continue;
                }
                assert_eq!(
                    from.check_transition(to),
                    Err(IllegalGenerationTransition { from, to }),
                    "{from:?} → {to:?} illegal",
                );
            }
        }
    }

    /// A store whose `generation.state` somehow holds a value outside the CHECK
    /// domain (corruption) must surface a typed conversion error from
    /// [`generation_state`], not a silent default. A minimal constraint-free table
    /// injects the bad value.
    #[test]
    fn generation_state_rejects_corrupt_enum() {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(
            "CREATE TABLE generation \
               (generation_id TEXT, worktree_id TEXT, generation_number INTEGER, \
                state TEXT, created_at INTEGER);\n\
             INSERT INTO generation VALUES ('g', 'w', 1, 'zombie', 1000);",
        )
        .expect("seed corrupt row");

        let bad = generation_state(&conn, "g");
        assert!(
            matches!(bad, Err(Error::FromSqlConversionFailure(0, Type::Text, _))),
            "corrupt state → typed conversion failure, got {bad:?}",
        );
        // An absent id is a clean `None`.
        assert_eq!(generation_state(&conn, "missing").expect("read"), None);
    }
}
