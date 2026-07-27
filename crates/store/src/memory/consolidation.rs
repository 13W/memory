//! `consolidation_run` / `processing_cursor`: the consolidation-run machine
//! (spec 03 §2.5, 04 §4). T14-01 ships only the pure transition legality and
//! the plain row primitives; lease acquisition/renewal against a clock
//! (120s/30s, spec 04 §4) and the router call itself are T14-06's runner.

use rusqlite::types::Type;
use rusqlite::{Connection, Error, OptionalExtension, Transaction, params};

/// `consolidation_run.state` (spec 03 §2.5 CHECK domain, spec 04 §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    Pending,
    Running,
    Applied,
    Failed,
}

impl RunState {
    pub fn as_str(self) -> &'static str {
        match self {
            RunState::Pending => "pending",
            RunState::Running => "running",
            RunState::Applied => "applied",
            RunState::Failed => "failed",
        }
    }

    /// Parse a stored value; `None` for anything the CHECK constraint forbids.
    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(RunState::Pending),
            "running" => Some(RunState::Running),
            "applied" => Some(RunState::Applied),
            "failed" => Some(RunState::Failed),
            _ => None,
        }
    }

    /// Check whether `self → to` is legal (spec 04 §4). Pure — no I/O.
    ///
    /// `pending → running` (lease acquired), `running → applied` (ops applied,
    /// cursor advanced), `running → failed` (router/LLM error). `applied` is
    /// terminal.
    ///
    /// As-built decision (T14-01, `[SPEC]`): the prose diagram draws a
    /// crash/lease-expiry retry as `running` re-entering `running` under a
    /// fresh lease — already covered by the project-wide self-transition
    /// convention, no extra edge needed. It labels `failed` itself
    /// "(retryable)" but does not draw the edge explicitly; since
    /// `idempotency_key = H(memory_op, run_id, op_index)` requires a *stable*
    /// `run_id` across a retry (spec 04 §4 bullet 2) and the spec describes no
    /// mechanism for minting a replacement run for the same window, this is
    /// read as `failed → running`: the same row is retried, under a fresh
    /// lease T14-06's runner sets. Lease-timing itself is not this function's
    /// concern — only whether the state edge is legal at all.
    pub fn check_transition(self, to: RunState) -> Result<(), IllegalRunTransition> {
        use RunState::{Applied, Failed, Pending, Running};
        let legal = match (self, to) {
            (a, b) if a == b => true,
            (Pending, Running) => true,
            (Running, Applied) => true,
            (Running, Failed) => true,
            (Failed, Running) => true,
            _ => false,
        };
        if legal {
            Ok(())
        } else {
            Err(IllegalRunTransition { from: self, to })
        }
    }
}

/// A rejected consolidation-run transition (spec 04 §4): the machine forbids
/// `from → to`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IllegalRunTransition {
    pub from: RunState,
    pub to: RunState,
}

impl std::fmt::Display for IllegalRunTransition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "illegal consolidation run transition {} → {}",
            self.from.as_str(),
            self.to.as_str()
        )
    }
}

impl std::error::Error for IllegalRunTransition {}

/// Why a [`transition_run`] request was rejected at the domain level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunTransitionError {
    /// No `consolidation_run` row has this id.
    UnknownRun,
    /// The machine (spec 04 §4) forbids the requested transition.
    Illegal(IllegalRunTransition),
}

impl std::fmt::Display for RunTransitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunTransitionError::UnknownRun => write!(f, "unknown consolidation run"),
            RunTransitionError::Illegal(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for RunTransitionError {}

/// A new `consolidation_run` row, mirroring the DDL 1:1 apart from `state` —
/// every run starts `pending` (spec 04 §4), so [`create_consolidation_run`]
/// fixes it. `lease_until` is therefore always `NULL` at creation: the lease is
/// acquired exactly at the `pending → running` edge (T14-06's runner).
#[derive(Debug, Clone, Copy)]
pub struct NewConsolidationRun<'a> {
    pub run_id: &'a str,
    pub session_id: &'a str,
    pub from_received_seq: i64,
    pub to_received_seq: i64,
    pub router_version: &'a str,
}

/// Insert a `consolidation_run` row, born `pending` with `lease_until = NULL`.
pub fn create_consolidation_run(
    tx: &Transaction<'_>,
    row: &NewConsolidationRun<'_>,
    now_ms: i64,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO consolidation_run \
           (run_id, session_id, from_received_seq, to_received_seq, router_version, state, \
            lease_until, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, 'pending', NULL, ?6, ?6)",
        params![
            row.run_id,
            row.session_id,
            row.from_received_seq,
            row.to_received_seq,
            row.router_version,
            now_ms,
        ],
    )?;
    Ok(())
}

