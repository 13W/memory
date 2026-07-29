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

// ---------------------------------------------------------------------------
// T14-06: lease/cursor runner primitives (spec 04 §4). Everything below is
// still pure, synchronous, tx/conn-scoped DB CRUD — the actual async
// generator-driving orchestration (`crate::memory::runner`) composes these.
// ---------------------------------------------------------------------------

/// Lease duration and renewal cadence for a `running` consolidation run (spec
/// 04 §4 `[SPEC]`: "`lease_until = now + 120s`, renewed every 30s while the
/// router runs").
pub const LEASE_DURATION_MS: i64 = 120_000;
pub const LEASE_RENEW_INTERVAL_MS: i64 = 30_000;

/// Set `lease_until` for `run_id` unconditionally (spec 04 §4). Deliberately
/// separate from [`transition_run`] — T14-01's doc comment already earmarks
/// "acquiring/renewing the lease" as this task's own concern, so the state
/// edge and the lease value are two explicit calls the runner composes
/// together inside one snapshot transaction ([`open_next_run`]) rather than
/// one primitive doing both.
pub fn acquire_lease(
    tx: &Transaction<'_>,
    run_id: &str,
    lease_until_ms: i64,
) -> rusqlite::Result<()> {
    tx.execute(
        "UPDATE consolidation_run SET lease_until = ?2 WHERE run_id = ?1",
        params![run_id, lease_until_ms],
    )?;
    Ok(())
}

/// Why [`renew_lease`] refused to extend a lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenewError {
    /// No `consolidation_run` row has this id.
    UnknownRun,
    /// The row exists but is no longer `running` (already `applied`/`failed`,
    /// or a fresh attempt already moved it) — a renewal arriving this late is
    /// stale bookkeeping noise, not a legitimate extension.
    NotRunning,
}

impl std::fmt::Display for RenewError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RenewError::UnknownRun => write!(f, "unknown consolidation run"),
            RenewError::NotRunning => write!(f, "consolidation run is no longer running"),
        }
    }
}

impl std::error::Error for RenewError {}

/// Extend `run_id`'s lease to `lease_until_ms`, but only while it is still
/// `running` (spec 04 §4's renewal cadence). A run that has already left
/// `running` (or never existed) is [`RenewError`], not a silent write — a
/// stale renewal must never resurrect a decided run.
pub fn renew_lease(
    tx: &Transaction<'_>,
    run_id: &str,
    lease_until_ms: i64,
) -> rusqlite::Result<Result<(), RenewError>> {
    let Some(state) = consolidation_run_state(tx, run_id)? else {
        return Ok(Err(RenewError::UnknownRun));
    };
    if state != RunState::Running {
        return Ok(Err(RenewError::NotRunning));
    }
    acquire_lease(tx, run_id, lease_until_ms)?;
    Ok(Ok(()))
}

/// `(state, lease_until)` for `run_id`, if it exists — the lease-fencing read
/// [`crate::memory::runner::apply_run`] uses to require `state == Running &&
/// lease_until == expected` before touching any op (T14-06 as-built decision:
/// `lease_until` doubles as its own compare-and-swap token, no new column;
/// see `docs/specification/04-state-machines.md` §4's T14-06 note).
pub(crate) fn run_state_and_lease(
    tx: &Transaction<'_>,
    run_id: &str,
) -> rusqlite::Result<Option<(RunState, Option<i64>)>> {
    tx.query_row(
        "SELECT state, lease_until FROM consolidation_run WHERE run_id = ?1",
        params![run_id],
        |r| {
            let raw_state: String = r.get(0)?;
            let state = RunState::from_db(&raw_state).ok_or_else(|| {
                Error::FromSqlConversionFailure(
                    0,
                    Type::Text,
                    format!("invalid consolidation_run.state {raw_state:?}").into(),
                )
            })?;
            let lease_until: Option<i64> = r.get(1)?;
            Ok((state, lease_until))
        },
    )
    .optional()
}

/// Whether a lease has passed its deadline (spec 04 §4), `now_ms >=
/// lease_until` — the same `>=`-at-the-boundary convention
/// [`crate::housekeeping::candidate_expiry_due`] already uses. `None` (never
/// acquired — a lease-less `pending`/`applied` row, or unreachable corrupt
/// state) counts as expired: nothing legitimately depends on a not-yet- or
/// no-longer-leased run still being "live."
pub fn lease_expired(now_ms: i64, lease_until: Option<i64>) -> bool {
    match lease_until {
        Some(until) => now_ms >= until,
        None => true,
    }
}

