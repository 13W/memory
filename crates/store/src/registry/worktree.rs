//! Worktree create/observe/transition operations and the worktree state machine
//! (spec 03 §2.1, 04 §7, 01 §5).
//!
//! These mirror the repository-side primitives (the `repository` submodule):
//! write operations take a [`Transaction`] so they compose inside a single
//! [`StateWriter::transaction`](crate::StateWriter::transaction) closure; read
//! operations take a [`Connection`] so they run on a read-only connection and,
//! via `Transaction`'s `Deref<Target = Connection>`, inside a write transaction
//! too.
//!
//! Two invariants from spec 01 §5 shape this module:
//!
//! - **No durable ID is derived from a filesystem path.** `worktree.worktree_id`
//!   is a random, caller-minted UUIDv7. The path a worktree is observed at lives
//!   only in `worktree_path`; `worktree_path.path_fingerprint` is a *lookup
//!   accelerator only* (never an identity and never an FK target).
//! - **The current generation belongs to its worktree, structurally.** The
//!   composite foreign key `(current_generation_id, worktree_id) →
//!   generation(generation_id, worktree_id)` makes it impossible to point a
//!   worktree at another worktree's generation ([`set_current_generation`]).
//!
//! `worktree_id` (and `repo_id`) are minted by the caller (a UUIDv7 from
//! [`identity::uuidv7`](local_rag_core::identity::uuidv7)) and passed in as
//! strings, keeping the clock and entropy out of the write path — the writer
//! closure is `Send + 'static`.
//!
//! Scope note (T02-03): the `generation` table is created by this migration
//! because the worktree composite FK requires it, but the generation builder,
//! occurrence schema (spec 03 §2.4), and generation state machine (spec 04 §1)
//! are group 05. This module ships only the worktree-side seam that writes
//! `worktree.current_generation_id`.

use rusqlite::types::Type;
use rusqlite::{Connection, Error, OptionalExtension, Transaction, params};

/// The kind of a worktree (spec 03 §2.1 `worktree.kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeKind {
    /// The main working tree of a git repository.
    Main,
    /// A linked git worktree (`git worktree add`).
    Linked,
    /// A non-git directory tracked as a worktree.
    NonGit,
}

impl WorktreeKind {
    /// The stored `worktree.kind` value.
    pub fn as_str(self) -> &'static str {
        match self {
            WorktreeKind::Main => "main",
            WorktreeKind::Linked => "linked",
            WorktreeKind::NonGit => "non_git",
        }
    }

    /// Parse a stored `worktree.kind` value; `None` for anything the CHECK
    /// constraint forbids.
    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "main" => Some(WorktreeKind::Main),
            "linked" => Some(WorktreeKind::Linked),
            "non_git" => Some(WorktreeKind::NonGit),
            _ => None,
        }
    }
}

/// The lifecycle state of a worktree (spec 03 §2.1 `worktree.state`, machine in
/// spec 04 §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeState {
    /// The worktree's path resolves; it participates in routing.
    Active,
    /// The path no longer resolves; identity is retained and reattachable
    /// (`repo attach`, T02-04).
    Detached,
    /// Marked for deletion; removed after shard/spool/GC cleanup (a grace
    /// period). Terminal.
    Removing,
}

impl WorktreeState {
    /// The stored `worktree.state` value.
    pub fn as_str(self) -> &'static str {
        match self {
            WorktreeState::Active => "active",
            WorktreeState::Detached => "detached",
            WorktreeState::Removing => "removing",
        }
    }

    /// Parse a stored `worktree.state` value; `None` for anything the CHECK
    /// constraint forbids.
    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "active" => Some(WorktreeState::Active),
            "detached" => Some(WorktreeState::Detached),
            "removing" => Some(WorktreeState::Removing),
            _ => None,
        }
    }

    /// Check whether `self → to` is a legal transition (spec 04 §7), returning a
    /// typed [`IllegalWorktreeTransition`] otherwise. Pure — no I/O.
    ///
    /// The machine is `active ⇄ detached` and `active|detached → removing`
    /// (`removing` is terminal). A self-transition (`X → X`) is an idempotent
    /// no-op and is legal: staying in a state honors the request rather than
    /// coercing it (spec 04 preamble), and it keeps a crash/retry that re-requests
    /// the current state safe (`[SPEC]` precision). Every other move out of
    /// `removing` is illegal.
    pub fn check_transition(self, to: WorktreeState) -> Result<(), IllegalWorktreeTransition> {
        use WorktreeState::{Active, Detached, Removing};
        let legal = match (self, to) {
            (a, b) if a == b => true,
            (Active, Detached) | (Detached, Active) => true,
            (Active, Removing) | (Detached, Removing) => true,
            _ => false,
        };
        if legal {
            Ok(())
        } else {
            Err(IllegalWorktreeTransition { from: self, to })
        }
    }
}

