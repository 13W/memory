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
//!    pass makes), then open a new consolidation window if either this
//!    import just saw a `Stop`/`SessionEnd` row, or the session's backlog
//!    ([`pending_backlog`]) has crossed the configured threshold.
//!
//! No step here ever sleeps — [`consolidation_trigger_tick`] is directly
//! unit-testable with fixed `now_ms` literals, mirroring
//! `crates/store/tests/consolidation_runner.rs`'s own style.
//!
//! ## A known race against the startup spool-import pass
//!
//! At daemon boot, this worker's very first tick races
//! `daemon::resume::resume_spool_import` (spec 02 §4.1 step 5) for the same
//! session's spool tail — both independently call `import_session_tail`.
//! Whichever import call actually consumes the fresh bytes is the one that
//! observes `saw_stop`/`saw_session_end`; if the startup pass wins, this
//! worker's own first-tick import sees nothing new, so the checkpoint
//! condition is **missed** for that specific envelope (not just a harmless
//! redundant read) — the queue-size-threshold path is the fallback that
//! still catches it once enough backlog accumulates. Away from daemon
//! startup (the overwhelmingly common case — a `Stop` arriving while the
//! daemon has been running for a while), no such race exists: nothing else
//! is concurrently importing that session's tail. Not engineered around
//! here: doing so would need `DaemonHandle::start` to serialize this
//! worker's first tick behind the startup pass's own `JoinHandle`, which
//! `resume_handles: Vec<JoinHandle<()>>` isn't structured to expose to a
//! second caller — not justified for a boot-time-only, self-healing gap.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use local_rag_core::identity::UuidSource;
use local_rag_core::paths::StoreLayout;
use local_rag_store::{
    ClassifiedFailure, ConsolidationWindow, GeneratedOp, ImportError, RequestRoot, RunOutcome,
    SnapshotOutcome, StateDb, WriteError, import_session_tail, known_spool_sessions, open_next_run,
    pending_backlog, run_once, sessions_with_pending_backlog,
};
use tokio::sync::oneshot;

use super::jobs::{JobKind, JobRegistry};
use super::resume::{log_resume_sweep, resume_stale_consolidation_runs};

/// `consolidation_run.router_version` for every window this worker opens —
/// the one value actually used anywhere in this codebase today (every
/// existing `open_next_run`/`create_consolidation_run` call site, in both
/// `crates/store`'s own tests and T15-01's daemon-side resume tests, passes
/// `"v1"`; no caller has ever passed `"v0"`).
const ROUTER_VERSION: &str = "v1";

/// Tunable parameters for the continuous consolidation-trigger worker
/// (D-024). `batch_size`/`queue_threshold` are `[SPEC]`-chosen values (no
/// default is specified anywhere normative — see
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
    pub queue_threshold: i64,
    pub payload_ttl_hours: u64,
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
    let request_root = RequestRoot::default();
    let mut results = Vec::with_capacity(sessions.len());

    for session_id in sessions {
        let saw_checkpoint = {
            let _job = jobs.begin(JobKind::SpoolImport);
            let outcome = import_session_tail(
                db,
                layout,
                &session_id,
                &request_root,
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

        let backlog_over_threshold = match db.open_read() {
            Ok(read) => pending_backlog(&read, &session_id).unwrap_or(0) >= params.queue_threshold,
            Err(_) => false,
        };

        if !(saw_checkpoint || backlog_over_threshold) {
            results.push((session_id, SessionTickOutcome::NoCheckpoint));
            continue;
        }

        let _job = jobs.begin(JobKind::ConsolidationTrigger);
        let run_id = uuids.next_uuid().to_string();
        let (batch, lease_ms) = (params.batch_size, params.lease_ms);
        let (rid, sid) = (run_id.clone(), session_id.clone());
        let snapshot = db
            .writer()
            .transaction(move |tx| {
                open_next_run(tx, &rid, &sid, batch, ROUTER_VERSION, lease_ms, now_ms)
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
    let mut ticker = tokio::time::interval(poll_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = &mut stop => return,
            _ = ticker.tick() => {
                let now_ms = system_now_ms();
                let results = consolidation_trigger_tick(&db, &layout, &*uuids, &jobs, &params, now_ms, build_id, &generate).await;
                log_session_tick_outcomes(&results);
            }
        }
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
fn log_session_tick_outcomes(results: &[(String, SessionTickOutcome)]) {
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
        LEASE_DURATION_MS, LEASE_RENEW_INTERVAL_MS, NewConsolidationRun, RunState, acquire_lease,
        create_consolidation_run, transition_run,
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

    fn default_params() -> ConsolidationTriggerParams {
        ConsolidationTriggerParams {
            lease_ms: LEASE_DURATION_MS,
            renew_interval_ms: LEASE_RENEW_INTERVAL_MS,
            batch_size: 20,
            queue_threshold: 50,
            payload_ttl_hours: 72,
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
            log_session_tick_outcomes(&results);
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
}
