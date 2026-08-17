//! Durable per-worktree background-indexing status (spec 03 §2.1) — X-006.
//!
//! This module ships migration **version 13** ([`SCHEMA_V13`]) and the
//! read/write layer over `worktree_indexing_status`: one row per worktree the
//! daemon has actually tried to index, recording when it last attempted, when
//! it last succeeded, which generation that was, how many consecutive failures
//! have followed, and the last error text.
//!
//! ## Why a table of its own, next to `managed_worktree`
//!
//! [`managed`](super::managed) is the **opt-in registry** — the user's answer
//! to "index this project in the background". T20-01 deliberately kept runtime
//! state out of it (spec 03 §2.1: "The table carries **no runtime columns**"),
//! because "the user paused indexing" and "the last cycle failed" are
//! orthogonal axes and conflating them makes both unreadable. That reasoning is
//! preserved here: enrollment stays in `managed_worktree`, outcome lives here,
//! and `local-rag project list`/`status` join the two.
//!
//! What X-006 changes is only the *storage duration* of the outcome, not its
//! owner. Before it, the daemon's `WorktreeTaskStatus` existed solely in the
//! supervisor's memory, so every idle shutdown (`daemon.idle_shutdown_secs`,
//! 15 min by default) erased the entire answer to "did background indexing ever
//! run?" — the observability gap this task was filed for. The shape mirrors
//! [`projection_state`](super::projection_state), which already sits beside
//! `worktree` for exactly this reason: durable per-worktree runtime state
//! belongs in its own table, keyed by the stable worktree id.
//!
//! ## What is *not* here
//!
//! `in_progress_since` — "a cycle is running right now" — is deliberately left
//! in memory. A persisted "in progress" is a lie the moment the process dies:
//! nothing would ever clear it, and every reader would have to second-guess it
//! against a liveness probe it already has (`admin/projects_list` answers that
//! question directly, and only a live daemon can answer it truthfully).

use rusqlite::{Connection, OptionalExtension, Transaction, params};

/// Version-13 migration DDL: durable background-indexing status (spec 03 §2.1).
///
/// A pure additive leaf, like [`SCHEMA_V10`](super::managed::SCHEMA_V10): it
/// creates one table with a single outward foreign key
/// (`worktree_indexing_status → worktree`), touches no existing row, and needs
/// no backfill — a worktree simply has no status row until its first indexing
/// cycle finishes.
///
/// `last_generation_id` is **advisory and deliberately unconstrained**: it
/// names the generation the last successful cycle projected, but retention/GC
/// (spec 06 §5) is free to retire that generation later, and a status row must
/// never be the reason a sweep fails. This is the same choice
/// `worktree_projection_state.projection_op_id` already makes — an id stored
/// for diagnosis, not for referential integrity.
///
/// **Frozen once shipped.** The migration checksum is the SHA-256 of this text
/// (see [`crate::migrate::Migration::checksum`]); any edit — even whitespace or
/// a comment — trips
/// [`ChecksumDrift`](crate::migrate::MigrationError::ChecksumDrift) on an
/// existing store. Future schema changes are new numbered migrations.
pub(crate) const SCHEMA_V13: &str = "\
CREATE TABLE worktree_indexing_status (                -- migration 13; X-006 durable indexing outcome
  worktree_id           TEXT PRIMARY KEY REFERENCES worktree(worktree_id),
  last_attempt_at       INTEGER,                       -- epoch ms; when the last cycle started
  last_success_at       INTEGER,                       -- epoch ms; when the last cycle succeeded
  last_generation_id    TEXT,                          -- advisory, no FK: GC may retire it
  consecutive_failures  INTEGER NOT NULL DEFAULT 0,
  last_error            TEXT,
  updated_at            INTEGER NOT NULL
);
-- Enrollment lives in managed_worktree; this table is the outcome axis only.
-- 'in progress' is NOT persisted: only a live daemon can answer that truthfully.
";

