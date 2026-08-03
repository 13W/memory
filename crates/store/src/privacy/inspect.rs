//! `inspect <observation|memory|generation> <id>` (spec 11 §6, T16-02) —
//! read-only, cross-table composition over the per-table full-row readers
//! (`crate::observation::observation_envelope_row`,
//! `crate::memory::memory_entry_by_id`, `crate::registry::generation_row`).
//! See the module doc in `super` for the placement rule.

use rusqlite::Connection;

use crate::memory::{
    AuditEventRow, MemoryEntryRow, memory_entry_by_id, memory_evidence_for,
    read_audit_events_for_entity,
};
use crate::observation::{
    EvidenceKind, PayloadStatus, TrustLevel, observation_envelope_row, observation_paths_for,
    observation_payload_status,
};
use crate::registry::{GenerationRow, generation_row};

/// `inspect observation <id>`'s result: the full envelope, every captured
/// path, and the payload's current TTL status (spec 03 §2.5). Fields are the
/// envelope's own columns flattened rather than wrapping the crate-internal
/// `ObservationEnvelopeRow` type, so this type can be `pub` without leaking a
/// `pub(crate)` type into a public interface.
#[derive(Debug, Clone, PartialEq)]
pub struct ObservationInspection {
    pub observation_id: String,
    pub source_event_id: String,
    pub dedup_key: Option<String>,
    pub payload_hash: String,
    pub event_type: String,
    pub evidence_kind: EvidenceKind,
    pub trust: TrustLevel,
    pub source_timestamp: Option<i64>,
    pub repo_id: Option<String>,
    pub worktree_id: Option<String>,
    pub session_id: String,
    pub agent_id: Option<String>,
    pub turn_id: Option<String>,
    pub batch_id: Option<String>,
    pub commit_hash: Option<String>,
    pub short_evidence_excerpt: Option<String>,
    pub redaction_version: Option<i64>,
    pub paths: Vec<String>,
    pub payload: PayloadStatus,
}

/// The full `observation_envelope` row for `observation_id`, its captured
/// paths, and its payload's TTL status as of `now_ms` — or `None` if the id
/// is unknown.
pub fn inspect_observation(
    conn: &Connection,
    observation_id: &str,
    now_ms: i64,
) -> rusqlite::Result<Option<ObservationInspection>> {
    let Some(envelope) = observation_envelope_row(conn, observation_id)? else {
        return Ok(None);
    };
    let paths = observation_paths_for(conn, observation_id)?;
    let payload = observation_payload_status(conn, observation_id, now_ms)?;
    Ok(Some(ObservationInspection {
        observation_id: envelope.observation_id,
        source_event_id: envelope.source_event_id,
        dedup_key: envelope.dedup_key,
        payload_hash: envelope.payload_hash,
        event_type: envelope.event_type,
        evidence_kind: envelope.evidence_kind,
        trust: envelope.trust,
        source_timestamp: envelope.source_timestamp,
        repo_id: envelope.repo_id,
        worktree_id: envelope.worktree_id,
        session_id: envelope.session_id,
        agent_id: envelope.agent_id,
        turn_id: envelope.turn_id,
        batch_id: envelope.batch_id,
        commit_hash: envelope.commit_hash,
        short_evidence_excerpt: envelope.short_evidence_excerpt,
        redaction_version: envelope.redaction_version,
        paths,
        payload,
    }))
}

/// One `memory_evidence` link resolved against its `observation_envelope` —
/// the shared shape `inspect_memory` and [`super::export::export_scope`] both
/// use, so export is never poorer than inspect for the identical
/// relationship.
#[derive(Debug, Clone, PartialEq)]
pub struct EvidenceSummary {
    pub observation_id: String,
    pub event_type: String,
    pub evidence_kind: EvidenceKind,
    pub trust: TrustLevel,
    pub session_id: String,
    pub source_timestamp: Option<i64>,
    pub short_evidence_excerpt: Option<String>,
    pub payload: PayloadStatus,
}

/// Every evidence observation linked to `memory_id`, ascending by
/// `observation_id` (the order [`memory_evidence_for`] already returns).
/// Silently skips an `observation_id` whose envelope has vanished — that can
/// only happen after a `purge --session` left the join row in place (see
/// `super`'s module doc on that accepted limitation); a caller that needs to
/// notice the gap can compare `len()` against `memory_evidence_for`'s own
/// count.
pub(crate) fn evidence_summaries_for(
    conn: &Connection,
    memory_id: &str,
    now_ms: i64,
) -> rusqlite::Result<Vec<EvidenceSummary>> {
    let mut out = Vec::new();
    for observation_id in memory_evidence_for(conn, memory_id)? {
        let Some(envelope) = observation_envelope_row(conn, &observation_id)? else {
            continue;
        };
        let payload = observation_payload_status(conn, &observation_id, now_ms)?;
        out.push(EvidenceSummary {
            observation_id: envelope.observation_id,
            event_type: envelope.event_type,
            evidence_kind: envelope.evidence_kind,
            trust: envelope.trust,
            session_id: envelope.session_id,
            source_timestamp: envelope.source_timestamp,
            short_evidence_excerpt: envelope.short_evidence_excerpt,
            payload,
        });
    }
    Ok(out)
}

/// `inspect memory <id>`'s result: the full entry row, its resolved evidence,
/// and its complete `audit_event` trail (spec 03 §2.5, 08 §3).
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryInspection {
    pub entry: MemoryEntryRow,
    pub evidence: Vec<EvidenceSummary>,
    pub audit_trail: Vec<AuditEventRow>,
}

/// The full `memory_entry` row for `memory_id`, its evidence, and its audit
/// trail — or `None` if the id is unknown. Unlike
/// [`crate::memory::active_entries_for_scope`], a terminal-state (retracted/
/// superseded/…) entry is still found: "was this purged" (no row, but a
/// tombstoned audit trail) and "was this retracted" (row present,
/// `state='retracted'`) are exactly the two states this function must tell
/// apart (spec 08 §3 "retract ≠ delete").
pub fn inspect_memory(
    conn: &Connection,
    memory_id: &str,
    now_ms: i64,
) -> rusqlite::Result<Option<MemoryInspection>> {
    let Some(entry) = memory_entry_by_id(conn, memory_id)? else {
        return Ok(None);
    };
    let evidence = evidence_summaries_for(conn, memory_id, now_ms)?;
    let audit_trail = read_audit_events_for_entity(conn, "memory_entry", memory_id)?;
    Ok(Some(MemoryInspection {
        entry,
        evidence,
        audit_trail,
    }))
}

/// `inspect generation <id>`'s result — the full `generation` row, or `None`
/// if unknown. No evidence/audit concept applies to a generation, so this is
/// a thin pass-through to `crate::registry::generation_row`, not a new
/// wrapper type.
pub fn inspect_generation(
    conn: &Connection,
    generation_id: &str,
) -> rusqlite::Result<Option<GenerationRow>> {
    generation_row(conn, generation_id)
}
