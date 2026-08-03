//! `purge --memory <id>|--session <id>|--all` (spec 08 §3, 12 §3, T16-02) —
//! the only hard-delete path this crate exposes. See `super`'s module doc for
//! the tombstone semantics and the one-transaction-for-`--all` rationale.

use rusqlite::{Connection, Transaction, params};

use crate::memory::{
    Actor, NewAuditEvent, all_memory_entry_ids, insert_audit_event, memory_entry_by_id,
};
use crate::observation::all_session_ids;

const ENTITY_KIND_MEMORY_ENTRY: &str = "memory_entry";

/// Why [`purge_memory`] refused. Both variants mirror
/// `crate::memory::MemoryOpError`'s identically-named variants — the same
/// "expected_version surfaced" contract the CLI's `--expected-version` flag
/// already gives `memory edit`/`retract`/`merge`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PurgeMemoryError {
    UnknownMemory,
    OptimisticConflict { expected: i64, actual: i64 },
}

/// What one memory entry's purge actually did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PurgeMemoryReport {
    pub descendants_relinked: u64,
    pub evidence_rows_removed: u64,
    pub audit_rows_tombstoned: u64,
}

/// Hard-delete `memory_id` (spec 08 §3 "hard removal exists only as an
/// explicit privacy purge, which also rewrites audit references to
/// tombstones"): relinks any descendant whose `supersedes_id` pointed at it,
/// deletes its `memory_evidence` rows, deletes the row itself, tombstones
/// every prior `audit_event` payload for it, and appends a terminal
/// `op = "purge"` audit row (see `super`'s module doc for why both the
/// tombstone rewrite and the new row exist). Refuses with no mutation on an
/// unknown id or a stale `expected_version`.
pub fn purge_memory(
    tx: &Transaction<'_>,
    memory_id: &str,
    expected_version: i64,
    now_ms: i64,
) -> rusqlite::Result<Result<PurgeMemoryReport, PurgeMemoryError>> {
    let Some(entry) = memory_entry_by_id(tx, memory_id)? else {
        return Ok(Err(PurgeMemoryError::UnknownMemory));
    };
    if entry.entry_version != expected_version {
        return Ok(Err(PurgeMemoryError::OptimisticConflict {
            expected: expected_version,
            actual: entry.entry_version,
        }));
    }
    let report = purge_memory_rows(tx, memory_id, entry.entry_version, now_ms)?;
    Ok(Ok(report))
}

/// The shared steps behind both [`purge_memory`] (version-checked, one entry)
/// and [`purge_all`] (unconditional, every entry) — see `super`'s module doc
/// for the tombstone-then-marker rationale. `current_version` is the caller's
/// already-confirmed version (checked by `purge_memory`, read fresh per
/// iteration by `purge_all`); this function performs no version check of its
/// own.
fn purge_memory_rows(
    tx: &Transaction<'_>,
    memory_id: &str,
    current_version: i64,
    now_ms: i64,
) -> rusqlite::Result<PurgeMemoryReport> {
    let descendants_relinked = tx.execute(
        "UPDATE memory_entry SET supersedes_id = NULL WHERE supersedes_id = ?1",
        params![memory_id],
    )?;
    let evidence_rows_removed = tx.execute(
        "DELETE FROM memory_evidence WHERE memory_id = ?1",
        params![memory_id],
    )?;
    tx.execute(
        "DELETE FROM memory_entry WHERE memory_id = ?1",
        params![memory_id],
    )?;
    let audit_rows_tombstoned = tx.execute(
        "UPDATE audit_event SET payload = NULL WHERE entity_kind = ?1 AND entity_id = ?2",
        params![ENTITY_KIND_MEMORY_ENTRY, memory_id],
    )?;

    #[cfg(feature = "failpoints")]
    local_rag_test_support::fail_point!(
        "privacy.purge.memory.before_final_audit",
        Err(rusqlite::Error::ToSqlConversionFailure(
            "failpoint: privacy.purge.memory.before_final_audit".into()
        ))
    );

    insert_audit_event(
        tx,
        &NewAuditEvent {
            entity_kind: ENTITY_KIND_MEMORY_ENTRY,
            entity_id: memory_id,
            entity_version: current_version + 1,
            op: "purge",
            actor: Actor::User,
            idempotency_key: None,
            payload: None,
        },
        now_ms,
    )?;

    Ok(PurgeMemoryReport {
        descendants_relinked: descendants_relinked as u64,
        evidence_rows_removed: evidence_rows_removed as u64,
        audit_rows_tombstoned: audit_rows_tombstoned as u64,
    })
}

/// What one session's purge actually did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PurgeSessionReport {
    pub observations_purged: u64,
    pub candidate_evidence_rows_removed: u64,
    pub memory_evidence_rows_removed: u64,
}

