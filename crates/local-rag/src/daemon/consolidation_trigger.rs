//! Continuous consolidation triggering (spec 07 §6 `[FIXED]`: "Consolidation
//! checkpoint on `Stop` and on queue size threshold; best-effort on
//! `SessionEnd`... a background worker owns all of it") — D-024.
//!
//! T15-01's `daemon::resume` shipped only the startup-catchup quarter of this
//! requirement (two one-shot passes run once at `DaemonHandle::start`); this
//! module is the missing continuous quarter, both as-built notes it left
//! behind (`docs/specification/02-architecture.md` §4.3,
//! `docs/specification/08-memory.md` §4) named as `T15-06`'s — a scope that
//! task's own card never actually carried, so it is closed here as D-024
//! instead, per the deviation workflow.
//!
//! [`consolidation_trigger_tick`] is one tick's worth of work, driven on a
//! fixed cadence by [`run_consolidation_trigger`]. A tick does two things, in
//! order:
//!
//! 1. **Stale-run recovery**, by calling
//!    [`super::resume::resume_stale_consolidation_runs`] verbatim — the exact
//!    same crash-recovery sweep T15-01's startup pass already runs once, now
//!    repeated every tick. This runs *first* because [`open_next_run`]
//!    refuses to open a second run while any non-`applied` row already exists
//!    for a session (`SnapshotOutcome::Existing`) — recovering a stale row
//!    first lets the very same tick also open a fresh window for that
//!    session, instead of losing a whole tick to a spurious `Existing`.
//! 2. **Per-session import + checkpoint-gated new-run open**: for every
//!    session [`known_spool_sessions`] or [`sessions_with_pending_backlog`]
//!    reports (D-040 — the latter is the only source that sees a session
//!    whose envelopes all arrived through a spool-bypassing daemon-internal
//!    write such as `give_feedback`), import its fresh tail
//!    (reusing the same `import_session_tail` call the startup spool-resume
//!    pass makes), then open a new consolidation window if this import just
//!    saw a `Stop`/`SessionEnd` row, or the session already has one
//!    un-consolidated in persisted state
//!    ([`has_unconsolidated_checkpoint`], D-061), or the session's backlog
//!    ([`pending_backlog`]) has crossed the configured threshold, or the
//!    session has gone idle past the configured timeout with nonzero
//!    backlog ([`session_idle_since`], X-005).
//!
//! No step here ever sleeps — [`consolidation_trigger_tick`] is directly
//! unit-testable with fixed `now_ms` literals, mirroring
//! `crates/store/tests/consolidation_runner.rs`'s own style.
//!
//! ## D-061: the startup spool-import race no longer loses the checkpoint
//!
//! At daemon boot, this worker's very first tick races
//! `daemon::resume::resume_spool_import` (spec 02 §4.1 step 5) for the same
//! session's spool tail — both independently call `import_session_tail`.
//! Whichever import call actually consumes the fresh bytes is the one whose
//! own `SpoolImportReport` observes `saw_stop`/`saw_session_end`; the other
//! call sees nothing new. That in-memory signal alone used to be the only
//! checkpoint source, so the loser's tick silently and **permanently** missed
//! it — confirmed live (D-061): 14 sessions whose spool tail had already been
//! imported (last `event_type` already `SessionEnd`) sat unconsolidated for
//! days, each under the size-threshold fallback, which a session that has
//! already ended can never cross. [`has_unconsolidated_checkpoint`] closes
//! this by asking the database directly — "does this session have a
//! `Stop`/`SessionEnd` row past its cursor" is race-free regardless of which
//! call actually imported the row, so no daemon-startup serialization is
//! needed.
//!
//! ## X-005: idle-timeout implicit checkpoint
//!
//! Even with D-061's fix, a session that crashes before ever sending a real
//! `Stop`/`SessionEnd` — the spool captures only its `SessionStart` — has no
//! checkpoint event to race in the first place, `saw_stop`/`saw_session_end`
//! and [`has_unconsolidated_checkpoint`] are both permanently `false` for
//! it, and no further observation will ever arrive to grow its backlog
//! toward `queue_threshold`. Confirmed live: three such sessions sat with a
//! single unconsolidated observation each for 52–162 hours, permanently —
//! the reason `local-rag stats`'s pending backlog never reaches exactly
//! zero even once every live session has genuinely stopped.
//! [`session_idle_since`] closes this by treating a session whose newest
//! observation is at least `idle_checkpoint_hours` old as an implicit
//! `Stop` — consolidated as-is, not silently dropped.

use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use local_rag_core::identity::UuidSource;
use local_rag_core::paths::StoreLayout;
use local_rag_store::{
    CheckpointMode, ClassifiedFailure, ConsolidationWindow, GeneratedOp, ImportError, RunOutcome,
    SnapshotOutcome, StateDb, WriteError, has_unconsolidated_checkpoint, import_session_tail,
    known_spool_sessions, open_next_run, pending_backlog, run_once, session_idle_since,
    sessions_with_pending_backlog,
};
use tokio::sync::oneshot;

use super::gitroot::ProbingRootResolver;
use super::jobs::{JobKind, JobRegistry};
use super::resume::{log_resume_sweep, resume_stale_consolidation_runs};

/// `consolidation_run.router_version` for every window this worker opens —
/// the one value actually used anywhere in this codebase today (every
/// existing `open_next_run`/`create_consolidation_run` call site, in both
/// `crates/store`'s own tests and T15-01's daemon-side resume tests, passes
/// `"v1"`; no caller has ever passed `"v0"`).
const ROUTER_VERSION: &str = "v1";

/// Tunable parameters for the continuous consolidation-trigger worker
/// (D-024). `batch_size`/`queue_threshold`/`idle_checkpoint_hours` are
/// `[SPEC]`-chosen values (no default is specified anywhere normative — see
/// `crates/core/src/config::MemoryConfig`'s own doc) surfaced through
/// `config.toml`'s `[memory]` section; `lease_ms`/`renew_interval_ms` are the
/// already-`[SPEC]`-fixed `LEASE_DURATION_MS`/`LEASE_RENEW_INTERVAL_MS`
/// values, threaded through the same way T15-01's startup resume pass
/// already does.
#[derive(Debug, Clone)]
pub struct ConsolidationTriggerParams {
    pub lease_ms: i64,
    pub renew_interval_ms: i64,
    pub batch_size: i64,
    /// `T23-04`: the same window, bounded in the unit that actually runs out.
    /// Derived once from the model's own context window
    /// (`local_rag_memory::budget::PromptBudget::window_chars`), never
    /// configured — a second knob is a second place for the arithmetic to
    /// drift from the model it is about.
    pub window_chars: i64,
    pub queue_threshold: i64,
    pub payload_ttl_hours: u64,
    /// X-005: a session whose newest observation is at least this old, with
    /// nonzero backlog, is treated as an implicit `Stop` — closes the gap
    /// for sessions that crash before ever sending a real `Stop`/
    /// `SessionEnd` (spool captures only `SessionStart`), which the
    /// queue-size threshold alone never reaches.
    pub idle_checkpoint_hours: u64,
    /// D-086: the `-wal` size above which this tick takes spec 03 §3's
    /// `TRUNCATE`, when no reader is open. Production passes
    /// [`local_rag_store::WAL_TRUNCATE_THRESHOLD_BYTES`]; a test passes a few
    /// kilobytes, because growing a real WAL past 64 MiB to observe a threshold
    /// would test the fixture rather than the policy.
    pub wal_truncate_threshold_bytes: u64,
}

