//! The consolidation lease/cursor runner (T14-06, spec 08 §4 `[FIXED]`).
//!
//! Composes T14-01's pure `consolidation_run`/`processing_cursor` primitives
//! and T14-02–05's transactional op engine into the actual runner spec 08
//! §4's pseudocode describes:
//!
//! ```text
//! 1. tx: create consolidation_run(pending→running, lease_until), snapshot
//!        [from_received_seq = cursor+1, to_received_seq = min(cursor+batch, max_seq)]
//! 2. Load envelopes (+ surviving payloads) of the window.
//! 3. ROUTER (LLM) — OUTSIDE any long tx. Input: window observations + recall of
//!    plausibly related existing entries. Output: ordered ops list.
//! 4. ONE short tx: apply ops, evidence links, audit, advance processing_cursor,
//!    run→applied.
//! 5. Crash anywhere ⇒ run retried after lease expiry; step 4 idempotent per op.
//! ```
//!
//! Step 1 is [`super::consolidation::open_next_run`] (this module doesn't
//! repeat it — deciding *whether* and *what* to open is the caller's job).
//! Steps 2–4 are [`run_once`]. **T14-06 does not implement the router** — the
//! actual local generator (closing open item O3) is T14-07's job; [`run_once`]
//! is generic over any `generate: FnOnce(ConsolidationWindow) -> Fut`, tested
//! here with mocks. The "candidate conflict set" recall spec 08 §4 step 3
//! mentions is T14-08's relevance pipeline — [`ConsolidationWindow`]
//! deliberately carries no such field; a real generator resolves it itself
//! from its own read connection, outside any tx this runner opens.
//!
//! # Atomicity: a single failing op aborts the *whole* batch
//!
//! [`StateWriter::transaction`](crate::StateWriter::transaction) commits
//! whenever its closure returns `Ok(_)` at the **outer** `rusqlite::Result`
//! level — an inner `Ok(Err(domain_error))` still commits. A naive
//! "loop over ops in one tx, return `Ok(Err(reason))` on a mid-batch
//! rejection" would therefore let earlier-in-the-batch mutations commit even
//! though the run never reaches `applied` and the cursor never advances —
//! silent corruption, and a direct violation of this project's "memory
//! mutations, evidence, audit, and consolidation cursor movement are
//! transactionally strict" guardrail.
//!
//! [`apply_run`] is therefore a crate-private core (directly testable, but
//! **not** the safe public entry point) returning `rusqlite::Result<Result<
//! ApplyReport, RunnerApplyError>>`; [`commit_apply_run`] is the only public
//! writer entry point, and on `Ok(Err(reason))` it converts to a genuine
//! `Err(rusqlite::Error::ToSqlConversionFailure(Box::new(reason)))` — the
//! same carrier this crate's own failpoints already use to force a real
//! abort from inside a `StateWriter::transaction` closure — so the whole
//! attempt really rolls back. Callers must go through [`commit_apply_run`];
//! calling [`apply_run`] directly inside an ad hoc transaction reproduces
//! the exact bug this design avoids.
//!
//! # Lease fencing (T14-06 as-built decision, `[SPEC]`)
//!
//! [`apply_run`]'s first action re-reads the run's current `(state,
//! lease_until)` and requires `state == Running && lease_until ==
//! expected_lease_until` (the value *this* attempt acquired or last renewed)
//! before touching any op — [`RunnerApplyError::Superseded`] otherwise, zero
//! mutation. This closes a real race: a slow-but-alive attempt A whose lease
//! expires while still mid-flight, racing a legitimate retry B that
//! re-acquired a fresh lease under the same `run_id` — without fencing, A's
//! stale apply could commit ops under B's `idempotency_key` space and
//! corrupt state. No new schema column: `lease_until` doubles as its own
//! compare-and-swap token, mirroring the `expected_version` optimistic-
//! concurrency idiom [`super::op`] already uses for `memory_entry` rows.
//! [`super::consolidation::open_next_run`]'s own existence-check + insert
//! already happen inside one transaction, so two concurrent callers opening
//! for the same `session_id` are fully serialized by
//! [`crate::StateWriter`]'s single-writer queue with no separate TOCTOU
//! window.
//!
//! # A rejected apply routes the run straight to `Failed`
//!
//! Any apply-time rejection (a generator error, or an op precondition
//! violated inside [`apply_run`]) transitions the run to `Failed` immediately
//! — see [`run_once`] — rather than leaving it `running` for a lease timeout
//! to eventually rediscover the identical failure. Since no partial-apply
//! state is ever persisted between attempts (the atomicity fix above
//! guarantees a clean all-or-nothing rollback), every retry re-invokes the
//! generator from scratch anyway, so marking `Failed` immediately costs
//! nothing and lets the *next* attempt's generator see current state instead
//! of reproducing a router/user race for up to
//! [`super::consolidation::LEASE_DURATION_MS`].
//! [`super::consolidation::stale_runs`] therefore selects `failed` runs too,
//! not lease-expiry alone.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::pin;
use std::time::Duration;

