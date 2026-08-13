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

/// How many envelopes are past `session_id`'s processing cursor, not yet
/// swept into any consolidation window (spec 07 §6's "queue size threshold"
/// trigger, D-024). `0` for an unknown session or a cursor already caught up.
///
/// D-052: a genuine row count, not `MAX(received_seq) - cursor`.
/// `observation_envelope.received_seq` is a single `AUTOINCREMENT` shared by
/// every session (spec 03's own envelope table), not a per-session counter —
/// the distance between a session's cursor and its own latest `received_seq`
/// includes every other session's interleaved inserts in between, and
/// overcounts by exactly that amount whenever more than one session writes
/// concurrently. Counting rows directly is served by the same
/// `envelope_session(session_id, received_seq)` index this query already
/// relies on for the range scan.
pub fn pending_backlog(conn: &Connection, session_id: &str) -> rusqlite::Result<i64> {
    let cursor = processing_cursor(conn, session_id)?.unwrap_or(0);
    conn.query_row(
        "SELECT COUNT(*) FROM observation_envelope WHERE session_id = ?1 AND received_seq > ?2",
        params![session_id, cursor],
        |r| r.get(0),
    )
}

/// D-061: whether `session_id` already has an un-consolidated `Stop`/`SessionEnd`
/// envelope past its cursor, evaluated straight against persisted state rather
/// than a single import call's own transient report.
///
/// `consolidation_trigger_tick`'s checkpoint signal used to come solely from
/// *that tick's own* `import_session_tail` call's `saw_stop`/`saw_session_end`
/// — but the daemon-startup catch-up pass (`resume::resume_spool_import`) and
/// this worker's first tick independently race to import the same session's
/// spool tail, and only whichever call actually consumes the fresh bytes ever
/// observes the checkpoint. The loser sees nothing new and silently drops the
/// signal forever, leaving the size-threshold fallback (`pending_backlog`) as
/// the only remaining path — which a session that has already ended will
/// never cross. Asking the database directly is race-free: it doesn't matter
/// which call imported the row, only that it is there, unconsolidated.
pub fn has_unconsolidated_checkpoint(
    conn: &Connection,
    session_id: &str,
) -> rusqlite::Result<bool> {
    let cursor = processing_cursor(conn, session_id)?.unwrap_or(0);
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM observation_envelope \
         WHERE session_id = ?1 AND received_seq > ?2 \
           AND event_type IN ('Stop', 'SessionEnd'))",
        params![session_id, cursor],
        |r| r.get(0),
    )
}

/// X-005: whether `session_id` has gone idle for at least `idle_threshold_ms`
/// — its newest `observation_envelope.source_timestamp` is that old or
/// older. Used as a third, implicit-`Stop` checkpoint trigger alongside
/// [`has_unconsolidated_checkpoint`]/the queue-size threshold: a session
/// that crashes before ever sending a real `Stop`/`SessionEnd` (spool
/// captures only `SessionStart`) would otherwise never checkpoint at all —
/// no further observation ever arrives to grow its backlog toward the size
/// threshold either.
///
/// `source_timestamp` is nullable (client-supplied, spec 03's envelope
/// table); a session with no timestamped rows at all returns `false` — a
/// safe default, not a forced checkpoint on data we cannot evaluate.
pub fn session_idle_since(
    conn: &Connection,
    session_id: &str,
    now_ms: i64,
    idle_threshold_ms: i64,
) -> rusqlite::Result<bool> {
    let newest: Option<i64> = conn.query_row(
        "SELECT MAX(source_timestamp) FROM observation_envelope WHERE session_id = ?1",
        params![session_id],
        |r| r.get(0),
    )?;
    Ok(newest.is_some_and(|ts| now_ms - ts >= idle_threshold_ms))
}

