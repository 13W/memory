//! Folding the whole pending-candidate backlog, one exact-duplicate group at
//! a time (`T23-08`, ADR-0014 Decision 2). [`super::review::fold_pending_duplicates`]
//! is the one-group primitive; this module is the multi-group driver over
//! it — the same split [`crate::observation::payload_ttl`] uses for its own
//! sweep (a domain-owned module, not [`crate::housekeeping`]: see below for
//! why).
//!
//! # Never `housekeeping`, never wired into `local-rag gc`
//!
//! ADR-0014's last rejected alternative is explicit: "Dedup only at review
//! time… `T23-08` still groups for review, but grouping is a reading aid,
//! not the fix" — and, separately, that these acts are "an operator's, not a
//! schedule's" (Decision 1). Every sweep in [`crate::housekeeping`] is wired
//! into `local-rag gc`, which runs unattended and without confirmation by
//! design. Living there would make a full backlog collapse a side effect of
//! routine maintenance the first time someone "completes" `gc`'s sweep list
//! — exactly the automatic mass-decision ADR-0014 refuses. This module has
//! no caller in `gc`, and none should ever be added; `crates/local-rag/src/
//! cli/gc.rs`'s own module doc says so.
//!
//! # One group, one transaction — not one sweep, one transaction
//!
//! [`fold_all_pending_duplicates`] takes one read pass
//! ([`super::review::pending_candidate_groups`]) and then one write
//! transaction *per group*, mirroring
//! [`crate::housekeeping::run_candidate_expiry_sweep`]'s per-row convention
//! at group granularity — except each group's membership is re-derived
//! inside its own transaction (`fold_pending_duplicates` calls
//! `pending_duplicates_of` itself), so a twin edited or a survivor
//! approved/rejected between the read pass and that group's write is simply
//! not part of the group any more, never a lost distinct proposal. A
//! [`ReviewError`](super::review::ReviewError) from one group (the survivor
//! was concurrently approved, rejected, or edited into a different claim)
//! is retained and the driver continues; a [`WriteError`] aborts the whole
//! run.

use crate::state::{OpenError, StateDb, WriteError};

use super::review::{FoldRetained, pending_candidate_groups};

/// A failure from [`fold_all_pending_duplicates`].
#[derive(Debug)]
#[non_exhaustive]
pub enum TriageError {
    /// Opening the read-only state connection (the group read pass) failed.
    Open(OpenError),
    /// The group read pass itself failed.
    Sqlite(rusqlite::Error),
    /// One group's fold transaction failed (rolled back; the store is
    /// unchanged for that group — every earlier group's fold already
    /// committed and stays committed).
    Write(WriteError),
}

impl std::fmt::Display for TriageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TriageError::Open(e) => write!(f, "could not open the state store: {e}"),
            TriageError::Sqlite(e) => write!(f, "reading the candidate backlog failed: {e}"),
            TriageError::Write(e) => write!(f, "candidate dedup fold failed: {e}"),
        }
    }
}

impl std::error::Error for TriageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            TriageError::Open(e) => Some(e),
            TriageError::Sqlite(e) => Some(e),
            TriageError::Write(e) => Some(e),
        }
    }
}

/// The outcome of one [`fold_all_pending_duplicates`] pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DedupTriageReport {
    /// Duplicate groups (size > 1) the read pass found.
    pub groups_examined: u64,
    /// Groups that had at least one twin actually folded.
    pub groups_folded: u64,
    /// Every candidate id transitioned `pending → rejected` (or, for a dry
    /// run, that would be), sorted.
    pub folded: Vec<String>,
    /// `candidate_evidence` rows linked onto a survivor from a folded twin.
    pub evidence_linked: u64,
    /// Twins left `pending` with the reason (see
    /// [`FoldRetained`](super::review::FoldRetained)), sorted by candidate
    /// id.
    pub retained: Vec<(String, FoldRetained)>,
    /// Distinct claims (group count, including groups of one) before this
    /// run.
    pub distinct_claims_before: u64,
    /// Distinct claims after — the card's acceptance made executable: a
    /// correct fold never changes this number.
    pub distinct_claims_after: u64,
    /// Whether this was a dry run (nothing was actually written).
    pub dry_run: bool,
}

/// Fold every exact-duplicate group in the pending queue (`T23-08`). See the
/// module doc for why this is never scheduled and never reachable from
/// `local-rag gc`.
pub async fn fold_all_pending_duplicates(
    db: &StateDb,
    now_ms: i64,
    dry_run: bool,
) -> Result<DedupTriageReport, TriageError> {
    let groups = {
        let conn = db.open_read().map_err(TriageError::Open)?;
        pending_candidate_groups(&conn).map_err(TriageError::Sqlite)?
    };
    let distinct_claims_before = groups.len() as u64;
    let groups_examined = groups
        .iter()
        .filter(|g| !g.duplicate_ids.is_empty())
        .count() as u64;

    let mut folded = Vec::new();
    let mut retained = Vec::new();
    let mut evidence_linked = 0u64;
    let mut groups_folded = 0u64;

    for group in &groups {
        if group.duplicate_ids.is_empty() {
            continue;
        }

        if dry_run {
            folded.extend(group.duplicate_ids.iter().cloned());
            groups_folded += 1;
            continue;
        }

        let survivor_id = group.survivor.candidate_id.clone();
        let outcome = db
            .writer()
            .transaction(move |tx| super::review::fold_pending_duplicates(tx, &survivor_id, now_ms))
            .await
            .map_err(TriageError::Write)?;

        match outcome {
            Ok(outcome) => {
                if !outcome.folded.is_empty() {
                    groups_folded += 1;
                }
                folded.extend(outcome.folded);
                evidence_linked += outcome.evidence_linked as u64;
                retained.extend(outcome.retained);
            }
            Err(_review_error) => {
                // The survivor itself moved (approved/rejected/edited) since
                // the read pass — the whole group is retained, not a
                // failure, matching every other sweep's read-then-write
                // convention.
                for twin_id in &group.duplicate_ids {
                    retained.push((twin_id.clone(), FoldRetained::SurvivorNoLongerPending));
                }
            }
        }
    }

    let distinct_claims_after = if dry_run {
        distinct_claims_before
    } else {
        let conn = db.open_read().map_err(TriageError::Open)?;
        pending_candidate_groups(&conn)
            .map_err(TriageError::Sqlite)?
            .len() as u64
    };

    folded.sort();
    retained.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(DedupTriageReport {
        groups_examined,
        groups_folded,
        folded,
        evidence_linked,
        retained,
        distinct_claims_before,
        distinct_claims_after,
        dry_run,
    })
}
