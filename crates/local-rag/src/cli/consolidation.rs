//! `local-rag consolidation retry|abandon <session-id>` — the operator's two
//! verbs for a consolidation run nothing else will move (`T23-03`,
//! ADR-0014 Decision 1).
//!
//! # Why this command exists at all
//!
//! `D-117`: a `mechanical` dead-letter that is not a context overflow is
//! declined by everything. `stale_runs` excludes it deliberately (`D-050`'s
//! guard, added after one three-observation window was retried 627 times),
//! `dead_letter_shrink_decision` only handles overflows, and until this command
//! nothing else in the product touched a run at all. Spec 08 §4 accepted that
//! cost with an escape — "until the binary is rebuilt" — which a released
//! binary does not have, because its `BUILD_ID` is fixed for the life of the
//! release.
//!
//! # Why it takes a session id
//!
//! Because that is what `stats` prints. After `T23-02` an operator reads
//! `consolidation backlog: session <id> — N observation(s) — PARKED on run
//! <run>` and can act on the identifier in front of them; the command finds the
//! blocking run itself and names it back.
//!
//! # Why it needs no daemon
//!
//! It writes straight to `state.sqlite`, the same direct access the rest of the
//! CLI uses (`T20-08`), and that is not a shortcut: a store that is wedged may
//! well be one whose daemon is unhealthy, and a repair that required a healthy
//! daemon would be unavailable exactly when it is needed. Nothing has to be
//! notified either — `consolidation_trigger_tick` begins with its own resume
//! sweep, so a retried run is picked up on the next tick.

use std::process::ExitCode;

use local_rag::indexing::open_state;
use local_rag_store::{
    AbandonOutcome, BacklogBlocker, RepairError, StateDb, abandon_run, pending_backlog_by_session,
    retry_parked_run,
};

use super::{block_on, fail, resolve_layout_and_config, system_now_ms};

const BIN: &str = "local-rag";

#[derive(Debug, clap::Subcommand)]
pub enum ConsolidationCommand {
    /// Give a parked run one more attempt. Not a loop: if it fails again it
    /// parks again, and asking twice takes two commands.
    Retry {
        /// The session `stats` reported as blocked.
        session_id: String,
    },
    /// Declare the blocking window unconsolidatable and move the session past
    /// it. Destructive: those observations never become memory.
    Abandon {
        /// The session `stats` reported as blocked.
        session_id: String,
    },
}

pub fn run(command: ConsolidationCommand) -> ExitCode {
    match command {
        ConsolidationCommand::Retry { session_id } => run_retry(session_id),
        ConsolidationCommand::Abandon { session_id } => run_abandon(session_id),
    }
}

fn open() -> Result<std::sync::Arc<StateDb>, String> {
    let (layout, _config) = resolve_layout_and_config()?;
    open_state(&layout)
}

/// The run blocking `session_id`, or a message saying why there is nothing to
/// repair.
///
/// Asks the same question `stats` answers, through the same function, so the
/// command can never disagree with the report that sent the operator here.
fn blocking_run(state: &StateDb, session_id: &str) -> Result<String, String> {
    let read = state
        .open_read()
        .map_err(|e| format!("could not open the store for reading: {e}"))?;
    let rows = pending_backlog_by_session(&read, local_rag_core::BUILD_ID)
        .map_err(|e| format!("could not read the consolidation backlog: {e}"))?;
    let Some(row) = rows.iter().find(|r| r.session_id == session_id) else {
        return Err(format!(
            "session {session_id} has no outstanding observations — nothing is blocked. \
             `local-rag stats` lists the sessions that are."
        ));
    };
    match &row.blocker {
        BacklogBlocker::None => Err(format!(
            "session {session_id} is waiting for the next tick, not blocked by a run"
        )),
        BacklogBlocker::InProgress { run_id } => Err(format!(
            "run {run_id} is in flight — nothing to repair while something is already acting on it"
        )),
        BacklogBlocker::Retryable {
            run_id,
            attempt_count,
        } => {
            // Not an error: retrying it by hand is legitimate, it is simply not
            // necessary. Say so and act, rather than refusing on a technicality.
            eprintln!(
                "{BIN}: note — run {run_id} has failed {attempt_count}x and would be retried on \
                 its own; proceeding anyway"
            );
            Ok(run_id.clone())
        }
        BacklogBlocker::Shrinking { run_id, .. } | BacklogBlocker::Floored { run_id } => {
            Ok(run_id.clone())
        }
        BacklogBlocker::Parked { run_id, .. } => Ok(run_id.clone()),
    }
}

fn run_retry(session_id: String) -> ExitCode {
    let state = match open().and_then(|s| {
        let run_id = blocking_run(&s, &session_id)?;
        Ok((s, run_id))
    }) {
        Ok(v) => v,
        Err(e) => return fail(BIN, &e),
    };
    let (state, run_id) = state;
    let rid = run_id.clone();
    let now_ms = system_now_ms();
    let outcome = block_on(async move {
        state
            .writer()
            .transaction(move |tx| retry_parked_run(tx, &rid, now_ms))
            .await
    });
    match outcome {
        Ok(Ok(())) => {
            println!(
                "{BIN}: run {run_id} queued for one more attempt — the daemon picks it up on its \
                 next tick. If it fails again it parks again; ask twice to try twice."
            );
            ExitCode::SUCCESS
        }
        Ok(Err(e)) => fail(BIN, &format!("could not retry run {run_id}: {e}")),
        Err(e) => fail(BIN, &format!("could not retry run {run_id}: {e}")),
    }
}

fn run_abandon(session_id: String) -> ExitCode {
    let state = match open().and_then(|s| {
        let run_id = blocking_run(&s, &session_id)?;
        Ok((s, run_id))
    }) {
        Ok(v) => v,
        Err(e) => return fail(BIN, &e),
    };
    let (state, run_id) = state;
    let rid = run_id.clone();
    let now_ms = system_now_ms();
    let outcome = block_on(async move {
        state
            .writer()
            .transaction(move |tx| abandon_run(tx, &rid, now_ms))
            .await
    });
    match outcome {
        Ok(Ok(AbandonOutcome::Abandoned {
            session_id,
            observations_skipped,
        })) => {
            println!(
                "{BIN}: abandoned run {run_id} — session {session_id} moved past it, \
                 {observations_skipped} observation(s) will never become memory. \
                 The envelopes survive; the audit records what was skipped."
            );
            ExitCode::SUCCESS
        }
        Ok(Ok(AbandonOutcome::AlreadyPast)) => {
            println!("{BIN}: run {run_id} was already abandoned — nothing to do");
            ExitCode::SUCCESS
        }
        Ok(Err(RepairError::NotBlocked)) => {
            fail(BIN, &format!("run {run_id} no longer blocks this session"))
        }
        Ok(Err(e)) => fail(BIN, &format!("could not abandon run {run_id}: {e}")),
        Err(e) => fail(BIN, &format!("could not abandon run {run_id}: {e}")),
    }
}