/// One worktree's durable indexing outcome (spec 03 §2.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeIndexingStatus {
    /// The worktree this outcome belongs to (`worktree.worktree_id`).
    pub worktree_id: String,
    /// When the most recent cycle **started**, epoch ms — set on every cycle,
    /// success or failure alike, so "it tried and keeps failing" is
    /// distinguishable from "it has not run since the daemon came up".
    pub last_attempt_at: Option<i64>,
    /// When the most recent **successful** cycle finished, epoch ms.
    pub last_success_at: Option<i64>,
    /// The generation the last successful cycle projected. Advisory: the
    /// generation may since have been retired by GC.
    pub last_generation_id: Option<String>,
    /// Consecutive failed cycles since the last success (`0` while healthy).
    pub consecutive_failures: u32,
    /// The most recent failure's human-readable cause, cleared on success.
    pub last_error: Option<String>,
    /// When this row was last written, epoch ms.
    pub updated_at: i64,
}

/// The outcome of one indexing cycle, as the caller already computed it.
///
/// Every field is supplied by the caller rather than derived in SQL — notably
/// `consecutive_failures`, which the daemon's in-memory `WorktreeTaskStatus`
/// already maintains. Writing the computed value (instead of an
/// `x = x + 1` increment) is what makes [`write_indexing_status`] idempotent:
/// replaying the same cycle's outcome, as a crash-retry may, leaves the row
/// identical instead of inflating the counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexingOutcome<'a> {
    /// When this cycle started, epoch ms.
    pub attempt_at: i64,
    /// `Some` with the projected generation when the cycle succeeded, `None`
    /// when it failed.
    pub success: Option<&'a str>,
    /// The caller's running count of consecutive failures (`0` on success).
    pub consecutive_failures: u32,
    /// The failure cause, `None` on success.
    pub last_error: Option<&'a str>,
}

/// Record one cycle's outcome for `worktree_id` (spec 03 §2.1).
///
/// A full-row upsert, never a read-modify-write: the caller owns the running
/// counters, so this is a pure mirror of the in-memory status and is safe to
/// replay. On success `last_success_at`/`last_generation_id` advance and
/// `last_error` clears; on failure both success fields keep their previous
/// values, so "last known good" survives an arbitrarily long failure streak —
/// which is exactly what a stale-index warning needs to read.
///
/// An unknown `worktree_id` is rejected by the foreign key (a
/// [`ConstraintViolation`](rusqlite::ErrorCode::ConstraintViolation) that rolls
/// the transaction back), so a status row can never dangle.
pub fn write_indexing_status(
    tx: &Transaction<'_>,
    worktree_id: &str,
    outcome: IndexingOutcome<'_>,
    now_ms: i64,
) -> rusqlite::Result<()> {
    let success_at = outcome.success.map(|_| now_ms);
    tx.execute(
        "INSERT INTO worktree_indexing_status \
           (worktree_id, last_attempt_at, last_success_at, last_generation_id, \
            consecutive_failures, last_error, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
         ON CONFLICT(worktree_id) DO UPDATE SET \
           last_attempt_at      = ?2, \
           last_success_at      = COALESCE(?3, last_success_at), \
           last_generation_id   = COALESCE(?4, last_generation_id), \
           consecutive_failures = ?5, \
           last_error           = ?6, \
           updated_at           = ?7",
        params![
            worktree_id,
            outcome.attempt_at,
            success_at,
            outcome.success,
            outcome.consecutive_failures,
            outcome.last_error,
            now_ms,
        ],
    )?;
    Ok(())
}

/// One worktree's status, or `None` if it has never completed a cycle.
pub fn indexing_status(
    conn: &Connection,
    worktree_id: &str,
) -> rusqlite::Result<Option<WorktreeIndexingStatus>> {
    conn.query_row(
        "SELECT worktree_id, last_attempt_at, last_success_at, last_generation_id, \
                consecutive_failures, last_error, updated_at \
         FROM worktree_indexing_status WHERE worktree_id = ?1",
        params![worktree_id],
        row_to_status,
    )
    .optional()
}