/// A rejected worktree state transition (spec 04 §7): the machine forbids
/// `from → to`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IllegalWorktreeTransition {
    /// The worktree's current state.
    pub from: WorktreeState,
    /// The requested (illegal) target state.
    pub to: WorktreeState,
}

impl std::fmt::Display for IllegalWorktreeTransition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "illegal worktree transition {} → {}",
            self.from.as_str(),
            self.to.as_str()
        )
    }
}

impl std::error::Error for IllegalWorktreeTransition {}

/// Why a [`transition_worktree_state`] request was rejected at the domain level
/// (as opposed to an infrastructure/SQLite failure, which surfaces as the outer
/// [`rusqlite::Error`] and rolls the transaction back).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeTransitionError {
    /// No `worktree` row has this id.
    UnknownWorktree,
    /// The state machine (spec 04 §7) forbids the requested transition.
    Illegal(IllegalWorktreeTransition),
}

impl std::fmt::Display for WorktreeTransitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorktreeTransitionError::UnknownWorktree => write!(f, "unknown worktree"),
            WorktreeTransitionError::Illegal(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for WorktreeTransitionError {}

/// One observed-path row of a worktree (spec 03 §2.1 `worktree_path`), for
/// reading back a worktree's full path history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreePathObservation {
    /// The canonical absolute path this worktree was observed at.
    pub observed_canonical_path: String,
    /// The original spelling of the path (identity never depends on it, 03 §1.3).
    pub display_path: String,
    /// `H(path_fingerprint, canonical_path)` — a lookup accelerator ONLY, never
    /// identity (spec 01 §5).
    pub path_fingerprint: String,
    /// Whether this is the worktree's single current path.
    pub is_current: bool,
    /// When this path was first observed (Unix ms).
    pub first_seen_at: i64,
    /// When this path was most recently observed (Unix ms).
    pub last_seen_at: i64,
}

/// Insert a `worktree` row (spec 03 §2.1).
///
/// `worktree_id` is a caller-minted UUIDv7 (never path-derived, spec 01 §5). The
/// worktree starts `active` with no current generation (`current_generation_id`
/// NULL). An unknown `repo_id` is rejected by the `worktree → repository` foreign
/// key. Call [`observe_worktree_path`] to record where it lives and
/// [`set_current_generation`] once a generation is active.
pub fn create_worktree(
    tx: &Transaction<'_>,
    worktree_id: &str,
    repo_id: &str,
    kind: WorktreeKind,
    now_ms: i64,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO worktree \
           (worktree_id, repo_id, kind, current_generation_id, state, created_at, last_seen_at) \
         VALUES (?1, ?2, ?3, NULL, 'active', ?4, ?4)",
        params![worktree_id, repo_id, kind.as_str(), now_ms],
    )?;
    Ok(())
}

/// Record that `worktree_id` is observed at `observed_canonical_path` and make it
/// the single current path (spec 03 §2.1).
///
/// Transactional and idempotent, mirroring
/// [`observe_repository_path`](super::observe_repository_path): the current flag
/// is cleared and re-set in two separate statements (SQLite has no deferred
/// UNIQUE constraints), so the `worktree_path_current` partial unique index is
/// never transiently violated. `display_path` and `path_fingerprint` are stored
/// alongside the canonical path; the fingerprint is a lookup accelerator only.
///
/// 1. clear the worktree's current flag (afterwards no row is current);
/// 2. upsert the target path as current (new path → `first_seen_at =
///    last_seen_at = now`; existing path → `first_seen_at` kept, `display_path`,
///    `path_fingerprint`, and `last_seen_at` refreshed);
/// 3. bump the worktree's own `last_seen_at`.
///
/// An unknown `worktree_id` is rejected by the `worktree_path → worktree` foreign
/// key at step 2. This never changes `worktree.state`; reattaching a detached
/// worktree is an explicit [`transition_worktree_state`] (T02-04 composes them).
pub fn observe_worktree_path(
    tx: &Transaction<'_>,
    worktree_id: &str,
    observed_canonical_path: &str,
    display_path: &str,
    path_fingerprint: &str,
    now_ms: i64,
) -> rusqlite::Result<()> {
    // 1) Clear the current flag: after this, no row for this worktree is current.
    tx.execute(
        "UPDATE worktree_path SET is_current = 0 WHERE worktree_id = ?1 AND is_current = 1",
        params![worktree_id],
    )?;
    // 2) Upsert the target path and make it current.
    tx.execute(
        "INSERT INTO worktree_path \
           (worktree_id, observed_canonical_path, display_path, path_fingerprint, \
            is_current, first_seen_at, last_seen_at) \
         VALUES (?1, ?2, ?3, ?4, 1, ?5, ?5) \
         ON CONFLICT(worktree_id, observed_canonical_path) \
         DO UPDATE SET is_current = 1, display_path = ?3, path_fingerprint = ?4, \
                       last_seen_at = ?5",
        params![
            worktree_id,
            observed_canonical_path,
            display_path,
            path_fingerprint,
            now_ms
        ],
    )?;
    // 3) Worktree liveness.
    tx.execute(
        "UPDATE worktree SET last_seen_at = ?2 WHERE worktree_id = ?1",
        params![worktree_id, now_ms],
    )?;
    Ok(())
}

