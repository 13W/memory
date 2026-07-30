//! Repository create/find/observe-path operations (spec 03 §2.1, 01 §5, 12 §7).
//!
//! These are the low-level registry primitives. Write operations take a
//! [`Transaction`] so they compose inside a single
//! [`StateWriter::transaction`](crate::StateWriter::transaction) closure; read
//! operations take a [`Connection`] so they run on a read-only connection
//! ([`StateDb::open_read`](crate::StateDb::open_read)) — and, via
//! `Transaction`'s `Deref<Target = Connection>`, inside a write transaction too.
//! Every operation returns [`rusqlite::Result`] so it slots directly into the
//! writer closure; a semantic failure such as observing a path for an unknown
//! repository surfaces as the natural foreign-key
//! [`ConstraintViolation`](rusqlite::ErrorCode::ConstraintViolation) and rolls
//! the transaction back. Typed resolution/ambiguity errors belong to the
//! request-root resolver (T02-04), not to these primitives.
//!
//! `repo_id` is minted by the caller (a UUIDv7 from
//! [`identity::uuidv7`](local_rag_core::identity::uuidv7)) and passed in as a
//! string: the writer closure is `Send + 'static`, so the caller generates the
//! id *outside* the transaction and moves an owned value in, keeping the clock
//! and entropy out of the write path.

use rusqlite::{Connection, OptionalExtension, Transaction, params};

/// One observed-path row of a repository (spec 03 §2.1 `repository_path`), for
/// reading back a repository's full path history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathObservation {
    /// The canonical absolute path this repository was observed at.
    pub observed_path: String,
    /// Whether this is the repository's single current path.
    pub is_current: bool,
    /// When this path was first observed (Unix ms).
    pub first_seen_at: i64,
    /// When this path was most recently observed (Unix ms).
    pub last_seen_at: i64,
}

/// Insert a `repository` row (spec 03 §2.1).
///
/// `repo_id` is a caller-minted UUIDv7. `git_remote_fingerprint` is the
/// `H(remote_fingerprint)` hint (see
/// [`identity::remote::fingerprint`](local_rag_core::identity::remote::fingerprint)),
/// which is nullable and NOT unique — the same remote may map to more than one
/// repository (spec 12 §7). This creates the repository only; call
/// [`observe_repository_path`] to record where it lives.
pub fn create_repository(
    tx: &Transaction<'_>,
    repo_id: &str,
    git_remote_fingerprint: Option<&str>,
    now_ms: i64,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO repository (repo_id, git_remote_fingerprint, created_at, last_seen_at) \
         VALUES (?1, ?2, ?3, ?3)",
        params![repo_id, git_remote_fingerprint, now_ms],
    )?;
    Ok(())
}

/// Record that `repo_id` is observed at `observed_path` and make it the single
/// current path (spec 03 §2.1).
///
/// Transactional and idempotent. The `repository_path_current` partial unique
/// index allows at most one `is_current = 1` row per repository; SQLite has no
/// deferred UNIQUE constraints, so the current flag is cleared and re-set in two
/// separate statements (never a single multi-row swap) to avoid a transient
/// double-current within the transaction:
///
/// 1. clear the repository's current flag (afterwards no row is current);
/// 2. upsert the target path as current (a new path gets
///    `first_seen_at = last_seen_at = now`; an existing path keeps its
///    `first_seen_at` and bumps `last_seen_at`);
/// 3. bump the repository's own `last_seen_at`.
///
/// An unknown `repo_id` is rejected by the `repository_path → repository`
/// foreign key at step 2. Re-observing the already-current path is a no-op on
/// `is_current` (cleared then re-set) that only refreshes `last_seen_at`.
pub fn observe_repository_path(
    tx: &Transaction<'_>,
    repo_id: &str,
    observed_path: &str,
    now_ms: i64,
) -> rusqlite::Result<()> {
    // 1) Clear the current flag: after this, no row for this repo is current.
    tx.execute(
        "UPDATE repository_path SET is_current = 0 WHERE repo_id = ?1 AND is_current = 1",
        params![repo_id],
    )?;
    // 2) Upsert the target path and make it current. New path → first/last seen
    //    both `now`; existing path → first_seen_at preserved, last_seen_at bumped.
    tx.execute(
        "INSERT INTO repository_path \
           (repo_id, observed_path, is_current, first_seen_at, last_seen_at) \
         VALUES (?1, ?2, 1, ?3, ?3) \
         ON CONFLICT(repo_id, observed_path) \
         DO UPDATE SET is_current = 1, last_seen_at = ?3",
        params![repo_id, observed_path, now_ms],
    )?;
    // 3) Repository liveness.
    tx.execute(
        "UPDATE repository SET last_seen_at = ?2 WHERE repo_id = ?1",
        params![repo_id, now_ms],
    )?;
    Ok(())
}

/// The `repo_id` whose **current** path is `observed_path`, if any (spec 03
/// §2.1).
///
/// Matches only the single current path; a path that was current in the past but
/// has since moved returns `None` (it remains in [`path_history`]). Full
/// move/attach resolution across history is the resolver's concern (T02-04).
pub fn find_repository_by_path(
    conn: &Connection,
    observed_path: &str,
) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT repo_id FROM repository_path WHERE observed_path = ?1 AND is_current = 1",
        params![observed_path],
        |r| r.get(0),
    )
    .optional()
}

/// Every `repo_id` in the store, ascending (mirrors
/// [`worktree::all_worktree_ids`](super::worktree::all_worktree_ids)) — a
/// store-wide reader, not scoped by remote fingerprint or current path
/// (`repo list`, T15-07).
pub fn all_repository_ids(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT repo_id FROM repository ORDER BY repo_id")?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Every `repo_id` sharing `git_remote_fingerprint`, ascending (spec 03 §2.1,
/// 12 §7).
///
/// Returns potentially **many** ids: the remote fingerprint is a hint, not a
/// unique identity, so a single remote can back several repositories.
pub fn find_repositories_by_remote(
    conn: &Connection,
    git_remote_fingerprint: &str,
) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT repo_id FROM repository WHERE git_remote_fingerprint = ?1 ORDER BY repo_id",
    )?;
    let rows = stmt.query_map(params![git_remote_fingerprint], |r| r.get::<_, String>(0))?;
    rows.collect()
}

/// The repository's single current observed path, if it has one (spec 03 §2.1).
pub fn current_path(conn: &Connection, repo_id: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT observed_path FROM repository_path WHERE repo_id = ?1 AND is_current = 1",
        params![repo_id],
        |r| r.get(0),
    )
    .optional()
}

/// The repository's full observed-path history, ordered by `first_seen_at` then
/// `observed_path` (spec 03 §2.1).
///
/// History is retained: moving the current path never deletes the prior row, it
/// only clears its `is_current` flag.
pub fn path_history(conn: &Connection, repo_id: &str) -> rusqlite::Result<Vec<PathObservation>> {
    let mut stmt = conn.prepare(
        "SELECT observed_path, is_current, first_seen_at, last_seen_at \
         FROM repository_path WHERE repo_id = ?1 \
         ORDER BY first_seen_at, observed_path",
    )?;
    let rows = stmt.query_map(params![repo_id], |r| {
        Ok(PathObservation {
            observed_path: r.get(0)?,
            is_current: r.get::<_, i64>(1)? != 0,
            first_seen_at: r.get(2)?,
            last_seen_at: r.get(3)?,
        })
    })?;
    rows.collect()
}
