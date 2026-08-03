//! `export [--scope …]` (spec 11 §6, 12 §3, T16-02) — deterministic,
//! scope-isolated dump of every memory entry (plus its evidence and audit
//! trail) in the caller-resolved scopes.

use rusqlite::Connection;

use crate::memory::{ScopeKind, list_memory_entries_for_scope, read_audit_events_for_entity};

use super::inspect::{MemoryInspection, evidence_summaries_for};

/// Every `memory_entry` (plus evidence and audit trail) across `scopes`,
/// ascending by `(created_at, memory_id)` — the same order the CLI's `memory
/// list` already applies when combining multiple scopes. `scopes` is the
/// caller's already-resolved `(ScopeKind, scope_owner_id)` set: this crate
/// has no worktree-resolution logic of its own, the CLI computes it via
/// `local_rag_memory::recall::scopes_for` exactly as `memory list` does
/// today. This function applies no filtering beyond what `scopes` already
/// specifies, which is what makes "scope isolation" a caller-controlled,
/// testable property rather than something buried in this function.
///
/// Deterministic by construction, not by extra bookkeeping: no `exported_at`/
/// wall-clock field is part of the output (`now_ms` is used only to evaluate
/// each evidence observation's payload TTL); every per-scope read and the
/// final combine are already `ORDER BY`-stable.
pub fn export_scope(
    conn: &Connection,
    scopes: &[(ScopeKind, String)],
    now_ms: i64,
) -> rusqlite::Result<Vec<MemoryInspection>> {
    let mut entries = Vec::new();
    for (kind, owner) in scopes {
        entries.extend(list_memory_entries_for_scope(
            conn, *kind, owner, None, None,
        )?);
    }
    entries.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.memory_id.cmp(&b.memory_id))
    });

    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        let evidence = evidence_summaries_for(conn, &entry.memory_id, now_ms)?;
        let audit_trail = read_audit_events_for_entity(conn, "memory_entry", &entry.memory_id)?;
        out.push(MemoryInspection {
            entry,
            evidence,
            audit_trail,
        });
    }
    Ok(out)
}