use rusqlite::Transaction;

use super::audit::Actor;
use super::candidate::candidate_state;
use super::consolidation::{
    ClassifiedFailure, RunState, RunWindow, record_run_failure, run_state_and_lease,
    transition_run, upsert_processing_cursor,
};
use super::op::{EvidenceInput, MemoryOpOutcome, apply_noop};
use super::review::{
    ProposeCandidateOutcome, ProposedOperation, ReviewError, apply_proposed_operation,
    observation_evidence_source, propose_candidate,
};
use crate::observation::{EvidenceKind, TrustLevel, envelopes_in_range};
use crate::{OpenError, StateDb, WriteError};

/// One envelope inside a consolidation window, plus its still-live payload if
/// any (spec 08 §4 step 2). `payload: None` is normal (TTL-swept or an
/// envelope-only event), never an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowObservation {
    pub observation_id: String,
    pub received_seq: i64,
    pub event_type: String,
    pub evidence_kind: EvidenceKind,
    pub trust: TrustLevel,
    pub session_id: String,
    /// Which repository/worktree this observation was captured against, if
    /// any (T14-07: the router resolves a promotion's `scope_kind=
    /// repository|worktree` target from this).
    pub repo_id: Option<String>,
    pub worktree_id: Option<String>,
    pub agent_id: Option<String>,
    pub commit_hash: Option<String>,
    pub short_evidence_excerpt: Option<String>,
    pub payload: Option<Vec<u8>>,
}

/// The generator's input (spec 08 §4 step 3): a bounded, already-loaded
/// window of observations. Plain owned data with no database handle of any
/// kind — a generator closure typed to receive exactly this can *never*
/// reach the runner's transaction, which is what makes "generator outside
/// tx" true structurally, not just by convention.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsolidationWindow {
    pub session_id: String,
    pub from_received_seq: i64,
    pub to_received_seq: i64,
    pub observations: Vec<WindowObservation>,
}

/// One op a generator produced (spec 08 §4's op envelope: `{create,
/// reinforce, supersede, resolve, retract, noop, propose_candidate}`). The
/// five materializing ops reuse [`ProposedOperation`] (spec 08 §4's own
/// envelope shape, already built for candidate approval, T14-05) rather than
/// a parallel type; `evidence_observation_ids` names observations (usually,
/// but not necessarily, inside the current window — reinforcing an older
/// entry with older evidence is legitimate) this op cites.
#[derive(Debug, Clone, PartialEq)]
pub enum GeneratedOp {
    Materialize {
        operation: ProposedOperation,
        evidence_observation_ids: Vec<String>,
    },
    Noop,
    ProposeCandidate {
        candidate_id: String,
        operation: ProposedOperation,
        conflicts: Vec<String>,
        evidence_observation_ids: Vec<String>,
    },
}

fn op_kind_tag(op: &GeneratedOp) -> &'static str {
    match op {
        GeneratedOp::Materialize { operation, .. } => match operation {
            ProposedOperation::Create { .. } => "create",
            ProposedOperation::Reinforce { .. } => "reinforce",
            ProposedOperation::Resolve { .. } => "resolve",
            ProposedOperation::Retract { .. } => "retract",
            ProposedOperation::Supersede { .. } => "supersede",
        },
        GeneratedOp::Noop => "noop",
        GeneratedOp::ProposeCandidate { .. } => "propose_candidate",
    }
}

/// What one [`apply_run`]/[`commit_apply_run`] call did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApplyReport {
    pub applied: usize,
    pub replayed: usize,
    pub noop: usize,
    pub proposed: usize,
    /// A `ProposeCandidate` op whose exact proposal already had a home —
    /// pending or an active entry — and so wrote no
    /// `pending_memory_candidate` row (`T23-07`, ADR-0014 Decision 2). Makes
    /// the card's own acceptance criterion ("re-running a window that
    /// previously produced a duplicate produces none") directly observable
    /// rather than inferred from an unchanged row count.
    pub deduped: usize,
}

