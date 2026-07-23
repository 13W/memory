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

/// A worktree's core identity fields (spec 03 §2.1), read in one query for the
/// request-root resolver and [`attach`](super::attach) (T02-04): the repository
/// it belongs to, its `kind`, and its lifecycle `state`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeSummary {
    /// The worktree's stable id (never path-derived, spec 01 §5).
    pub worktree_id: String,
    /// The repository this worktree belongs to.
    pub repo_id: String,
    /// Whether it is the main tree, a linked worktree, or a non-git directory.
    pub kind: WorktreeKind,
    /// Its current lifecycle state (spec 04 §7).
    pub state: WorktreeState,
}

/// Insert a `worktree` row (spec 03 §2.1).
///
/// `worktree_id` is a caller-minted UUIDv7 (never path-derived, spec 01 §5). The
/// worktree starts `active` with no current generation (`current_generation_id`
/// NULL). An unknown `repo_id` is rejected by the `worktree → repository` foreign
/// key. Call [`observe_worktree_path`] to record where it lives and
/// [`set_current_generation`] once a generation is active.
///
/// `state_changed_at` is stamped with `now_ms` too (D-007): a freshly created
/// `active` worktree has "entered its current state" now, which keeps the
/// column non-null and monotone from the very first row rather than needing a
/// sentinel the grace-period reader would have to special-case.
pub fn create_worktree(
    tx: &Transaction<'_>,
    worktree_id: &str,
    repo_id: &str,
    kind: WorktreeKind,
    now_ms: i64,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO worktree \
           (worktree_id, repo_id, kind, current_generation_id, state, created_at, last_seen_at, \
            state_changed_at) \
         VALUES (?1, ?2, ?3, NULL, 'active', ?4, ?4, ?4)",
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
/// - a legal transition to a different state → the row's `state` **and**
///   `state_changed_at` (`now_ms`) are updated;
/// - a legal self-transition (`X → X`) → a no-op success.
///
/// Splitting infrastructure failure (retry) from domain rejection (do not retry)
/// is deliberate. A stored `state` outside the CHECK domain (corruption) surfaces
/// as the outer [`rusqlite::Error`].
///
/// `state_changed_at` is deliberately stamped **only on an effective change**
/// (D-007), matching this function's existing "self-transition is an idempotent
/// no-op" contract: a crash/retry that re-requests the state a worktree is
/// already in must not push the shard-destruction grace period (spec 05 §8)
/// forward, or a retry loop could keep a doomed shard alive indefinitely.
pub fn transition_worktree_state(
    tx: &Transaction<'_>,
    worktree_id: &str,
    to: WorktreeState,
    now_ms: i64,
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
            "UPDATE worktree SET state = ?2, state_changed_at = ?3 WHERE worktree_id = ?1",
            params![worktree_id, to.as_str(), now_ms],
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

/// Map a `(worktree_id, repo_id, kind, state)` row into a [`WorktreeSummary`],
/// parsing `kind`/`state` with the same `FromSqlConversionFailure` fallback used
/// by [`worktree_state`] so a value outside the CHECK domain (corruption)
/// surfaces as a typed SQLite error rather than a silent default.
fn summary_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<WorktreeSummary> {
    let worktree_id: String = r.get(0)?;
    let repo_id: String = r.get(1)?;
    let kind_raw: String = r.get(2)?;
    let state_raw: String = r.get(3)?;
    let kind = WorktreeKind::from_db(&kind_raw).ok_or_else(|| {
        Error::FromSqlConversionFailure(
            2,
            Type::Text,
            format!("invalid worktree.kind {kind_raw:?}").into(),
        )
    })?;
    let state = WorktreeState::from_db(&state_raw).ok_or_else(|| {
        Error::FromSqlConversionFailure(
            3,
            Type::Text,
            format!("invalid worktree.state {state_raw:?}").into(),
        )
    })?;
    Ok(WorktreeSummary {
        worktree_id,
        repo_id,
        kind,
        state,
    })
}

/// The single worktree whose **current** (`is_current = 1`) observed canonical
/// path is `observed_canonical_path`, if any (spec 03 §2.1). Symmetric to
/// [`find_repository_by_path`](super::find_repository_by_path).
///
/// This is the resolver's only auto-resolution key (T02-04): it matches strictly
/// on the current path, never on history or a fingerprint. Because
/// `worktree_path_current` is a *per-worktree* partial unique index (not a global
/// one), a canonical path is not guaranteed unique across worktrees; the daemon
/// maintains a single current occupant per path, and this query is deterministic
/// regardless via `ORDER BY worktree_id LIMIT 1`.
pub fn find_worktree_by_current_path(
    conn: &Connection,
    observed_canonical_path: &str,
) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT worktree_id FROM worktree_path \
         WHERE observed_canonical_path = ?1 AND is_current = 1 \
         ORDER BY worktree_id LIMIT 1",
        params![observed_canonical_path],
        |r| r.get(0),
    )
    .optional()
}

/// The [`WorktreeSummary`] for `worktree_id`, if it exists (spec 03 §2.1) — the
/// repo/kind/state trio the resolver and [`attach`](super::attach) need in one
/// query.
pub fn worktree_summary(
    conn: &Connection,
    worktree_id: &str,
) -> rusqlite::Result<Option<WorktreeSummary>> {
    conn.query_row(
        "SELECT worktree_id, repo_id, kind, state FROM worktree WHERE worktree_id = ?1",
        params![worktree_id],
        summary_from_row,
    )
    .optional()
}

