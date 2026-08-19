//! Startup consolidation crash-resume (spec 02 §4.1 step 5: "crashed
//! consolidation runs with expired leases (08 §4)") — T15-01.
//!
//! `stale_runs`/`retry_run`/`run_once` (`local_rag_store::memory`, T14-06)
//! are the ready-made primitives; this module is the daemon-side driver that
//! walks every stale run. Continuous consolidation triggering (checkpoint on
//! `Stop`, queue-size threshold, best-effort `SessionEnd`) is explicitly
//! **not** this module's job — T14-06's own as-built note names **T15-06**
//! as the owner of that daemon-level trigger; this module's own card text is
//! the narrower "crashed runs with expired leases".
//!
//! # Exactly one caller (D-073)
//!
//! [`resume_stale_consolidation_runs`] is driven **only** from
//! `daemon::consolidation_trigger::consolidation_trigger_tick`, whose
//! `tokio::time::interval` fires its first tick immediately — so spec 02
//! §4.1 step 5's startup catch-up still happens at startup, it is simply the
//! trigger's first tick rather than a second spawned pass.
//!
//! It used to have two callers: this one and a one-shot
//! `lifecycle::spawn_consolidation_resume`, both `tokio::spawn`ed at startup.
//! They read [`stale_runs`] in the same instant — before either had written
//! its failures back — so both retried the identical set, and every parked
//! run burned **two** attempts (and two local-model calls) per daemon start
//! against D-050's documented "a rebuild earns it exactly one more attempt".
//! Measured on a live store: all nine dead-lettered runs gained exactly two
//! attempts across two consecutive restarts, with each `run_id` appearing
//! twice in one microsecond of `logs/daemon.<date>.log`. The lease is no
//! defence here — it serialises the two attempts, it does not prevent the
//! second.

use std::future::Future;
use std::sync::Arc;

use local_rag_core::paths::StoreLayout;
use local_rag_embed::{GeneratorEntry, GeneratorPool};
use local_rag_store::{
    ClassifiedFailure, RunOutcome, RunTransitionError, RunWindow, RunnerError, StaleRun, StateDb,
    WriteError, retry_run, run_once, stale_runs,
};

use super::super::jobs::{JobKind, JobRegistry};

/// A best-effort, network-free generator pool for consolidation resume
/// (spec 10 §1 `[FIXED]`: "the local backend is the working default";
/// D-008's precedent for how a not-yet-installed model degrades — loudly,
/// never by fetching over the network).
///
/// [`local_rag_generate::LlamaGenerator::open`] never fetches: if
/// `ADR-0004`'s default model (`local_rag_generate::DEFAULT_MODEL_ID`) is not
/// already installed on disk (`local-rag init --download-models` never ran,
/// or hasn't finished), this returns an **empty** pool rather than erroring
/// or blocking daemon startup on a multi-GB download. An empty pool's
/// `generate` call fails deterministically
/// ([`local_rag_embed::gen_pool::GeneratorPool::generate`]'s own
/// `GenError::NoProvider`), which [`resume_stale_consolidation_runs`] already
/// turns into `ResumeOutcome::Ran(RunOutcome::Failed(_))` — picked up again
/// by the next daemon start (or T15-06's continuous trigger, once it lands),
/// never silently lost and never blocking this one.
pub fn build_best_effort_pool(layout: &StoreLayout) -> GeneratorPool {
    let Some(entry) = local_rag_generate::find(local_rag_generate::DEFAULT_MODEL_ID) else {
        return GeneratorPool::new(Vec::new());
    };
    match local_rag_generate::LlamaGenerator::open(layout, entry) {
        Ok(generator) => GeneratorPool::new(vec![GeneratorEntry::local(
            entry.model_id,
            Arc::new(generator),
        )]),
        Err(e) => {
            tracing::error!(
                "local-rag: generator pool build failed for {}: {e} — consolidation will report \
                 NoProvider until the next daemon restart",
                entry.model_id
            );
            GeneratorPool::new(Vec::new())
        }
    }
}

/// The result of attempting to resume one stale run.
#[derive(Debug)]
pub enum ResumeOutcome {
    /// `retry_run`'s `failed`/expired-`running` → `running` transition was
    /// refused (most likely a racing caller already reclaimed this run
    /// between the [`stale_runs`] read and this attempt). Not retried
    /// further this pass; the next sweep re-evaluates it from scratch.
    RetryRefused(RunTransitionError),
    /// The lease-acquiring transition itself could not be written.
    RetryWriteFailed(WriteError),
    /// The run was successfully retried and driven through [`run_once`].
    Ran(RunOutcome),
}