/// What one session's checkpoint-gated pass did this tick.
#[derive(Debug)]
pub enum SessionTickOutcome {
    /// Neither a checkpoint event nor a backlog over threshold — nothing to
    /// do this tick.
    NoCheckpoint,
    /// A run was already open for this session ([`SnapshotOutcome::Existing`])
    /// — never double-opened; the stale-run recovery pass earlier in the same
    /// tick is what would have reclaimed it, if it was reclaimable.
    SkippedExisting,
    /// The session's cursor was already caught up to its `max_received_seq`.
    NothingPending,
    /// A window was opened and driven through [`run_once`].
    Ran(RunOutcome),
    /// This tick's own tail-import for the session failed — best-effort:
    /// silently retried next tick, never propagated as a hard error.
    ImportFailed(ImportError),
    /// Opening a new window failed at the infrastructure level (not a
    /// domain rejection — those already surface as `Ran(RunOutcome::Failed)`).
    OpenFailed(WriteError),
    /// D-058: this session's dead-letter has already been shrunk to a single
    /// observation and even that alone still overflows the model's context —
    /// no narrower window is possible. Distinct from the silent
    /// `SkippedExisting` on purpose: this session makes no further progress
    /// without a human decision, and must not be mistaken for routine
    /// backoff.
    Unconsolidatable {
        from_received_seq: i64,
        to_received_seq: i64,
        dead_letter_run_id: String,
    },
}

/// One tick: stale-run recovery, then per-session import + checkpoint-gated
/// new-run open (spec 07 §6, D-024). No internal sleeping.
///
/// D-047: the stale-run recovery step's own outcome is reported via
/// [`log_resume_sweep`] — before this, it was discarded (`let _ = …await`)
/// separately from the `SessionTickOutcome` vector this function returns
/// (which D-046 already logs), so a run stuck failing here kept retrying
/// silently on every tick, forever, even after D-046 landed.
#[allow(clippy::too_many_arguments)]
pub async fn consolidation_trigger_tick<G, Fut>(
    db: &StateDb,
    layout: &StoreLayout,
    uuids: &(dyn UuidSource + Send + Sync),
    jobs: &JobRegistry,
    params: &ConsolidationTriggerParams,
    now_ms: i64,
    build_id: &str,
    generate: &G,
) -> Vec<(String, SessionTickOutcome)>
where
    G: Fn(ConsolidationWindow) -> Fut,
    Fut: Future<Output = Result<Vec<GeneratedOp>, ClassifiedFailure>>,
{
    log_resume_sweep(
        resume_stale_consolidation_runs(
            db,
            jobs,
            params.lease_ms,
            params.renew_interval_ms,
            now_ms,
            build_id,
            generate,
        )
        .await,
    );

    // Two independent session sources, unioned (D-040): the spool directory
    // sees a session the moment a hook writes for it (even before any of its
    // bytes have been imported), while `sessions_with_pending_backlog` sees
    // envelopes however they were inserted — including the daemon-internal
    // `give_feedback` write that bypasses the spool entirely, which no spool
    // directory ever represents.
    let mut session_set: std::collections::BTreeSet<String> = known_spool_sessions(layout)
        .unwrap_or_default()
        .into_iter()
        .collect();
    if let Ok(read) = db.open_read() {
        session_set.extend(sessions_with_pending_backlog(&read).unwrap_or_default());
    }
    let sessions: Vec<String> = session_set.into_iter().collect();
    // D-063: one resolver per tick — every session of one repository reports
    // the same `cwd`, so the memoization collapses them into a single `git`
    // probe, while a per-tick instance keeps a path that has since appeared or
    // moved from being answered from a stale probe forever.
    let root_resolver = ProbingRootResolver::default();
    let mut results = Vec::with_capacity(sessions.len());

    for session_id in sessions {
        let saw_checkpoint = {
            let _job = jobs.begin(JobKind::SpoolImport);
            let outcome = import_session_tail(
                db,
                layout,
                &session_id,
                &root_resolver,
                uuids,
                now_ms,
                params.payload_ttl_hours,
            )
            .await;
            match outcome {
                Ok(o) => o.report.saw_stop || o.report.saw_session_end,
                Err(_) => false,
            }
        }; // `_job` dropped here — before this session's open-new-run work,
        // and always before the next tick's wait.

        // D-061: `saw_checkpoint` only reflects *this tick's own* import call
        // — if the daemon-startup resume pass raced it and won, the bytes are
        // already in `observation_envelope` but nothing here ever saw them
        // arrive. Ask the database directly, race-free regardless of which
        // call actually imported the row.
        //
        // X-005: `idle_timed_out` covers the session that never gets a real
        // `Stop`/`SessionEnd` at all (crashed right after `SessionStart`) —
        // its backlog would otherwise sit below `queue_threshold` forever,
        // since no further observation ever arrives to grow it.
        let (backlog_over_threshold, persisted_checkpoint, idle_timed_out) = match db.open_read() {
            Ok(read) => (
                pending_backlog(&read, &session_id).unwrap_or(0) >= params.queue_threshold,
                has_unconsolidated_checkpoint(&read, &session_id).unwrap_or(false),
                session_idle_since(
                    &read,
                    &session_id,
                    now_ms,
                    (params.idle_checkpoint_hours as i64).saturating_mul(3_600_000),
                )
                .unwrap_or(false),
            ),
            Err(_) => (false, false, false),
        };

        if !(saw_checkpoint || backlog_over_threshold || persisted_checkpoint || idle_timed_out) {
            results.push((session_id, SessionTickOutcome::NoCheckpoint));
            continue;
        }

        let _job = jobs.begin(JobKind::ConsolidationTrigger);
        let run_id = uuids.next_uuid().to_string();
        let (batch, lease_ms) = (params.batch_size, params.lease_ms);
        let window_chars = params.window_chars;
        let (rid, sid, bid) = (run_id.clone(), session_id.clone(), build_id.to_string());
        let snapshot = db
            .writer()
            .transaction(move |tx| {
                open_next_run(
                    tx,
                    &rid,
                    &sid,
                    batch,
                    window_chars,
                    ROUTER_VERSION,
                    lease_ms,
                    now_ms,
                    &bid,
                )
            })
            .await;

        let outcome = match snapshot {
            Ok(SnapshotOutcome::Opened(window)) => {
                let lease_until = now_ms + params.lease_ms;
                match run_once(
                    db,
                    window,
                    lease_until,
                    params.lease_ms,
                    params.renew_interval_ms,
                    now_ms,
                    build_id,
                    generate,
                )
                .await
                {
                    Ok(outcome) => SessionTickOutcome::Ran(outcome),
                    Err(runner_err) => {
                        SessionTickOutcome::Ran(RunOutcome::Failed(runner_err.to_string()))
                    }
                }
            }
            Ok(SnapshotOutcome::Existing { .. }) => SessionTickOutcome::SkippedExisting,
            Ok(SnapshotOutcome::NothingPending) => SessionTickOutcome::NothingPending,
            Ok(SnapshotOutcome::Unconsolidatable {
                window,
                dead_letter_run_id,
            }) => SessionTickOutcome::Unconsolidatable {
                from_received_seq: window.from_received_seq,
                to_received_seq: window.to_received_seq,
                dead_letter_run_id,
            },
            Err(write_err) => SessionTickOutcome::OpenFailed(write_err),
        };
        results.push((session_id, outcome));
    }
    results
}

