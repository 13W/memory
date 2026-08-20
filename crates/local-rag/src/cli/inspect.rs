//! `local-rag inspect <observation|memory|generation> <id>` (spec 11 §6, 12
//! §3, T16-02) — a read-only JSON dump of one row (plus, for `memory`, its
//! resolved evidence and complete audit trail) via
//! `local_rag_store::privacy::inspect_*`. No `--json` opt-in flag, unlike
//! `stats`: three heterogeneous row shapes plus a list have no natural
//! human-glance format that would earn its own code path.
//!
//! The JSON helpers here ([`memory_inspection_json`], [`payload_status_json`])
//! are `pub(crate)` because `export` reuses them verbatim for its own
//! `Vec<MemoryInspection>` — export is never poorer than inspect for the
//! identical shape.

use std::process::ExitCode;

use local_rag_store::privacy::{
    EvidenceSummary, MemoryInspection, ObservationInspection, inspect_generation, inspect_memory,
    inspect_observation,
};
use local_rag_store::{AuditEventRow, GenerationRow, NormalizationRow, PayloadStatus};

use super::{fail, resolve_layout_and_config, system_now_ms};
use local_rag::indexing::open_state;

const BIN: &str = "local-rag";

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum InspectKind {
    Observation,
    Memory,
    Generation,
}

#[derive(Debug, clap::Args)]
pub struct InspectArgs {
    kind: InspectKind,
    id: String,
}

pub fn run(args: InspectArgs) -> ExitCode {
    let InspectArgs { kind, id } = args;

    let (layout, _config) = match resolve_layout_and_config() {
        Ok(v) => v,
        Err(e) => return fail(BIN, &e),
    };
    let state = match open_state(&layout) {
        Ok(s) => s,
        Err(e) => return fail(BIN, &e),
    };
    let conn = match state.open_read() {
        Ok(c) => c,
        Err(e) => return fail(BIN, &format!("could not open state.sqlite: {e}")),
    };
    let now_ms = system_now_ms();

    let value = match kind {
        InspectKind::Observation => match inspect_observation(&conn, &id, now_ms) {
            Ok(Some(found)) => observation_inspection_json(&found),
            Ok(None) => return fail(BIN, &format!("no observation with id {id}")),
            Err(e) => return fail(BIN, &format!("could not inspect observation {id}: {e}")),
        },
        InspectKind::Memory => match inspect_memory(&conn, &id, now_ms) {
            Ok(Some(found)) => memory_inspection_json(&found),
            Ok(None) => return fail(BIN, &format!("no memory entry with id {id}")),
            Err(e) => return fail(BIN, &format!("could not inspect memory {id}: {e}")),
        },
        InspectKind::Generation => match inspect_generation(&conn, &id) {
            Ok(Some(found)) => generation_row_json(&found),
            Ok(None) => return fail(BIN, &format!("no generation with id {id}")),
            Err(e) => return fail(BIN, &format!("could not inspect generation {id}: {e}")),
        },
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&value).expect("inspect result always serializes")
    );
    ExitCode::SUCCESS
}

pub(crate) fn payload_status_json(payload: &PayloadStatus) -> serde_json::Value {
    match payload {
        PayloadStatus::Present {
            byte_size,
            expires_at,
            text,
        } => serde_json::json!({
            "status": "present",
            "byte_size": byte_size,
            "expires_at": expires_at,
            "text": text,
        }),
        PayloadStatus::Expired { expires_at } => serde_json::json!({
            "status": "expired",
            "expires_at": expires_at,
        }),
        PayloadStatus::None => serde_json::json!({ "status": "none" }),
    }
}