/// Point `worktree_id` at `generation_id` as its current generation (spec 03
/// §2.1) — the worktree-side seam for the generation switch.
///
/// The composite foreign key `(current_generation_id, worktree_id) →
/// generation(generation_id, worktree_id)` is enforced on this UPDATE: a
/// generation belonging to a *different* worktree cannot be pointed to (the tuple
/// `(generation_id, worktree_id)` will not match), and an unknown generation is
/// likewise rejected. The app invariant that the referenced generation is in
/// state `active` is asserted in tests, not enforced structurally (the full
/// generation lifecycle is group 05).
pub fn set_current_generation(
    tx: &Transaction<'_>,
    worktree_id: &str,
    generation_id: &str,
) -> rusqlite::Result<()> {
    tx.execute(
        "UPDATE worktree SET current_generation_id = ?2 WHERE worktree_id = ?1",
        params![worktree_id, generation_id],
    )?;
    Ok(())
}

/// Transition `worktree_id` to `to`, enforcing the state machine (spec 04 §7).
///
/// The outer [`rusqlite::Result`] carries only infrastructure failures (a SQLite
/// error rolls the transaction back). The inner [`Result`] carries the domain
/// outcome:
///
/// - no such worktree → `Ok(Err(WorktreeTransitionError::UnknownWorktree))`;
/// - an illegal transition → `Ok(Err(WorktreeTransitionError::Illegal(..)))`,
///   with **no mutation** (the illegality is detected before any write, so the
///   enclosing transaction commits a no-op);
/// - a legal transition to a different state → the row's `state` is updated;
/// - a legal self-transition (`X → X`) → a no-op success.
///
/// Splitting infrastructure failure (retry) from domain rejection (do not retry)
/// is deliberate. A stored `state` outside the CHECK domain (corruption) surfaces
/// as the outer [`rusqlite::Error`].
pub fn transition_worktree_state(
    tx: &Transaction<'_>,
    worktree_id: &str,
    to: WorktreeState,
) -> rusqlite::Result<Result<(), WorktreeTransitionError>> {
    let from: Option<WorktreeState> = tx
        .query_row(
            "SELECT state FROM worktree WHERE worktree_id = ?1",
            params![worktree_id],
            |r| {
                let raw: String = r.get(0)?;
                WorktreeState::from_db(&raw).ok_or_else(|| {
                    Error::FromSqlConversionFailure(
                        0,
                        Type::Text,
                        format!("invalid worktree.state {raw:?}").into(),
                    )
                })
            },
        )
        .optional()?;

    let Some(from) = from else {
        return Ok(Err(WorktreeTransitionError::UnknownWorktree));
    };

    if let Err(illegal) = from.check_transition(to) {
        return Ok(Err(WorktreeTransitionError::Illegal(illegal)));
    }

    if from != to {
        tx.execute(
            "UPDATE worktree SET state = ?2 WHERE worktree_id = ?1",
            params![worktree_id, to.as_str()],
        )?;
    }
    Ok(Ok(()))
}

/// The worktree's current lifecycle state, if it exists (spec 03 §2.1).
pub fn worktree_state(
    conn: &Connection,
    worktree_id: &str,
) -> rusqlite::Result<Option<WorktreeState>> {
    conn.query_row(
        "SELECT state FROM worktree WHERE worktree_id = ?1",
        params![worktree_id],
        |r| {
            let raw: String = r.get(0)?;
            WorktreeState::from_db(&raw).ok_or_else(|| {
                Error::FromSqlConversionFailure(
                    0,
                    Type::Text,
                    format!("invalid worktree.state {raw:?}").into(),
                )
            })
        },
    )
    .optional()
}