/// Why [`apply_run`] rejected a batch — always **zero mutation** (the whole
/// transaction rolls back via [`commit_apply_run`]'s conversion to a genuine
/// `rusqlite::Error`, see the module doc).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunnerApplyError {
    /// The run's `(state, lease_until)` no longer matches what this attempt
    /// acquired — a fresher attempt has already superseded it.
    Superseded,
    /// A `Materialize`/`ProposeCandidate` op cited an `observation_id` that
    /// is neither in the loaded window nor a known `observation_envelope`.
    UnknownEvidenceObservation {
        op_index: usize,
        observation_id: String,
    },
    /// The op engine rejected op `op_index` (optimistic conflict, illegal
    /// transition, canonical-key conflict, ...).
    Materialization {
        op_index: usize,
        source: ReviewError,
    },
}

impl std::fmt::Display for RunnerApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunnerApplyError::Superseded => write!(
                f,
                "consolidation run lease no longer matches this attempt (superseded)"
            ),
            RunnerApplyError::UnknownEvidenceObservation {
                op_index,
                observation_id,
            } => write!(
                f,
                "op {op_index}: unknown evidence observation {observation_id}"
            ),
            RunnerApplyError::Materialization { op_index, source } => {
                write!(f, "op {op_index}: {source}")
            }
        }
    }
}

impl std::error::Error for RunnerApplyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RunnerApplyError::Materialization { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// One evidence observation's fields, resolved to owned data so
/// [`apply_run`] can build [`EvidenceInput`] slices without lifetime
/// entanglement between the window's borrowed rows and a fallback DB read.
struct OwnedEvidence {
    observation_id: String,
    evidence_kind: EvidenceKind,
    session_id: String,
    agent_id: Option<String>,
    commit_hash: Option<String>,
}

/// D-069: collapse repeated citations, keeping the router's order.
///
/// A generated op's `evidence_observation_ids` is **untrusted model output**
/// (spec 12 §4), and both evidence tables are keyed on `(owner_id,
/// observation_id)` — so one `observation_id` repeated inside a single op's
/// citation list violates a PRIMARY KEY and rolls back the *whole* window,
/// which is how one three-observation window burned ~6 hours of GPU across
/// 627 identical retries before this fix.
///
/// The deduplication belongs here, at the boundary where untrusted input
/// enters the store, and **not** as `INSERT OR IGNORE` inside
/// [`super::candidate::insert_candidate_evidence`]/
/// [`super::evidence::insert_memory_evidence`]: those primitives document
/// "a duplicate surfaces as the natural PRIMARY KEY error", which stays
/// exactly right for callers that mint already-unique ids. Every path that
/// carries router output into the store goes through [`apply_run`], so one
/// call site per branch covers all of them.
///
/// Duplication *across* two ops in one batch (two reinforces of the same
/// entry citing the same observation) is deliberately out of this helper's
/// reach — it is a semantically different batch, and [`run_once`]'s
/// `Mechanical` classification of a constraint violation is what bounds it.
fn dedup_evidence_ids(ids: &[String]) -> Vec<&str> {
    let mut seen = HashSet::with_capacity(ids.len());
    ids.iter()
        .map(String::as_str)
        .filter(|id| seen.insert(*id))
        .collect()
}

/// Resolve `ids` against the already-loaded window first (fast path, no
/// read); on a miss, fall back to [`observation_evidence_source`] — citing
/// evidence from outside the current window (e.g. reinforcing an older
/// entry) is legitimate and must not be structurally forbidden.
fn resolve_evidence(
    tx: &Transaction<'_>,
    by_id: &HashMap<&str, &WindowObservation>,
    ids: &[&str],
    op_index: usize,
) -> rusqlite::Result<Result<Vec<OwnedEvidence>, RunnerApplyError>> {
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(w) = by_id.get(id) {
            out.push(OwnedEvidence {
                observation_id: (*id).to_string(),
                evidence_kind: w.evidence_kind,
                session_id: w.session_id.clone(),
                agent_id: w.agent_id.clone(),
                commit_hash: w.commit_hash.clone(),
            });
            continue;
        }
        match observation_evidence_source(tx, id)? {
            Some((evidence_kind, session_id)) => out.push(OwnedEvidence {
                observation_id: (*id).to_string(),
                evidence_kind,
                session_id,
                agent_id: None,
                commit_hash: None,
            }),
            None => {
                return Ok(Err(RunnerApplyError::UnknownEvidenceObservation {
                    op_index,
                    observation_id: (*id).to_string(),
                }));
            }
        }
    }
    Ok(Ok(out))
}