/// A failure enumerating the stale-run set itself (distinct from a
/// per-run [`ResumeOutcome`]).
#[derive(Debug)]
pub enum ConsolidationResumeError {
    /// Opening the read-only state connection failed.
    Open(local_rag_store::OpenError),
    /// The `stale_runs` query itself failed.
    Sqlite(rusqlite::Error),
}

/// D-047: report a [`resume_stale_consolidation_runs`] sweep's outcome via
/// `tracing`, shared by both of its call sites — the one-shot startup pass
/// (`daemon::lifecycle::spawn_consolidation_resume`) and the continuous
/// trigger's own per-tick stale-run-recovery step
/// (`daemon::consolidation_trigger::consolidation_trigger_tick`, which
/// D-046 left un-instrumented: it discarded this same sweep's result
/// separately from the `SessionTickOutcome` vector D-046 did log, so a
/// stale run retried every tick — not just once at startup — kept failing
/// silently). Mirrors `lifecycle::spawn_spool_resume`'s per-outcome shape:
/// routine outcomes (`Ran(Applied(_))`) stay silent, everything else is
/// logged — this sweep runs on every tick, forever.
pub fn log_resume_sweep(sweep: Result<Vec<(String, ResumeOutcome)>, ConsolidationResumeError>) {
    match sweep {
        Ok(results) => {
            for (run_id, outcome) in results {
                match outcome {
                    ResumeOutcome::Ran(RunOutcome::Failed(reason)) => {
                        tracing::error!(
                            "local-rag: consolidation resume run {run_id} failed: {reason}"
                        );
                    }
                    ResumeOutcome::RetryWriteFailed(e) => {
                        tracing::error!(
                            "local-rag: consolidation resume retry-write failed for run \
                             {run_id}: {e}"
                        );
                    }
                    ResumeOutcome::RetryRefused(e) => {
                        tracing::warn!(
                            "local-rag: consolidation resume retry refused for run {run_id}: {e}"
                        );
                    }
                    ResumeOutcome::Ran(RunOutcome::Applied(_)) => {}
                }
            }
        }
        Err(e) => {
            tracing::error!("local-rag: consolidation resume sweep failed: {e:?}");
        }
    }
}

/// Resume every stale consolidation run (spec 02 §4.1 step 5, 08 §4), one
/// [`JobRegistry`]-tracked job per run.
///
/// `generate` is the router (`local_rag_memory::router::route` in
/// production, composed by the caller over a
/// [`local_rag_embed::GeneratorPool`] — see
/// [`super::build_best_effort_pool`]); passed by shared reference so one
/// value serves every run in the sweep (mirrors `run_once`'s own
/// `FnOnce(ConsolidationWindow) -> Fut` shape: `&G` also satisfies
/// `FnOnce` for any `G: Fn`).
#[allow(clippy::too_many_arguments)]
pub async fn resume_stale_consolidation_runs<G, Fut>(
    db: &StateDb,
    jobs: &JobRegistry,
    lease_ms: i64,
    renew_interval_ms: i64,
    now_ms: i64,
    build_id: &str,
    generate: G,
) -> Result<Vec<(String, ResumeOutcome)>, ConsolidationResumeError>
where
    G: Fn(local_rag_store::ConsolidationWindow) -> Fut,
    Fut: Future<Output = Result<Vec<local_rag_store::GeneratedOp>, ClassifiedFailure>>,
{
    let stale: Vec<StaleRun> = {
        let conn = db.open_read().map_err(ConsolidationResumeError::Open)?;
        stale_runs(&conn, now_ms, build_id).map_err(ConsolidationResumeError::Sqlite)?
    };

    let mut results = Vec::with_capacity(stale.len());
    for run in stale {
        let _job = jobs.begin(JobKind::ConsolidationResume);
        let run_id = run.run_id.clone();

        let retry_outcome = db
            .writer()
            .transaction({
                let run_id = run_id.clone();
                move |tx| retry_run(tx, &run_id, lease_ms, now_ms)
            })
            .await;

        let outcome = match retry_outcome {
            Err(write_err) => ResumeOutcome::RetryWriteFailed(write_err),
            Ok(Err(transition_err)) => ResumeOutcome::RetryRefused(transition_err),
            Ok(Ok(())) => {
                let window = RunWindow {
                    run_id: run.run_id.clone(),
                    session_id: run.session_id.clone(),
                    from_received_seq: run.from_received_seq,
                    to_received_seq: run.to_received_seq,
                };
                let lease_until = now_ms + lease_ms;
                let run_result = run_once(
                    db,
                    window,
                    lease_until,
                    lease_ms,
                    renew_interval_ms,
                    now_ms,
                    build_id,
                    &generate,
                )
                .await;
                match run_result {
                    Ok(outcome) => ResumeOutcome::Ran(outcome),
                    Err(runner_err) => {
                        // `run_once` itself already routes a generator/apply
                        // rejection to `RunOutcome::Failed` internally; a
                        // `RunnerError` here is an infra failure (couldn't
                        // even read the window, or a lease-renewal write
                        // failed) — represent it as a Ran(Failed(..)) so this
                        // driver's own outcome type does not need to grow a
                        // second, largely-redundant error arm.
                        ResumeOutcome::Ran(RunOutcome::Failed(runner_err_description(&runner_err)))
                    }
                }
            }
        };
        results.push((run_id, outcome));
    }
    Ok(results)
}