/// Every `session_id` whose [`pending_backlog`] is non-zero, ascending — the
/// same "past the cursor" criterion, evaluated for all sessions at once
/// (D-040).
///
/// The consolidation trigger's other session source
/// (`known_spool_sessions`) enumerates the spool *directory*, so it is blind
/// to a session whose envelopes only ever arrived through a daemon-internal
/// write that bypasses the spool (`give_feedback`, spec 11 §2). This is the
/// state-side enumeration that closes that gap: it sees an envelope however
/// it was inserted.
pub fn sessions_with_pending_backlog(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    // Grouped rather than a per-row `WHERE received_seq > cursor` scan: this
    // shape lets the `envelope_session(session_id, received_seq)` index serve
    // both the grouping and each group's `MAX`, and evaluates the cursor
    // lookup once per session instead of once per envelope.
    let mut stmt = conn.prepare(
        "SELECT e.session_id FROM observation_envelope e \
         GROUP BY e.session_id \
         HAVING MAX(e.received_seq) > COALESCE( \
             (SELECT c.last_consolidated_received_seq FROM processing_cursor c \
               WHERE c.session_id = e.session_id), 0) \
         ORDER BY e.session_id",
    )?;
    let ids = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(ids)
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
    /// D-058: the blocking row is a context-overflow dead-letter that has
    /// already been shrunk to a single observation, and even that alone
    /// still overflows the model's context — there is no narrower window
    /// left to try. `dead_letter_run_id` names the permanent, durable
    /// record; per the product decision behind this variant, this session
    /// stays blocked here for a human to review rather than being silently
    /// skipped or auto-retracted.
    Unconsolidatable {
        window: RunWindow,
        dead_letter_run_id: String,
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

/// Create, open, and lease a fresh window of up to `batch` of `session_id`'s
/// own envelopes past its cursor — the actual window-opening mechanics
/// [`open_next_run`] composes for both its normal path and its D-058
/// shrink-and-retry path. Never checks for a blocking existing row; callers
/// establish that themselves.
///
/// `from = cursor + 1`, `to` = the `received_seq` of the `batch`-th of *this
/// session's own* envelopes at or past `from` (capped at `max_received_seq`
/// when fewer than `batch` remain); [`SnapshotOutcome::NothingPending`] if
/// there is no envelope past the cursor at all.
fn open_window(
    tx: &Transaction<'_>,
    run_id: &str,
    session_id: &str,
    batch: i64,
    router_version: &str,
    lease_ms: i64,
    now_ms: i64,
) -> rusqlite::Result<SnapshotOutcome> {
    let cursor = processing_cursor(tx, session_id)?.unwrap_or(0);
    let from = cursor + 1;
    let Some(max_seq) = crate::observation::max_received_seq(tx, session_id)? else {
        return Ok(SnapshotOutcome::NothingPending);
    };
    if max_seq < from {
        return Ok(SnapshotOutcome::NothingPending);
    }
    // D-053: `to` is anchored on this session's own `batch`-th row, not
    // `from + batch - 1` — `received_seq` is one sequence shared by every
    // session (D-052's own finding), so a fixed-width slice of it can be
    // almost entirely another session's rows when sessions interleave
    // heavily, opening a window with zero of this session's own envelopes
    // (confirmed live: a real session's cursor sat ~7800 received_seq behind
    // its own next row, needing ~390 empty 20-wide windows — each still a
    // real local LLM call, `router::route()` never short-circuits an empty
    // window — to walk through). Falls back to `max_seq` when fewer than
    // `batch` of this session's own rows remain past `from`.
    let to = tx
        .query_row(
            "SELECT received_seq FROM observation_envelope \
             WHERE session_id = ?1 AND received_seq >= ?2 \
             ORDER BY received_seq LIMIT 1 OFFSET ?3",
            params![session_id, from, batch.max(1) - 1],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(max_seq);

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

/// Whether — and how — a `Failed` row blocking [`open_next_run`] should be
/// superseded by a narrower window instead of left as-is (D-058).
enum DeadLetterShrink {
    /// Not a context-overflow dead-letter on the current build: leave the
    /// blocking row exactly as [`SnapshotOutcome::Existing`] already did
    /// before D-058 — this must never widen to a failure class D-050 already
    /// owns (a corrective-reprompt parse failure, a materialization
    /// rejection, a still-live lease, a `Transient` backoff).
    NotEligible,
    /// Open a new window at the same starting point with this many of the
    /// session's own observations — half of the dead-lettered window's own
    /// count, floored at 1.
    Shrink(i64),
    /// The dead-lettered window was already a single observation and it
    /// alone still overflows the model's context — no narrower window is
    /// possible.
    Floor,
}

/// D-058: decide whether `blocking_run_id` (the row [`open_next_run`] found
/// via [`latest_non_applied_run`]) is a dead-letter this session can make
/// progress past by retrying narrower, rather than a row that must simply
/// block (a live lease, a `Transient` backoff, or a `Mechanical` failure
/// D-050 already dead-letters permanently for reasons unrelated to context
/// size). Eligible only when *all* of: `Failed`, `last_failure_kind =
/// 'mechanical'`, `last_failure_context_overflow = 1`, and
/// `last_failure_fingerprint = current_build_id` — exactly the row shape
/// [`stale_runs`] itself will never retry again, so this carve-out only ever
/// fires once a window is provably permanently stuck, never on a row that
/// might still resolve on its own.
/// `(last_failure_kind, last_failure_context_overflow, last_failure_fingerprint,
/// session_id, from_received_seq, to_received_seq)` — [`dead_letter_shrink_decision`]'s
/// own row shape, named so clippy's `type_complexity` lint has somewhere to
/// point instead of an inline tuple.
type DeadLetterRow = (Option<String>, bool, Option<String>, String, i64, i64);

fn dead_letter_shrink_decision(
    tx: &Transaction<'_>,
    blocking_run_id: &str,
    blocking_state: RunState,
    current_build_id: &str,
) -> rusqlite::Result<DeadLetterShrink> {
    if blocking_state != RunState::Failed {
        return Ok(DeadLetterShrink::NotEligible);
    }
    let row: Option<DeadLetterRow> = tx
        .query_row(
            "SELECT last_failure_kind, last_failure_context_overflow, last_failure_fingerprint, \
                    session_id, from_received_seq, to_received_seq \
             FROM consolidation_run WHERE run_id = ?1",
            params![blocking_run_id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .optional()?;
    let Some((kind, context_overflow, fingerprint, session_id, from, to)) = row else {
        return Ok(DeadLetterShrink::NotEligible);
    };
    let eligible = kind.as_deref() == Some(FailureKind::Mechanical.as_str())
        && context_overflow
        && fingerprint.as_deref() == Some(current_build_id);
    if !eligible {
        return Ok(DeadLetterShrink::NotEligible);
    }

    let previous_count: i64 = tx.query_row(
        "SELECT COUNT(*) FROM observation_envelope \
         WHERE session_id = ?1 AND received_seq BETWEEN ?2 AND ?3",
        params![session_id, from, to],
        |r| r.get(0),
    )?;
    if previous_count <= 1 {
        Ok(DeadLetterShrink::Floor)
    } else {
        Ok(DeadLetterShrink::Shrink((previous_count / 2).max(1)))
    }
}

/// Open the next consolidation window for `session_id`, or report why not
/// (spec 08 §4 step 1, `[FIXED]`; D-058 amendment below) — one transaction:
///
/// 1. Refuse to open a second run while any non-`applied` row already exists
///    for this session ([`SnapshotOutcome::Existing`]) — **unless** that row
///    is a context-overflow dead-letter on the current build
///    ([`dead_letter_shrink_decision`]), in which case a narrower window
///    opens instead ([`SnapshotOutcome::Opened`]), or, once shrinking has no
///    room left, [`SnapshotOutcome::Unconsolidatable`]. Two concurrent
///    callers for the same `session_id` are fully serialized by
///    [`crate::StateWriter`]'s single-writer queue, so the second caller's
///    own transaction already observes the first's committed row here — no
///    separate read-then-write pre-check, and no TOCTOU gap.
/// 2. Else [`open_window`] with the configured `batch`.
#[allow(clippy::too_many_arguments)]
pub fn open_next_run(
    tx: &Transaction<'_>,
    run_id: &str,
    session_id: &str,
    batch: i64,
    router_version: &str,
    lease_ms: i64,
    now_ms: i64,
    current_build_id: &str,
) -> rusqlite::Result<SnapshotOutcome> {
    if let Some((existing_id, state, lease_until)) = latest_non_applied_run(tx, session_id)? {
        return match dead_letter_shrink_decision(tx, &existing_id, state, current_build_id)? {
            DeadLetterShrink::Shrink(narrower_batch) => open_window(
                tx,
                run_id,
                session_id,
                narrower_batch,
                router_version,
                lease_ms,
                now_ms,
            ),
            DeadLetterShrink::Floor => Ok(SnapshotOutcome::Unconsolidatable {
                window: read_window(tx, &existing_id)?,
                dead_letter_run_id: existing_id,
            }),
            DeadLetterShrink::NotEligible => Ok(SnapshotOutcome::Existing {
                window: read_window(tx, &existing_id)?,
                state,
                lease_until,
            }),
        };
    }

    open_window(
        tx,
        run_id,
        session_id,
        batch,
        router_version,
        lease_ms,
        now_ms,
    )
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

/// `consolidation_run.last_failure_kind` (D-050's retry-storm circuit
/// breaker): whether a `failed` run's most recent failure is expected to
/// reproduce **identically** on an unchanged rebuild (`Mechanical` — a
/// router/schema/prompt code defect, or a fixed generation token budget too
/// small for this window's content; retrying with the same code changes
/// nothing) or might resolve on its own by simply waiting (`Transient` — the
/// generator/model unavailable, an infra error, a concurrent-write race).
/// [`stale_runs`] uses this to stop retrying a `Mechanical` failure every
/// tick forever — dead-lettered until the build fingerprint changes — while
/// still backing off-and-retrying a `Transient` one on a timer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    Mechanical,
    Transient,
}

impl FailureKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FailureKind::Mechanical => "mechanical",
            FailureKind::Transient => "transient",
        }
    }

    /// Parse a stored value; `None` for anything the CHECK constraint
    /// forbids (or a pre-D-050 row that predates this column, `NULL`).
    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "mechanical" => Some(FailureKind::Mechanical),
            "transient" => Some(FailureKind::Transient),
            _ => None,
        }
    }
}

/// A `generate`-closure failure, classified for [`record_run_failure`]'s
/// retry-storm bookkeeping (D-050) — [`crate::memory::runner::run_once`]'s
/// generic `Fut::Output` error type. The actual router
/// (`local_rag_memory::router::route`) constructs one at each of its own
/// failure points; this crate only constructs one itself for the
/// apply-time-rejection default (see `run_once`'s own module doc).
#[derive(Debug, Clone)]
pub struct ClassifiedFailure {
    pub kind: FailureKind,
    pub reason: String,
    /// D-058: `Mechanical` caused specifically by a deterministic model-
    /// context overflow (D-057) — never set for `Transient`. Narrower than
    /// `kind == Mechanical` alone: [`open_next_run`]'s shrink-and-retry
    /// carve-out only ever fires on this exact shape, not on the corrective-
    /// reprompt-parse-failure/materialization-rejection `Mechanical` causes
    /// D-050 already dead-letters permanently.
    pub context_overflow: bool,
}

impl ClassifiedFailure {
    pub fn mechanical(reason: impl Into<String>) -> Self {
        Self {
            kind: FailureKind::Mechanical,
            reason: reason.into(),
            context_overflow: false,
        }
    }

    /// A `Mechanical` failure specifically caused by a deterministic model-
    /// context overflow (D-057/D-058) — see [`Self::context_overflow`].
    pub fn mechanical_context_overflow(reason: impl Into<String>) -> Self {
        Self {
            kind: FailureKind::Mechanical,
            reason: reason.into(),
            context_overflow: true,
        }
    }

    pub fn transient(reason: impl Into<String>) -> Self {
        Self {
            kind: FailureKind::Transient,
            reason: reason.into(),
            context_overflow: false,
        }
    }
}

impl std::fmt::Display for ClassifiedFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.reason)
    }
}