/// The transactional core of spec 08 §4 step 4 — **crate-private, not the
/// safe entry point**; see the module doc's "Atomicity" section. Callers
/// must go through [`commit_apply_run`].
fn apply_run(
    tx: &Transaction<'_>,
    window: &RunWindow,
    observations: &[WindowObservation],
    expected_lease_until: i64,
    ops: &[GeneratedOp],
    now_ms: i64,
) -> rusqlite::Result<Result<ApplyReport, RunnerApplyError>> {
    let Some((state, lease_until)) = run_state_and_lease(tx, &window.run_id)? else {
        return Ok(Err(RunnerApplyError::Superseded));
    };
    if state != RunState::Running || lease_until != Some(expected_lease_until) {
        return Ok(Err(RunnerApplyError::Superseded));
    }

    let by_id: HashMap<&str, &WindowObservation> = observations
        .iter()
        .map(|o| (o.observation_id.as_str(), o))
        .collect();

    let mut report = ApplyReport::default();

    for (op_index, op) in ops.iter().enumerate() {
        let idempotency_key = format!(
            "consolidation:{}:{op_index}:{}",
            window.run_id,
            op_kind_tag(op)
        );
        match op {
            GeneratedOp::Materialize {
                operation,
                evidence_observation_ids,
            } => {
                let cited = dedup_evidence_ids(evidence_observation_ids);
                let owned = match resolve_evidence(tx, &by_id, &cited, op_index)? {
                    Ok(owned) => owned,
                    Err(e) => return Ok(Err(e)),
                };
                let evidence: Vec<EvidenceInput<'_>> = owned
                    .iter()
                    .map(|e| EvidenceInput {
                        observation_id: &e.observation_id,
                        evidence_kind: e.evidence_kind,
                        session_id: &e.session_id,
                        agent_id: e.agent_id.as_deref(),
                        commit_hash: e.commit_hash.as_deref(),
                    })
                    .collect();
                match apply_proposed_operation(
                    tx,
                    operation,
                    &evidence,
                    Actor::Router,
                    &idempotency_key,
                    now_ms,
                )? {
                    Ok(MemoryOpOutcome::Applied(_)) => report.applied += 1,
                    Ok(MemoryOpOutcome::Replayed(_)) => report.replayed += 1,
                    Err(source) => {
                        return Ok(Err(RunnerApplyError::Materialization { op_index, source }));
                    }
                }
            }
            GeneratedOp::Noop => {
                apply_noop();
                report.noop += 1;
            }
            GeneratedOp::ProposeCandidate {
                candidate_id,
                operation,
                conflicts,
                evidence_observation_ids,
            } => {
                // Idempotent retry guard: candidates have no idempotency_key
                // mechanism of their own (no audit_event row), so an
                // already-proposed candidate_id is simply left alone. A
                // different concern from `T23-07`'s content dedup below: this
                // one keys on the caller-minted `candidate_id` and exists
                // only for crash-and-retry, so it always counts as
                // `proposed` — the run already wrote (or, on retry, already
                // had written) exactly this row.
                if candidate_state(tx, candidate_id)?.is_none() {
                    let conflict_refs: Vec<&str> = conflicts.iter().map(String::as_str).collect();
                    let evidence_refs = dedup_evidence_ids(evidence_observation_ids);
                    match propose_candidate(
                        tx,
                        candidate_id,
                        operation,
                        &conflict_refs,
                        &evidence_refs,
                        now_ms,
                    )? {
                        ProposeCandidateOutcome::Proposed => report.proposed += 1,
                        ProposeCandidateOutcome::DuplicateOfPending { .. }
                        | ProposeCandidateOutcome::AlreadyAnEntry { .. } => {
                            report.deduped += 1;
                        }
                    }
                } else {
                    report.proposed += 1;
                }
            }
        }
    }

    #[cfg(feature = "failpoints")]
    local_rag_test_support::fail_point!(
        "memory.consolidation.apply.before_cursor_advance",
        Err(rusqlite::Error::ToSqlConversionFailure(
            "failpoint: memory.consolidation.apply.before_cursor_advance".into()
        ))
    );

    upsert_processing_cursor(tx, &window.session_id, window.to_received_seq)?;
    transition_run(tx, &window.run_id, RunState::Applied, now_ms)?
        .expect("running -> applied is always legal; fencing above already confirmed Running");

    Ok(Ok(report))
}

