//! `audit_event`: the append-only trail every memory mutation writes to
//! (spec 03 §2.5, 08 §3). Plain insert/read — the atomic
//! mutation+evidence+audit+idempotency operation contract, and recognizing a
//! retried `idempotency_key` as already-applied, are T14-02's transactional
//! op engine.
//!
//! `entity_kind` and `op` are documented, open-ended value sets in the spec
//! (`-- memory_entry | candidate | …`, `-- create|reinforce|…`) with **no** SQL
//! `CHECK`, unlike `actor` — mirroring how `observation_envelope.event_type`
//! stays a plain `&str` in [`crate::observation::NewObservationEnvelope`] while
//! `evidence_kind`/`trust` (which *are* `CHECK`-backed) get typed mirrors. Only
//! `actor` gets one here, for the same reason.

use rusqlite::types::Type;
use rusqlite::{Connection, Error, Transaction, params};

/// `audit_event.actor` (spec 03 §2.5 CHECK domain).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Actor {
    User,
    Router,
    System,
}

impl Actor {
    pub fn as_str(self) -> &'static str {
        match self {
            Actor::User => "user",
            Actor::Router => "router",
            Actor::System => "system",
        }
    }

    /// Parse a stored value; `None` for anything the CHECK constraint forbids.
    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "user" => Some(Actor::User),
            "router" => Some(Actor::Router),
            "system" => Some(Actor::System),
            _ => None,
        }
    }
}

/// A new `audit_event` row, mirroring the DDL 1:1. `entity_kind`/`op` are
/// plain strings (see the module doc); `idempotency_key` is `Some` only for
/// router-originated ops (spec 08 §3).
#[derive(Debug, Clone, Copy)]
pub struct NewAuditEvent<'a> {
    pub entity_kind: &'a str,
    pub entity_id: &'a str,
    pub entity_version: i64,
    pub op: &'a str,
    pub actor: Actor,
    pub idempotency_key: Option<&'a str>,
    pub payload: Option<&'a str>,
}

/// One read-back `audit_event` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEventRow {
    pub audit_id: i64,
    pub entity_kind: String,
    pub entity_id: String,
    pub entity_version: i64,
    pub op: String,
    pub actor: Actor,
    pub idempotency_key: Option<String>,
    pub payload: Option<String>,
    pub created_at: i64,
}

/// Insert an `audit_event` row, returning its assigned `audit_id`. A conflict
/// on `UNIQUE (entity_kind, entity_id, entity_version)` or `UNIQUE
/// (idempotency_key)` surfaces as the natural `rusqlite::Error` constraint
/// violation — no special handling here; recognizing a retried
/// `idempotency_key` as already-applied (spec 08 §3) is T14-02's concern.
pub fn insert_audit_event(
    tx: &Transaction<'_>,
    row: &NewAuditEvent<'_>,
    now_ms: i64,
) -> rusqlite::Result<i64> {
    tx.query_row(
        "INSERT INTO audit_event \
           (entity_kind, entity_id, entity_version, op, actor, idempotency_key, payload, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
         RETURNING audit_id",
        params![
            row.entity_kind,
            row.entity_id,
            row.entity_version,
            row.op,
            row.actor.as_str(),
            row.idempotency_key,
            row.payload,
            now_ms,
        ],
        |r| r.get(0),
    )
}

/// Every `audit_event` row for `(entity_kind, entity_id)`, ascending by
/// `entity_version` (spec 03 §2.5). Used by `inspect_memory_evidence`-adjacent
/// review reads (11 §2, a later task) and by this task's own uniqueness tests.
pub fn read_audit_events_for_entity(
    conn: &Connection,
    entity_kind: &str,
    entity_id: &str,
) -> rusqlite::Result<Vec<AuditEventRow>> {
    let mut stmt = conn.prepare(
        "SELECT audit_id, entity_kind, entity_id, entity_version, op, actor, idempotency_key, \
                payload, created_at \
         FROM audit_event WHERE entity_kind = ?1 AND entity_id = ?2 \
         ORDER BY entity_version",
    )?;
    let rows = stmt.query_map(params![entity_kind, entity_id], |r| {
        let raw_actor: String = r.get(5)?;
        let actor = Actor::from_db(&raw_actor).ok_or_else(|| {
            Error::FromSqlConversionFailure(
                5,
                Type::Text,
                format!("invalid audit_event.actor {raw_actor:?}").into(),
            )
        })?;
        Ok(AuditEventRow {
            audit_id: r.get(0)?,
            entity_kind: r.get(1)?,
            entity_id: r.get(2)?,
            entity_version: r.get(3)?,
            op: r.get(4)?,
            actor,
            idempotency_key: r.get(6)?,
            payload: r.get(7)?,
            created_at: r.get(8)?,
        })
    })?;
    rows.collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actor_round_trips() {
        for actor in [Actor::User, Actor::Router, Actor::System] {
            assert_eq!(Actor::from_db(actor.as_str()), Some(actor));
        }
        assert_eq!(Actor::from_db("bogus"), None);
    }
}