fn observation_inspection_json(found: &ObservationInspection) -> serde_json::Value {
    serde_json::json!({
        "observation_id": found.observation_id,
        "source_event_id": found.source_event_id,
        "dedup_key": found.dedup_key,
        "payload_hash": found.payload_hash,
        "event_type": found.event_type,
        "evidence_kind": found.evidence_kind.as_str(),
        "trust": found.trust.as_str(),
        "source_timestamp": found.source_timestamp,
        "repo_id": found.repo_id,
        "worktree_id": found.worktree_id,
        "session_id": found.session_id,
        "agent_id": found.agent_id,
        "turn_id": found.turn_id,
        "batch_id": found.batch_id,
        "commit_hash": found.commit_hash,
        "short_evidence_excerpt": found.short_evidence_excerpt,
        "redaction_version": found.redaction_version,
        "paths": found.paths,
        "payload": payload_status_json(&found.payload),
    })
}

fn evidence_summary_json(evidence: &EvidenceSummary) -> serde_json::Value {
    serde_json::json!({
        "observation_id": evidence.observation_id,
        "event_type": evidence.event_type,
        "evidence_kind": evidence.evidence_kind.as_str(),
        "trust": evidence.trust.as_str(),
        "session_id": evidence.session_id,
        "source_timestamp": evidence.source_timestamp,
        "short_evidence_excerpt": evidence.short_evidence_excerpt,
        "payload": payload_status_json(&evidence.payload),
    })
}

fn audit_event_json(event: &AuditEventRow) -> serde_json::Value {
    serde_json::json!({
        "audit_id": event.audit_id,
        "entity_kind": event.entity_kind,
        "entity_id": event.entity_id,
        "entity_version": event.entity_version,
        "op": event.op,
        "actor": event.actor.as_str(),
        "idempotency_key": event.idempotency_key,
        "payload": event.payload,
        "created_at": event.created_at,
    })
}

pub(crate) fn memory_inspection_json(found: &MemoryInspection) -> serde_json::Value {
    serde_json::json!({
        "entry": {
            "memory_id": found.entry.memory_id,
            "kind": found.entry.kind.as_str(),
            "state": found.entry.state.as_str(),
            "text": found.entry.text,
            "canonical_key": found.entry.canonical_key,
            "scope_kind": found.entry.scope_kind.as_str(),
            "scope_owner_id": found.entry.scope_owner_id,
            "confidence": found.entry.confidence,
            "importance": found.entry.importance,
            "valid_from_tree": found.entry.valid_from_tree,
            "last_verified_tree": found.entry.last_verified_tree,
            "supersedes_id": found.entry.supersedes_id,
            "entry_version": found.entry.entry_version,
            "created_at": found.entry.created_at,
            "updated_at": found.entry.updated_at,
        },
        "evidence": found.evidence.iter().map(evidence_summary_json).collect::<Vec<_>>(),
        "audit_trail": found.audit_trail.iter().map(audit_event_json).collect::<Vec<_>>(),
        "normalization": found.normalization.as_ref().map(normalization_json),
    })
}

/// What the author wrote, and how the canon came to be English (T21-13,
/// ADR-0011).
///
/// `source_text` is printed, not elided: durable memory is stored in English
/// (08 §3), so this row *is* the owner's own words, and `export` exists to show
/// everything the store holds about them (12 §3 `[FIXED, ADR-0011]`).
/// Provenance travels with it so a reader can tell a translation by a known
/// model under a known prompt version from one left behind by an older
/// normalizer — and, on a `failed` row, why the canon is not English at all.
fn normalization_json(row: &NormalizationRow) -> serde_json::Value {
    serde_json::json!({
        "status": row.status.as_str(),
        "canon_text_sha256": row.canon_text_sha256,
        "source_text": row.source_text,
        "source_language": row.source_language,
        "normalizer_model_id": row.normalizer_model_id,
        "prompt_version": row.prompt_version,
        "normalizer_version": row.normalizer_version,
        "attempt_count": row.attempt_count,
        "last_error": row.last_error,
        "next_attempt_at": row.next_attempt_at,
        "created_at": row.created_at,
        "updated_at": row.updated_at,
    })
}

fn generation_row_json(found: &GenerationRow) -> serde_json::Value {
    serde_json::json!({
        "generation_id": found.generation_id,
        "worktree_id": found.worktree_id,
        "generation_number": found.generation_number,
        "state": found.state.as_str(),
        "created_at": found.created_at,
    })
}