/// Wrap a rejection so [`StateWriter::transaction`](crate::StateWriter::transaction)
/// treats it as a genuine abort — see the module doc's "Atomicity" section.
fn abort(reason: RunnerApplyError) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(reason))
}

/// Why [`commit_apply_run`] did not return a report.
#[derive(Debug)]
pub enum RunOutcomeError {
    /// [`apply_run`] rejected the batch (see [`RunnerApplyError`]'s
    /// `Display`); the whole transaction rolled back, nothing committed.
    Rejected(String),
    /// The write transaction itself failed for an infrastructure reason
    /// (writer gone, or a genuine — non-`apply_run` — SQLite error).
    Write(WriteError),
}

impl std::fmt::Display for RunOutcomeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunOutcomeError::Rejected(msg) => write!(f, "consolidation apply rejected: {msg}"),
            RunOutcomeError::Write(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for RunOutcomeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RunOutcomeError::Rejected(_) => None,
            RunOutcomeError::Write(e) => Some(e),
        }
    }
}

/// D-069: classify a [`commit_apply_run`] failure for
/// [`record_run_failure`](super::consolidation::record_run_failure)'s
/// retry-storm circuit breaker.
///
/// A SQLite **constraint violation** is `Mechanical`: the ops are already
/// generated and fixed, the rows they touch are unchanged (the whole attempt
/// rolled back), so re-running the identical batch on the identical build
/// reproduces the identical violation — which is exactly what
/// `Mechanical` means, and exactly what a fingerprint-gated dead-letter is
/// for. Before this, every apply failure was `Transient`, so a repeated
/// evidence citation retried on the 4s backoff cap forever, one full local
/// generation per attempt.
///
/// Everything else stays `Transient`, deliberately:
/// [`RunOutcomeError::Rejected`]'s variants are timing-shaped (a lease
/// fencing race, an optimistic conflict) and the remaining
/// [`WriteError`]s are genuine infrastructure (writer gone, disk I/O) that
/// waiting can resolve. Splitting [`RunnerApplyError`] per variant is still
/// future work; the transient attempt ceiling
/// ([`TRANSIENT_ATTEMPT_CAP`](super::consolidation::TRANSIENT_ATTEMPT_CAP))
/// bounds those paths in the meantime.
fn classify_apply_failure(error: &RunOutcomeError) -> ClassifiedFailure {
    match error {
        RunOutcomeError::Write(WriteError::Sqlite(e))
            if e.sqlite_error_code() == Some(rusqlite::ErrorCode::ConstraintViolation) =>
        {
            ClassifiedFailure::mechanical(error.to_string())
        }
        _ => ClassifiedFailure::transient(error.to_string()),
    }
}

/// The only public writer entry point for spec 08 §4 step 4 (see the module
/// doc's "Atomicity" section): applies `ops` and advances the cursor/run
/// state in **one** short transaction, or rolls back everything.
pub async fn commit_apply_run(
    state_db: &StateDb,
    window: RunWindow,
    observations: Vec<WindowObservation>,
    expected_lease_until: i64,
    ops: Vec<GeneratedOp>,
    now_ms: i64,
) -> Result<ApplyReport, RunOutcomeError> {
    let outcome = state_db
        .writer()
        .transaction(move |tx| -> rusqlite::Result<ApplyReport> {
            match apply_run(
                tx,
                &window,
                &observations,
                expected_lease_until,
                &ops,
                now_ms,
            )? {
                Ok(report) => Ok(report),
                Err(reason) => Err(abort(reason)),
            }
        })
        .await;

    match outcome {
        Ok(report) => Ok(report),
        Err(WriteError::Sqlite(rusqlite::Error::ToSqlConversionFailure(boxed))) => {
            match boxed.downcast::<RunnerApplyError>() {
                Ok(reason) => Err(RunOutcomeError::Rejected(reason.to_string())),
                Err(boxed) => Err(RunOutcomeError::Write(WriteError::Sqlite(
                    rusqlite::Error::ToSqlConversionFailure(boxed),
                ))),
            }
        }
        Err(e) => Err(RunOutcomeError::Write(e)),
    }
}

