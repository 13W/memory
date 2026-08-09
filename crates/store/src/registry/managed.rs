//! The daemon-managed indexing registry (`managed_worktree`, spec 03 §2.1,
//! ADR-0009) — T20-01.
//!
//! One row per worktree the daemon indexes in the background. This table is
//! the **truth**; a live daemon is only *notified* of a change
//! (`admin/projects_reload`, T20-07) and re-reads the table on a slow
//! backstop poll regardless — the same "notify is a hint, the table is
//! truth" discipline spec 06 §1 already fixes for the reconcile watcher
//! itself. Registration is always **explicit** (ADR-0009): nothing here
//! auto-enrolls a worktree because it happened to be indexed once.
//!
//! Keyed by `worktree_id`, the stable UUID — never a path (spec 01 §5's
//! system-wide invariant) — and the foreign key into `worktree` makes an
//! unknown id a [`ConstraintViolation`](rusqlite::ErrorCode::ConstraintViolation)
//! that rolls the transaction back, never a dangling enrollment.
//!
//! Why a table of its own, rather than any existing home (ADR-0009's
//! rejected alternatives, recorded here because the reasons are normative):
//!
//! - **not a `repo_settings` key** — wrong granularity (repository, not
//!   worktree), and spec 02 §3.2 defines that table as the mirror of the
//!   global `[models]`/`[index]` config sections, not as a work queue;
//! - **not `worktree.state`** — spec 04 §7's `active|detached|removing`
//!   machine answers "does this path still resolve", an orthogonal axis;
//!   conflating them would make "the user paused indexing" indistinguishable
//!   from "the path vanished" and would require editing `[SPEC]` transitions;
//! - **not a JSON blob in `store_settings`** — bootstrap framework storage
//!   for singletons; a blob has no foreign key, no per-row query, and one
//!   toggle would rewrite the whole value.
//!
//! The table carries **no runtime columns** (`running`, `last_error`): those
//! are in-memory supervisor state (T20-05/T20-06) surfaced by
//! `admin/projects_list` (T20-07), never persisted. Like every other registry
//! primitive, writes take a [`Transaction`] so they compose inside one
//! [`StateWriter::transaction`](crate::StateWriter::transaction) closure —
//! enrolling a brand-new path is *one* transaction alongside
//! [`create_repository`](super::create_repository)/
//! [`create_worktree`](super::create_worktree) — and reads take a
//! [`Connection`] so they run on a read-only connection. Every operation
//! returns [`rusqlite::Result`]; there is no typed error enum at this layer,
//! matching [`settings`](super::settings) and [`repository`](super::repository).
//!
//! Consumers are deliberately out of this task's scope: the daemon
//! supervisor is T20-06, the `local-rag project` CLI is T20-08, and the
//! double-indexing advisory is T20-09.

use rusqlite::{Connection, OptionalExtension, Transaction, params};

/// Version-10 migration DDL: the daemon-managed indexing registry (spec 03
/// §2.1, ADR-0009).
///
/// Byte-exact reproduction of the `state.sqlite` §2.1 `managed_worktree`
/// block. Referenced by [`crate::migrate::ALL`] as migration version 10.
///
/// The table is a pure additive leaf: it creates no circular foreign key
/// (only `managed_worktree → worktree`, one direction) and touches no
/// existing row, so it needs no backfill — unlike `SCHEMA_V5`, whose `ALTER
/// TABLE` had existing rows to repair. `enabled` follows spec 03 §1.1's
/// boolean convention (`INTEGER` 0/1 with `CHECK (x IN (0,1))`), the same
/// shape `model_space_representation.required` already uses.
///
/// **Frozen once shipped.** Like the earlier `SCHEMA_V*` constants, the
/// checksum is the SHA-256 of this text (see
/// [`crate::migrate::Migration::checksum`]); any edit — even whitespace or a
/// comment — changes the checksum and trips
/// [`ChecksumDrift`](crate::migrate::MigrationError::ChecksumDrift) on an
/// existing store. Future schema changes are new numbered migrations, never
/// an edit here.
pub(crate) const SCHEMA_V10: &str = "\
CREATE TABLE managed_worktree (                        -- migration 10; ADR-0009 opt-in list [SPEC]
  worktree_id    TEXT PRIMARY KEY REFERENCES worktree(worktree_id),
  enabled        INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0,1)),
  registered_at  INTEGER NOT NULL,
  updated_at     INTEGER NOT NULL
);
-- Keyed by the stable worktree UUID, never a path [FIXED]. No runtime columns
-- (running/last_error): supervisor state is in-memory, surfaced by admin/projects_list.
";