/// The tick loop: [`consolidation_trigger_tick`] on `poll_interval` cadence
/// until `stop` fires — mirrors `handshake::serve_connections`'s exact
/// `select!` shape (signal-then-await cancellation, not a blind
/// `JoinHandle::await`, since this loop never completes on its own).
#[allow(clippy::too_many_arguments)]
pub async fn run_consolidation_trigger<G, Fut>(
    db: Arc<StateDb>,
    layout: StoreLayout,
    uuids: Arc<dyn UuidSource + Send + Sync>,
    jobs: JobRegistry,
    params: ConsolidationTriggerParams,
    poll_interval: Duration,
    build_id: &'static str,
    generate: G,
    mut stop: oneshot::Receiver<()>,
) where
    G: Fn(ConsolidationWindow) -> Fut,
    Fut: Future<Output = Result<Vec<GeneratedOp>, ClassifiedFailure>>,
{
    // D-095: which floor-case runs this worker has already reported, so a
    // terminal state is announced on transition rather than on every tick.
    let mut reported_unconsolidatable: HashSet<String> = HashSet::new();
    let mut ticker = tokio::time::interval(poll_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = &mut stop => return,
            _ = ticker.tick() => {
                let now_ms = system_now_ms();
                let results = consolidation_trigger_tick(&db, &layout, &*uuids, &jobs, &params, now_ms, build_id, &generate).await;
                log_session_tick_outcomes(&results, &mut reported_unconsolidatable);
                maybe_truncate_wal(&db, &jobs, params.wal_truncate_threshold_bytes).await;
            }
        }
    }
}

/// spec 03 §3's `TRUNCATE` clause, on a boundary that does not depend on
/// indexing (D-086).
///
/// The policy had exactly one driver — the end of an indexing cycle (D-083) —
/// and D-089 has just stopped a reconcile from producing a cycle when nothing
/// changed, so a repository nobody is editing now reaches that boundary never.
/// This tick does, every `poll_interval`, for the daemon's whole life. It is
/// also the right place on the merits: consolidation is the largest writer
/// outside indexing and spool import runs inside this same tick, so both
/// non-indexing writers share one boundary. That falsifies D-083's own closing
/// sentence ("Consolidation is deliberately left out: its per-run write volume
/// is a handful of rows"), which is amended in 03 §3.
///
/// "No readers" is approximated by "no `Reconcile` job running", and the
/// approximation's edge is worth stating rather than glossing. It covers the
/// reader D-083 actually measured as the blocker — the embedding backfill's
/// `state_read`, held across `blob_index`/`context_index`/`write_coverage` — and
/// every short-lived reader (search, this tick's own queries) opens and drops
/// within one call, so those never matter. It does **not** cover
/// `local_rag_index::reconcile::build`'s own read connection, which is opened
/// for the whole build and sits *before* `project_one` takes the job guard.
///
/// That gap is a cost, not a hazard: a `TRUNCATE` under a live reader transfers
/// what it may, leaves the file at its high-water mark and returns — so the
/// worst case is one wasted `PRAGMA` per tick during a build, bounded further by
/// the 64 MiB threshold. Closing it properly means giving the build phase a job
/// guard, which belongs to the indexing task, not to this tick.
///
/// Failure is logged, never propagated: this is housekeeping on a loop that must
/// keep ticking, and the next tick tries again.
async fn maybe_truncate_wal(db: &StateDb, jobs: &JobRegistry, threshold_bytes: u64) {
    let wal = local_rag_store::wal_bytes(db.path());
    if !local_rag_store::should_truncate_wal(
        wal,
        threshold_bytes,
        jobs.any_running(&[JobKind::Reconcile]),
    ) {
        return;
    }
    match db.writer().checkpoint(CheckpointMode::Truncate).await {
        Ok(_) => tracing::debug!(
            wal_bytes_before = wal,
            wal_bytes_after = local_rag_store::wal_bytes(db.path()),
            "wal checkpoint above the truncate threshold"
        ),
        Err(e) => tracing::warn!(
            wal_bytes = wal,
            error = %e,
            "wal checkpoint above the truncate threshold failed"
        ),
    }
}

/// D-046: report each session's tick outcome via `tracing`, mirroring
/// `lifecycle::spawn_spool_resume`'s per-outcome logging — the only existing
/// precedent in this codebase for turning a per-session outcome vector into
/// log lines. Before this, [`run_consolidation_trigger`]'s loop discarded
/// [`consolidation_trigger_tick`]'s return value outright (`let _ = …await`)
/// every 15s, forever; a session stuck permanently `Failed` left no trace in
/// `local-rag serve`'s stderr, only discoverable by reading `state.sqlite`
/// directly. Routine outcomes stay silent — this runs on every tick, so
/// logging `NoCheckpoint`/`SkippedExisting`/`NothingPending`/a successful
/// `Applied` would be pure noise.
fn log_session_tick_outcomes(
    results: &[(String, SessionTickOutcome)],
    reported_unconsolidatable: &mut HashSet<String>,
) {
    for (session_id, outcome) in results {
        match outcome {
            SessionTickOutcome::Ran(RunOutcome::Failed(reason)) => {
                tracing::error!(
                    "local-rag: consolidation run failed for session {session_id}: {reason}"
                );
            }
            SessionTickOutcome::OpenFailed(e) => {
                tracing::error!(
                    "local-rag: consolidation run-open failed for session {session_id}: {e}"
                );
            }
            SessionTickOutcome::ImportFailed(e) => {
                tracing::warn!(
                    "local-rag: consolidation tail-import failed for session {session_id}: {e}"
                );
            }
            SessionTickOutcome::Unconsolidatable {
                from_received_seq,
                to_received_seq,
                dead_letter_run_id,
            } => {
                // D-095: once per blocking run, not once per tick. The
                // state is terminal until a human acts, so re-reporting it
                // every 15 s says nothing new and says it forever: measured
                // on a live store, this one line was 10 707 of 11 134 lines
                // in a day's log — 96.2 % — drowning the surface D-088 was
                // diagnosed from. Durable visibility does not depend on this
                // line: `stats` and `doctor` both carry the state (D-071).
                if reported_unconsolidatable.insert(dead_letter_run_id.clone()) {
                    tracing::error!(
                        "local-rag: consolidation unconsolidatable for session {session_id}, \
                         received_seq {from_received_seq}..={to_received_seq} (D-058 floor case, \
                         needs manual review — dead-letter run {dead_letter_run_id})"
                    );
                }
            }
            SessionTickOutcome::NoCheckpoint
            | SessionTickOutcome::SkippedExisting
            | SessionTickOutcome::NothingPending
            | SessionTickOutcome::Ran(RunOutcome::Applied(_)) => {}
        }
    }
}