/// Load a [`RunWindow`]'s observations (spec 08 §4 step 2) — a plain read,
/// no transaction.
async fn load_window(
    state_db: &StateDb,
    window: &RunWindow,
) -> Result<Vec<WindowObservation>, RunnerError> {
    let conn = state_db.open_read().map_err(RunnerError::Read)?;
    let rows = envelopes_in_range(
        &conn,
        &window.session_id,
        window.from_received_seq,
        window.to_received_seq,
    )
    .map_err(RunnerError::Sqlite)?;
    Ok(rows
        .into_iter()
        .map(|r| WindowObservation {
            observation_id: r.observation_id,
            received_seq: r.received_seq,
            event_type: r.event_type,
            evidence_kind: r.evidence_kind,
            trust: r.trust,
            session_id: window.session_id.clone(),
            repo_id: r.repo_id,
            worktree_id: r.worktree_id,
            agent_id: r.agent_id,
            commit_hash: r.commit_hash,
            short_evidence_excerpt: r.short_evidence_excerpt,
            payload: r.payload,
        })
        .collect())
}

/// Mark `run_id` `Failed` (spec 04 §4: "router/LLM error ⇒ failed
/// (retryable)", generalized to any apply-time rejection — see the module
/// doc) and record D-050's retry-storm circuit-breaker bookkeeping
/// (`failure`'s kind/reason, `build_id` for a `Mechanical` fingerprint) in
/// the same write via
/// [`record_run_failure`](super::consolidation::record_run_failure).
/// `running -> failed` is always legal; if some other racer already moved
/// the row elsewhere, both the transition and the bookkeeping are silently
/// left alone rather than treated as an error, matching this crate's other
/// best-effort cleanup sweeps (and `record_run_failure`'s own "never
/// overwrite an already-applied row" contract).
async fn mark_failed(
    state_db: &StateDb,
    run_id: &str,
    failure: &ClassifiedFailure,
    build_id: &str,
    now_ms: i64,
) -> Result<(), RunnerError> {
    let run_id = run_id.to_string();
    let kind = failure.kind;
    let reason = failure.reason.clone();
    let context_overflow = failure.context_overflow;
    let build_id = build_id.to_string();
    state_db
        .writer()
        .transaction(move |tx| {
            let _ = record_run_failure(
                tx,
                &run_id,
                kind,
                &reason,
                context_overflow,
                Some(&build_id),
                now_ms,
            )?;
            Ok(())
        })
        .await
        .map_err(RunnerError::Write)
}

/// Why [`run_once`] could not complete (infrastructure only — a generator
/// error or an apply rejection are reported as [`RunOutcome::Failed`], not
/// this type).
#[derive(Debug)]
#[non_exhaustive]
pub enum RunnerError {
    /// Opening a read connection to load the window failed.
    Read(OpenError),
    /// The window-load query itself failed.
    Sqlite(rusqlite::Error),
    /// A write transaction (lease renewal, or marking the run `failed`)
    /// failed.
    Write(WriteError),
    /// A named failpoint fired (test builds only).
    #[cfg(feature = "failpoints")]
    FailpointInjected,
}

impl std::fmt::Display for RunnerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunnerError::Read(e) => write!(f, "could not open state for the window read: {e}"),
            RunnerError::Sqlite(e) => write!(f, "window read failed: {e}"),
            RunnerError::Write(e) => write!(f, "{e}"),
            #[cfg(feature = "failpoints")]
            RunnerError::FailpointInjected => write!(f, "failpoint fired"),
        }
    }
}

impl std::error::Error for RunnerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RunnerError::Read(e) => Some(e),
            RunnerError::Sqlite(e) => Some(e),
            RunnerError::Write(e) => Some(e),
            #[cfg(feature = "failpoints")]
            RunnerError::FailpointInjected => None,
        }
    }
}

/// What [`run_once`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOutcome {
    Applied(ApplyReport),
    /// The run was transitioned to `Failed` — either the generator itself
    /// errored, or [`commit_apply_run`] rejected the batch. Carries a
    /// human-readable reason for diagnostics; the run row is retry-eligible
    /// via [`super::consolidation::stale_runs`]/[`super::consolidation::retry_run`].
    Failed(String),
}