/// One enrolled worktree's durable row (spec 03 §2.1 `managed_worktree`).
///
/// Deliberately a plain data row with no runtime fields and no clock of its
/// own: live status (`running`, `last_error`) belongs to the in-memory
/// supervisor (T20-06) and is joined onto this shape by `admin/projects_list`
/// (T20-07), never stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedWorktree {
    /// The enrolled worktree's stable id (`worktree.worktree_id`, a UUIDv7 —
    /// never path-derived, spec 01 §5).
    pub worktree_id: String,
    /// Whether the daemon brings a background indexing task up for it at
    /// start. `false` keeps the row enrolled but **dormant** — a paused
    /// project, not an unregistered one.
    pub enabled: bool,
    /// When it was first enrolled, epoch ms. Never restamped by a repeated
    /// [`register_managed_worktree`].
    pub registered_at: i64,
    /// When this row was last **written** — enrollment or an enable/disable
    /// toggle — epoch ms. Unlike `worktree.state_changed_at` (D-007) nothing
    /// measures a grace budget from it, so a repeated idempotent write may
    /// restamp it harmlessly.
    pub updated_at: i64,
}

/// Enroll `worktree_id` in daemon-managed background indexing (spec 03 §2.1,
/// ADR-0009).
///
/// Idempotent upsert: a repeat leaves a single row and bumps `updated_at`
/// only. It deliberately does **not** reset `registered_at` (the first
/// enrollment is a durable fact) and does **not** re-enable a row a user
/// deliberately disabled — enabling is its own verb
/// ([`set_managed_enabled`]), so `local-rag project add` on an
/// already-paused project cannot silently resume background indexing.
///
/// New rows are born `enabled = 1`: enrollment is an explicit user act, so
/// the only sensible initial state is "start indexing it".
///
/// An unknown `worktree_id` is rejected by the `managed_worktree → worktree`
/// foreign key (a
/// [`ConstraintViolation`](rusqlite::ErrorCode::ConstraintViolation) that
/// rolls the transaction back), so an enrollment can never dangle.
pub fn register_managed_worktree(
    tx: &Transaction<'_>,
    worktree_id: &str,
    now_ms: i64,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO managed_worktree (worktree_id, enabled, registered_at, updated_at) \
         VALUES (?1, 1, ?2, ?2) \
         ON CONFLICT(worktree_id) DO UPDATE SET updated_at = ?2",
        params![worktree_id, now_ms],
    )?;
    Ok(())
}

/// Remove `worktree_id` from the managed registry, returning whether a row
/// was removed (spec 03 §2.1).
///
/// Unmanaging **only**: the worktree, its generations, and its index are
/// untouched (`local-rag project remove`'s own contract, spec 11 §8).
/// Idempotent — a second call is `Ok(false)`, never an error — and the
/// `bool` lets the CLI distinguish "unenrolled" from "was never enrolled"
/// without a second read.
pub fn unregister_managed_worktree(
    tx: &Transaction<'_>,
    worktree_id: &str,
) -> rusqlite::Result<bool> {
    let removed = tx.execute(
        "DELETE FROM managed_worktree WHERE worktree_id = ?1",
        params![worktree_id],
    )?;
    Ok(removed > 0)
}

/// Pause or resume background indexing for an already-enrolled worktree,
/// returning whether a managed row matched (spec 03 §2.1).
///
/// An `UPDATE`, deliberately **not** an upsert: registration is explicit
/// (ADR-0009), so toggling a worktree that was never registered must not
/// implicitly enroll it — it writes nothing and returns `false`, which is
/// what `local-rag project enable|disable` reports as "not a managed
/// project".
///
/// `updated_at` records the last *write*, not the last *change*: re-issuing
/// the value a row already holds restamps it. That is safe here precisely
/// because — unlike `worktree.state_changed_at` (D-007, whose
/// self-transition no-op is load-bearing for spec 05 §8's destruction
/// deadline) — nothing measures a budget from this column.
pub fn set_managed_enabled(
    tx: &Transaction<'_>,
    worktree_id: &str,
    enabled: bool,
    now_ms: i64,
) -> rusqlite::Result<bool> {
    let updated = tx.execute(
        "UPDATE managed_worktree SET enabled = ?2, updated_at = ?3 WHERE worktree_id = ?1",
        params![worktree_id, i64::from(enabled), now_ms],
    )?;
    Ok(updated > 0)
}