fn runner_err_description(e: &RunnerError) -> String {
    e.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use local_rag_store::{
        GeneratedOp, LEASE_DURATION_MS, LEASE_RENEW_INTERVAL_MS, NewConsolidationRun, RunState,
        acquire_lease, create_consolidation_run, transition_run,
    };
    use local_rag_test_support::TempHome;

    fn open_state() -> (TempHome, StoreLayout, StateDb) {
        let home = TempHome::new().expect("temp home");
        let layout = StoreLayout::new(home.join("local-rag"));
        layout.ensure().expect("ensure store tree");
        let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
        (home, layout, db)
    }

    async fn seed_failed_run(db: &StateDb, run_id: &str, session_id: &str) {
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
                            to_received_seq: 3,
                            router_version: "v1",
                        },
                        1_000,
                    )?;
                    transition_run(tx, &run_id, RunState::Running, 1_000)?.expect("legal");
                    transition_run(tx, &run_id, RunState::Failed, 1_500)?.expect("legal");
                    Ok(())
                }
            })
            .await
            .expect("seed failed run");
    }

    async fn seed_lease_expired_running_run(db: &StateDb, run_id: &str, session_id: &str) {
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
                            to_received_seq: 3,
                            router_version: "v1",
                        },
                        1_000,
                    )?;
                    transition_run(tx, &run_id, RunState::Running, 1_000)?.expect("legal");
                    acquire_lease(tx, &run_id, 1_200)?; // already expired by now=5_000
                    Ok(())
                }
            })
            .await
            .expect("seed expired-lease run");
    }

    fn noop_ops() -> Vec<GeneratedOp> {
        vec![GeneratedOp::Noop]
    }

    #[tokio::test]
    async fn no_stale_runs_resumes_nothing() {
        let (_home, _layout, db) = open_state();
        let jobs = JobRegistry::new();
        let results = resume_stale_consolidation_runs(
            &db,
            &jobs,
            LEASE_DURATION_MS,
            LEASE_RENEW_INTERVAL_MS,
            5_000,
            "build-test",
            |_window| async { Ok(noop_ops()) },
        )
        .await
        .expect("resume");
        assert!(results.is_empty());
        assert!(jobs.is_empty());
    }

    #[tokio::test]
    async fn a_failed_run_is_retried_and_applied() {
        let (_home, _layout, db) = open_state();
        let jobs = JobRegistry::new();
        seed_failed_run(&db, "run-failed", "sess-1").await;

        let results = resume_stale_consolidation_runs(
            &db,
            &jobs,
            LEASE_DURATION_MS,
            LEASE_RENEW_INTERVAL_MS,
            5_000,
            "build-test",
            |_window| async { Ok(noop_ops()) },
        )
        .await
        .expect("resume");

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "run-failed");
        match &results[0].1 {
            ResumeOutcome::Ran(RunOutcome::Applied(_)) => {}
            other => panic!("expected Ran(Applied(_)), got {other:?}"),
        }
        assert!(jobs.is_empty());
    }

    #[tokio::test]
    async fn a_lease_expired_running_run_is_retried_too() {
        let (_home, _layout, db) = open_state();
        let jobs = JobRegistry::new();
        seed_lease_expired_running_run(&db, "run-expired", "sess-1").await;

        let results = resume_stale_consolidation_runs(
            &db,
            &jobs,
            LEASE_DURATION_MS,
            LEASE_RENEW_INTERVAL_MS,
            5_000,
            "build-test",
            |_window| async { Ok(noop_ops()) },
        )
        .await
        .expect("resume");

        assert_eq!(results.len(), 1);
        match &results[0].1 {
            ResumeOutcome::Ran(RunOutcome::Applied(_)) => {}
            other => panic!("expected Ran(Applied(_)), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_generator_rejection_routes_to_failed_not_a_hard_error() {
        let (_home, _layout, db) = open_state();
        let jobs = JobRegistry::new();
        seed_failed_run(&db, "run-failed", "sess-1").await;

        let results = resume_stale_consolidation_runs(
            &db,
            &jobs,
            LEASE_DURATION_MS,
            LEASE_RENEW_INTERVAL_MS,
            5_000,
            "build-test",
            |_window| async { Err(ClassifiedFailure::transient("router refused")) },
        )
        .await
        .expect("resume must not hard-fail on a generator rejection");

        assert_eq!(results.len(), 1);
        match &results[0].1 {
            ResumeOutcome::Ran(RunOutcome::Failed(reason)) => {
                assert!(reason.contains("router refused"), "{reason}");
            }
            other => panic!("expected Ran(Failed(_)), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_applied_run_is_never_selected_as_stale() {
        let (_home, _layout, db) = open_state();
        let jobs = JobRegistry::new();
        db.writer()
            .transaction(|tx| {
                create_consolidation_run(
                    tx,
                    &NewConsolidationRun {
                        run_id: "run-applied",
                        session_id: "sess-1",
                        from_received_seq: 1,
                        to_received_seq: 3,
                        router_version: "v1",
                    },
                    1_000,
                )?;
                transition_run(tx, "run-applied", RunState::Running, 1_000)?.expect("legal");
                transition_run(tx, "run-applied", RunState::Applied, 1_500)?.expect("legal");
                Ok(())
            })
            .await
            .expect("seed applied run");

        let results = resume_stale_consolidation_runs(
            &db,
            &jobs,
            LEASE_DURATION_MS,
            LEASE_RENEW_INTERVAL_MS,
            5_000,
            "build-test",
            |_window| async { Ok(noop_ops()) },
        )
        .await
        .expect("resume");
        assert!(results.is_empty());
    }

    /// D-050: the actual retry-storm bug this task fixes. Before this task,
    /// `daemon::consolidation_trigger_tick` called this exact function every
    /// 15s, forever, with no attempt counter or dead-letter — a
    /// deterministically-failing generator (the live incident: `missing
    /// field confidence_signal`, `trailing characters`, `EOF while parsing a
    /// string`, all byte-for-byte identical on every retry) burned a real
    /// local-model inference call on every single tick, hours on end, across
    /// a daemon restart. This proves the fix: a `Mechanical` failure is
    /// retried exactly once per build — the sweep that classifies it — never
    /// again on the same `build_id`, no matter how many more sweeps run;
    /// only a rebuild (a new `build_id`) earns it one more attempt.
    #[tokio::test]
    async fn a_mechanical_failure_is_retried_once_then_dead_lettered_until_the_build_changes() {
        let (_home, _layout, db) = open_state();
        let jobs = JobRegistry::new();
        seed_failed_run(&db, "run-broken", "sess-1").await;

        let calls = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let generate = {
            let calls = std::sync::Arc::clone(&calls);
            move |_window| {
                let calls = std::sync::Arc::clone(&calls);
                async move {
                    calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    Err(local_rag_store::ClassifiedFailure::mechanical(
                        "missing field confidence_signal",
                    ))
                }
            }
        };

        // Three sweeps on the same build: the first one classifies the run
        // Mechanical and burns the one real attempt; the next two must find
        // it dead-lettered and never call the generator again.
        for _ in 0..3 {
            resume_stale_consolidation_runs(
                &db,
                &jobs,
                LEASE_DURATION_MS,
                LEASE_RENEW_INTERVAL_MS,
                5_000,
                "build-1",
                &generate,
            )
            .await
            .expect("resume");
        }
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "retry-storm: a mechanical failure must not be re-invoked every sweep on the same build"
        );

        // A rebuild (new build_id) earns it exactly one more attempt.
        resume_stale_consolidation_runs(
            &db,
            &jobs,
            LEASE_DURATION_MS,
            LEASE_RENEW_INTERVAL_MS,
            5_000,
            "build-2",
            &generate,
        )
        .await
        .expect("resume");
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::Relaxed),
            2,
            "a rebuild gets exactly one more attempt, not a fresh unlimited retry budget"
        );
    }
}
