//! Aggregate counts over `memory_entry`/`pending_memory_candidate` (spec 11
//! §2 `stats()`, T15-04). Whole-store totals, not scope-filtered — "counts
//! per pillar" is a store-wide health figure, distinct from the
//! per-request-scoped reads elsewhere in this module (`list_memory_entries_
//! for_scope`, the recall pipeline).

use rusqlite::types::Type;
use rusqlite::{Connection, Error};

use super::candidate::CandidateState;
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
