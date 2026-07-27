//! `pending_memory_candidate` / `candidate_evidence`: the review-candidate
//! machine (spec 03 §2.5, 04 §6). All three transitions out of `pending` are
//! terminal — approval materializes the proposed operation through the same
//! transactional path as the router (T14-05); this module ships only the
//! schema and the pure legality guard.

use rusqlite::types::Type;
use rusqlite::{Connection, Error, OptionalExtension, Transaction, params};

/// `pending_memory_candidate.review_state` (spec 03 §2.5 CHECK domain, spec 04
/// §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateState {
    Pending,
    Approved,
    Rejected,
    Expired,
}

impl CandidateState {
    pub fn as_str(self) -> &'static str {
        match self {
            CandidateState::Pending => "pending",
            CandidateState::Approved => "approved",
            CandidateState::Rejected => "rejected",
            CandidateState::Expired => "expired",
        }
    }

    /// Parse a stored value; `None` for anything the CHECK constraint forbids.
    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(CandidateState::Pending),
            "approved" => Some(CandidateState::Approved),
            "rejected" => Some(CandidateState::Rejected),
            "expired" => Some(CandidateState::Expired),
            _ => None,
        }
    }

    /// Check whether `self → to` is legal (spec 04 §6): `pending → approved |
    /// rejected | expired`, all three terminal. Self-transition is always
    /// legal (idempotent no-op), the project-wide convention. Pure — no I/O.
    pub fn check_transition(self, to: CandidateState) -> Result<(), IllegalCandidateTransition> {
        use CandidateState::{Approved, Expired, Pending, Rejected};
        let legal = match (self, to) {
            (a, b) if a == b => true,
            (Pending, Approved) => true,
            (Pending, Rejected) => true,
            (Pending, Expired) => true,
            _ => false,
        };
        if legal {
            Ok(())
        } else {
            Err(IllegalCandidateTransition { from: self, to })
        }
    }
}

/// A rejected candidate transition (spec 04 §6): the machine forbids `from →
/// to`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IllegalCandidateTransition {
    pub from: CandidateState,
    pub to: CandidateState,
}

impl std::fmt::Display for IllegalCandidateTransition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "illegal candidate transition {} → {}",
            self.from.as_str(),
            self.to.as_str()
        )
    }
}

impl std::error::Error for IllegalCandidateTransition {}

/// Why a [`transition_candidate`] request was rejected at the domain level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateTransitionError {
    /// No `pending_memory_candidate` row has this id.
    UnknownCandidate,
    /// The machine (spec 04 §6) forbids the requested transition.
    Illegal(IllegalCandidateTransition),
}

impl std::fmt::Display for CandidateTransitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CandidateTransitionError::UnknownCandidate => write!(f, "unknown candidate"),
            CandidateTransitionError::Illegal(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for CandidateTransitionError {}

/// A new `pending_memory_candidate` row, mirroring the DDL 1:1 apart from
/// `review_state` — every candidate starts `pending` (spec 04 §6), so
/// [`create_candidate`] fixes it. `proposed_operation`/`conflicts` are
/// caller-serialized JSON (spec 03 §2.5: "JSON: op + target + text + …");
/// this module does not interpret their shape.
#[derive(Debug, Clone, Copy)]
pub struct NewCandidate<'a> {
    pub candidate_id: &'a str,
    pub proposed_operation: &'a str,
    pub conflicts: Option<&'a str>,
}

/// Insert a `pending_memory_candidate` row, born `pending`.
pub fn create_candidate(
    tx: &Transaction<'_>,
    row: &NewCandidate<'_>,
    now_ms: i64,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO pending_memory_candidate \
           (candidate_id, proposed_operation, conflicts, review_state, created_at) \
         VALUES (?1, ?2, ?3, 'pending', ?4)",
        params![
            row.candidate_id,
            row.proposed_operation,
            row.conflicts,
            now_ms,
        ],
    )?;
    Ok(())
}