/// The current wall-clock time as Unix milliseconds — mirrors
/// `lifecycle::system_now_ms`/`main.rs::system_now_ms`/
/// `local_rag_hook::clock::system_now_ms` exactly, this project's established
/// convention of each call site carrying its own trivial copy rather than a
/// shared helper.
fn system_now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use local_rag_core::identity::{Uuid, uuidv7_from};
    use local_rag_core::spool::{FramePayload, encode_frame, encode_segment_header};
    use local_rag_store::{
        LEASE_DURATION_MS, LEASE_RENEW_INTERVAL_MS, NewConsolidationRun, RequestRoot, RunState,
        acquire_lease, create_consolidation_run, transition_run,
    };
    use local_rag_test_support::TempHome;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct SeqUuidV7 {
        counter: AtomicU64,
    }
    impl SeqUuidV7 {
        fn new() -> Self {
            Self {
                counter: AtomicU64::new(0),
            }
        }
    }
    impl UuidSource for SeqUuidV7 {
        fn next_uuid(&self) -> Uuid {
            let n = self.counter.fetch_add(1, Ordering::Relaxed);
            uuidv7_from(1000 + n, [0xCD; 10])
        }
    }

    fn open_state() -> (TempHome, StoreLayout, StateDb) {
        let home = TempHome::new().expect("temp home");
        let layout = StoreLayout::new(home.join("local-rag"));
        layout.ensure().expect("ensure store tree");
        let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
        (home, layout, db)
    }

    fn spool_fixture(
        session_id: &str,
        source_event_id: &str,
        event_type: &str,
        captured_at: i64,
    ) -> FramePayload {
        FramePayload {
            format_version: 1,
            source_event_id: source_event_id.to_string(),
            dedup_key: None,
            event_type: event_type.to_string(),
            captured_at,
            session_id: session_id.to_string(),
            agent_id: None,
            turn_id: None,
            batch_id: None,
            worktree_root: None,
            commit: None,
            evidence_kind: "model_claim".to_string(),
            trust: "low".to_string(),
            paths: vec![],
            redaction_version: None,
            payload: None,
            short_evidence_excerpt: None,
        }
    }

    fn write_spool_segment(layout: &StoreLayout, session_id: &str, seq: u32, frame: &FramePayload) {
        let session_dir = layout.spool_session(session_id);
        std::fs::create_dir_all(&session_dir).expect("session dir");
        let mut bytes = encode_segment_header().to_vec();
        bytes.extend_from_slice(&encode_frame(frame).expect("under the frame cap"));
        std::fs::write(session_dir.join(format!("{seq:06}.seg")), bytes).expect("write segment");
    }

    /// Grow the `-wal` past a small test threshold: rows through the real write
    /// queue, which is what a WAL records.
    async fn grow_wal(db: &StateDb) {
        db.writer()
            .transaction(|tx| {
                tx.execute_batch("CREATE TABLE IF NOT EXISTS d086 (id INTEGER PRIMARY KEY, v TEXT)")
            })
            .await
            .expect("create table");
        for chunk in 0..8 {
            db.writer()
                .transaction(move |tx| {
                    for i in 0..200 {
                        tx.execute(
                            "INSERT INTO d086 (id, v) VALUES (?1, ?2)",
                            rusqlite::params![chunk * 200 + i, "x".repeat(512)],
                        )?;
                    }
                    Ok(())
                })
                .await
                .expect("insert rows");
        }
    }

    /// D-086: spec 03 §3's `TRUNCATE` clause fires on a boundary that does not
    /// depend on indexing.
    ///
    /// Until this, the policy's only driver was the end of an indexing cycle
    /// (D-083), and D-089 has just stopped a reconcile from producing a cycle
    /// when nothing changed — so a repository nobody edits reaches that boundary
    /// never, and the `-wal` keeps its high-water mark for as long as the daemon
    /// runs (`journal_size_limit` is unset, and `PASSIVE` moves frames without
    /// returning disk).
    #[tokio::test]
    async fn a_tick_truncates_a_wal_over_the_threshold_when_no_reconcile_is_running() {
        let (_home, layout, db) = open_state();
        grow_wal(&db).await;
        let before = local_rag_store::wal_bytes(&layout.state_db());
        assert!(
            before > 4096,
            "the fixture must produce a real WAL: {before}"
        );

        let jobs = JobRegistry::new();
        maybe_truncate_wal(&db, &jobs, 4096).await;

        let after = local_rag_store::wal_bytes(&layout.state_db());
        assert!(
            after < before,
            "a WAL over the threshold with no reader must be truncated: {before} -> {after}"
        );
    }

    /// The "and no readers" half, which is not politeness: a reader pins the
    /// frames after its snapshot, so a truncate under one pays the blocking cost
    /// and still leaves the file at its high-water mark. `JobKind::Reconcile` is
    /// the daemon's only read connection held across `await`s (the embedding
    /// backfill's), so the job registry answers the spec's question exactly.
    #[tokio::test]
    async fn a_tick_leaves_the_wal_alone_while_a_reconcile_is_running() {
        let (_home, layout, db) = open_state();
        grow_wal(&db).await;
        let before = local_rag_store::wal_bytes(&layout.state_db());

        let jobs = JobRegistry::new();
        let _job = jobs.begin(JobKind::Reconcile);
        maybe_truncate_wal(&db, &jobs, 4096).await;

        assert_eq!(
            local_rag_store::wal_bytes(&layout.state_db()),
            before,
            "a reconcile is a live reader; the tick must not truncate under it"
        );
    }

    /// And it is a threshold, not "truncate on every tick": below it the tick
    /// does nothing, which is what keeps a blocking truncate rare.
    #[tokio::test]
    async fn a_tick_below_the_threshold_does_nothing() {
        let (_home, layout, db) = open_state();
        grow_wal(&db).await;
        let before = local_rag_store::wal_bytes(&layout.state_db());

        let jobs = JobRegistry::new();
        maybe_truncate_wal(&db, &jobs, before + 1).await;

        assert_eq!(
            local_rag_store::wal_bytes(&layout.state_db()),
            before,
            "below the threshold nothing should happen"
        );
    }

    fn default_params() -> ConsolidationTriggerParams {
        ConsolidationTriggerParams {
            lease_ms: LEASE_DURATION_MS,
            renew_interval_ms: LEASE_RENEW_INTERVAL_MS,
            batch_size: 20,
            // A budget no fixture here can reach, so a test that is not about
            // `T23-04` keeps asserting exactly what it asserted before the
            // window budget existed (`D-095`'s `NO_BUDGET_LIMIT` idiom).
            window_chars: local_rag_store::UNBOUNDED_WINDOW_CHARS,
            queue_threshold: 50,
            payload_ttl_hours: 72,
            idle_checkpoint_hours: 24,
            wal_truncate_threshold_bytes: local_rag_store::WAL_TRUNCATE_THRESHOLD_BYTES,
        }
    }

    fn noop_ops() -> Vec<GeneratedOp> {
        vec![GeneratedOp::Noop]
    }

    #[tokio::test]
    async fn a_stop_event_triggers_immediately() {
        let (_home, layout, db) = open_state();
        let uuids = SeqUuidV7::new();
        let jobs = JobRegistry::new();
        write_spool_segment(
            &layout,
            "sess-a",
            1,
            &spool_fixture("sess-a", "st:a:1", "Stop", 1_000),
        );

        let results = consolidation_trigger_tick(
            &db,
            &layout,
            &uuids,
            &jobs,
            &default_params(),
            1_000,
            "build-test",
            &|_window| async { Ok(noop_ops()) },
        )
        .await;

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "sess-a");
        match &results[0].1 {
            SessionTickOutcome::Ran(RunOutcome::Applied(_)) => {}
            other => panic!("expected Ran(Applied(_)), got {other:?}"),
        }
        assert!(jobs.is_empty());
    }

    #[tokio::test]
    async fn backlog_over_threshold_triggers_without_a_checkpoint_event() {
        let (_home, layout, db) = open_state();
        let uuids = SeqUuidV7::new();
        let jobs = JobRegistry::new();
        for i in 0..3 {
            write_spool_segment(
                &layout,
                "sess-a",
                1 + i,
                &spool_fixture(
                    "sess-a",
                    &format!("pt:a:{i}"),
                    "UserPromptSubmit",
                    1_000 + i as i64,
                ),
            );
        }
        let mut params = default_params();
        params.queue_threshold = 3;

        let results = consolidation_trigger_tick(
            &db,
            &layout,
            &uuids,
            &jobs,
            &params,
            1_000,
            "build-test",
            &|_window| async { Ok(noop_ops()) },
        )
        .await;

        assert_eq!(results.len(), 1);
        match &results[0].1 {
            SessionTickOutcome::Ran(RunOutcome::Applied(_)) => {}
            other => panic!("expected Ran(Applied(_)), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_session_end_already_imported_by_a_prior_pass_still_triggers() {
        // D-061 regression: reproduces the daemon-startup race the module doc
        // describes — some other caller (standing in for
        // `resume::resume_spool_import`) already consumed the SessionEnd
        // spool bytes into `observation_envelope` before this tick's own
        // `import_session_tail` call ever runs, so the tick's own
        // `SpoolImportReport` will report nothing new. Backlog is kept well
        // under the threshold, so the only remaining path is the persisted
        // `has_unconsolidated_checkpoint` check.
        let (_home, layout, db) = open_state();
        let uuids = SeqUuidV7::new();
        let jobs = JobRegistry::new();
        write_spool_segment(
            &layout,
            "sess-a",
            1,
            &spool_fixture("sess-a", "se:a:1", "SessionEnd", 1_000),
        );

        import_session_tail(
            &db,
            &layout,
            "sess-a",
            &RequestRoot::default(),
            &uuids,
            1_000,
            72,
        )
        .await
        .expect("prior pass imports the SessionEnd byte first");

        let mut params = default_params();
        params.queue_threshold = 50;

        let results = consolidation_trigger_tick(
            &db,
            &layout,
            &uuids,
            &jobs,
            &params,
            1_000,
            "build-test",
            &|_window| async { Ok(noop_ops()) },
        )
        .await;

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "sess-a");
        match &results[0].1 {
            SessionTickOutcome::Ran(RunOutcome::Applied(_)) => {}
            other => {
                panic!("expected Ran(Applied(_)) via the persisted checkpoint check, got {other:?}")
            }
        }
    }

    #[tokio::test]
    async fn an_idle_session_that_never_got_a_stop_still_triggers() {
        // X-005 regression: a session that crashed right after `SessionStart`
        // (no `Stop`/`SessionEnd` ever, backlog forever under threshold) must
        // still eventually consolidate once it has been idle long enough.
        let (_home, layout, db) = open_state();
        let uuids = SeqUuidV7::new();
        let jobs = JobRegistry::new();
        write_spool_segment(
            &layout,
            "sess-a",
            1,
            &spool_fixture("sess-a", "ss:a:1", "SessionStart", 1_000),
        );

        let mut params = default_params();
        params.queue_threshold = 50;
        params.idle_checkpoint_hours = 24;

        let results = consolidation_trigger_tick(
            &db,
            &layout,
            &uuids,
            &jobs,
            &params,
            1_000 + 24 * 3_600_000,
            "build-test",
            &|_window| async { Ok(noop_ops()) },
        )
        .await;

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "sess-a");
        match &results[0].1 {
            SessionTickOutcome::Ran(RunOutcome::Applied(_)) => {}
            other => panic!("expected Ran(Applied(_)) via the idle-timeout check, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_idle_session_under_the_timeout_does_not_trigger_yet() {
        let (_home, layout, db) = open_state();
        let uuids = SeqUuidV7::new();
        let jobs = JobRegistry::new();
        write_spool_segment(
            &layout,
            "sess-a",
            1,
            &spool_fixture("sess-a", "ss:a:1", "SessionStart", 1_000),
        );

        let mut params = default_params();
        params.queue_threshold = 50;
        params.idle_checkpoint_hours = 24;

        let results = consolidation_trigger_tick(
            &db,
            &layout,
            &uuids,
            &jobs,
            &params,
            1_000 + 3_600_000,
            "build-test",
            &|_window| async { Ok(noop_ops()) },
        )
        .await;

        assert_eq!(results.len(), 1);
        match &results[0].1 {
            SessionTickOutcome::NoCheckpoint => {}
            other => panic!("expected NoCheckpoint (idle timeout not reached), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn backlog_under_threshold_with_no_event_does_not_trigger() {
        let (_home, layout, db) = open_state();
        let uuids = SeqUuidV7::new();
        let jobs = JobRegistry::new();
        write_spool_segment(
            &layout,
            "sess-a",
            1,
            &spool_fixture("sess-a", "pt:a:1", "UserPromptSubmit", 1_000),
        );
        let mut params = default_params();
        params.queue_threshold = 50;

        let results = consolidation_trigger_tick(
            &db,
            &layout,
            &uuids,
            &jobs,
            &params,
            1_000,
            "build-test",
            &|_window| async { Ok(noop_ops()) },
        )
        .await;

        assert_eq!(results.len(), 1);
        assert!(matches!(results[0].1, SessionTickOutcome::NoCheckpoint));
    }

    #[tokio::test]
    async fn session_end_best_effort_generator_rejection_does_not_propagate() {
        let (_home, layout, db) = open_state();
        let uuids = SeqUuidV7::new();
        let jobs = JobRegistry::new();
        write_spool_segment(
            &layout,
            "sess-a",
            1,
            &spool_fixture("sess-a", "se:a:1", "SessionEnd", 1_000),
        );

        let results = consolidation_trigger_tick(
            &db,
            &layout,
            &uuids,
            &jobs,
            &default_params(),
            1_000,
            "build-test",
            &|_window| async { Err(ClassifiedFailure::transient("router refused")) },
        )
        .await;

        assert_eq!(results.len(), 1);
        match &results[0].1 {
            SessionTickOutcome::Ran(RunOutcome::Failed(reason)) => {
                assert!(reason.contains("router refused"), "{reason}");
            }
            other => panic!("expected Ran(Failed(_)), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_existing_non_applied_run_is_skipped_not_double_opened() {
        let (_home, layout, db) = open_state();
        let uuids = SeqUuidV7::new();
        let jobs = JobRegistry::new();
        write_spool_segment(
            &layout,
            "sess-a",
            1,
            &spool_fixture("sess-a", "st:a:1", "Stop", 1_000),
        );
        // Seed an already-`running`, not-yet-expired run for the same
        // session — the same synthetic seed T15-01's own resume tests use.
        db.writer()
            .transaction(|tx| {
                create_consolidation_run(
                    tx,
                    &NewConsolidationRun {
                        run_id: "run-live",
                        session_id: "sess-a",
                        from_received_seq: 1,
                        to_received_seq: 1,
                        router_version: "v1",
                    },
                    500,
                )?;
                transition_run(tx, "run-live", RunState::Running, 500)?.expect("legal");
                acquire_lease(tx, "run-live", 999_999_999)?; // far from expired
                Ok(())
            })
            .await
            .expect("seed live run");

        let results = consolidation_trigger_tick(
            &db,
            &layout,
            &uuids,
            &jobs,
            &default_params(),
            1_000,
            "build-test",
            &|_window| async { Ok(noop_ops()) },
        )
        .await;

        assert_eq!(results.len(), 1);
        assert!(matches!(results[0].1, SessionTickOutcome::SkippedExisting));
    }

    /// D-058 end-to-end: a window that overflows the model's context fails
    /// and is dead-lettered on its own tick; the very next tick — same
    /// session, same build — opens a narrower window at the same starting
    /// point automatically and it succeeds, without any operator
    /// intervention. This is the actual behavior the live incident (a
    /// session retried 1700+ times over 7+ hours, permanently blocked) was
    /// missing before D-057/D-058.
    #[tokio::test]
    async fn a_context_overflow_dead_letter_shrinks_and_succeeds_on_the_next_tick() {
        let (_home, layout, db) = open_state();
        let uuids = SeqUuidV7::new();
        let jobs = JobRegistry::new();
        for i in 0..4u32 {
            write_spool_segment(
                &layout,
                "sess-a",
                1 + i,
                &spool_fixture(
                    "sess-a",
                    &format!("evt-{i}"),
                    if i == 3 { "Stop" } else { "UserPromptSubmit" },
                    1_000 + i as i64,
                ),
            );
        }
        let mut params = default_params();
        params.batch_size = 4;
        // Low enough that the second tick (no new spool bytes, so no fresh
        // checkpoint) still re-evaluates this session on its own backlog.
        params.queue_threshold = 1;

        // Scripted "generator": overflows for any window wider than 2
        // observations, succeeds otherwise — deterministic on window size,
        // exactly like a real token-budget overflow is deterministic on
        // prompt size.
        let generate = |window: ConsolidationWindow| async move {
            if window.observations.len() > 2 {
                Err(ClassifiedFailure::mechanical_context_overflow(
                    "request needs 99999 tokens, model context is 32768",
                ))
            } else {
                Ok(noop_ops())
            }
        };

        let results = consolidation_trigger_tick(
            &db, &layout, &uuids, &jobs, &params, 1_000, "build-1", &generate,
        )
        .await;
        assert_eq!(results.len(), 1);
        match &results[0].1 {
            SessionTickOutcome::Ran(RunOutcome::Failed(reason)) => {
                assert!(reason.contains("context"), "{reason}");
            }
            other => panic!("expected the full 4-observation window to fail, got {other:?}"),
        }
        {
            let read = db.open_read().expect("read conn");
            assert_eq!(
                pending_backlog(&read, "sess-a").expect("backlog"),
                4,
                "nothing applied yet — the failed run never advanced the cursor"
            );
        }

        let results = consolidation_trigger_tick(
            &db, &layout, &uuids, &jobs, &params, 2_000, "build-1", &generate,
        )
        .await;
        assert_eq!(results.len(), 1);
        match &results[0].1 {
            SessionTickOutcome::Ran(RunOutcome::Applied(_)) => {}
            other => {
                panic!("expected the shrunk 2-observation window to apply cleanly, got {other:?}")
            }
        }
        let read = db.open_read().expect("read conn");
        assert_eq!(
            pending_backlog(&read, "sess-a").expect("backlog"),
            2,
            "the shrunk window (2 of the original 4 observations) applied"
        );
    }

    #[tokio::test]
    async fn stale_run_recovery_runs_before_the_new_run_pass_in_the_same_tick() {
        let (_home, layout, db) = open_state();
        let uuids = SeqUuidV7::new();
        let jobs = JobRegistry::new();
        // A real envelope at received_seq=1 (direct SQL insert, mirroring
        // `crates/store/tests/consolidation_runner.rs::seed_envelopes` — the
        // recovered run's window must cover a genuine row, or its noop apply
        // would advance the cursor past nothing at all)...
        db.writer()
            .transaction(|tx| {
                tx.execute(
                    "INSERT INTO observation_envelope \
                       (observation_id, source_event_id, payload_hash, event_type, \
                        evidence_kind, trust, session_id) \
                     VALUES ('obs-seed-1', 'evt-seed-1', 'deadbeef', 'Stop', 'user_statement', 'normal', 'sess-a')",
                    [],
                )?;
                Ok(())
            })
            .await
            .expect("seed envelope");
        // ...and a failed run from a previous tick, over exactly that window.
        db.writer()
            .transaction(|tx| {
                create_consolidation_run(
                    tx,
                    &NewConsolidationRun {
                        run_id: "run-failed",
                        session_id: "sess-a",
                        from_received_seq: 1,
                        to_received_seq: 1,
                        router_version: "v1",
                    },
                    500,
                )?;
                transition_run(tx, "run-failed", RunState::Running, 500)?.expect("legal");
                transition_run(tx, "run-failed", RunState::Failed, 600)?.expect("legal");
                Ok(())
            })
            .await
            .expect("seed failed run");
        // ...and a fresh Stop this same tick, past that window (lands at
        // received_seq=2, since received_seq is one global sequence).
        write_spool_segment(
            &layout,
            "sess-a",
            1,
            &spool_fixture("sess-a", "st:a:2", "Stop", 1_000),
        );

        let results = consolidation_trigger_tick(
            &db,
            &layout,
            &uuids,
            &jobs,
            &default_params(),
            1_000,
            "build-test",
            &|_window| async { Ok(noop_ops()) },
        )
        .await;

        // The stale run recovered (proved indirectly: it is no longer a
        // blocking `Existing` row, so the fresh Stop's own pass ran too).
        assert_eq!(results.len(), 1);
        match &results[0].1 {
            SessionTickOutcome::Ran(RunOutcome::Applied(_)) => {}
            other => panic!("expected Ran(Applied(_)) for the fresh window, got {other:?}"),
        }
        let read = db.open_read().expect("read conn");
        let recovered_state: String = read
            .query_row(
                "SELECT state FROM consolidation_run WHERE run_id = 'run-failed'",
                [],
                |r| r.get(0),
            )
            .expect("read recovered state");
        assert_eq!(
            recovered_state, "applied",
            "the stale run itself was also recovered this same tick"
        );
    }

    /// D-040: `give_feedback` inserts its envelope straight through
    /// `insert_envelope` (the documented daemon-internal exemption from the
    /// spool-only constraint), so a session that only ever used it has **no
    /// spool directory at all** — `known_spool_sessions` cannot see it, and
    /// before this fix it was structurally unreachable for consolidation no
    /// matter how large its backlog grew.
    #[tokio::test]
    async fn a_session_with_no_spool_directory_is_still_consolidated() {
        let (_home, layout, db) = open_state();
        let uuids = SeqUuidV7::new();
        let jobs = JobRegistry::new();
        // Exactly what `mcp::memory_write::give_feedback` writes: `McpFeedback`
        // envelopes, `mcp:<session>:<request>` source identity, no spool.
        db.writer()
            .transaction(|tx| {
                for i in 0..3 {
                    tx.execute(
                        "INSERT INTO observation_envelope \
                           (observation_id, source_event_id, dedup_key, payload_hash, event_type, \
                            evidence_kind, trust, session_id) \
                         VALUES (?1, ?2, ?2, 'deadbeef', 'McpFeedback', 'user_statement', 'normal', 'sess-fb')",
                        rusqlite::params![
                            format!("obs-fb-{i}"),
                            format!("mcp:sess-fb:{i}"),
                        ],
                    )?;
                }
                Ok(())
            })
            .await
            .expect("seed give_feedback envelopes");
        assert!(
            !layout.spool_session("sess-fb").exists(),
            "the premise: this session never touched the spool",
        );
        // `McpFeedback` is neither `Stop` nor `SessionEnd`, so the backlog
        // threshold is the only trigger this session can ever cross.
        let mut params = default_params();
        params.queue_threshold = 3;

        let results = consolidation_trigger_tick(
            &db,
            &layout,
            &uuids,
            &jobs,
            &params,
            1_000,
            "build-test",
            &|_window| async { Ok(noop_ops()) },
        )
        .await;

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "sess-fb");
        match &results[0].1 {
            SessionTickOutcome::Ran(RunOutcome::Applied(_)) => {}
            other => panic!("expected Ran(Applied(_)) for the spool-less session, got {other:?}"),
        }
        // The cursor really moved: the session is no longer backlogged, so a
        // second tick finds nothing to do rather than re-opening the window.
        let read = db.open_read().expect("read conn");
        assert_eq!(pending_backlog(&read, "sess-fb").expect("backlog"), 0,);
    }

    #[tokio::test]
    async fn each_session_is_evaluated_independently() {
        let (_home, layout, db) = open_state();
        let uuids = SeqUuidV7::new();
        let jobs = JobRegistry::new();
        write_spool_segment(
            &layout,
            "sess-a",
            1,
            &spool_fixture("sess-a", "st:a:1", "Stop", 1_000),
        );
        write_spool_segment(
            &layout,
            "sess-b",
            1,
            &spool_fixture("sess-b", "pt:b:1", "UserPromptSubmit", 1_000),
        );

        let results = consolidation_trigger_tick(
            &db,
            &layout,
            &uuids,
            &jobs,
            &default_params(),
            1_000,
            "build-test",
            &|_window| async { Ok(noop_ops()) },
        )
        .await;

        let mut by_session: std::collections::HashMap<String, &SessionTickOutcome> =
            results.iter().map(|(id, o)| (id.clone(), o)).collect();
        assert!(matches!(
            by_session.remove("sess-a"),
            Some(SessionTickOutcome::Ran(RunOutcome::Applied(_)))
        ));
        assert!(matches!(
            by_session.remove("sess-b"),
            Some(SessionTickOutcome::NoCheckpoint)
        ));
    }

    // -------------------------------------------------------------------
    // run_consolidation_trigger: cadence + cancellation
    // -------------------------------------------------------------------

    /// This crate has no `tokio` `test-util` feature (its own `Cargo.toml`
    /// curates tokio's feature set tightly), so — unlike
    /// `consolidation_runner.rs`'s paused-virtual-time style — this mirrors
    /// `crates/local-rag/tests/idle_shutdown.rs::wait_until_idle_eligible`'s
    /// own established idiom instead: a real but tiny poll interval, with
    /// every assertion driven by bounded convergence (a
    /// [`tokio::time::timeout`]), never by a fixed sleep duration standing in
    /// for "enough time must have passed."
    #[tokio::test]
    async fn ticks_fire_on_cadence_and_stop_returns_promptly() {
        let (_home, layout, db) = open_state();
        let db = Arc::new(db);
        let uuids: Arc<dyn UuidSource + Send + Sync> = Arc::new(SeqUuidV7::new());
        let jobs = JobRegistry::new();
        let (stop_tx, stop_rx) = oneshot::channel();

        let handle = tokio::spawn(run_consolidation_trigger(
            Arc::clone(&db),
            layout,
            uuids,
            jobs,
            default_params(),
            Duration::from_millis(5),
            "build-test",
            |_window: ConsolidationWindow| async { Ok(noop_ops()) },
            stop_rx,
        ));

        // No spool sessions exist, so `generate` above is never actually
        // reached (nothing to trigger on) — this test only proves the loop
        // does not exit on its own while ticking with nothing to do, and
        // returns promptly once explicitly stopped. The trigger conditions
        // themselves are covered above at the `consolidation_trigger_tick`
        // level.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !handle.is_finished(),
            "the loop must keep ticking until explicitly stopped"
        );

        let _ = stop_tx.send(());
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("the loop must return promptly once stopped")
            .expect("no panic");
    }

    /// A tick already in flight (blocked inside `generate`) must finish
    /// before `stop` takes effect — `select!` only re-evaluates once the
    /// current tick's future resolves, never tearing it off mid-way. Uses a
    /// very short real poll interval rather than paused virtual time: the
    /// assertion is driven entirely by channel synchronization
    /// (`started_rx`/`unblock_tx`), never by a fixed sleep duration, so
    /// correctness does not depend on how quickly the interval actually
    /// fires.
    #[tokio::test]
    async fn a_tick_already_in_progress_finishes_before_stop_takes_effect() {
        let (_home, layout, db) = open_state();
        write_spool_segment(
            &layout,
            "sess-a",
            1,
            &spool_fixture("sess-a", "st:a:1", "Stop", 1_000),
        );
        let db = Arc::new(db);
        let uuids: Arc<dyn UuidSource + Send + Sync> = Arc::new(SeqUuidV7::new());
        let jobs = JobRegistry::new();
        let (stop_tx, stop_rx) = oneshot::channel();

        let (started_tx, started_rx) = oneshot::channel::<()>();
        let (unblock_tx, unblock_rx) = oneshot::channel::<()>();
        let started_tx = Arc::new(std::sync::Mutex::new(Some(started_tx)));
        let unblock_rx = Arc::new(tokio::sync::Mutex::new(Some(unblock_rx)));

        let handle = tokio::spawn(run_consolidation_trigger(
            Arc::clone(&db),
            layout,
            uuids,
            jobs,
            default_params(),
            Duration::from_millis(1),
            "build-test",
            move |_window: ConsolidationWindow| {
                let started_tx = Arc::clone(&started_tx);
                let unblock_rx = Arc::clone(&unblock_rx);
                async move {
                    if let Some(tx) = started_tx.lock().expect("lock").take() {
                        let _ = tx.send(());
                    }
                    if let Some(mut rx) = unblock_rx.lock().await.take() {
                        let _ = (&mut rx).await;
                    }
                    Ok(noop_ops())
                }
            },
            stop_rx,
        ));

        started_rx.await.expect("the first tick reached generate");
        let _ = stop_tx.send(());
        assert!(
            !handle.is_finished(),
            "an in-flight tick must not be torn off mid-way by stop"
        );

        let _ = unblock_tx.send(());
        handle.await.expect("no panic");
    }

    /// A `Write` sink that appends into a shared buffer — enough to capture
    /// `tracing` output for an assertion, no new dependency (`fmt`'s blanket
    /// `MakeWriter` impl for `Fn() -> W where W: io::Write` already covers a
    /// closure returning this).
    #[derive(Clone)]
    struct SharedBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("lock").extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// D-046 regression: a failed run is reported, a routine `NoCheckpoint`
    /// is not — logging every tick's routine outcomes would be pure noise
    /// (this loop runs every 15s in production, forever).
    #[tokio::test]
    async fn log_session_tick_outcomes_reports_failures_but_stays_silent_on_routine_outcomes() {
        let (_home, layout, db) = open_state();
        let uuids = SeqUuidV7::new();
        let jobs = JobRegistry::new();
        write_spool_segment(
            &layout,
            "sess-fail",
            1,
            &spool_fixture("sess-fail", "se:fail:1", "SessionEnd", 1_000),
        );
        write_spool_segment(
            &layout,
            "sess-quiet",
            1,
            &spool_fixture("sess-quiet", "pt:quiet:1", "UserPromptSubmit", 1_000),
        );
        let mut params = default_params();
        params.queue_threshold = 50;

        let results = consolidation_trigger_tick(
            &db,
            &layout,
            &uuids,
            &jobs,
            &params,
            1_000,
            "build-test",
            &|_window| async { Err(ClassifiedFailure::transient("router refused")) },
        )
        .await;
        assert_eq!(results.len(), 2, "{results:?}");

        let buf = SharedBuf(std::sync::Arc::new(std::sync::Mutex::new(Vec::new())));
        let subscriber = tracing_subscriber::fmt()
            .with_writer({
                let buf = buf.clone();
                move || buf.clone()
            })
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            log_session_tick_outcomes(&results, &mut HashSet::new());
        });

        let logged = String::from_utf8(buf.0.lock().expect("lock").clone()).expect("utf8");
        assert!(logged.contains("sess-fail"), "{logged}");
        assert!(logged.contains("router refused"), "{logged}");
        assert!(
            !logged.contains("sess-quiet"),
            "a routine NoCheckpoint outcome must not be logged: {logged}"
        );
    }

    /// D-047 regression: `consolidation_trigger_tick`'s own stale-run
    /// recovery step (its first line, ahead of the per-session loop
    /// `log_session_tick_outcomes` already covers) must report a run that
    /// fails again on retry — before D-047 this was a second, un-instrumented
    /// discard site, so a run stuck failing here retried silently forever,
    /// even after D-046 landed.
    #[tokio::test]
    async fn consolidation_trigger_tick_reports_a_still_failing_stale_run() {
        let (_home, layout, db) = open_state();
        let uuids = SeqUuidV7::new();
        let jobs = JobRegistry::new();
        db.writer()
            .transaction(|tx| {
                tx.execute(
                    "INSERT INTO observation_envelope \
                       (observation_id, source_event_id, payload_hash, event_type, \
                        evidence_kind, trust, session_id) \
                     VALUES ('obs-seed-2', 'evt-seed-2', 'deadbeef', 'Stop', 'user_statement', \
                             'normal', 'sess-still-broken')",
                    [],
                )?;
                Ok(())
            })
            .await
            .expect("seed envelope");
        db.writer()
            .transaction(|tx| {
                create_consolidation_run(
                    tx,
                    &NewConsolidationRun {
                        run_id: "run-still-broken",
                        session_id: "sess-still-broken",
                        from_received_seq: 1,
                        to_received_seq: 1,
                        router_version: "v1",
                    },
                    500,
                )?;
                transition_run(tx, "run-still-broken", RunState::Running, 500)?.expect("legal");
                transition_run(tx, "run-still-broken", RunState::Failed, 600)?.expect("legal");
                Ok(())
            })
            .await
            .expect("seed a stale failed run");

        let buf = SharedBuf(std::sync::Arc::new(std::sync::Mutex::new(Vec::new())));
        let subscriber = tracing_subscriber::fmt()
            .with_writer({
                let buf = buf.clone();
                move || buf.clone()
            })
            .with_ansi(false)
            .finish();
        // `set_default` (not `with_default`, which needs a sync closure) so
        // the guard can stay live across the `.await` below — sound because
        // `#[tokio::test]` here is single-threaded (this crate carries no
        // tokio `test-util`/multi-thread feature), so no other task can
        // observe a different subscriber mid-poll.
        let guard = tracing::subscriber::set_default(subscriber);
        let _ = consolidation_trigger_tick(
            &db,
            &layout,
            &uuids,
            &jobs,
            &default_params(),
            1_000,
            "build-test",
            &|_window| async {
                Err(ClassifiedFailure::transient(
                    "no generation provider configured",
                ))
            },
        )
        .await;
        drop(guard);

        let logged = String::from_utf8(buf.0.lock().expect("lock").clone()).expect("utf8");
        assert!(logged.contains("run-still-broken"), "{logged}");
        assert!(
            logged.contains("no generation provider configured"),
            "{logged}"
        );
    }

    /// `D-095`: a terminal floor case is announced on transition, not on every
    /// tick.
    ///
    /// The state is terminal until a human acts, so repeating it says nothing
    /// new and says it forever — measured on a live store, this one line was
    /// 10 707 of 11 134 lines in a day's log (96.2 %), drowning the surface
    /// `D-088` was diagnosed from. Nothing is lost by reporting once: `stats`
    /// and `doctor` both carry the state durably (`D-071`).
    #[tokio::test]
    async fn an_unconsolidatable_session_is_reported_once_not_every_tick() {
        let results = vec![(
            "sess-floor".to_string(),
            SessionTickOutcome::Unconsolidatable {
                from_received_seq: 7,
                to_received_seq: 7,
                dead_letter_run_id: "run-floor".to_string(),
            },
        )];

        let buf = SharedBuf(std::sync::Arc::new(std::sync::Mutex::new(Vec::new())));
        let subscriber = tracing_subscriber::fmt()
            .with_writer({
                let buf = buf.clone();
                move || buf.clone()
            })
            .with_ansi(false)
            .finish();

        let mut reported = HashSet::new();
        tracing::subscriber::with_default(subscriber, || {
            // Three ticks in a row, exactly as the worker would produce them.
            log_session_tick_outcomes(&results, &mut reported);
            log_session_tick_outcomes(&results, &mut reported);
            log_session_tick_outcomes(&results, &mut reported);
        });

        let logged = String::from_utf8(buf.0.lock().expect("lock").clone()).expect("utf8");
        let lines = logged.matches("consolidation unconsolidatable").count();
        assert_eq!(
            lines, 1,
            "a terminal state belongs in the log once per blocking run, not once per tick:\n{logged}"
        );
        assert!(
            logged.contains("run-floor"),
            "the one line must still name the run that needs review:\n{logged}"
        );
    }
}