/// One consolidation run's bounded window (spec 04 §4 step 1's snapshot):
/// which session, and the `received_seq` range it covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunWindow {
    pub run_id: String,
    pub session_id: String,
    pub from_received_seq: i64,
    pub to_received_seq: i64,
}

/// The result of [`open_next_run`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotOutcome {
    /// A fresh run was created, `pending → running`, lease acquired.
    Opened(RunWindow),
    /// Nothing to consolidate: no envelope past the session's cursor.
    NothingPending,
    /// A non-`applied` run already exists for this session — never open a
    /// second one; the caller inspects `state`/`lease_until` to decide
    /// whether to wait (a live lease) or resume it (via [`retry_run`]).
    Existing {
        window: RunWindow,
        state: RunState,
        lease_until: Option<i64>,
    },
}

/// The most recently created non-`applied` run for `session_id`, if any.
fn latest_non_applied_run(
    tx: &Transaction<'_>,
    session_id: &str,
) -> rusqlite::Result<Option<(String, RunState, Option<i64>)>> {
    tx.query_row(
        "SELECT run_id, state, lease_until FROM consolidation_run \
         WHERE session_id = ?1 AND state != 'applied' \
         ORDER BY created_at DESC LIMIT 1",
        params![session_id],
        |r| {
            let run_id: String = r.get(0)?;
            let raw_state: String = r.get(1)?;
            let state = RunState::from_db(&raw_state).ok_or_else(|| {
                Error::FromSqlConversionFailure(
                    1,
                    Type::Text,
                    format!("invalid consolidation_run.state {raw_state:?}").into(),
                )
            })?;
            let lease_until: Option<i64> = r.get(2)?;
            Ok((run_id, state, lease_until))
        },
    )
    .optional()
}

fn read_window(tx: &Transaction<'_>, run_id: &str) -> rusqlite::Result<RunWindow> {
    tx.query_row(
        "SELECT session_id, from_received_seq, to_received_seq \
         FROM consolidation_run WHERE run_id = ?1",
        params![run_id],
        |r| {
            Ok(RunWindow {
                run_id: run_id.to_string(),
                session_id: r.get(0)?,
                from_received_seq: r.get(1)?,
                to_received_seq: r.get(2)?,
            })
        },
    )
}

/// Open the next consolidation window for `session_id`, or report why not
/// (spec 08 §4 step 1, `[FIXED]`) — one transaction:
///
/// 1. Refuse to open a second run while any non-`applied` row already exists
///    for this session ([`SnapshotOutcome::Existing`]). Two concurrent
///    callers for the same `session_id` are fully serialized by
///    [`crate::StateWriter`]'s single-writer queue, so the second caller's
///    own transaction already observes the first's committed row here — no
///    separate read-then-write pre-check, and no TOCTOU gap.
/// 2. Else compute `from = cursor + 1`, `to = min(from + batch - 1,
///    max_received_seq)`; refuse with [`SnapshotOutcome::NothingPending`] if
///    there is no envelope past the cursor at all.
/// 3. Else create the row, transition `pending → running`, and acquire the
///    lease — [`SnapshotOutcome::Opened`].
pub fn open_next_run(
    tx: &Transaction<'_>,
    run_id: &str,
    session_id: &str,
    batch: i64,
    router_version: &str,
    lease_ms: i64,
    now_ms: i64,
) -> rusqlite::Result<SnapshotOutcome> {
    if let Some((existing_id, state, lease_until)) = latest_non_applied_run(tx, session_id)? {
        return Ok(SnapshotOutcome::Existing {
            window: read_window(tx, &existing_id)?,
            state,
            lease_until,
        });
    }

    let cursor = processing_cursor(tx, session_id)?.unwrap_or(0);
    let from = cursor + 1;
    let Some(max_seq) = crate::observation::max_received_seq(tx, session_id)? else {
        return Ok(SnapshotOutcome::NothingPending);
    };
    if max_seq < from {
        return Ok(SnapshotOutcome::NothingPending);
    }
    let to = (from + batch.max(1) - 1).min(max_seq);

    create_consolidation_run(
        tx,
        &NewConsolidationRun {
            run_id,
            session_id,
            from_received_seq: from,
            to_received_seq: to,
            router_version,
        },
        now_ms,
    )?;
    transition_run(tx, run_id, RunState::Running, now_ms)?
        .expect("pending -> running is always legal immediately after creation");
    acquire_lease(tx, run_id, now_ms + lease_ms)?;

    Ok(SnapshotOutcome::Opened(RunWindow {
        run_id: run_id.to_string(),
        session_id: session_id.to_string(),
        from_received_seq: from,
        to_received_seq: to,
    }))
}

