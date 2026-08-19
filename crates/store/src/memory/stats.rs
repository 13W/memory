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

/// One consolidation run D-071 reports as stuck: retried past
/// [`STUCK_RUN_ATTEMPT_THRESHOLD`], or dead-lettered on the running build.
///
/// `last_failure_reason` is already truncated to
/// [`STUCK_RUN_REASON_MAX_CHARS`] — this row exists to be printed, and a
/// router-failure reason can be arbitrarily long.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StuckRunRow {
    pub run_id: String,
    pub session_id: String,
    pub attempt_count: i64,
    /// `mechanical` **and** fingerprinted with the running build: D-050's
    /// dead-letter, which no sweep will retry until the binary changes.
    pub dead_lettered: bool,
    pub last_failure_kind: Option<String>,
    pub last_failure_reason: Option<String>,
    pub from_received_seq: i64,
    pub to_received_seq: i64,
}

/// How many recorded failures make a still-retryable run worth reporting
/// (D-071, `[SPEC]`-chosen like `CONSOLIDATION_THROUGHPUT_WINDOW_MS`, not
/// measured). Low on purpose: it is an early warning, meant to fire well
/// before
/// [`TRANSIENT_ATTEMPT_CAP`](super::consolidation::TRANSIENT_ATTEMPT_CAP)
/// gives up entirely, and a healthy run reaches `applied` on its first
/// attempt.
pub const STUCK_RUN_ATTEMPT_THRESHOLD: i64 = 3;

/// Ceiling on the reported `last_failure_reason`, in **characters** (not
/// bytes — a reason can carry any UTF-8 the router or SQLite produced, and a
/// byte slice would split a multi-byte character).
pub const STUCK_RUN_REASON_MAX_CHARS: usize = 200;

fn truncate_reason(reason: Option<String>) -> Option<String> {
    reason.map(|r| {
        if r.chars().count() <= STUCK_RUN_REASON_MAX_CHARS {
            return r;
        }
        let mut out: String = r.chars().take(STUCK_RUN_REASON_MAX_CHARS).collect();
        out.push('…');
        out
    })
}

/// Every `failed` consolidation run that needs a human's attention (D-071):
/// the store-side half of "nothing degrades silently" (spec 02 §6) for the
/// consolidation pillar, whose retry bookkeeping (D-050, schema v11) has been
/// recorded but never reported by `stats`/`doctor`.
///
/// A run qualifies when it is `failed` **and** either
///
/// - it has failed at least `attempt_threshold` times (callers pass
///   [`STUCK_RUN_ATTEMPT_THRESHOLD`]) — still retry-eligible, but visibly not
///   converging; or
/// - it is dead-lettered on `current_build_id` (`mechanical` +
///   matching fingerprint) — exactly the rows
///   [`stale_runs`](super::consolidation::stale_runs) will never pick up
///   again, so nothing but a rebuild can move them.
///
/// **Minus** the one shape that resolves itself: a context-overflow
/// dead-letter that is its session's latest `failed` run and still spans
/// more than one observation is
/// [`open_next_run`](super::consolidation::open_next_run)'s shrink-and-retry
/// carve-out (D-058) — the next tick opens a narrower window for it, so
/// calling it stuck would be a false alarm. The floor case (a window already
/// down to a single observation) stays: that one really is stuck, and is the
/// same set [`unconsolidatable_sessions`](super::consolidation::unconsolidatable_sessions)
/// reports from the session's side.
///
/// D-072: "latest" there means latest **`failed`**, deliberately not latest
/// non-`applied` the way `open_next_run`'s own `latest_non_applied_run` asks.
/// The two questions differ: `open_next_run` asks what blocks this session
/// *right now*, so a run in flight counts; this report asks whether anything
/// will ever act on this row again, which a transient `running` neighbour
/// does not change. Keyed off non-`applied`, the answer depended on whether
/// some run happened to be executing at the instant of the query — live
/// verification caught `doctor` and `stats`, minutes apart, reporting three
/// runs and then two on an unchanged store. A health report that flickers is
/// worse than one that says nothing.
///
/// Ordered worst-first (`attempt_count` descending, then `run_id`) so a
/// truncated human report still shows the loudest row.
pub fn stuck_consolidation_runs(
    conn: &Connection,
    current_build_id: &str,
    attempt_threshold: i64,
) -> rusqlite::Result<Vec<StuckRunRow>> {
    let mut stmt = conn.prepare(
        "SELECT c.run_id, c.session_id, c.attempt_count, c.last_failure_kind, \
                c.last_failure_reason, c.from_received_seq, c.to_received_seq, \
                (COALESCE(c.last_failure_kind, '') = 'mechanical' \
                 AND COALESCE(c.last_failure_fingerprint, '') = ?1) AS dead_lettered \
         FROM consolidation_run c \
         WHERE c.state = 'failed' \
           AND ( \
                 c.attempt_count >= ?2 \
                 OR ( \
                      COALESCE(c.last_failure_kind, '') = 'mechanical' \
                      AND COALESCE(c.last_failure_fingerprint, '') = ?1 \
                    ) \
               ) \
           AND NOT ( \
                 c.last_failure_context_overflow = 1 \
                 AND COALESCE(c.last_failure_kind, '') = 'mechanical' \
                 AND COALESCE(c.last_failure_fingerprint, '') = ?1 \
                 AND c.created_at = ( \
                       SELECT MAX(c2.created_at) FROM consolidation_run c2 \
                       WHERE c2.session_id = c.session_id AND c2.state = 'failed' \
                     ) \
                 AND ( \
                       SELECT COUNT(*) FROM observation_envelope e \
                       WHERE e.session_id = c.session_id \
                         AND e.received_seq BETWEEN c.from_received_seq AND c.to_received_seq \
                     ) > 1 \
               ) \
         ORDER BY c.attempt_count DESC, c.run_id",
    )?;
    stmt.query_map(params![current_build_id, attempt_threshold], |r| {
        Ok(StuckRunRow {
            run_id: r.get(0)?,
            session_id: r.get(1)?,
            attempt_count: r.get(2)?,
            dead_lettered: r.get(7)?,
            last_failure_kind: r.get(3)?,
            last_failure_reason: truncate_reason(r.get(4)?),
            from_received_seq: r.get(5)?,
            to_received_seq: r.get(6)?,
        })
    })?
    .collect()
}