/// Every worktree's status, ascending by `worktree_id` — the deterministic
/// order `local-rag project list`/`status` join against `managed_worktrees`.
pub fn indexing_statuses(conn: &Connection) -> rusqlite::Result<Vec<WorktreeIndexingStatus>> {
    let mut stmt = conn.prepare(
        "SELECT worktree_id, last_attempt_at, last_success_at, last_generation_id, \
                consecutive_failures, last_error, updated_at \
         FROM worktree_indexing_status ORDER BY worktree_id",
    )?;
    let rows = stmt.query_map([], row_to_status)?;
    rows.collect()
}

fn row_to_status(r: &rusqlite::Row<'_>) -> rusqlite::Result<WorktreeIndexingStatus> {
    Ok(WorktreeIndexingStatus {
        worktree_id: r.get(0)?,
        last_attempt_at: r.get(1)?,
        last_success_at: r.get(2)?,
        last_generation_id: r.get(3)?,
        consecutive_failures: r.get::<_, i64>(4)? as u32,
        last_error: r.get(5)?,
        updated_at: r.get(6)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use local_rag_test_support::TempHome;

    /// A bare `worktree` foreign-key parent plus this module's own table — the
    /// minimum `SCHEMA_V13` needs — with foreign keys ON, as `state::open` sets
    /// them on every real `state.sqlite` connection.
    fn conn_with_status() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        conn.execute_batch("CREATE TABLE worktree (worktree_id TEXT PRIMARY KEY);")
            .unwrap();
        conn.execute_batch(SCHEMA_V13).unwrap();
        conn.execute_batch("INSERT INTO worktree (worktree_id) VALUES ('wt-a'),('wt-b');")
            .unwrap();
        conn
    }

    fn write(conn: &mut Connection, wt: &str, outcome: IndexingOutcome<'_>, now: i64) {
        let tx = conn.transaction().unwrap();
        write_indexing_status(&tx, wt, outcome, now).unwrap();
        tx.commit().unwrap();
    }

    fn success(attempt_at: i64, generation: &str) -> IndexingOutcome<'_> {
        IndexingOutcome {
            attempt_at,
            success: Some(generation),
            consecutive_failures: 0,
            last_error: None,
        }
    }

    fn failure(attempt_at: i64, failures: u32, error: &str) -> IndexingOutcome<'_> {
        IndexingOutcome {
            attempt_at,
            success: None,
            consecutive_failures: failures,
            last_error: Some(error),
        }
    }

    #[test]
    fn a_worktree_with_no_cycle_yet_has_no_row() {
        let conn = conn_with_status();
        assert_eq!(indexing_status(&conn, "wt-a").unwrap(), None);
        assert!(indexing_statuses(&conn).unwrap().is_empty());
    }

    #[test]
    fn a_successful_cycle_records_both_clocks_and_the_generation() {
        let mut conn = conn_with_status();
        write(&mut conn, "wt-a", success(1000, "gen-1"), 1500);

        let row = indexing_status(&conn, "wt-a").unwrap().unwrap();
        assert_eq!(
            row,
            WorktreeIndexingStatus {
                worktree_id: "wt-a".to_string(),
                last_attempt_at: Some(1000),
                last_success_at: Some(1500),
                last_generation_id: Some("gen-1".to_string()),
                consecutive_failures: 0,
                last_error: None,
                updated_at: 1500,
            }
        );
    }

    /// The whole point of the table: the row is read back from a *different*
    /// connection, i.e. it outlives the process that wrote it — which the
    /// in-memory `WorktreeTaskStatus` never did.
    #[test]
    fn the_status_survives_reopening_the_store() {
        let home = TempHome::new().expect("temp home");
        let path = home.join("state.sqlite");
        {
            let mut conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "foreign_keys", true).unwrap();
            conn.execute_batch("CREATE TABLE worktree (worktree_id TEXT PRIMARY KEY);")
                .unwrap();
            conn.execute_batch(SCHEMA_V13).unwrap();
            conn.execute_batch("INSERT INTO worktree (worktree_id) VALUES ('wt-a');")
                .unwrap();
            write(&mut conn, "wt-a", success(1000, "gen-1"), 1500);
        }
        let reopened = Connection::open(&path).unwrap();
        let row = indexing_status(&reopened, "wt-a").unwrap().unwrap();
        assert_eq!(row.last_success_at, Some(1500));
        assert_eq!(row.last_generation_id.as_deref(), Some("gen-1"));
    }

    #[test]
    fn a_failure_keeps_the_last_known_good_and_records_the_cause() {
        let mut conn = conn_with_status();
        write(&mut conn, "wt-a", success(1000, "gen-1"), 1500);
        write(&mut conn, "wt-a", failure(2000, 1, "disk on fire"), 2100);

        let row = indexing_status(&conn, "wt-a").unwrap().unwrap();
        assert_eq!(row.last_attempt_at, Some(2000), "the attempt advances");
        assert_eq!(
            row.last_success_at,
            Some(1500),
            "last known good survives the failure",
        );
        assert_eq!(
            row.last_generation_id.as_deref(),
            Some("gen-1"),
            "so does the generation it produced",
        );
        assert_eq!(row.consecutive_failures, 1);
        assert_eq!(row.last_error.as_deref(), Some("disk on fire"));
    }

    #[test]
    fn a_success_after_failures_clears_the_counter_and_the_error() {
        let mut conn = conn_with_status();
        write(&mut conn, "wt-a", failure(1000, 1, "boom"), 1100);
        write(&mut conn, "wt-a", failure(2000, 2, "boom again"), 2100);
        write(&mut conn, "wt-a", success(3000, "gen-7"), 3100);

        let row = indexing_status(&conn, "wt-a").unwrap().unwrap();
        assert_eq!(row.consecutive_failures, 0);
        assert_eq!(row.last_error, None);
        assert_eq!(row.last_success_at, Some(3100));
        assert_eq!(row.last_generation_id.as_deref(), Some("gen-7"));
    }

    /// Replaying one cycle's outcome (a crash-retry, or a second call from a
    /// caller that already wrote) must leave the row identical — the reason
    /// this layer writes computed values instead of incrementing in SQL.
    #[test]
    fn replaying_the_same_outcome_is_idempotent() {
        let mut conn = conn_with_status();
        write(&mut conn, "wt-a", failure(2000, 3, "same"), 2100);
        let first = indexing_status(&conn, "wt-a").unwrap().unwrap();
        write(&mut conn, "wt-a", failure(2000, 3, "same"), 2100);
        let second = indexing_status(&conn, "wt-a").unwrap().unwrap();

        assert_eq!(first, second, "a replayed write must not inflate anything");
        assert_eq!(indexing_statuses(&conn).unwrap().len(), 1);
    }

    #[test]
    fn statuses_are_returned_ascending_by_worktree_id() {
        let mut conn = conn_with_status();
        write(&mut conn, "wt-b", success(1000, "gen-b"), 1100);
        write(&mut conn, "wt-a", success(1000, "gen-a"), 1100);

        let ids: Vec<String> = indexing_statuses(&conn)
            .unwrap()
            .into_iter()
            .map(|s| s.worktree_id)
            .collect();
        assert_eq!(ids, vec!["wt-a".to_string(), "wt-b".to_string()]);
    }

    #[test]
    fn an_unknown_worktree_is_rejected_by_the_foreign_key() {
        let mut conn = conn_with_status();
        let tx = conn.transaction().unwrap();
        let err = write_indexing_status(&tx, "wt-nope", success(1000, "gen-1"), 1100).unwrap_err();
        assert!(
            matches!(
                err.sqlite_error_code(),
                Some(rusqlite::ErrorCode::ConstraintViolation)
            ),
            "expected a foreign-key violation, got {err:?}",
        );
    }
}