/// Re-acquire an existing non-`applied` run under a fresh lease (spec 04 §4):
/// a lease-expired `running` row self-loops, a `failed` row re-enters
/// `running` — both already legal per [`RunState::check_transition`]. Used
/// for the "startup expired-lease retry" trigger the task card names, and
/// for resuming a run the runner itself just marked `failed` after an
/// apply-time rejection.
pub fn retry_run(
    tx: &Transaction<'_>,
    run_id: &str,
    lease_ms: i64,
    now_ms: i64,
) -> rusqlite::Result<Result<(), RunTransitionError>> {
    match transition_run(tx, run_id, RunState::Running, now_ms)? {
        Ok(()) => {
            acquire_lease(tx, run_id, now_ms + lease_ms)?;
            Ok(Ok(()))
        }
        Err(e) => Ok(Err(e)),
    }
}

/// One run eligible for a retry sweep (spec 04 §4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleRun {
    pub run_id: String,
    pub session_id: String,
    pub state: RunState,
    pub lease_until: Option<i64>,
    /// The run's snapshot window (spec 04 §4 step 1) — carried here (not just
    /// `run_id`/`session_id`) so a caller can go straight from [`stale_runs`]
    /// to [`crate::memory::runner::run_once`]'s `RunWindow` without a second
    /// read (T15-01's startup consolidation-resume driver, spec 02 §4.1
    /// step 5, is the first such caller).
    pub from_received_seq: i64,
    pub to_received_seq: i64,
}