impl std::error::Error for ClassifiedFailure {}

/// D-050's transient-failure backoff table — the same shape (250ms base,
/// doubling, capped) `local-rag-proxy::connect::DEFAULT_BACKOFF` already
/// established for "wait, then retry a call that might just be temporarily
/// down" (a daemon binary never depends on the proxy binary, so this is the
/// same well-tested progression reproduced at this call site, not shared
/// code across that boundary).
pub const TRANSIENT_BACKOFF_BASE_MS: i64 = 250;
pub const TRANSIENT_BACKOFF_CAP_MS: i64 = 4_000;

/// `attempt_count` (1-based: the first recorded failure is attempt 1) to a
/// backoff delay in ms: `0` for the first attempt (most transient failures
/// are momentary — retry promptly), then [`TRANSIENT_BACKOFF_BASE_MS`]
/// doubling, capped at [`TRANSIENT_BACKOFF_CAP_MS`].
pub fn transient_backoff_delay_ms(attempt_count: i64) -> i64 {
    if attempt_count <= 1 {
        return 0;
    }
    let exponent = (attempt_count - 2).clamp(0, 62) as u32;
    TRANSIENT_BACKOFF_BASE_MS
        .saturating_mul(1_i64 << exponent)
        .min(TRANSIENT_BACKOFF_CAP_MS)
}

