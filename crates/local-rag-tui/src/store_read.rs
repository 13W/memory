//! Shared offline-safe `state.sqlite` read helper (spec 11 §7). Extracted at T18-04 — the third
//! screen needing the identical `StateDb::diagnose_versions`-before-`StateDb::open` precaution
//! `status.rs` (T18-02) and `repositories.rs` (T18-03) each independently pasted first, per
//! `repositories.rs`'s own doc comment naming this exact task as the deferral trigger. A pure
//! move: every error string/branch is byte-identical to what both screens already had, so their
//! own `*_offline.rs` fixture tests keep passing unchanged.
//!
//! `StateDb::open` applies pending migrations as a side effect of opening (spec 02 §4.1's
//! open → migrate → serve ordering) — every screen in this dashboard reads `state.sqlite`
//! read-only, so none of them may trigger that as a side effect of merely being looked at. Callers
//! probe `StateDb::diagnose_versions` (a raw read-only connection, never `StateDb::open`) first,
//! and open for real only once that confirms the store is `Applied` with an empty `pending` list —
//! at which point `open` is a genuine no-op with respect to migration. `cli stats` (the closest CLI
//! cousin to these screens) does not take this precaution, because none of its cards are framed as
//! "offline-safe" the way every screen in this dashboard explicitly is.

use local_rag_core::paths::StoreLayout;
use local_rag_store::rusqlite::Connection;
use local_rag_store::{StateDb, VersionDiagnosis};

/// `VersionDiagnosis` → a human reason string, when it blocks opening `StateDb` for real. Mirrors
/// `cli::doctor::describe_versions_blocker`'s own four live branches (that function additionally
/// handles a pre-mapped `OpenError` branch, folded into each caller's own `Unavailable`
/// construction at each call site instead).
pub fn describe_versions_blocker(versions: &VersionDiagnosis) -> String {
    match versions {
        VersionDiagnosis::NotInitialized => "store not yet initialized".to_string(),
        VersionDiagnosis::MissingBookkeeping => {
            "state.sqlite exists but is not a recognized store".to_string()
        }
        VersionDiagnosis::Applied(r) => format!(
            "{} migration(s) pending; run `local-rag serve`/`index` first",
            r.pending.len()
        ),
        VersionDiagnosis::Fault(e) => e.to_string(),
        _ => "unknown version diagnosis".to_string(),
    }
}

/// Open a read-only `state.sqlite` connection, never applying a pending migration (module doc).
pub fn open_read_offline_safe(layout: &StoreLayout) -> Result<Connection, String> {
    let versions = StateDb::diagnose_versions(&layout.state_db(), local_rag_store::ALL)
        .map_err(|e| format!("could not read state.sqlite versions: {e}"))?;
    let ready = matches!(&versions, VersionDiagnosis::Applied(r) if r.pending.is_empty());
    if !ready {
        return Err(describe_versions_blocker(&versions));
    }
    let state = StateDb::open(layout.state_db())
        .map_err(|e| format!("could not open state.sqlite: {e}"))?;
    state
        .open_read()
        .map_err(|e| format!("could not open a read connection: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_versions_blocker_covers_every_named_variant() {
        assert_eq!(
            describe_versions_blocker(&VersionDiagnosis::NotInitialized),
            "store not yet initialized"
        );
        assert_eq!(
            describe_versions_blocker(&VersionDiagnosis::MissingBookkeeping),
            "state.sqlite exists but is not a recognized store"
        );
    }
}