/// Every enrolled worktree, ascending by `worktree_id` (spec 03 §2.1).
///
/// Returns **all** rows, disabled ones included, so the "should a task run
/// for this?" decision lives in one place in the supervisor (T20-06) rather
/// than being split between a SQL `WHERE` clause and Rust — the same
/// discipline [`worktree_state_clocks`](super::worktree_state_clocks)
/// established for the shard sweep. Ascending order makes the result
/// deterministic, so a supervisor reload diff and a `local-rag project list
/// --json` snapshot are both stable.
pub fn managed_worktrees(conn: &Connection) -> rusqlite::Result<Vec<ManagedWorktree>> {
    let mut stmt = conn.prepare(
        "SELECT worktree_id, enabled, registered_at, updated_at FROM managed_worktree \
         ORDER BY worktree_id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(ManagedWorktree {
            worktree_id: r.get(0)?,
            enabled: r.get::<_, i64>(1)? != 0,
            registered_at: r.get(2)?,
            updated_at: r.get(3)?,
        })
    })?;
    rows.collect()
}

/// Whether `worktree_id` is enrolled in the managed registry at all —
/// regardless of `enabled` (spec 03 §2.1).
///
/// "Enrolled", not "currently indexing": a paused project is still
/// daemon-managed territory, which is exactly the question the
/// double-indexing advisory asks before printing its `local-rag project
/// reindex` hint (T20-09, spec 11 §6). Callers that need the
/// enabled/disabled axis read [`managed_worktrees`].
pub fn is_managed(conn: &Connection, worktree_id: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT 1 FROM managed_worktree WHERE worktree_id = ?1",
        params![worktree_id],
        |_| Ok(()),
    )
    .optional()
    .map(|row| row.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bare `worktree` foreign-key parent plus this module's own table —
    /// the minimum `SCHEMA_V10` needs — with foreign keys ON, as
    /// `state::open` sets them on every real `state.sqlite` connection.
    fn conn_with_managed() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        conn.execute_batch("CREATE TABLE worktree (worktree_id TEXT PRIMARY KEY);")
            .unwrap();
        conn.execute_batch(SCHEMA_V10).unwrap();
        conn.execute_batch("INSERT INTO worktree (worktree_id) VALUES ('wt-a'),('wt-b'),('wt-c');")
            .unwrap();
        conn
    }

    #[test]
    fn register_is_born_enabled_and_stamps_both_clocks() {
        let mut conn = conn_with_managed();
        let tx = conn.transaction().unwrap();
        register_managed_worktree(&tx, "wt-a", 1000).unwrap();
        tx.commit().unwrap();

        let rows = managed_worktrees(&conn).unwrap();
        assert_eq!(
            rows,
            vec![ManagedWorktree {
                worktree_id: "wt-a".to_string(),
                enabled: true,
                registered_at: 1000,
                updated_at: 1000,
            }]
        );
        assert!(is_managed(&conn, "wt-a").unwrap());
    }

    #[test]
    fn repeated_register_keeps_one_row_and_bumps_only_updated_at() {
        let mut conn = conn_with_managed();
        {
            let tx = conn.transaction().unwrap();
            register_managed_worktree(&tx, "wt-a", 1000).unwrap();
            tx.commit().unwrap();
        }
        {
            let tx = conn.transaction().unwrap();
            register_managed_worktree(&tx, "wt-a", 2000).unwrap();
            tx.commit().unwrap();
        }

        let rows = managed_worktrees(&conn).unwrap();
        assert_eq!(
            rows.len(),
            1,
            "a repeated register must not duplicate the row"
        );
        assert_eq!(rows[0].registered_at, 1000, "first enrollment is durable");
        assert_eq!(rows[0].updated_at, 2000, "the latest write wins");
    }

    #[test]
    fn register_never_re_enables_a_disabled_row() {
        let mut conn = conn_with_managed();
        {
            let tx = conn.transaction().unwrap();
            register_managed_worktree(&tx, "wt-a", 1000).unwrap();
            tx.commit().unwrap();
        }
        {
            let tx = conn.transaction().unwrap();
            set_managed_enabled(&tx, "wt-a", false, 2000).unwrap();
            tx.commit().unwrap();
        }
        {
            let tx = conn.transaction().unwrap();
            register_managed_worktree(&tx, "wt-a", 3000).unwrap();
            tx.commit().unwrap();
        }

        let rows = managed_worktrees(&conn).unwrap();
        assert!(
            !rows[0].enabled,
            "the ON CONFLICT clause touches updated_at only, never enabled"
        );
    }

    #[test]
    fn disabled_rows_are_returned_by_the_reader() {
        let mut conn = conn_with_managed();
        let tx = conn.transaction().unwrap();
        register_managed_worktree(&tx, "wt-a", 1000).unwrap();
        set_managed_enabled(&tx, "wt-a", false, 2000).unwrap();
        tx.commit().unwrap();

        let rows = managed_worktrees(&conn).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].enabled);
        assert!(is_managed(&conn, "wt-a").unwrap());
    }

    #[test]
    fn set_enabled_reports_whether_a_row_matched() {
        let mut conn = conn_with_managed();
        let tx = conn.transaction().unwrap();
        register_managed_worktree(&tx, "wt-a", 1000).unwrap();
        assert!(set_managed_enabled(&tx, "wt-a", false, 2000).unwrap());
        assert!(
            !set_managed_enabled(&tx, "wt-c", false, 2000).unwrap(),
            "wt-c was never registered"
        );
        tx.commit().unwrap();

        assert!(
            managed_worktrees(&conn)
                .unwrap()
                .iter()
                .all(|r| r.worktree_id != "wt-c"),
            "toggling an unregistered worktree must not implicitly enroll it"
        );
    }

    #[test]
    fn unregister_is_idempotent_and_reports_removal() {
        let mut conn = conn_with_managed();
        let tx = conn.transaction().unwrap();
        register_managed_worktree(&tx, "wt-a", 1000).unwrap();
        register_managed_worktree(&tx, "wt-b", 1000).unwrap();
        assert!(unregister_managed_worktree(&tx, "wt-a").unwrap());
        assert!(!unregister_managed_worktree(&tx, "wt-a").unwrap());
        tx.commit().unwrap();

        assert!(!is_managed(&conn, "wt-a").unwrap());
        assert!(is_managed(&conn, "wt-b").unwrap(), "sibling row untouched");
    }

    #[test]
    fn listing_is_ordered_by_worktree_id() {
        let mut conn = conn_with_managed();
        let tx = conn.transaction().unwrap();
        for id in ["wt-c", "wt-a", "wt-b"] {
            register_managed_worktree(&tx, id, 1000).unwrap();
        }
        tx.commit().unwrap();

        let rows = managed_worktrees(&conn).unwrap();
        let ids: Vec<&str> = rows.iter().map(|r| r.worktree_id.as_str()).collect();
        assert_eq!(ids, vec!["wt-a", "wt-b", "wt-c"]);
    }

    #[test]
    fn an_unknown_worktree_id_is_rejected_by_the_foreign_key() {
        let mut conn = conn_with_managed();
        let tx = conn.transaction().unwrap();
        let result = register_managed_worktree(&tx, "ghost", 1000);
        let err = result.expect_err("an unknown worktree_id must be rejected by the FK");
        assert_eq!(
            err.sqlite_error_code(),
            Some(rusqlite::ErrorCode::ConstraintViolation),
        );
    }

    #[test]
    fn enabled_rejects_a_value_outside_the_boolean_domain() {
        let conn = conn_with_managed();
        conn.execute_batch(
            "INSERT INTO managed_worktree (worktree_id, registered_at, updated_at) \
             VALUES ('wt-a', 1000, 1000);",
        )
        .unwrap();
        let result = conn.execute(
            "UPDATE managed_worktree SET enabled = 7 WHERE worktree_id = 'wt-a'",
            [],
        );
        assert!(
            result.is_err(),
            "the §1.1 boolean CHECK must reject a non-0/1 value"
        );
    }
}