/// Transition `candidate_id` to state `to`, enforcing the machine (spec 04
/// §6). Mirrors [`transition_generation`](crate::registry::transition_generation):
/// nested result, no mutation on rejection, corrupt stored value → typed
/// conversion failure.
pub fn transition_candidate(
    tx: &Transaction<'_>,
    candidate_id: &str,
    to: CandidateState,
) -> rusqlite::Result<Result<(), CandidateTransitionError>> {
    let from: Option<CandidateState> = tx
        .query_row(
            "SELECT review_state FROM pending_memory_candidate WHERE candidate_id = ?1",
            params![candidate_id],
            |r| {
                let raw: String = r.get(0)?;
                CandidateState::from_db(&raw).ok_or_else(|| {
                    Error::FromSqlConversionFailure(
                        0,
                        Type::Text,
                        format!("invalid pending_memory_candidate.review_state {raw:?}").into(),
                    )
                })
            },
        )
        .optional()?;

    let Some(from) = from else {
        return Ok(Err(CandidateTransitionError::UnknownCandidate));
    };

    if let Err(illegal) = from.check_transition(to) {
        return Ok(Err(CandidateTransitionError::Illegal(illegal)));
    }

    if from != to {
        tx.execute(
            "UPDATE pending_memory_candidate SET review_state = ?2 WHERE candidate_id = ?1",
            params![candidate_id, to.as_str()],
        )?;
    }
    Ok(Ok(()))
}

/// The candidate's current review state, if it exists (spec 03 §2.5).
///
/// A stored value outside the CHECK domain (corruption) surfaces as
/// [`rusqlite::Error::FromSqlConversionFailure`], never a silent default.
pub fn candidate_state(
    conn: &Connection,
    candidate_id: &str,
) -> rusqlite::Result<Option<CandidateState>> {
    conn.query_row(
        "SELECT review_state FROM pending_memory_candidate WHERE candidate_id = ?1",
        params![candidate_id],
        |r| {
            let raw: String = r.get(0)?;
            CandidateState::from_db(&raw).ok_or_else(|| {
                Error::FromSqlConversionFailure(
                    0,
                    Type::Text,
                    format!("invalid pending_memory_candidate.review_state {raw:?}").into(),
                )
            })
        },
    )
    .optional()
}

/// Insert a `candidate_evidence` row (FK provenance only, spec 03 §2.5 — "not
/// embedded snapshots"). An unknown `candidate_id`/`observation_id` is
/// rejected by the composite FKs.
pub fn insert_candidate_evidence(
    tx: &Transaction<'_>,
    candidate_id: &str,
    observation_id: &str,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO candidate_evidence (candidate_id, observation_id) VALUES (?1, ?2)",
        params![candidate_id, observation_id],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_state_round_trips() {
        for state in [
            CandidateState::Pending,
            CandidateState::Approved,
            CandidateState::Rejected,
            CandidateState::Expired,
        ] {
            assert_eq!(CandidateState::from_db(state.as_str()), Some(state));
        }
        assert_eq!(CandidateState::from_db("bogus"), None);
    }

    #[test]
    fn check_transition_covers_the_whole_matrix() {
        use CandidateState::{Approved, Expired, Pending, Rejected};
        let all = [Pending, Approved, Rejected, Expired];
        let legal = [(Pending, Approved), (Pending, Rejected), (Pending, Expired)];

        for (from, to) in legal {
            assert_eq!(from.check_transition(to), Ok(()), "{from:?} → {to:?} legal");
        }
        for s in all {
            assert_eq!(s.check_transition(s), Ok(()), "{s:?} → {s:?} idempotent");
        }
        for from in all {
            for to in all {
                if from == to || legal.contains(&(from, to)) {
                    continue;
                }
                assert_eq!(
                    from.check_transition(to),
                    Err(IllegalCandidateTransition { from, to }),
                    "{from:?} → {to:?} illegal",
                );
            }
        }
    }

    #[test]
    fn candidate_state_rejects_corrupt_enum() {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(
            "CREATE TABLE pending_memory_candidate (candidate_id TEXT, review_state TEXT);\n\
             INSERT INTO pending_memory_candidate VALUES ('c', 'zombie');",
        )
        .expect("seed corrupt row");

        let bad = candidate_state(&conn, "c");
        assert!(
            matches!(bad, Err(Error::FromSqlConversionFailure(0, Type::Text, _))),
            "corrupt review_state → typed conversion failure, got {bad:?}",
        );
        assert_eq!(candidate_state(&conn, "missing").expect("read"), None);
    }
}