/// Hard-delete every `observation_envelope` (and its cascaded `path`/
/// `payload` rows) for `session_id`. An unknown/already-empty `session_id` is
/// a harmless no-op report, not an error — there is no "does this session
/// exist" precondition to violate, unlike a specific `memory_id`. See
/// `super`'s module doc for the accepted "evidence-less candidate/entry"
/// limitation and why `audit_event` is never touched here.
pub fn purge_session(
    tx: &Transaction<'_>,
    session_id: &str,
) -> rusqlite::Result<PurgeSessionReport> {
    let candidate_evidence_rows_removed = tx.execute(
        "DELETE FROM candidate_evidence WHERE observation_id IN \
           (SELECT observation_id FROM observation_envelope WHERE session_id = ?1)",
        params![session_id],
    )?;
    let memory_evidence_rows_removed = tx.execute(
        "DELETE FROM memory_evidence WHERE observation_id IN \
           (SELECT observation_id FROM observation_envelope WHERE session_id = ?1)",
        params![session_id],
    )?;

    #[cfg(feature = "failpoints")]
    local_rag_test_support::fail_point!(
        "privacy.purge.session.before_envelope_delete",
        Err(rusqlite::Error::ToSqlConversionFailure(
            "failpoint: privacy.purge.session.before_envelope_delete".into()
        ))
    );

    let observations_purged = tx.execute(
        "DELETE FROM observation_envelope WHERE session_id = ?1",
        params![session_id],
    )?;

    Ok(PurgeSessionReport {
        observations_purged: observations_purged as u64,
        candidate_evidence_rows_removed: candidate_evidence_rows_removed as u64,
        memory_evidence_rows_removed: memory_evidence_rows_removed as u64,
    })
}

/// What `purge --all` actually did, summed over every memory entry and
/// session it touched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PurgeAllReport {
    pub memory_entries_purged: u64,
    pub sessions_purged: u64,
    pub observations_purged: u64,
}

/// Unconditionally purge every `memory_entry` and every session's
/// observations, in the caller's single transaction (see `super`'s module
/// doc: this is a deliberate atomicity choice, not an unbatched oversight).
/// Iteration order over the two id sets is inconsequential — relinking a
/// descendant's `supersedes_id` never removes a row, so every id
/// `all_memory_entry_ids`/`all_session_ids` names up front is still present
/// when its own turn comes.
pub fn purge_all(tx: &Transaction<'_>, now_ms: i64) -> rusqlite::Result<PurgeAllReport> {
    let mut memory_entries_purged = 0u64;
    for memory_id in all_memory_entry_ids(tx)? {
        let Some(entry) = memory_entry_by_id(tx, &memory_id)? else {
            continue;
        };
        purge_memory_rows(tx, &memory_id, entry.entry_version, now_ms)?;
        memory_entries_purged += 1;
    }

    let mut sessions_purged = 0u64;
    let mut observations_purged = 0u64;
    for session_id in all_session_ids(tx)? {
        let report = purge_session(tx, &session_id)?;
        observations_purged += report.observations_purged;
        sessions_purged += 1;
    }

    Ok(PurgeAllReport {
        memory_entries_purged,
        sessions_purged,
        observations_purged,
    })
}

// ---------------------------------------------------------------------------
// Read-only previews: the CLI's `--yes` confirmation UX computes and prints
// one of these *before* refusing an unconfirmed purge, so the operator sees
// exactly what would be removed without any mutation happening.
// ---------------------------------------------------------------------------

/// What `purge --memory <id>` would do, without doing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PurgeMemoryPreview {
    pub exists: bool,
    pub current_version: Option<i64>,
    pub evidence_rows: u64,
    pub descendant_rows: u64,
}

pub fn preview_purge_memory(
    conn: &Connection,
    memory_id: &str,
) -> rusqlite::Result<PurgeMemoryPreview> {
    let Some(entry) = memory_entry_by_id(conn, memory_id)? else {
        return Ok(PurgeMemoryPreview::default());
    };
    let evidence_rows: i64 = conn.query_row(
        "SELECT COUNT(*) FROM memory_evidence WHERE memory_id = ?1",
        params![memory_id],
        |r| r.get(0),
    )?;
    let descendant_rows: i64 = conn.query_row(
        "SELECT COUNT(*) FROM memory_entry WHERE supersedes_id = ?1",
        params![memory_id],
        |r| r.get(0),
    )?;
    Ok(PurgeMemoryPreview {
        exists: true,
        current_version: Some(entry.entry_version),
        evidence_rows: evidence_rows as u64,
        descendant_rows: descendant_rows as u64,
    })
}

/// What `purge --session <id>` would do, without doing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PurgeSessionPreview {
    pub observations: u64,
}

pub fn preview_purge_session(
    conn: &Connection,
    session_id: &str,
) -> rusqlite::Result<PurgeSessionPreview> {
    let observations: i64 = conn.query_row(
        "SELECT COUNT(*) FROM observation_envelope WHERE session_id = ?1",
        params![session_id],
        |r| r.get(0),
    )?;
    Ok(PurgeSessionPreview {
        observations: observations as u64,
    })
}

/// What `purge --all` would do, without doing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PurgeAllPreview {
    pub memory_entries: u64,
    pub sessions: u64,
    pub observations: u64,
}

pub fn preview_purge_all(conn: &Connection) -> rusqlite::Result<PurgeAllPreview> {
    let memory_entries: i64 =
        conn.query_row("SELECT COUNT(*) FROM memory_entry", [], |r| r.get(0))?;
    let sessions: i64 = conn.query_row(
        "SELECT COUNT(DISTINCT session_id) FROM observation_envelope",
        [],
        |r| r.get(0),
    )?;
    let observations: i64 =
        conn.query_row("SELECT COUNT(*) FROM observation_envelope", [], |r| {
            r.get(0)
        })?;
    Ok(PurgeAllPreview {
        memory_entries: memory_entries as u64,
        sessions: sessions as u64,
        observations: observations as u64,
    })
}
