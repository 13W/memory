//! `memory_evidence`: provenance links from a `memory_entry` to the
//! `observation_envelope` rows that support it (spec 03 §2.5). Survives
//! payload TTL — the FK targets the durable envelope, never the short-lived
//! `observation_payload` (spec 12 §3). No state machine; plain insert/read.

use rusqlite::{Connection, Transaction, params};

use crate::observation::EvidenceKind;

/// A new `memory_evidence` row, mirroring the DDL 1:1.
#[derive(Debug, Clone, Copy)]
pub struct NewMemoryEvidence<'a> {
    pub memory_id: &'a str,
    pub observation_id: &'a str,
    pub evidence_kind: EvidenceKind,
    pub session_id: &'a str,
    pub agent_id: Option<&'a str>,
    pub commit_hash: Option<&'a str>,
}

/// Insert a `memory_evidence` row. An unknown `memory_id`/`observation_id` is
/// rejected by the composite FKs (the transaction rolls back); a duplicate
/// `(memory_id, observation_id)` pair by the primary key.
pub fn insert_memory_evidence(
    tx: &Transaction<'_>,
    row: &NewMemoryEvidence<'_>,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO memory_evidence \
           (memory_id, observation_id, evidence_kind, session_id, agent_id, commit_hash) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            row.memory_id,
            row.observation_id,
            row.evidence_kind.as_str(),
            row.session_id,
            row.agent_id,
            row.commit_hash,
        ],
    )?;
    Ok(())
}

/// Every `observation_id` linked as evidence for `memory_id`, ascending
/// (spec 03 §2.5). Used by `inspect_memory_evidence` (11 §2, a later task).
pub fn memory_evidence_for(conn: &Connection, memory_id: &str) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT observation_id FROM memory_evidence WHERE memory_id = ?1 ORDER BY observation_id",
    )?;
    let ids = stmt
        .query_map(params![memory_id], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(ids)
}
