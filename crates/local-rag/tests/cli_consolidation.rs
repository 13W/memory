//! `local-rag consolidation retry|abandon <session-id>` acceptance tests
//! (spec 11 §6, `T23-03`, ADR-0014 Decision 1), driving the real compiled
//! binary — mirrors `tests/cli_stats.rs`'s own `open_layout`/`run_cli`/
//! run-seeding helpers (duplicated here per this crate's established
//! per-file-fixture convention).

#![cfg(unix)]

use std::path::Path;
use std::process::{Output, Stdio};

use local_rag_core::paths::StoreLayout;
use local_rag_store::{
    AUDIT_ENTITY_CONSOLIDATION_RUN, FailureKind, LEASE_DURATION_MS, NewConsolidationRun, RunState,
    StateDb, create_consolidation_run, processing_cursor, read_audit_events_for_entity,
    record_run_failure, retry_run, transition_run,
};
use local_rag_test_support::TempHome;

fn open_layout() -> (TempHome, StoreLayout) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    (home, layout)
}

fn run_cli(home: &TempHome, dir: &Path, args: &[&str]) -> Output {
    let mut cmd = home.command(env!("CARGO_BIN_EXE_local-rag"));
    cmd.args(args);
    cmd.current_dir(dir);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.output().expect("run local-rag")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

async fn seed_observations(state: &StateDb, session_id: &str, count: usize) {
    let session_id = session_id.to_string();
    state
        .writer()
        .transaction(move |tx| {
            for i in 0..count {
                tx.execute(
                    "INSERT INTO observation_envelope \
                       (observation_id, source_event_id, payload_hash, event_type, \
                        evidence_kind, trust, session_id) \
                     VALUES (?1, ?2, 'deadbeef', 'Stop', 'user_statement', 'normal', ?3)",
                    rusqlite::params![
                        format!("{session_id}-obs-{i}"),
                        format!("{session_id}-evt-{i}"),
                        session_id
                    ],
                )?;
            }
            Ok(())
        })
        .await
        .expect("seed envelopes");
}

/// A run parked exactly the way `D-117` describes: `mechanical`, **not** a
/// context overflow, fingerprinted with the build the CLI itself will report
/// under — so nothing retries it and nothing shrinks it.
async fn seed_parked_run(
    state: &StateDb,
    run_id: &str,
    session_id: &str,
    from_seq: i64,
    to_seq: i64,
) {
    let (run_id, session_id) = (run_id.to_string(), session_id.to_string());
    state
        .writer()
        .transaction(move |tx| {
            create_consolidation_run(
                tx,
                &NewConsolidationRun {
                    run_id: &run_id,
                    session_id: &session_id,
                    from_received_seq: from_seq,
                    to_received_seq: to_seq,
                    router_version: "v1",
                },
                1_000,
            )?;
            for attempt in 0..2 {
                if attempt == 0 {
                    transition_run(tx, &run_id, RunState::Running, 1_000)?
                        .expect("pending -> running");
                } else {
                    retry_run(tx, &run_id, LEASE_DURATION_MS, 1_000)?.expect("failed -> running");
                }
                record_run_failure(
                    tx,
                    &run_id,
                    FailureKind::Mechanical,
                    "optimistic conflict: expected entry_version 2, found 3",
                    false,
                    Some(local_rag_core::BUILD_ID),
                    1_000,
                )?
                .expect("running -> failed");
            }
            Ok(())
        })
        .await
        .expect("seed parked run");
}

#[tokio::test]
async fn retry_queues_a_parked_run_and_says_it_is_one_attempt() {
    let (home, layout) = open_layout();
    {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        seed_observations(&state, "sess-parked", 3).await;
        seed_parked_run(&state, "run-parked", "sess-parked", 1, 2).await;
    }

    let output = run_cli(
        &home,
        home.path(),
        &["consolidation", "retry", "sess-parked"],
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let text = stdout(&output);
    assert!(
        text.contains("run run-parked queued for one more attempt"),
        "{text}"
    );
    assert!(
        text.contains("ask twice to try twice"),
        "the message must say it is one attempt, not a loop: {text}"
    );

    let state = StateDb::open(layout.state_db()).expect("reopen");
    let read = state.open_read().expect("read conn");
    let state_now: String = read
        .query_row(
            "SELECT state FROM consolidation_run WHERE run_id = 'run-parked'",
            [],
            |r| r.get(0),
        )
        .expect("read state");
    assert_eq!(
        state_now, "running",
        "failed -> running, awaiting the daemon"
    );
}

#[tokio::test]
async fn abandon_moves_the_session_past_the_window_and_says_what_was_lost() {
    let (home, layout) = open_layout();
    {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        seed_observations(&state, "sess-parked", 3).await;
        seed_parked_run(&state, "run-parked", "sess-parked", 1, 2).await;
    }

    let output = run_cli(
        &home,
        home.path(),
        &["consolidation", "abandon", "sess-parked"],
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let text = stdout(&output);
    assert!(text.contains("abandoned run run-parked"), "{text}");
    assert!(
        text.contains("2 observation(s) will never become memory"),
        "the destructive half must be stated, not implied: {text}"
    );

    let state = StateDb::open(layout.state_db()).expect("reopen");
    let read = state.open_read().expect("read conn");
    assert_eq!(
        processing_cursor(&read, "sess-parked").expect("cursor"),
        Some(2)
    );
    let audit = read_audit_events_for_entity(&read, AUDIT_ENTITY_CONSOLIDATION_RUN, "run-parked")
        .expect("audit rows");
    assert_eq!(audit.len(), 1, "the act is recorded exactly once");
    assert_eq!(audit[0].op, "abandon");
}

#[tokio::test]
async fn abandoning_twice_reports_the_no_op_instead_of_failing() {
    let (home, layout) = open_layout();
    {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        seed_observations(&state, "sess-parked", 3).await;
        seed_parked_run(&state, "run-parked", "sess-parked", 1, 2).await;
    }
    let first = run_cli(
        &home,
        home.path(),
        &["consolidation", "abandon", "sess-parked"],
    );
    assert_eq!(first.status.code(), Some(0), "{first:?}");

    // The session still has its third observation, so it is still listed —
    // but nothing blocks it any more, so there is nothing left to abandon.
    let second = run_cli(
        &home,
        home.path(),
        &["consolidation", "abandon", "sess-parked"],
    );
    assert_eq!(second.status.code(), Some(1), "{second:?}");
    assert!(
        stderr(&second).contains("waiting for the next tick, not blocked by a run"),
        "a second abandon must say why there is nothing to do: {}",
        stderr(&second)
    );
}

#[tokio::test]
async fn an_unknown_session_is_a_typed_refusal_not_a_panic() {
    let (home, _layout) = open_layout();
    for verb in ["retry", "abandon"] {
        let output = run_cli(&home, home.path(), &["consolidation", verb, "sess-nope"]);
        assert_eq!(output.status.code(), Some(1), "{verb}: {output:?}");
        let err = stderr(&output);
        assert!(
            err.contains("has no outstanding observations"),
            "{verb}: {err}"
        );
        assert!(
            err.contains("local-rag stats"),
            "{verb}: the refusal must name the way to find the real session ids: {err}"
        );
    }
}