/// Transition `run_id` to state `to`, enforcing the machine (spec 04 §4) and
/// bumping `updated_at` on an effective transition — mirroring
/// [`transition_model_space`](crate::registry::transition_model_space), whose
/// `updated_at` is the same kind of plain last-touched bookkeeping column
/// (unlike `memory_entry.entry_version`, which spec 04 §5 couples to a
/// matching `audit_event` and this crate therefore leaves to T14-02).
/// Deliberately does **not** touch `lease_until` — acquiring/renewing the
/// lease is T14-06's runner.
pub fn transition_run(
    tx: &Transaction<'_>,
    run_id: &str,
    to: RunState,
    now_ms: i64,
) -> rusqlite::Result<Result<(), RunTransitionError>> {
    let from: Option<RunState> = tx
        .query_row(
            "SELECT state FROM consolidation_run WHERE run_id = ?1",
            params![run_id],
            |r| {
                let raw: String = r.get(0)?;
                RunState::from_db(&raw).ok_or_else(|| {
                    Error::FromSqlConversionFailure(
                        0,
                        Type::Text,
                        format!("invalid consolidation_run.state {raw:?}").into(),
                    )
                })
            },
        )
        .optional()?;

    let Some(from) = from else {
        return Ok(Err(RunTransitionError::UnknownRun));
    };

    if let Err(illegal) = from.check_transition(to) {
        return Ok(Err(RunTransitionError::Illegal(illegal)));
    }

    if from != to {
        tx.execute(
            "UPDATE consolidation_run SET state = ?2, updated_at = ?3 WHERE run_id = ?1",
            params![run_id, to.as_str(), now_ms],
        )?;
    }
    Ok(Ok(()))
}

/// The run's current state, if it exists (spec 03 §2.5).
///
/// A stored value outside the CHECK domain (corruption) surfaces as
/// [`rusqlite::Error::FromSqlConversionFailure`], never a silent default.
pub fn consolidation_run_state(
    conn: &Connection,
    run_id: &str,
) -> rusqlite::Result<Option<RunState>> {
    conn.query_row(
        "SELECT state FROM consolidation_run WHERE run_id = ?1",
        params![run_id],
        |r| {
            let raw: String = r.get(0)?;
            RunState::from_db(&raw).ok_or_else(|| {
                Error::FromSqlConversionFailure(
                    0,
                    Type::Text,
                    format!("invalid consolidation_run.state {raw:?}").into(),
                )
            })
        },
    )
    .optional()
}

/// The `session_id`'s current consolidation cursor
/// (`last_consolidated_received_seq`), or `None` if this session has never
/// been consolidated before (spec 03 §2.5).
pub fn processing_cursor(conn: &Connection, session_id: &str) -> rusqlite::Result<Option<i64>> {
    conn.query_row(
        "SELECT last_consolidated_received_seq FROM processing_cursor WHERE session_id = ?1",
        params![session_id],
        |r| r.get(0),
    )
    .optional()
}

/// Upsert the `session_id`'s consolidation cursor, mirroring
/// [`observation`](crate::observation)'s `spool_import_cursor` upsert idiom.
pub fn upsert_processing_cursor(
    tx: &Transaction<'_>,
    session_id: &str,
    last_consolidated_received_seq: i64,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO processing_cursor (session_id, last_consolidated_received_seq) \
         VALUES (?1, ?2) \
         ON CONFLICT(session_id) DO UPDATE SET \
           last_consolidated_received_seq = excluded.last_consolidated_received_seq",
        params![session_id, last_consolidated_received_seq],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_state_round_trips() {
        for state in [
            RunState::Pending,
            RunState::Running,
            RunState::Applied,
            RunState::Failed,
        ] {
            assert_eq!(RunState::from_db(state.as_str()), Some(state));
        }
        assert_eq!(RunState::from_db("bogus"), None);
    }

    #[test]
    fn check_transition_covers_the_whole_matrix() {
        use RunState::{Applied, Failed, Pending, Running};
        let all = [Pending, Running, Applied, Failed];
        let legal = [
            (Pending, Running),
            (Running, Applied),
            (Running, Failed),
            (Failed, Running),
        ];

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
                    Err(IllegalRunTransition { from, to }),
                    "{from:?} → {to:?} illegal",
                );
            }
        }
    }

    #[test]
    fn applied_is_terminal_no_legal_edges_out() {
        for to in [RunState::Pending, RunState::Running, RunState::Failed] {
            assert_eq!(
                RunState::Applied.check_transition(to),
                Err(IllegalRunTransition {
                    from: RunState::Applied,
                    to,
                }),
            );
        }
    }

    #[test]
    fn run_state_rejects_corrupt_enum() {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(
            "CREATE TABLE consolidation_run (run_id TEXT, state TEXT);\n\
             INSERT INTO consolidation_run VALUES ('r', 'zombie');",
        )
        .expect("seed corrupt row");

        let bad = consolidation_run_state(&conn, "r");
        assert!(
            matches!(bad, Err(Error::FromSqlConversionFailure(0, Type::Text, _))),
            "corrupt state → typed conversion failure, got {bad:?}",
        );
        assert_eq!(
            consolidation_run_state(&conn, "missing").expect("read"),
            None
        );
    }
}