/// Every worktree of `repo_id`, ascending by `worktree_id` (spec 03 §2.1).
///
/// Used by the resolver to expand a remote-fingerprint hint into that
/// repository's worktrees when disambiguating a move (T02-04); the remote
/// fingerprint is a hint, so this is advisory input, never identity (spec 12 §7).
pub fn worktrees_of_repo(
    conn: &Connection,
    repo_id: &str,
) -> rusqlite::Result<Vec<WorktreeSummary>> {
    let mut stmt = conn.prepare(
        "SELECT worktree_id, repo_id, kind, state FROM worktree \
         WHERE repo_id = ?1 ORDER BY worktree_id",
    )?;
    let rows = stmt.query_map(params![repo_id], summary_from_row)?;
    rows.collect()
}

/// Every `worktree_id` in the store, ascending (spec 03 §2.1).
///
/// A store-wide reader, not scoped to a repository. Two consumers rely on the
/// complete set: retention's store-wide pin union (T06-02) and orphan-shard
/// housekeeping (T06-03) — a `projection/<name>` directory whose `<name>` is not
/// returned here has no worktree row and is an orphan shard (spec 05 §8).
/// Ascending order makes the result deterministic.
pub fn all_worktree_ids(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT worktree_id FROM worktree ORDER BY worktree_id")?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// One worktree's lifecycle state plus the timestamp it entered it (D-007).
///
/// The input shape of spec 05 §8's shard grace period: "remove/detach: grace
/// period `[SPEC: 7 days]`, then destroy". Deliberately a plain data row with no
/// clock of its own so the eligibility predicate
/// ([`shard_destroy_due`](crate::housekeeping::shard_destroy_due)) stays pure
/// and table-testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeStateClock {
    /// The worktree's stable id.
    pub worktree_id: String,
    /// Its current lifecycle state (spec 04 §7).
    pub state: WorktreeState,
    /// When it entered `state`, epoch ms (`created_at` for a never-transitioned
    /// row; `last_seen_at` for rows backfilled by migration 5).
    pub state_changed_at: i64,
}

/// Every worktree's `(state, state_changed_at)` pair, ascending by id (D-007).
///
/// The store-wide reader behind the grace-period shard sweep
/// ([`run_expired_shard_sweep`](crate::housekeeping::run_expired_shard_sweep)):
/// it returns *all* worktrees, not just the non-`active` ones, so the sweep's
/// eligibility decision lives in one pure predicate rather than being split
/// between a SQL `WHERE` clause and Rust. Ascending order makes it
/// deterministic.
pub fn worktree_state_clocks(conn: &Connection) -> rusqlite::Result<Vec<WorktreeStateClock>> {
    let mut stmt = conn.prepare(
        "SELECT worktree_id, state, state_changed_at FROM worktree ORDER BY worktree_id",
    )?;
    let rows = stmt.query_map([], |r| {
        let raw: String = r.get(1)?;
        let state = WorktreeState::from_db(&raw).ok_or_else(|| {
            Error::FromSqlConversionFailure(
                1,
                Type::Text,
                format!("invalid worktree.state {raw:?}").into(),
            )
        })?;
        Ok(WorktreeStateClock {
            worktree_id: r.get(0)?,
            state,
            state_changed_at: r.get(2)?,
        })
    })?;
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

    /// A store whose `worktree.kind`/`state` somehow hold a value outside the
    /// CHECK domain (corruption) must surface a typed conversion error from
    /// [`worktree_summary`], not a silent default. A minimal constraint-free
    /// table injects the bad value.
    #[test]
    fn summary_from_row_rejects_corrupt_enum() {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(
            "CREATE TABLE worktree (worktree_id TEXT, repo_id TEXT, kind TEXT, state TEXT);\n\
             INSERT INTO worktree VALUES ('w1', 'r', 'bogus', 'active');\n\
             INSERT INTO worktree VALUES ('w2', 'r', 'main', 'bogus');",
        )
        .expect("seed corrupt rows");

        let bad_kind = worktree_summary(&conn, "w1");
        assert!(
            matches!(
                bad_kind,
                Err(Error::FromSqlConversionFailure(2, Type::Text, _))
            ),
            "corrupt kind → typed conversion failure, got {bad_kind:?}",
        );
        let bad_state = worktree_summary(&conn, "w2");
        assert!(
            matches!(
                bad_state,
                Err(Error::FromSqlConversionFailure(3, Type::Text, _))
            ),
            "corrupt state → typed conversion failure, got {bad_state:?}",
        );
    }

    /// A well-formed row parses into the expected [`WorktreeSummary`]; an absent
    /// id yields `None`.
    #[test]
    fn summary_from_row_parses_valid_row() {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(
            "CREATE TABLE worktree (worktree_id TEXT, repo_id TEXT, kind TEXT, state TEXT);\n\
             INSERT INTO worktree VALUES ('w', 'r', 'linked', 'detached');",
        )
        .expect("seed row");

        assert_eq!(
            worktree_summary(&conn, "w").expect("summary"),
            Some(WorktreeSummary {
                worktree_id: "w".to_string(),
                repo_id: "r".to_string(),
                kind: WorktreeKind::Linked,
                state: WorktreeState::Detached,
            }),
        );
        assert_eq!(worktree_summary(&conn, "absent").expect("absent"), None);
    }
}