/// Transition `run_id` to `Failed` (spec 04 §4: "router/LLM error ⇒ failed
/// (retryable)") and record D-050's retry-storm circuit-breaker bookkeeping
/// in the same write: `last_failure_kind`/`last_failure_reason`, the build
/// fingerprint that produced a `Mechanical` failure (`current_fingerprint`
/// is ignored for `Transient` — fingerprint-gating never applies to it,
/// [`stale_runs`] always stores `NULL`), the bumped `attempt_count`, and —
/// `Transient` only — `next_retry_at` computed from the new attempt count
/// via [`transient_backoff_delay_ms`]. `Mechanical`'s `next_retry_at` is
/// always `NULL`: it is gated by [`stale_runs`]'s fingerprint comparison,
/// not by time — a `Mechanical` failure that would still fail identically
/// in 4 seconds would still fail identically in 4 years, on the same build.
///
/// Mirrors [`transition_run`]'s own "silently left alone, not an error"
/// contract for a rejected transition (a racing attempt already moved the
/// row elsewhere) — the bookkeeping columns are only written when the
/// transition itself actually lands; a run some other attempt already
/// carried to `applied` must never be overwritten with stale failure info.
pub fn record_run_failure(
    tx: &Transaction<'_>,
    run_id: &str,
    kind: FailureKind,
    reason: &str,
    context_overflow: bool,
    current_fingerprint: Option<&str>,
    now_ms: i64,
) -> rusqlite::Result<Result<(), RunTransitionError>> {
    if let Err(e) = transition_run(tx, run_id, RunState::Failed, now_ms)? {
        return Ok(Err(e));
    }
    let prior_attempts: i64 = tx.query_row(
        "SELECT attempt_count FROM consolidation_run WHERE run_id = ?1",
        params![run_id],
        |r| r.get(0),
    )?;
    let attempt_count = prior_attempts + 1;
    let (fingerprint, next_retry_at) = match kind {
        FailureKind::Mechanical => (current_fingerprint, None),
        FailureKind::Transient => (
            None,
            Some(now_ms + transient_backoff_delay_ms(attempt_count)),
        ),
    };
    tx.execute(
        "UPDATE consolidation_run SET \
           last_failure_kind = ?2, last_failure_reason = ?3, last_failure_fingerprint = ?4, \
           attempt_count = ?5, next_retry_at = ?6, last_failure_context_overflow = ?7 \
         WHERE run_id = ?1",
        params![
            run_id,
            kind.as_str(),
            reason,
            fingerprint,
            attempt_count,
            next_retry_at,
            context_overflow,
        ],
    )?;
    Ok(Ok(()))
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
/// own "(retryable)" label), or `running` with an expired lease — **minus**
/// D-050's retry-storm circuit breaker's two exclusions:
///
/// - A `failed` row whose last failure was [`FailureKind::Mechanical`] **and**
///   whose `last_failure_fingerprint` matches `current_build_id` is
///   dead-lettered: retrying it would re-run the exact same code against the
///   exact same window and reproduce the exact same failure (already proven —
///   that is what `Mechanical` means). It becomes eligible again the moment
///   `current_build_id` changes (a rebuild), for exactly one more attempt.
/// - A `failed` row still short of its `next_retry_at` (set only for
///   [`FailureKind::Transient`] — [`record_run_failure`]'s exponential
///   backoff) is skipped until that deadline passes.
///
/// A row with `last_failure_kind IS NULL` (a pre-D-050 row this migration
/// left un-backfilled, or a `failed` row from a version-9-vintage code path
/// that never called [`record_run_failure`]) matches neither exclusion — the
/// safe default is "never classified, always retry-eligible," not a special
/// case. `current_build_id` is assumed non-empty (`local_rag_core::BUILD_ID`
/// always is — real `git describe` output or the literal fallback
/// `"unknown"`), which is what makes `COALESCE(last_failure_fingerprint, '')`
/// a safe "never accidentally matches" sentinel for a `NULL` fingerprint.
///
/// As-built decision (T14-06, `[SPEC]`): the runner (`crate::memory::runner`)
/// routes *any* apply-time rejection straight to `failed` rather than leaving
/// the row `running` for a lease timeout to eventually rediscover — retrying
/// immediately lets the next attempt's generator see current state instead of
/// deterministically reproducing the same rejection for up to
/// [`LEASE_DURATION_MS`]. Consequently this sweep must select both cases, not
/// lease-expiry alone, or a `failed` run would never be picked back up.
pub fn stale_runs(
    conn: &Connection,
    now_ms: i64,
    current_build_id: &str,
) -> rusqlite::Result<Vec<StaleRun>> {
    let mut stmt = conn.prepare(
        "SELECT run_id, session_id, state, lease_until, from_received_seq, to_received_seq \
         FROM consolidation_run \
         WHERE (state = 'running' AND (lease_until IS NULL OR lease_until <= ?1)) \
            OR ( \
                 state = 'failed' \
                 AND NOT ( \
                       COALESCE(last_failure_kind, '') = 'mechanical' \
                       AND COALESCE(last_failure_fingerprint, '') = ?2 \
                     ) \
                 AND (next_retry_at IS NULL OR next_retry_at <= ?1) \
               ) \
         ORDER BY created_at",
    )?;
    let rows = stmt
        .query_map(params![now_ms, current_build_id], |r| {
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

/// One session stuck in D-058's floor case (`SnapshotOutcome::
/// Unconsolidatable`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnconsolidatableSession {
    pub session_id: String,
    pub dead_letter_run_id: String,
    pub from_received_seq: i64,
    pub to_received_seq: i64,
}

/// Every session currently in D-058's floor case, store-wide (D-058: `local-
/// rag stats` visibility for the exact incident this task fixes — the prior
/// retry-storm was only found by reading `state.sqlite` directly, because
/// nothing surfaced it). Not every context-overflow dead-letter: most are
/// resolved automatically by [`open_next_run`]'s shrink-and-retry carve-out
/// on the next tick. Only a session whose latest non-`applied` run is a
/// context-overflow dead-letter on `current_build_id` **and** whose own
/// window is already down to a single observation — the same condition
/// [`open_next_run`] itself uses to return
/// [`SnapshotOutcome::Unconsolidatable`] — makes no further progress without
/// a human decision.
pub fn unconsolidatable_sessions(
    conn: &Connection,
    current_build_id: &str,
) -> rusqlite::Result<Vec<UnconsolidatableSession>> {
    let mut stmt = conn.prepare(
        "SELECT c.session_id, c.run_id, c.from_received_seq, c.to_received_seq \
         FROM consolidation_run c \
         WHERE c.state = 'failed' \
           AND c.last_failure_kind = 'mechanical' \
           AND c.last_failure_context_overflow = 1 \
           AND c.last_failure_fingerprint = ?1 \
           AND c.created_at = ( \
             SELECT MAX(c2.created_at) FROM consolidation_run c2 \
             WHERE c2.session_id = c.session_id AND c2.state != 'applied' \
           ) \
           AND ( \
             SELECT COUNT(*) FROM observation_envelope e \
             WHERE e.session_id = c.session_id \
               AND e.received_seq BETWEEN c.from_received_seq AND c.to_received_seq \
           ) <= 1 \
         ORDER BY c.session_id",
    )?;
    stmt.query_map(params![current_build_id], |r| {
        Ok(UnconsolidatableSession {
            session_id: r.get(0)?,
            dead_letter_run_id: r.get(1)?,
            from_received_seq: r.get(2)?,
            to_received_seq: r.get(3)?,
        })
    })?
    .collect()
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
                open_next_run(
                    tx,
                    "run-1",
                    "sess-1",
                    10,
                    "v1",
                    LEASE_DURATION_MS,
                    1_000,
                    "build-test",
                )
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
                open_next_run(
                    tx,
                    "run-2",
                    "sess-2",
                    10,
                    "v1",
                    LEASE_DURATION_MS,
                    1_000,
                    "build-test",
                )
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
                open_next_run(
                    tx,
                    "run-1",
                    "sess-1",
                    3,
                    "v1",
                    LEASE_DURATION_MS,
                    1_000,
                    "build-test",
                )
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
            "to = received_seq of sess-1's own 3rd row from `from` = 3, never past max_seq",
        );

        let read = db.open_read().expect("read conn");
        assert_eq!(
            consolidation_run_state(&read, "run-1").expect("state"),
            Some(RunState::Running),
            "opened run is already running with a lease"
        );
    }

    #[tokio::test]
    async fn open_next_run_skips_another_sessions_interleaved_rows_to_reach_a_full_batch() {
        // D-053 regression: `received_seq` is one sequence shared by every
        // session (same finding as D-052). Interleave sess-a's rows across a
        // wide span dominated by sess-b so the pre-fix `to = from + batch - 1`
        // would open an empty window (batch=2, from=1..=2 is entirely
        // sess-b's own rows) instead of reaching sess-a's own 2 rows.
        let (_home, db) = open_state();
        db.writer()
            .transaction(|tx| {
                for (i, session_id) in ["sess-b", "sess-b", "sess-b", "sess-a", "sess-b", "sess-a"]
                    .into_iter()
                    .enumerate()
                {
                    tx.execute(
                        "INSERT INTO observation_envelope \
                           (observation_id, source_event_id, payload_hash, event_type, \
                            evidence_kind, trust, session_id) \
                         VALUES (?1, ?2, 'deadbeef', 'Stop', 'user_statement', 'normal', ?3)",
                        params![format!("obs-{i}"), format!("evt-{i}"), session_id],
                    )?;
                }
                Ok(())
            })
            .await
            .expect("seed interleaved envelopes");

        // sess-a's own rows land at received_seq 4 and 6.
        let outcome = db
            .writer()
            .transaction(|tx| {
                open_next_run(
                    tx,
                    "run-a",
                    "sess-a",
                    2,
                    "v1",
                    LEASE_DURATION_MS,
                    1_000,
                    "build-test",
                )
            })
            .await
            .expect("open tx");
        assert_eq!(
            outcome,
            SnapshotOutcome::Opened(RunWindow {
                run_id: "run-a".to_string(),
                session_id: "sess-a".to_string(),
                from_received_seq: 1,
                to_received_seq: 6,
            }),
            "window reaches sess-a's own 2nd row (received_seq 6), not from+batch-1=2 \
             (which would be entirely sess-b's rows)",
        );

        let SnapshotOutcome::Opened(window) = outcome else {
            unreachable!("asserted Opened above");
        };
        let read = db.open_read().expect("read conn");
        assert_eq!(
            crate::observation::envelopes_in_range(
                &read,
                "sess-a",
                window.from_received_seq,
                window.to_received_seq
            )
            .expect("envelopes in range")
            .len(),
            2,
            "the opened window actually contains both of sess-a's own rows, not zero"
        );
    }

    #[tokio::test]
    async fn open_next_run_returns_existing_instead_of_opening_a_second_row() {
        let (_home, db) = open_state();
        seed_envelopes(&db, "sess-1", 5).await;

        db.writer()
            .transaction(|tx| {
                open_next_run(
                    tx,
                    "run-1",
                    "sess-1",
                    2,
                    "v1",
                    LEASE_DURATION_MS,
                    1_000,
                    "build-test",
                )
            })
            .await
            .expect("open tx");

        let outcome = db
            .writer()
            .transaction(|tx| {
                open_next_run(
                    tx,
                    "run-2",
                    "sess-1",
                    2,
                    "v1",
                    LEASE_DURATION_MS,
                    2_000,
                    "build-test",
                )
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

    /// D-058: the actual bug this task fixes. A window that structurally
    /// cannot fit the model's context is halved and retried at the same
    /// starting point until it either succeeds or hits a single observation
    /// — proving forward progress without data loss, that every
    /// dead-lettered row along the way survives untouched (the audit trail
    /// this design promises), and that `pending_backlog` stays correct no
    /// matter how many dead-lettered rows pile up (D-052's own row-count
    /// definition, not `consolidation_run`-row-count).
    #[tokio::test]
    async fn open_next_run_shrinks_a_context_overflow_dead_letter_until_it_fits_or_floors() {
        let (_home, db) = open_state();
        seed_envelopes(&db, "sess-1", 8).await;

        async fn dead_letter(db: &crate::StateDb, run_id: &str, now_ms: i64) {
            let run_id = run_id.to_string();
            db.writer()
                .transaction(move |tx| {
                    record_run_failure(
                        tx,
                        &run_id,
                        FailureKind::Mechanical,
                        "request needs 99999 tokens, model context is 32768",
                        true,
                        Some("build-1"),
                        now_ms,
                    )
                })
                .await
                .expect("record tx")
                .expect("legal running -> failed");
        }

        // 8 -> opens the full window.
        let opened = db
            .writer()
            .transaction(|tx| {
                open_next_run(
                    tx,
                    "run-8",
                    "sess-1",
                    8,
                    "v1",
                    LEASE_DURATION_MS,
                    1_000,
                    "build-1",
                )
            })
            .await
            .expect("open tx");
        assert_eq!(
            opened,
            SnapshotOutcome::Opened(RunWindow {
                run_id: "run-8".to_string(),
                session_id: "sess-1".to_string(),
                from_received_seq: 1,
                to_received_seq: 8,
            })
        );
        dead_letter(&db, "run-8", 2_000).await;

        // 8 -> 4, same starting point, batch argument (8) ignored in favor
        // of the dead-lettered window's own halved count.
        let outcome = db
            .writer()
            .transaction(|tx| {
                open_next_run(
                    tx,
                    "run-4",
                    "sess-1",
                    8,
                    "v1",
                    LEASE_DURATION_MS,
                    3_000,
                    "build-1",
                )
            })
            .await
            .expect("open tx");
        assert_eq!(
            outcome,
            SnapshotOutcome::Opened(RunWindow {
                run_id: "run-4".to_string(),
                session_id: "sess-1".to_string(),
                from_received_seq: 1,
                to_received_seq: 4,
            }),
            "halves the dead-lettered window's own 8-observation count to 4"
        );
        dead_letter(&db, "run-4", 4_000).await;

        // 4 -> 2.
        let outcome = db
            .writer()
            .transaction(|tx| {
                open_next_run(
                    tx,
                    "run-2",
                    "sess-1",
                    8,
                    "v1",
                    LEASE_DURATION_MS,
                    5_000,
                    "build-1",
                )
            })
            .await
            .expect("open tx");
        assert_eq!(
            outcome,
            SnapshotOutcome::Opened(RunWindow {
                run_id: "run-2".to_string(),
                session_id: "sess-1".to_string(),
                from_received_seq: 1,
                to_received_seq: 2,
            })
        );
        dead_letter(&db, "run-2", 6_000).await;

        // 2 -> 1 (floored at 1, not 0).
        let outcome = db
            .writer()
            .transaction(|tx| {
                open_next_run(
                    tx,
                    "run-1b",
                    "sess-1",
                    8,
                    "v1",
                    LEASE_DURATION_MS,
                    7_000,
                    "build-1",
                )
            })
            .await
            .expect("open tx");
        assert_eq!(
            outcome,
            SnapshotOutcome::Opened(RunWindow {
                run_id: "run-1b".to_string(),
                session_id: "sess-1".to_string(),
                from_received_seq: 1,
                to_received_seq: 1,
            })
        );
        dead_letter(&db, "run-1b", 8_000).await;

        // A single observation, alone, still overflows: no narrower window
        // is possible — Unconsolidatable, not another shrink.
        let outcome = db
            .writer()
            .transaction(|tx| {
                open_next_run(
                    tx,
                    "run-final",
                    "sess-1",
                    8,
                    "v1",
                    LEASE_DURATION_MS,
                    9_000,
                    "build-1",
                )
            })
            .await
            .expect("open tx");
        assert_eq!(
            outcome,
            SnapshotOutcome::Unconsolidatable {
                window: RunWindow {
                    run_id: "run-1b".to_string(),
                    session_id: "sess-1".to_string(),
                    from_received_seq: 1,
                    to_received_seq: 1,
                },
                dead_letter_run_id: "run-1b".to_string(),
            }
        );

        let read = db.open_read().expect("read conn");
        assert_eq!(
            consolidation_run_state(&read, "run-final").expect("state"),
            None,
            "the floor case creates no new row"
        );
        for run_id in ["run-8", "run-4", "run-2", "run-1b"] {
            assert_eq!(
                consolidation_run_state(&read, run_id).expect("state"),
                Some(RunState::Failed),
                "{run_id} stays failed, never deleted or overwritten — the audit trail"
            );
        }
        assert_eq!(
            pending_backlog(&read, "sess-1").expect("backlog"),
            8,
            "cursor never advanced (nothing ever applied) — all 8 observations still pending"
        );

        let stuck = unconsolidatable_sessions(&read, "build-1").expect("query");
        assert_eq!(stuck.len(), 1);
        assert_eq!(stuck[0].session_id, "sess-1");
        assert_eq!(stuck[0].dead_letter_run_id, "run-1b");
    }

    /// Regression guard: a `Mechanical` dead-letter for a reason other than
    /// context overflow (D-050's original two causes — a corrective-reprompt
    /// parse failure, a materialization rejection) must never be shrunk —
    /// the D-058 carve-out is narrower than `kind == Mechanical` alone.
    #[tokio::test]
    async fn open_next_run_does_not_shrink_a_non_context_overflow_mechanical_dead_letter() {
        let (_home, db) = open_state();
        seed_envelopes(&db, "sess-1", 8).await;
        db.writer()
            .transaction(|tx| {
                open_next_run(
                    tx,
                    "run-1",
                    "sess-1",
                    8,
                    "v1",
                    LEASE_DURATION_MS,
                    1_000,
                    "build-1",
                )
            })
            .await
            .expect("open tx");
        db.writer()
            .transaction(|tx| {
                record_run_failure(
                    tx,
                    "run-1",
                    FailureKind::Mechanical,
                    "missing field confidence_signal",
                    false,
                    Some("build-1"),
                    2_000,
                )
            })
            .await
            .expect("record tx")
            .expect("legal");

        let outcome = db
            .writer()
            .transaction(|tx| {
                open_next_run(
                    tx,
                    "run-2",
                    "sess-1",
                    8,
                    "v1",
                    LEASE_DURATION_MS,
                    3_000,
                    "build-1",
                )
            })
            .await
            .expect("open tx");
        match outcome {
            SnapshotOutcome::Existing { window, .. } => {
                assert_eq!(window.run_id, "run-1", "left exactly as before D-058")
            }
            other => panic!("expected Existing, got {other:?}"),
        }
        let read = db.open_read().expect("read conn");
        assert_eq!(
            consolidation_run_state(&read, "run-2").expect("state"),
            None,
            "no narrower window was opened"
        );
    }

    /// Regression guard: a context-overflow dead-letter from a *different*
    /// build must not shrink — D-050's existing "one free retry per rebuild"
    /// guarantee (`stale_runs_excludes_a_mechanical_failure_on_the_current_build_only`)
    /// must not silently become a shrink cycle instead.
    #[tokio::test]
    async fn open_next_run_does_not_shrink_a_context_overflow_dead_letter_from_a_different_build() {
        let (_home, db) = open_state();
        seed_envelopes(&db, "sess-1", 8).await;
        db.writer()
            .transaction(|tx| {
                open_next_run(
                    tx,
                    "run-1",
                    "sess-1",
                    8,
                    "v1",
                    LEASE_DURATION_MS,
                    1_000,
                    "build-1",
                )
            })
            .await
            .expect("open tx");
        db.writer()
            .transaction(|tx| {
                record_run_failure(
                    tx,
                    "run-1",
                    FailureKind::Mechanical,
                    "request needs 99999 tokens, model context is 32768",
                    true,
                    Some("build-1"),
                    2_000,
                )
            })
            .await
            .expect("record tx")
            .expect("legal");

        let outcome = db
            .writer()
            .transaction(|tx| {
                open_next_run(
                    tx,
                    "run-2",
                    "sess-1",
                    8,
                    "v1",
                    LEASE_DURATION_MS,
                    3_000,
                    "build-2",
                )
            })
            .await
            .expect("open tx");
        match outcome {
            SnapshotOutcome::Existing { window, .. } => {
                assert_eq!(window.run_id, "run-1")
            }
            other => panic!("expected Existing on a different build, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn retry_run_legal_on_running_and_failed_illegal_on_applied() {
        let (_home, db) = open_state();
        seed_envelopes(&db, "sess-1", 3).await;
        db.writer()
            .transaction(|tx| {
                open_next_run(
                    tx,
                    "run-1",
                    "sess-1",
                    3,
                    "v1",
                    LEASE_DURATION_MS,
                    1_000,
                    "build-test",
                )
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
        let stale = stale_runs(&read, 5_000, "build-x").expect("stale runs");
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

    // -----------------------------------------------------------------------
    // D-024: pending_backlog
    // -----------------------------------------------------------------------

    #[test]
    fn pending_backlog_is_zero_for_an_unknown_session() {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(
            "CREATE TABLE processing_cursor (session_id TEXT, last_consolidated_received_seq INTEGER);\n\
             CREATE TABLE observation_envelope (session_id TEXT, received_seq INTEGER);",
        )
        .expect("seed empty tables");
        assert_eq!(pending_backlog(&conn, "nobody").expect("read"), 0);
    }

    #[tokio::test]
    async fn pending_backlog_counts_envelopes_past_the_cursor() {
        let (_home, db) = open_state();
        seed_envelopes(&db, "sess-1", 5).await;

        let read = db.open_read().expect("read conn");
        assert_eq!(
            pending_backlog(&read, "sess-1").expect("backlog"),
            5,
            "no cursor yet: every envelope is pending"
        );
    }

    #[tokio::test]
    async fn pending_backlog_shrinks_after_the_cursor_advances() {
        let (_home, db) = open_state();
        seed_envelopes(&db, "sess-1", 5).await;
        db.writer()
            .transaction(|tx| upsert_processing_cursor(tx, "sess-1", 3))
            .await
            .expect("advance cursor");

        let read = db.open_read().expect("read conn");
        assert_eq!(pending_backlog(&read, "sess-1").expect("backlog"), 2);

        db.writer()
            .transaction(|tx| upsert_processing_cursor(tx, "sess-1", 5))
            .await
            .expect("catch up cursor");
        let read = db.open_read().expect("read conn");
        assert_eq!(
            pending_backlog(&read, "sess-1").expect("backlog"),
            0,
            "cursor caught up to max_received_seq"
        );
    }

    #[tokio::test]
    async fn pending_backlog_counts_only_this_sessions_rows_when_another_session_interleaves() {
        // D-052 regression: `received_seq` is a single AUTOINCREMENT shared by
        // every session, not a per-session counter. Interleave two sessions'
        // inserts so `sess-a`'s own rows land at received_seq 1, 3, 5 (3 rows)
        // while its last row is received_seq 5 — the pre-fix formula
        // (`max_received_seq - cursor` = 5 - 0 = 5) would overcount by
        // exactly the 2 rows `sess-b` inserted in between.
        let (_home, db) = open_state();
        db.writer()
            .transaction(|tx| {
                for (i, session_id) in ["sess-a", "sess-b", "sess-a", "sess-b", "sess-a"]
                    .into_iter()
                    .enumerate()
                {
                    tx.execute(
                        "INSERT INTO observation_envelope \
                           (observation_id, source_event_id, payload_hash, event_type, \
                            evidence_kind, trust, session_id) \
                         VALUES (?1, ?2, 'deadbeef', 'Stop', 'user_statement', 'normal', ?3)",
                        params![format!("obs-{i}"), format!("evt-{i}"), session_id],
                    )?;
                }
                Ok(())
            })
            .await
            .expect("seed interleaved envelopes");

        let read = db.open_read().expect("read conn");
        assert_eq!(
            pending_backlog(&read, "sess-a").expect("backlog"),
            3,
            "sess-a has 3 of its own rows, not received_seq 5 (its own max) minus cursor 0"
        );
        assert_eq!(
            pending_backlog(&read, "sess-b").expect("backlog"),
            2,
            "sess-b has 2 of its own rows"
        );
    }

    // -----------------------------------------------------------------------
    // D-040: sessions_with_pending_backlog
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn sessions_with_pending_backlog_lists_only_sessions_past_their_own_cursor() {
        let (_home, db) = open_state();
        seed_envelopes(&db, "sess-b", 2).await;
        seed_envelopes(&db, "sess-a", 3).await;

        let read = db.open_read().expect("read conn");
        assert_eq!(
            sessions_with_pending_backlog(&read).expect("enumerate"),
            vec!["sess-a".to_string(), "sess-b".to_string()],
            "no cursor at all still counts as pending, ascending"
        );
        drop(read);

        // sess-b's envelopes are received_seq 1..=2, sess-a's 3..=5.
        db.writer()
            .transaction(|tx| upsert_processing_cursor(tx, "sess-b", 2))
            .await
            .expect("catch sess-b up");
        db.writer()
            .transaction(|tx| upsert_processing_cursor(tx, "sess-a", 4))
            .await
            .expect("advance sess-a partway");

        let read = db.open_read().expect("read conn");
        assert_eq!(
            sessions_with_pending_backlog(&read).expect("enumerate"),
            vec!["sess-a".to_string()],
            "a caught-up session drops out; a partially consolidated one stays"
        );

        assert!(
            sessions_with_pending_backlog(&read)
                .expect("enumerate")
                .iter()
                .all(|s| pending_backlog(&read, s).expect("backlog") > 0),
            "the same criterion as pending_backlog, per session"
        );
    }

    #[tokio::test]
    async fn sessions_with_pending_backlog_is_empty_with_no_envelopes() {
        let (_home, db) = open_state();
        let read = db.open_read().expect("read conn");
        assert!(
            sessions_with_pending_backlog(&read)
                .expect("enumerate")
                .is_empty()
        );
    }

    // -----------------------------------------------------------------------
    // D-061: has_unconsolidated_checkpoint
    // -----------------------------------------------------------------------

    async fn seed_envelope_with_event_type(
        db: &crate::StateDb,
        session_id: &str,
        observation_id: &str,
        event_type: &str,
    ) {
        let (session_id, observation_id, event_type) = (
            session_id.to_string(),
            observation_id.to_string(),
            event_type.to_string(),
        );
        db.writer()
            .transaction(move |tx| {
                tx.execute(
                    "INSERT INTO observation_envelope \
                       (observation_id, source_event_id, payload_hash, event_type, \
                        evidence_kind, trust, session_id) \
                     VALUES (?1, ?2, 'deadbeef', ?3, 'user_statement', 'normal', ?4)",
                    params![
                        observation_id,
                        format!("evt-{observation_id}"),
                        event_type,
                        session_id
                    ],
                )?;
                Ok(())
            })
            .await
            .expect("seed envelope");
    }

    #[tokio::test]
    async fn has_unconsolidated_checkpoint_is_false_without_a_checkpoint_event_type() {
        let (_home, db) = open_state();
        seed_envelope_with_event_type(&db, "sess-a", "obs-1", "UserPromptSubmit").await;
        seed_envelope_with_event_type(&db, "sess-a", "obs-2", "PostToolUse").await;

        let read = db.open_read().expect("read conn");
        assert!(
            !has_unconsolidated_checkpoint(&read, "sess-a").expect("checkpoint check"),
            "no Stop/SessionEnd row past the cursor"
        );
    }

    #[tokio::test]
    async fn has_unconsolidated_checkpoint_is_true_for_an_already_imported_session_end() {
        // D-061 regression: this is the exact shape of the daemon-startup race
        // — some other caller (`resume::resume_spool_import`) already imported
        // the SessionEnd byte into `observation_envelope` before this tick's
        // own `import_session_tail` call ever ran, so no in-memory
        // `SpoolImportReport` will ever report `saw_session_end` for it again.
        // The persisted check must still see it.
        let (_home, db) = open_state();
        seed_envelope_with_event_type(&db, "sess-a", "obs-1", "UserPromptSubmit").await;
        seed_envelope_with_event_type(&db, "sess-a", "obs-2", "SessionEnd").await;

        let read = db.open_read().expect("read conn");
        assert!(has_unconsolidated_checkpoint(&read, "sess-a").expect("checkpoint check"));
    }

    #[tokio::test]
    async fn has_unconsolidated_checkpoint_is_false_once_the_cursor_passes_it() {
        let (_home, db) = open_state();
        seed_envelope_with_event_type(&db, "sess-a", "obs-1", "SessionEnd").await;
        db.writer()
            .transaction(|tx| upsert_processing_cursor(tx, "sess-a", 1))
            .await
            .expect("consolidate past the checkpoint row");

        let read = db.open_read().expect("read conn");
        assert!(
            !has_unconsolidated_checkpoint(&read, "sess-a").expect("checkpoint check"),
            "already consolidated — not a pending checkpoint anymore"
        );
    }

    #[tokio::test]
    async fn has_unconsolidated_checkpoint_is_false_for_an_unknown_session() {
        let (_home, db) = open_state();
        let read = db.open_read().expect("read conn");
        assert!(!has_unconsolidated_checkpoint(&read, "nobody").expect("checkpoint check"));
    }

    // -----------------------------------------------------------------------
    // X-005: session_idle_since
    // -----------------------------------------------------------------------

    async fn seed_envelope_with_timestamp(
        db: &crate::StateDb,
        session_id: &str,
        observation_id: &str,
        source_timestamp: i64,
    ) {
        let (session_id, observation_id) = (session_id.to_string(), observation_id.to_string());
        db.writer()
            .transaction(move |tx| {
                tx.execute(
                    "INSERT INTO observation_envelope \
                       (observation_id, source_event_id, payload_hash, event_type, \
                        evidence_kind, trust, session_id, source_timestamp) \
                     VALUES (?1, ?2, 'deadbeef', 'UserPromptSubmit', 'user_statement', 'normal', \
                             ?3, ?4)",
                    params![
                        observation_id,
                        format!("evt-{observation_id}"),
                        session_id,
                        source_timestamp
                    ],
                )?;
                Ok(())
            })
            .await
            .expect("seed envelope with timestamp");
    }

    #[tokio::test]
    async fn session_idle_since_is_true_once_the_threshold_has_elapsed() {
        let (_home, db) = open_state();
        seed_envelope_with_timestamp(&db, "sess-a", "obs-1", 1_000).await;

        let read = db.open_read().expect("read conn");
        assert!(
            session_idle_since(&read, "sess-a", 1_000 + 86_400_000, 86_400_000)
                .expect("idle check"),
            "exactly at the threshold counts as idle"
        );
        assert!(
            session_idle_since(&read, "sess-a", 1_000 + 90_000_000, 86_400_000)
                .expect("idle check"),
            "past the threshold counts as idle"
        );
    }

    #[tokio::test]
    async fn session_idle_since_is_false_before_the_threshold_elapses() {
        let (_home, db) = open_state();
        seed_envelope_with_timestamp(&db, "sess-a", "obs-1", 1_000).await;

        let read = db.open_read().expect("read conn");
        assert!(
            !session_idle_since(&read, "sess-a", 1_000 + 1_000, 86_400_000).expect("idle check"),
            "far short of the threshold"
        );
    }

    #[tokio::test]
    async fn session_idle_since_tracks_the_newest_observation_not_the_oldest() {
        let (_home, db) = open_state();
        seed_envelope_with_timestamp(&db, "sess-a", "obs-1", 1_000).await;
        seed_envelope_with_timestamp(&db, "sess-a", "obs-2", 50_000_000).await;

        let read = db.open_read().expect("read conn");
        assert!(
            !session_idle_since(&read, "sess-a", 50_000_000 + 1_000, 86_400_000)
                .expect("idle check"),
            "the session's newest observation is what resets the idle clock"
        );
    }

    #[tokio::test]
    async fn session_idle_since_is_false_for_an_unknown_session() {
        let (_home, db) = open_state();
        let read = db.open_read().expect("read conn");
        assert!(
            !session_idle_since(&read, "nobody", 1_000_000_000, 86_400_000).expect("idle check"),
            "no timestamped rows to evaluate — never force a checkpoint on missing data"
        );
    }

    // -----------------------------------------------------------------------
    // D-050: retry-storm circuit breaker
    // -----------------------------------------------------------------------

    #[test]
    fn failure_kind_round_trips() {
        for kind in [FailureKind::Mechanical, FailureKind::Transient] {
            assert_eq!(FailureKind::from_db(kind.as_str()), Some(kind));
        }
        assert_eq!(FailureKind::from_db("bogus"), None);
        assert_eq!(
            FailureKind::from_db("bogus"),
            None,
            "a pre-D-050 NULL row's absence must parse as None, not panic"
        );
    }

    #[test]
    fn transient_backoff_doubles_from_the_base_and_caps() {
        assert_eq!(transient_backoff_delay_ms(0), 0, "no negative attempt");
        assert_eq!(
            transient_backoff_delay_ms(1),
            0,
            "first attempt: retry promptly"
        );
        assert_eq!(transient_backoff_delay_ms(2), 250);
        assert_eq!(transient_backoff_delay_ms(3), 500);
        assert_eq!(transient_backoff_delay_ms(4), 1_000);
        assert_eq!(transient_backoff_delay_ms(5), 2_000);
        assert_eq!(transient_backoff_delay_ms(6), 4_000);
        assert_eq!(transient_backoff_delay_ms(7), 4_000, "capped");
        assert_eq!(
            transient_backoff_delay_ms(1_000),
            4_000,
            "still capped, no overflow"
        );
    }

    async fn seed_running_run(db: &crate::StateDb, run_id: &str, session_id: &str, now_ms: i64) {
        db.writer()
            .transaction({
                let run_id = run_id.to_string();
                let session_id = session_id.to_string();
                move |tx| {
                    create_consolidation_run(
                        tx,
                        &NewConsolidationRun {
                            run_id: &run_id,
                            session_id: &session_id,
                            from_received_seq: 1,
                            to_received_seq: 5,
                            router_version: "v1",
                        },
                        now_ms,
                    )?;
                    transition_run(tx, &run_id, RunState::Running, now_ms)?.expect("legal");
                    acquire_lease(tx, &run_id, now_ms + LEASE_DURATION_MS)?;
                    Ok(())
                }
            })
            .await
            .expect("seed running run");
    }

    #[tokio::test]
    async fn record_run_failure_writes_mechanical_bookkeeping_with_no_backoff() {
        let (_home, db) = open_state();
        seed_running_run(&db, "run-1", "sess-1", 1_000).await;

        db.writer()
            .transaction(|tx| {
                record_run_failure(
                    tx,
                    "run-1",
                    FailureKind::Mechanical,
                    "missing field confidence_signal",
                    false,
                    Some("build-abc"),
                    5_000,
                )
            })
            .await
            .expect("record tx")
            .expect("legal running -> failed");

        let read = db.open_read().expect("read conn");
        let row: (String, String, String, Option<String>, i64, Option<i64>) = read
            .query_row(
                "SELECT state, last_failure_kind, last_failure_reason, last_failure_fingerprint, \
                        attempt_count, next_retry_at \
                 FROM consolidation_run WHERE run_id = 'run-1'",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .expect("read bookkeeping");
        assert_eq!(row.0, "failed");
        assert_eq!(row.1, "mechanical");
        assert_eq!(row.2, "missing field confidence_signal");
        assert_eq!(row.3, Some("build-abc".to_string()));
        assert_eq!(row.4, 1, "first recorded failure");
        assert_eq!(
            row.5, None,
            "mechanical is gated by fingerprint, never by time"
        );
    }

    #[tokio::test]
    async fn record_run_failure_writes_transient_backoff_with_no_fingerprint() {
        let (_home, db) = open_state();
        seed_running_run(&db, "run-1", "sess-1", 1_000).await;

        db.writer()
            .transaction(|tx| {
                record_run_failure(
                    tx,
                    "run-1",
                    FailureKind::Transient,
                    "no generation provider configured",
                    false,
                    Some("build-abc"),
                    5_000,
                )
            })
            .await
            .expect("record tx")
            .expect("legal running -> failed");

        let read = db.open_read().expect("read conn");
        let row: (String, Option<String>, i64, Option<i64>) = read
            .query_row(
                "SELECT last_failure_kind, last_failure_fingerprint, attempt_count, next_retry_at \
                 FROM consolidation_run WHERE run_id = 'run-1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .expect("read bookkeeping");
        assert_eq!(row.0, "transient");
        assert_eq!(
            row.1, None,
            "transient never stores a fingerprint, even if the caller passed one"
        );
        assert_eq!(row.2, 1);
        assert_eq!(
            row.3,
            Some(5_000 + transient_backoff_delay_ms(1)),
            "next_retry_at = now + backoff(attempt_count)"
        );
    }

    #[tokio::test]
    async fn record_run_failure_increments_attempt_count_across_retries() {
        let (_home, db) = open_state();
        seed_running_run(&db, "run-1", "sess-1", 1_000).await;

        for (attempt, now_ms) in [(1, 5_000), (2, 6_000), (3, 7_000)] {
            db.writer()
                .transaction(move |tx| {
                    // failed -> running (retry_run's own edge) before each
                    // subsequent failure, mirroring the runner's real cycle.
                    if attempt > 1 {
                        retry_run(tx, "run-1", LEASE_DURATION_MS, now_ms - 500)?.expect("legal");
                    }
                    record_run_failure(
                        tx,
                        "run-1",
                        FailureKind::Transient,
                        "transient",
                        false,
                        None,
                        now_ms,
                    )
                })
                .await
                .expect("record tx")
                .expect("legal");

            let read = db.open_read().expect("read conn");
            let count: i64 = read
                .query_row(
                    "SELECT attempt_count FROM consolidation_run WHERE run_id = 'run-1'",
                    [],
                    |r| r.get(0),
                )
                .expect("read attempt_count");
            assert_eq!(count, attempt, "attempt {attempt}");
        }
    }

    #[tokio::test]
    async fn record_run_failure_leaves_an_already_applied_run_untouched() {
        let (_home, db) = open_state();
        seed_running_run(&db, "run-1", "sess-1", 1_000).await;
        db.writer()
            .transaction(|tx| transition_run(tx, "run-1", RunState::Applied, 2_000))
            .await
            .expect("transition tx")
            .expect("running -> applied is legal");

        let outcome = db
            .writer()
            .transaction(|tx| {
                record_run_failure(
                    tx,
                    "run-1",
                    FailureKind::Mechanical,
                    "late",
                    false,
                    None,
                    5_000,
                )
            })
            .await
            .expect("record tx");
        assert!(
            matches!(
                outcome,
                Err(RunTransitionError::Illegal(IllegalRunTransition {
                    from: RunState::Applied,
                    to: RunState::Failed,
                }))
            ),
            "a racing applied run must not be overwritten with stale failure info: {outcome:?}"
        );

        let read = db.open_read().expect("read conn");
        let state: String = read
            .query_row(
                "SELECT state FROM consolidation_run WHERE run_id = 'run-1'",
                [],
                |r| r.get(0),
            )
            .expect("read state");
        assert_eq!(state, "applied", "the applied row was never touched");
    }

    /// The actual bug this whole task fixes: a `Mechanical` failure on the
    /// current build is retried exactly once (the attempt that classified it
    /// as `Mechanical` in the first place), then never again — no matter how
    /// many sweeps run — until the build fingerprint changes.
    #[tokio::test]
    async fn stale_runs_excludes_a_mechanical_failure_on_the_current_build_only() {
        let (_home, db) = open_state();
        seed_running_run(&db, "run-mech", "sess-1", 1_000).await;
        db.writer()
            .transaction(|tx| {
                record_run_failure(
                    tx,
                    "run-mech",
                    FailureKind::Mechanical,
                    "missing field confidence_signal",
                    false,
                    Some("build-1"),
                    2_000,
                )
            })
            .await
            .expect("record tx")
            .expect("legal");

        let read = db.open_read().expect("read conn");
        let stale_same_build = stale_runs(&read, 100_000, "build-1").expect("stale runs");
        assert!(
            stale_same_build.is_empty(),
            "dead-lettered: same build, same fingerprint, no matter how much time passes"
        );

        let stale_new_build = stale_runs(&read, 100_000, "build-2").expect("stale runs");
        assert_eq!(
            stale_new_build.len(),
            1,
            "a rebuild gets exactly one more attempt"
        );
        assert_eq!(stale_new_build[0].run_id, "run-mech");
    }

    #[tokio::test]
    async fn stale_runs_gates_a_transient_failure_on_next_retry_at_not_the_fingerprint() {
        let (_home, db) = open_state();
        seed_running_run(&db, "run-trans", "sess-1", 1_000).await;
        db.writer()
            .transaction(|tx| {
                record_run_failure(
                    tx,
                    "run-trans",
                    FailureKind::Transient,
                    "no generation provider configured",
                    false,
                    Some("build-1"),
                    2_000,
                )
            })
            .await
            .expect("record tx")
            .expect("legal");
        // attempt_count is now 1, so next_retry_at = 2_000 + backoff(1) = 2_000 + 0 = 2_000.

        let read = db.open_read().expect("read conn");
        assert!(
            stale_runs(&read, 1_999, "build-1")
                .expect("stale runs")
                .is_empty(),
            "still before next_retry_at"
        );
        let ready = stale_runs(&read, 2_000, "build-1").expect("stale runs");
        assert_eq!(ready.len(), 1, "at next_retry_at, eligible again");
        assert_eq!(ready[0].run_id, "run-trans");

        // Same build id does NOT dead-letter a transient failure — only
        // mechanical failures are fingerprint-gated.
        let still_ready = stale_runs(&read, 2_000, "build-1").expect("stale runs");
        assert_eq!(still_ready.len(), 1);
    }

    #[tokio::test]
    async fn stale_runs_treats_a_legacy_unclassified_failed_row_as_always_eligible() {
        let (_home, db) = open_state();
        // A `failed` row with none of D-050's columns ever written (as if it
        // predates this migration, or failed via a code path that still only
        // calls the plain `transition_run` — the safe default must not
        // silently dead-letter it).
        db.writer()
            .transaction(|tx| {
                create_consolidation_run(
                    tx,
                    &NewConsolidationRun {
                        run_id: "run-legacy",
                        session_id: "sess-1",
                        from_received_seq: 1,
                        to_received_seq: 5,
                        router_version: "v1",
                    },
                    1_000,
                )?;
                transition_run(tx, "run-legacy", RunState::Running, 1_000)?.expect("legal");
                transition_run(tx, "run-legacy", RunState::Failed, 1_500)?.expect("legal");
                Ok(())
            })
            .await
            .expect("seed legacy failed run");

        let read = db.open_read().expect("read conn");
        let stale = stale_runs(&read, 5_000, "any-build").expect("stale runs");
        assert_eq!(
            stale.len(),
            1,
            "NULL last_failure_kind is never classified as mechanical"
        );
        assert_eq!(stale[0].run_id, "run-legacy");
    }
}