/// The worktree's current generation id, if one is set (spec 03 §2.1).
pub fn current_generation(
    conn: &Connection,
    worktree_id: &str,
) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT current_generation_id FROM worktree WHERE worktree_id = ?1",
        params![worktree_id],
        |r| r.get::<_, Option<String>>(0),
    )
    .optional()
    // A row exists but `current_generation_id` is NULL, or the worktree is
    // absent: both collapse to `None`.
    .map(|opt| opt.flatten())
}

/// The worktree's single current observed canonical path, if it has one (spec 03
/// §2.1).
pub fn current_worktree_path(
    conn: &Connection,
    worktree_id: &str,
) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT observed_canonical_path FROM worktree_path \
         WHERE worktree_id = ?1 AND is_current = 1",
        params![worktree_id],
        |r| r.get(0),
    )
    .optional()
}

/// The worktree's full observed-path history, ordered by `first_seen_at` then
/// `observed_canonical_path` (spec 03 §2.1).
///
/// History is retained: moving the current path never deletes the prior row, it
/// only clears its `is_current` flag.
pub fn worktree_path_history(
    conn: &Connection,
    worktree_id: &str,
) -> rusqlite::Result<Vec<WorktreePathObservation>> {
    let mut stmt = conn.prepare(
        "SELECT observed_canonical_path, display_path, path_fingerprint, is_current, \
                first_seen_at, last_seen_at \
         FROM worktree_path WHERE worktree_id = ?1 \
         ORDER BY first_seen_at, observed_canonical_path",
    )?;
    let rows = stmt.query_map(params![worktree_id], |r| {
        Ok(WorktreePathObservation {
            observed_canonical_path: r.get(0)?,
            display_path: r.get(1)?,
            path_fingerprint: r.get(2)?,
            is_current: r.get::<_, i64>(3)? != 0,
            first_seen_at: r.get(4)?,
            last_seen_at: r.get(5)?,
        })
    })?;
    rows.collect()
}

/// Every distinct `worktree_id` whose observed path (current or historical)
/// carries `path_fingerprint`, ascending (spec 03 §2.1; uses the
/// `worktree_path_fp` index).
///
/// The fingerprint is a **lookup accelerator only**, never identity (spec 01 §5):
/// it may match rows across several worktrees, so this returns potentially many
/// ids. Turning a fingerprint match into a resolved worktree (disambiguating
/// current vs historical, move vs recreate) is the resolver's concern (T02-04).
pub fn find_worktrees_by_path_fingerprint(
    conn: &Connection,
    path_fingerprint: &str,
) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT worktree_id FROM worktree_path \
         WHERE path_fingerprint = ?1 ORDER BY worktree_id",
    )?;
    let rows = stmt.query_map(params![path_fingerprint], |r| r.get::<_, String>(0))?;
    rows.collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worktree_kind_round_trips() {
        for kind in [
            WorktreeKind::Main,
            WorktreeKind::Linked,
            WorktreeKind::NonGit,
        ] {
            assert_eq!(WorktreeKind::from_db(kind.as_str()), Some(kind));
        }
        assert_eq!(WorktreeKind::from_db("bogus"), None);
    }

    #[test]
    fn worktree_state_round_trips() {
        for state in [
            WorktreeState::Active,
            WorktreeState::Detached,
            WorktreeState::Removing,
        ] {
            assert_eq!(WorktreeState::from_db(state.as_str()), Some(state));
        }
        assert_eq!(WorktreeState::from_db("bogus"), None);
    }

    #[test]
    fn check_transition_covers_the_whole_matrix() {
        use WorktreeState::{Active, Detached, Removing};

        // Legal directed transitions (spec 04 §7).
        for (from, to) in [
            (Active, Detached),
            (Detached, Active),
            (Active, Removing),
            (Detached, Removing),
        ] {
            assert_eq!(from.check_transition(to), Ok(()), "{from:?} → {to:?} legal");
        }

        // Self-transitions are idempotent no-ops (legal).
        for s in [Active, Detached, Removing] {
            assert_eq!(s.check_transition(s), Ok(()), "{s:?} → {s:?} idempotent");
        }

        // `removing` is terminal: every move out of it is illegal.
        for to in [Active, Detached] {
            assert_eq!(
                Removing.check_transition(to),
                Err(IllegalWorktreeTransition { from: Removing, to }),
                "removing → {to:?} illegal",
            );
        }
    }
}
