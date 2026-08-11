//! Aggregate counts over `memory_entry`/`pending_memory_candidate`/
//! `consolidation_run` (spec 11 §2 `stats()`, T15-04; D-049 added the
//! consolidation-run/backlog counts — before it, `stats()` reported only the
//! memory pillar, never the observations pillar `01-overview.md` §5-9 also
//! names). Whole-store totals, not scope-filtered — "counts per pillar" is a
//! store-wide health figure, distinct from the per-request-scoped reads
//! elsewhere in this module (`list_memory_entries_for_scope`, the recall
//! pipeline).

use rusqlite::types::Type;
use rusqlite::{Connection, Error, params};

use super::candidate::CandidateState;
use super::consolidation::{RunState, pending_backlog, sessions_with_pending_backlog};
use super::entry::{MemoryKind, MemoryState};

/// One `(kind, state, count)` bucket of [`memory_entry_counts`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryCountRow {
    pub kind: MemoryKind,
    pub state: MemoryState,
    pub count: i64,
}

/// Every `memory_entry` row, grouped by `(kind, state)` — a store-wide
/// census, not scoped to any `(scope_kind, scope_owner_id)`. `GROUP BY`
/// never invents a zero row for an empty combination, so an empty store (or
/// one missing a particular kind/state pair) simply omits that bucket
/// rather than returning it with `count: 0`.
pub fn memory_entry_counts(conn: &Connection) -> rusqlite::Result<Vec<MemoryCountRow>> {
    let mut stmt = conn.prepare(
        "SELECT kind, state, COUNT(*) AS n FROM memory_entry \
         GROUP BY kind, state \
         ORDER BY kind, state",
    )?;
    stmt.query_map([], |r| {
        let raw_kind: String = r.get(0)?;
        let kind = MemoryKind::from_db(&raw_kind).ok_or_else(|| {
            Error::FromSqlConversionFailure(
                0,
                Type::Text,
                format!("invalid memory_entry.kind {raw_kind:?}").into(),
            )
        })?;
        let raw_state: String = r.get(1)?;
        let state = MemoryState::from_db(&raw_state).ok_or_else(|| {
            Error::FromSqlConversionFailure(
                1,
                Type::Text,
                format!("invalid memory_entry.state {raw_state:?}").into(),
            )
        })?;
        Ok(MemoryCountRow {
            kind,
            state,
            count: r.get(2)?,
        })
    })?
    .collect()
}

/// One `(review_state, count)` bucket of [`pending_candidate_counts`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CandidateCountRow {
    pub state: CandidateState,
    pub count: i64,
}

/// Every `pending_memory_candidate` row, grouped by `review_state` —
/// store-wide, the candidate-table twin of [`memory_entry_counts`].
pub fn pending_candidate_counts(conn: &Connection) -> rusqlite::Result<Vec<CandidateCountRow>> {
    let mut stmt = conn.prepare(
        "SELECT review_state, COUNT(*) AS n FROM pending_memory_candidate \
         GROUP BY review_state \
         ORDER BY review_state",
    )?;
    stmt.query_map([], |r| {
        let raw_state: String = r.get(0)?;
        let state = CandidateState::from_db(&raw_state).ok_or_else(|| {
            Error::FromSqlConversionFailure(
                0,
                Type::Text,
                format!("invalid pending_memory_candidate.review_state {raw_state:?}").into(),
            )
        })?;
        Ok(CandidateCountRow {
            state,
            count: r.get(1)?,
        })
    })?
    .collect()
}

/// One `(state, count)` bucket of [`consolidation_run_counts`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunCountRow {
    pub state: RunState,
    pub count: i64,
}

/// Every `consolidation_run` row, grouped by `state` — store-wide, the
/// consolidation-run twin of [`memory_entry_counts`]/[`pending_candidate_counts`]
/// (D-049, spec 11 §2 `stats()`'s "counts per pillar").
pub fn consolidation_run_counts(conn: &Connection) -> rusqlite::Result<Vec<RunCountRow>> {
    let mut stmt = conn.prepare(
        "SELECT state, COUNT(*) AS n FROM consolidation_run \
         GROUP BY state \
         ORDER BY state",
    )?;
    stmt.query_map([], |r| {
        let raw_state: String = r.get(0)?;
        let state = RunState::from_db(&raw_state).ok_or_else(|| {
            Error::FromSqlConversionFailure(
                0,
                Type::Text,
                format!("invalid consolidation_run.state {raw_state:?}").into(),
            )
        })?;
        Ok(RunCountRow {
            state,
            count: r.get(1)?,
        })
    })?
    .collect()
}

/// Store-wide sum of [`pending_backlog`] across every session
/// [`sessions_with_pending_backlog`] reports (D-049). Composes the two
/// already-tested primitives rather than a new aggregate query — the
/// session count is always small (bounded by concurrent sessions, not by
/// observation volume), so the extra round trips cost nothing and this stays
/// obviously correct against `pending_backlog`'s own arithmetic.
pub fn total_pending_backlog(conn: &Connection) -> rusqlite::Result<i64> {
    sessions_with_pending_backlog(conn)?
        .iter()
        .map(|session_id| pending_backlog(conn, session_id))
        .sum()
}

/// The total observations consolidated (D-049): the sum of window sizes
/// (`to_received_seq - from_received_seq + 1`) over every `consolidation_run`
/// that reached `applied` after `since_ms`. Measured in observations, the
/// same unit [`total_pending_backlog`] uses — a run count would be
/// misleading here since windows vary in size (checkpoint-triggered windows
/// can be far smaller than a full `batch_size`).
pub fn observations_applied_since(conn: &Connection, since_ms: i64) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT COALESCE(SUM(to_received_seq - from_received_seq + 1), 0) \
         FROM consolidation_run \
         WHERE state = 'applied' AND updated_at > ?1",
        params![since_ms],
        |r| r.get(0),
    )
}

/// The `created_at` of the oldest still-open (`state != 'applied'`)
/// `consolidation_run` row, or `None` if every run has been applied (D-049).
/// Named for exactly what it measures — when the oldest currently-pending
/// unit of work was created — not "when the backlog started," a history this
/// schema does not keep.
pub fn oldest_open_run_created_at(conn: &Connection) -> rusqlite::Result<Option<i64>> {
    conn.query_row(
        "SELECT MIN(created_at) FROM consolidation_run WHERE state != 'applied'",
        [],
        |r| r.get(0),
    )
}