/// Every run eligible for a startup/checkpoint retry: `failed` (spec 04 §4's
/// own "(retryable)" label), or `running` with an expired lease.
///
/// As-built decision (T14-06, `[SPEC]`): the runner (`crate::memory::runner`)
/// routes *any* apply-time rejection straight to `failed` rather than leaving
/// the row `running` for a lease timeout to eventually rediscover — retrying
/// immediately lets the next attempt's generator see current state instead of
/// deterministically reproducing the same rejection for up to
/// [`LEASE_DURATION_MS`]. Consequently this sweep must select both cases, not
/// lease-expiry alone, or a `failed` run would never be picked back up.
pub fn stale_runs(conn: &Connection, now_ms: i64) -> rusqlite::Result<Vec<StaleRun>> {
    let mut stmt = conn.prepare(
        "SELECT run_id, session_id, state, lease_until, from_received_seq, to_received_seq \
         FROM consolidation_run \
         WHERE state = 'failed' \
            OR (state = 'running' AND (lease_until IS NULL OR lease_until <= ?1)) \
         ORDER BY created_at",
    )?;
    let rows = stmt
        .query_map(params![now_ms], |r| {
            let run_id: String = r.get(0)?;
            let session_id: String = r.get(1)?;
            let raw_state: String = r.get(2)?;
            let state = RunState::from_db(&raw_state).ok_or_else(|| {
                Error::FromSqlConversionFailure(
                    2,
                    Type::Text,
                    format!("invalid consolidation_run.state {raw_state:?}").into(),
                )
            })?;
            let lease_until: Option<i64> = r.get(3)?;
            let from_received_seq: i64 = r.get(4)?;
            let to_received_seq: i64 = r.get(5)?;
            Ok(StaleRun {
                run_id,
                session_id,
                state,
                lease_until,
                from_received_seq,
                to_received_seq,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
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

    // -----------------------------------------------------------------------
    // T14-06: lease/snapshot primitives
    // -----------------------------------------------------------------------

    fn open_state() -> (local_rag_test_support::TempHome, crate::StateDb) {
        let home = local_rag_test_support::TempHome::new().expect("temp home");
        let layout = local_rag_core::paths::StoreLayout::new(home.join("local-rag"));
        layout.ensure().expect("ensure store tree");
        let db = crate::StateDb::open(layout.state_db()).expect("open state.sqlite");
        (home, db)
    }

    /// Insert a minimal, standalone `observation_envelope` row at a specific
    /// `received_seq` isn't possible directly (it's an autoincrement PK), so
    /// this inserts `count` rows in order and returns nothing — callers use
    /// `received_seq` 1..=count.
    async fn seed_envelopes(db: &crate::StateDb, session_id: &str, count: i64) {
        let session_id = session_id.to_string();
        db.writer()
            .transaction(move |tx| {
                for i in 0..count {
                    tx.execute(
                        "INSERT INTO observation_envelope \
                           (observation_id, source_event_id, payload_hash, event_type, \
                            evidence_kind, trust, session_id) \
                         VALUES (?1, ?2, 'deadbeef', 'Stop', 'user_statement', 'normal', ?3)",
                        params![
                            format!("obs-{session_id}-{i}"),
                            format!("evt-{i}"),
                            session_id
                        ],
                    )?;
                }
                Ok(())
            })
            .await
            .expect("seed envelopes");
    }

    #[test]
    fn lease_expired_boundary_and_none() {
        assert!(!lease_expired(1_999, Some(2_000)), "not yet due");
        assert!(
            lease_expired(2_000, Some(2_000)),
            "inclusive at the boundary"
        );
        assert!(lease_expired(2_001, Some(2_000)), "past the boundary");
        assert!(lease_expired(0, None), "never acquired counts as expired");
    }

    #[tokio::test]
    async fn open_next_run_reports_nothing_pending_with_no_envelopes_or_caught_up_cursor() {
        let (_home, db) = open_state();

        let outcome = db
            .writer()
            .transaction(|tx| {
                open_next_run(tx, "run-1", "sess-1", 10, "v1", LEASE_DURATION_MS, 1_000)
            })
            .await
            .expect("open tx");
        assert_eq!(
            outcome,
            SnapshotOutcome::NothingPending,
            "no envelopes at all"
        );

        seed_envelopes(&db, "sess-2", 3).await;
        db.writer()
            .transaction(|tx| upsert_processing_cursor(tx, "sess-2", 3))
            .await
            .expect("cursor caught up");
        let outcome = db
            .writer()
            .transaction(|tx| {
                open_next_run(tx, "run-2", "sess-2", 10, "v1", LEASE_DURATION_MS, 1_000)
            })
            .await
            .expect("open tx");
        assert_eq!(
            outcome,
            SnapshotOutcome::NothingPending,
            "cursor already at max_received_seq"
        );
    }

    #[tokio::test]
    async fn open_next_run_bounds_the_window_to_batch_never_past_max_seq() {
        let (_home, db) = open_state();
        seed_envelopes(&db, "sess-1", 5).await;

        let outcome = db
            .writer()
            .transaction(|tx| {
                open_next_run(tx, "run-1", "sess-1", 3, "v1", LEASE_DURATION_MS, 1_000)
            })
            .await
            .expect("open tx");
        assert_eq!(
            outcome,
            SnapshotOutcome::Opened(RunWindow {
                run_id: "run-1".to_string(),
                session_id: "sess-1".to_string(),
                from_received_seq: 1,
                to_received_seq: 3,
            }),
            "to = min(from+batch-1, max_seq) = min(3, 5) = 3, never past to_seq",
        );

        let read = db.open_read().expect("read conn");
        assert_eq!(
            consolidation_run_state(&read, "run-1").expect("state"),
            Some(RunState::Running),
            "opened run is already running with a lease"
        );
    }

    #[tokio::test]
    async fn open_next_run_returns_existing_instead_of_opening_a_second_row() {
        let (_home, db) = open_state();
        seed_envelopes(&db, "sess-1", 5).await;

        db.writer()
            .transaction(|tx| {
                open_next_run(tx, "run-1", "sess-1", 2, "v1", LEASE_DURATION_MS, 1_000)
            })
            .await
            .expect("open tx");

        let outcome = db
            .writer()
            .transaction(|tx| {
                open_next_run(tx, "run-2", "sess-1", 2, "v1", LEASE_DURATION_MS, 2_000)
            })
            .await
            .expect("open tx");
        match outcome {
            SnapshotOutcome::Existing {
                window,
                state,
                lease_until,
            } => {
                assert_eq!(window.run_id, "run-1", "the first run, not a new run-2 row");
                assert_eq!(state, RunState::Running);
                assert_eq!(lease_until, Some(1_000 + LEASE_DURATION_MS));
            }
            other => panic!("expected Existing, got {other:?}"),
        }

        let read = db.open_read().expect("read conn");
        assert_eq!(
            consolidation_run_state(&read, "run-2").expect("state"),
            None,
            "no second row was ever created"
        );
    }

    #[tokio::test]
    async fn retry_run_legal_on_running_and_failed_illegal_on_applied() {
        let (_home, db) = open_state();
        seed_envelopes(&db, "sess-1", 3).await;
        db.writer()
            .transaction(|tx| {
                open_next_run(tx, "run-1", "sess-1", 3, "v1", LEASE_DURATION_MS, 1_000)
            })
            .await
            .expect("open tx");

        // running -> running (self-loop), fresh lease.
        let outcome = db
            .writer()
            .transaction(|tx| retry_run(tx, "run-1", LEASE_DURATION_MS, 5_000))
            .await
            .expect("retry tx");
        assert_eq!(outcome, Ok(()));
        let read = db.open_read().expect("read conn");
        assert_eq!(
            consolidation_run_state(&read, "run-1").expect("state"),
            Some(RunState::Running)
        );
        drop(read);

        // failed -> running.
        db.writer()
            .transaction(|tx| transition_run(tx, "run-1", RunState::Failed, 6_000))
            .await
            .expect("transition tx")
            .expect("running -> failed is legal");
        let outcome = db
            .writer()
            .transaction(|tx| retry_run(tx, "run-1", LEASE_DURATION_MS, 7_000))
            .await
            .expect("retry tx");
        assert_eq!(outcome, Ok(()));

        // applied -> running is illegal.
        db.writer()
            .transaction(|tx| transition_run(tx, "run-1", RunState::Applied, 8_000))
            .await
            .expect("transition tx")
            .expect("running -> applied is legal");
        let outcome = db
            .writer()
            .transaction(|tx| retry_run(tx, "run-1", LEASE_DURATION_MS, 9_000))
            .await
            .expect("retry tx");
        assert_eq!(
            outcome,
            Err(RunTransitionError::Illegal(IllegalRunTransition {
                from: RunState::Applied,
                to: RunState::Running,
            }))
        );
    }

    #[tokio::test]
    async fn stale_runs_selects_failed_and_lease_expired_running_only() {
        let (_home, db) = open_state();

        db.writer()
            .transaction(|tx| {
                create_consolidation_run(
                    tx,
                    &NewConsolidationRun {
                        run_id: "run-expired",
                        session_id: "sess-1",
                        from_received_seq: 1,
                        to_received_seq: 5,
                        router_version: "v1",
                    },
                    1_000,
                )?;
                transition_run(tx, "run-expired", RunState::Running, 1_000)?.expect("legal");
                acquire_lease(tx, "run-expired", 2_000)?; // already in the past at now=5_000

                create_consolidation_run(
                    tx,
                    &NewConsolidationRun {
                        run_id: "run-live",
                        session_id: "sess-2",
                        from_received_seq: 1,
                        to_received_seq: 5,
                        router_version: "v1",
                    },
                    1_000,
                )?;
                transition_run(tx, "run-live", RunState::Running, 1_000)?.expect("legal");
                acquire_lease(tx, "run-live", 10_000)?; // still live at now=5_000

                create_consolidation_run(
                    tx,
                    &NewConsolidationRun {
                        run_id: "run-failed",
                        session_id: "sess-3",
                        from_received_seq: 1,
                        to_received_seq: 5,
                        router_version: "v1",
                    },
                    1_000,
                )?;
                transition_run(tx, "run-failed", RunState::Running, 1_000)?.expect("legal");
                transition_run(tx, "run-failed", RunState::Failed, 1_500)?.expect("legal");

                create_consolidation_run(
                    tx,
                    &NewConsolidationRun {
                        run_id: "run-applied",
                        session_id: "sess-4",
                        from_received_seq: 1,
                        to_received_seq: 5,
                        router_version: "v1",
                    },
                    1_000,
                )?;
                transition_run(tx, "run-applied", RunState::Running, 1_000)?.expect("legal");
                transition_run(tx, "run-applied", RunState::Applied, 1_500)?.expect("legal");
                Ok(())
            })
            .await
            .expect("seed runs");

        let read = db.open_read().expect("read conn");
        let stale = stale_runs(&read, 5_000).expect("stale runs");
        let mut ids: Vec<String> = stale.iter().map(|r| r.run_id.clone()).collect();
        ids.sort();
        assert_eq!(
            ids,
            vec!["run-expired".to_string(), "run-failed".to_string()]
        );

        // The window bounds must round-trip too (T15-01 builds a `RunWindow`
        // straight from these, with no second read).
        let expired = stale
            .iter()
            .find(|r| r.run_id == "run-expired")
            .expect("run-expired present");
        assert_eq!(expired.from_received_seq, 1);
        assert_eq!(expired.to_received_seq, 5);
    }
}