/// Steps 2–4 of spec 08 §4 for an already-opened [`RunWindow`] — step 1
/// (deciding *whether* and *what* to open) is the caller's job via
/// [`super::consolidation::open_next_run`]/[`super::consolidation::retry_run`];
/// this function does not decide *when* to run (that's a daemon-level
/// trigger, out of this task's scope).
///
/// Loads the window (plain read), calls `generate` **outside any
/// transaction** while renewing the lease every `renew_interval_ms` (spec 04
/// §4's 30s cadence) via `tokio::time::timeout` in a loop over the same
/// pinned generator future — driven by `tokio::time::Instant`, so tests
/// control it deterministically with `tokio::time::pause`/`advance`, never a
/// live wall-clock read (this crate's DB-facing functions always take an
/// explicit `now_ms`; only the *cadence* of renewal ticks is real elapsed
/// time). Then commits via [`commit_apply_run`]. A generator error or an
/// apply-time rejection transitions the run to `Failed` in its own short
/// follow-up transaction (see the module doc).
#[allow(clippy::too_many_arguments)]
pub async fn run_once<G, Fut>(
    state_db: &StateDb,
    window: RunWindow,
    lease_until: i64,
    lease_ms: i64,
    renew_interval_ms: i64,
    now_ms: i64,
    build_id: &str,
    generate: G,
) -> Result<RunOutcome, RunnerError>
where
    G: FnOnce(ConsolidationWindow) -> Fut,
    Fut: Future<Output = Result<Vec<GeneratedOp>, ClassifiedFailure>>,
{
    let observations = load_window(state_db, &window).await?;
    let consolidation_window = ConsolidationWindow {
        session_id: window.session_id.clone(),
        from_received_seq: window.from_received_seq,
        to_received_seq: window.to_received_seq,
        observations: observations.clone(),
    };

    #[cfg(feature = "failpoints")]
    local_rag_test_support::fail_point!(
        "memory.consolidation.after_snapshot",
        Err(RunnerError::FailpointInjected)
    );

    let generator_fut = generate(consolidation_window);
    let mut generator_fut = pin!(generator_fut);
    let start = tokio::time::Instant::now();
    let mut current_lease_until = lease_until;
    let renew_every = Duration::from_millis(renew_interval_ms.max(1) as u64);

    let generated = loop {
        match tokio::time::timeout(renew_every, generator_fut.as_mut()).await {
            Ok(result) => break result,
            Err(_elapsed) => {
                let elapsed_ms = start.elapsed().as_millis() as i64;
                let renewed_now = now_ms.saturating_add(elapsed_ms);
                let target_lease = renewed_now.saturating_add(lease_ms);
                let run_id = window.run_id.clone();
                let renewed = state_db
                    .writer()
                    .transaction(move |tx| {
                        super::consolidation::renew_lease(tx, &run_id, target_lease)
                    })
                    .await
                    .map_err(RunnerError::Write)?;
                if renewed.is_ok() {
                    current_lease_until = target_lease;
                }
            }
        }
    };

    #[cfg(feature = "failpoints")]
    local_rag_test_support::fail_point!(
        "memory.consolidation.after_generate",
        Err(RunnerError::FailpointInjected)
    );

    let ops = match generated {
        Ok(ops) => ops,
        Err(failure) => {
            mark_failed(state_db, &window.run_id, &failure, build_id, now_ms).await?;
            return Ok(RunOutcome::Failed(failure.reason));
        }
    };

    match commit_apply_run(
        state_db,
        window.clone(),
        observations,
        current_lease_until,
        ops,
        now_ms,
    )
    .await
    {
        Ok(report) => Ok(RunOutcome::Applied(report)),
        Err(rejected) => {
            // D-050 classified every apply failure `Transient`, on the
            // reasoning that `RunnerApplyError`'s variants are all
            // timing-shaped. D-069 disproved the general claim: the live
            // retry storm did not arrive as a `Rejected` variant at all, it
            // arrived as a genuine SQLite constraint violation on the
            // `Write` path — deterministic, and retried forever because of
            // this default. `classify_apply_failure` now splits exactly that
            // case out; the timing-shaped rejections keep the old default.
            let failure = classify_apply_failure(&rejected);
            mark_failed(state_db, &window.run_id, &failure, build_id, now_ms).await?;
            Ok(RunOutcome::Failed(failure.reason))
        }
    }
}
