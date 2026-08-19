//! `local-rag stats [--json]` acceptance tests (spec 11 §6, D-025), driving
//! the real compiled binary — mirrors `tests/cli_repo.rs`'s own
//! `open_layout`/`run_cli`/worktree-seeding helpers (duplicated here per
//! this crate's established per-file-fixture convention).

#![cfg(unix)]

use std::path::Path;
use std::process::{Output, Stdio};

use local_rag::daemon::gitroot;
use local_rag_core::paths::StoreLayout;
use local_rag_store::{
    CURRENT_NORMALIZER_VERSION, FailureKind, GLOBAL_SCOPE_OWNER_ID, GenerationState,
    LEASE_DURATION_MS, MAX_NORMALIZATION_ATTEMPTS, MemoryKind, NewConsolidationRun, NewMemoryEntry,
    NormalizationStatus, NormalizationWrite, ProposedOperation, RunState, ScopeKind, StateDb,
    UpsertOutcome, allocate_generation, create_consolidation_run, create_memory_entry,
    create_repository, create_worktree, insert_projection_state, observe_repository_path,
    observe_worktree_path, propose_candidate, record_run_failure, retry_run, transition_generation,
    transition_run, upsert_normalization,
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

async fn seed_active_repo_and_worktree(
    layout: &StoreLayout,
    repo_id: &str,
    worktree_id: &str,
    path: &Path,
) {
    let facts = gitroot::probe(path).expect("probe the seeded path");
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    let (repo_id, worktree_id) = (repo_id.to_string(), worktree_id.to_string());
    db.writer()
        .transaction(move |tx| {
            create_repository(tx, &repo_id, facts.remote_fingerprint.as_deref(), 1_000)?;
            create_worktree(tx, &worktree_id, &repo_id, facts.kind, 1_000)?;
            observe_worktree_path(
                tx,
                &worktree_id,
                &facts.observed_canonical_path,
                &facts.display_path,
                &facts.path_fingerprint,
                1_000,
            )?;
            observe_repository_path(tx, &repo_id, &facts.observed_canonical_path, 1_000)?;
            insert_projection_state(tx, &worktree_id, 1_000)
        })
        .await
        .expect("seed active repo+worktree");
}

async fn seed_entry(state: &StateDb, memory_id: &str, kind: MemoryKind, text: &str, now_ms: i64) {
    let (id, text) = (memory_id.to_string(), text.to_string());
    state
        .writer()
        .transaction(move |tx| {
            create_memory_entry(
                tx,
                &NewMemoryEntry {
                    memory_id: &id,
                    kind,
                    text: &text,
                    canonical_key: None,
                    scope_kind: ScopeKind::Global,
                    scope_owner_id: GLOBAL_SCOPE_OWNER_ID,
                    confidence: 0.5,
                    importance: 0.5,
                    valid_from_tree: None,
                    last_verified_tree: None,
                    supersedes_id: None,
                },
                now_ms,
            )
        })
        .await
        .expect("seed entry tx")
        .expect("seed entry domain");
}

/// Insert a minimal, standalone `observation_envelope` row (D-049) — no
/// repo/worktree/payload, just enough for `observation_envelope_count`/
/// `total_pending_backlog` to see it. Mirrors `crates/store/tests/memory.rs`'s
/// own `seed_observation`, duplicated here per this crate's established
/// per-file-fixture convention.
async fn seed_observation_envelope(state: &StateDb, observation_id: &str, session_id: &str) {
    let (oid, sid) = (observation_id.to_string(), session_id.to_string());
    state
        .writer()
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO observation_envelope \
                   (observation_id, source_event_id, payload_hash, event_type, evidence_kind, \
                    trust, session_id) \
                 VALUES (?1, ?1, 'deadbeef', 'Stop', 'user_statement', 'normal', ?2)",
                [&oid, &sid],
            )
        })
        .await
        .expect("seed observation envelope");
}

/// Create a `consolidation_run` row in its initial `pending` state (D-049) —
/// no existing seed helper for this table in this crate's tests.
async fn seed_consolidation_run(
    state: &StateDb,
    run_id: &str,
    session_id: &str,
    from_received_seq: i64,
    to_received_seq: i64,
    now_ms: i64,
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
                    from_received_seq,
                    to_received_seq,
                    router_version: "v1",
                },
                now_ms,
            )
        })
        .await
        .expect("seed consolidation run");
}

/// Create a `consolidation_run` row already in D-058's floor case (`pending
/// -> running -> failed`, classified `Mechanical`/context-overflow, on
/// `local_rag_core::BUILD_ID` — the same build the CLI subprocess runs
/// under) — exactly the shape `unconsolidatable_sessions` looks for.
async fn seed_unconsolidatable_run(
    state: &StateDb,
    run_id: &str,
    session_id: &str,
    from_received_seq: i64,
    to_received_seq: i64,
    now_ms: i64,
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
                    from_received_seq,
                    to_received_seq,
                    router_version: "v1",
                },
                now_ms,
            )?;
            transition_run(tx, &run_id, RunState::Running, now_ms)?.expect("pending -> running");
            record_run_failure(
                tx,
                &run_id,
                FailureKind::Mechanical,
                "request needs 99999 tokens, model context is 32768",
                true,
                Some(local_rag_core::BUILD_ID),
                now_ms,
            )?
            .expect("running -> failed");
            Ok(())
        })
        .await
        .expect("seed unconsolidatable run");
}

/// D-071: a run that really went through `attempts` recorded failures and
/// ended dead-lettered on the running build — the shape of the D-069 incident
/// (run `01a01648-…`, three observations, 627 attempts, the same
/// `UNIQUE constraint` failure every time), reproduced through the real
/// `retry_run`/`record_run_failure` cycle rather than a hand-written row.
async fn seed_stuck_run(
    state: &StateDb,
    run_id: &str,
    session_id: &str,
    from_received_seq: i64,
    to_received_seq: i64,
    attempts: i64,
    now_ms: i64,
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
                    from_received_seq,
                    to_received_seq,
                    router_version: "v1",
                },
                now_ms,
            )?;
            for attempt in 0..attempts {
                if attempt == 0 {
                    transition_run(tx, &run_id, RunState::Running, now_ms)?
                        .expect("pending -> running");
                } else {
                    retry_run(tx, &run_id, LEASE_DURATION_MS, now_ms)?.expect("failed -> running");
                }
                record_run_failure(
                    tx,
                    &run_id,
                    FailureKind::Mechanical,
                    "state transaction failed (rolled back): UNIQUE constraint failed: \
                     candidate_evidence.candidate_id, candidate_evidence.observation_id",
                    false,
                    Some(local_rag_core::BUILD_ID),
                    now_ms,
                )?
                .expect("running -> failed");
            }
            Ok(())
        })
        .await
        .expect("seed stuck run");
}

async fn seed_candidate(state: &StateDb, candidate_id: &str, now_ms: i64) {
    let cid = candidate_id.to_string();
    state
        .writer()
        .transaction(move |tx| {
            let op = ProposedOperation::Create {
                memory_id: "mem-target".to_string(),
                kind: "fact".to_string(),
                text: "candidate-proposed text".to_string(),
                canonical_key: None,
                scope_kind: "global".to_string(),
                scope_owner_id: GLOBAL_SCOPE_OWNER_ID.to_string(),
                confidence: 0.5,
                importance: 0.5,
                valid_from_tree: None,
                last_verified_tree: None,
            };
            propose_candidate(tx, &cid, &op, &[], &[], now_ms)
        })
        .await
        .expect("seed candidate tx");
}

#[test]
fn stats_rejects_an_unknown_argument() {
    let (home, _layout) = open_layout();
    let output = run_cli(&home, home.path(), &["stats", "--bogus"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
}

#[test]
fn stats_on_an_empty_store_reports_zero_counts_and_no_worktree() {
    let (home, _layout) = open_layout();
    let output = run_cli(&home, home.path(), &["stats"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let text = stdout(&output);
    assert!(text.contains("memory entries: none"), "{text}");
    assert!(text.contains("pending candidates: none"), "{text}");
    assert!(text.contains("worktree: (unresolved)"), "{text}");
    // D-049: observations pillar + consolidation backlog/progress, honestly
    // "unknown"/"none" on an empty store rather than a fabricated number.
    assert!(text.contains("observations: 0 total"), "{text}");
    assert!(text.contains("consolidation runs: none"), "{text}");
    assert!(text.contains("consolidation pending backlog: 0"), "{text}");
    assert!(
        text.contains("consolidation progress: unknown (no observations yet)"),
        "{text}"
    );
    assert!(
        text.contains("consolidation eta: unknown (no measurable throughput)"),
        "{text}"
    );
    assert!(
        text.contains("consolidation oldest pending run: none (fully caught up)"),
        "{text}"
    );
    // D-058: silent on a healthy store — no line at all, not "0 sessions".
    assert!(!text.contains("consolidation unconsolidatable"), "{text}");
}

#[tokio::test]
async fn stats_reports_seeded_counts_and_the_resolved_worktree_block() {
    let (home, layout) = open_layout();
    {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        seed_entry(&state, "mem-a", MemoryKind::Fact, "a fact", 1_000).await;
        seed_entry(&state, "mem-b", MemoryKind::Task, "a task", 2_000).await;
        seed_candidate(&state, "cand-a", 1_000).await;
        // D-049: two envelopes (received_seq 1, 2 -- fresh store, one global
        // sequence) and one still-`pending` consolidation run, no cursor
        // advanced yet -- backlog equals the session's own max received_seq.
        seed_observation_envelope(&state, "obs-1", "sess-1").await;
        seed_observation_envelope(&state, "obs-2", "sess-1").await;
        seed_consolidation_run(&state, "run-1", "sess-1", 1, 1, 1_000).await;
    }
    seed_active_repo_and_worktree(&layout, "repo-1", "wt-1", home.path()).await;

    let output = run_cli(&home, home.path(), &["stats"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let text = stdout(&output);
    assert!(text.contains("memory entries  fact/active: 1"), "{text}");
    assert!(text.contains("memory entries  task/active: 1"), "{text}");
    assert!(text.contains("pending candidates  pending: 1"), "{text}");
    assert!(
        text.contains("worktree: repo repo-1 / worktree wt-1"),
        "{text}"
    );
    assert!(text.contains("observations: 2 total"), "{text}");
    assert!(text.contains("consolidation runs  pending: 1"), "{text}");
    assert!(text.contains("consolidation pending backlog: 2"), "{text}");
    assert!(text.contains("consolidation progress: 0.0%"), "{text}");
    assert!(
        text.contains("consolidation eta: unknown (no measurable throughput)"),
        "{text}",
    );
    assert!(
        !text.contains("consolidation oldest pending run: none"),
        "a pending run exists, its created_at must be reported: {text}"
    );

    let output = run_cli(&home, home.path(), &["stats", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid json");
    assert_eq!(json["worktree"]["repo_id"], "repo-1");
    assert_eq!(json["worktree"]["worktree_id"], "wt-1");
    assert_eq!(
        json["memory"]["entries_by_kind_state"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(json["observations"]["total"], 2);
    assert_eq!(
        json["consolidation"]["runs_by_state"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(json["consolidation"]["pending_backlog_total"], 2);
    assert_eq!(json["consolidation"]["progress_pct"], 0.0);
    assert!(json["consolidation"]["eta_seconds"].is_null());
    assert!(!json["consolidation"]["oldest_pending_run_created_at"].is_null());
    assert_eq!(
        json["consolidation"]["unconsolidatable_sessions"]
            .as_array()
            .unwrap()
            .len(),
        0,
        "no unconsolidatable session was seeded"
    );
}

/// D-058: `local-rag stats` is the exact tool this deviation's own incident
/// was found *without* — a session permanently stuck at the floor case
/// (a single observation still overflowing the model's context) must be
/// named explicitly, not folded silently into the ordinary backlog/progress
/// numbers.
#[tokio::test]
async fn stats_reports_an_unconsolidatable_session_needing_manual_review() {
    let (home, layout) = open_layout();
    {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        seed_observation_envelope(&state, "obs-1", "sess-stuck").await;
        seed_unconsolidatable_run(&state, "run-stuck", "sess-stuck", 1, 1, 1_000).await;
    }

    let output = run_cli(&home, home.path(), &["stats"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let text = stdout(&output);
    assert!(
        text.contains("consolidation unconsolidatable: 1 session(s)"),
        "{text}"
    );
    assert!(text.contains("session sess-stuck"), "{text}");
    assert!(text.contains("dead-letter run run-stuck"), "{text}");

    let output = run_cli(&home, home.path(), &["stats", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid json");
    let sessions = json["consolidation"]["unconsolidatable_sessions"]
        .as_array()
        .unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["session_id"], "sess-stuck");
    assert_eq!(sessions[0]["dead_letter_run_id"], "run-stuck");
    assert_eq!(sessions[0]["from_received_seq"], 1);
    assert_eq!(sessions[0]["to_received_seq"], 1);
}

/// X-008: `active_generation=<uuid>` never answered "is this index current?".
/// The age of that generation does, and a generation built but never switched on
/// is called out — the same two facts `doctor` and `project status` now report.
#[tokio::test]
async fn stats_reports_the_index_age_and_any_stuck_generation() {
    let (home, layout) = open_layout();
    let dir = home.join("wt");
    std::fs::create_dir_all(&dir).expect("create worktree dir");
    let worktree_id = "018f0000-0000-7000-8000-0000000000c1";
    seed_active_repo_and_worktree(
        &layout,
        "018f0000-0000-7000-8000-0000000000c0",
        worktree_id,
        &dir,
    )
    .await;

    let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
    // One generation left in `projection_ready`: nothing is active, so this is
    // built work that is not being served.
    let generation_id = "018f0000-0000-7000-8000-0000000000c2";
    let (w, g) = (worktree_id.to_string(), generation_id.to_string());
    state
        .writer()
        .transaction(move |tx| allocate_generation(tx, &w, &g, 1_786_000_000_000).map(|_| ()))
        .await
        .expect("allocate generation");
    let g2 = generation_id.to_string();
    state
        .writer()
        .transaction(move |tx| transition_generation(tx, &g2, GenerationState::ProjectionReady))
        .await
        .expect("transition tx")
        .expect("building -> projection_ready is legal");
    drop(state);

    let out = run_cli(&home, &dir, &["stats"]);
    let text = stdout(&out);
    assert!(out.status.success(), "stats must succeed: {text}");
    assert!(
        text.contains("index age: nothing active — nothing is being served"),
        "with no active generation the age line must say so plainly: {text}"
    );
    assert!(
        text.contains("STUCK: generation #1 is projection_ready but never became active"),
        "and the built-but-unserved generation must be named: {text}"
    );
}

/// D-071: the blind spot the D-069 incident exposed — every number `stats`
/// printed looked healthy while one run was on its 627th attempt, each attempt
/// a full local-model generation. The run must now be named, in both output
/// modes, without a single manual SQL query.
#[tokio::test]
async fn stats_reports_a_stuck_consolidation_run_with_its_attempt_count() {
    let (home, layout) = open_layout();
    {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        seed_observation_envelope(&state, "obs-1", "sess-wedged").await;
        seed_stuck_run(
            &state,
            "run-wedged",
            "sess-wedged",
            26115,
            26117,
            627,
            1_000,
        )
        .await;
    }

    let output = run_cli(&home, home.path(), &["stats"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let text = stdout(&output);
    assert!(text.contains("consolidation stuck runs: 1"), "{text}");
    assert!(
        text.contains("run run-wedged session sess-wedged"),
        "{text}"
    );
    assert!(text.contains("received_seq 26115..=26117"), "{text}");
    assert!(text.contains("627 attempt(s)"), "{text}");
    assert!(text.contains("dead-lettered on this build"), "{text}");
    assert!(
        text.contains("UNIQUE constraint failed: candidate_evidence"),
        "the recorded failure itself, not just a count: {text}"
    );

    let output = run_cli(&home, home.path(), &["stats", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid json");
    let stuck = json["consolidation"]["stuck_runs"].as_array().unwrap();
    assert_eq!(stuck.len(), 1);
    assert_eq!(stuck[0]["run_id"], "run-wedged");
    assert_eq!(stuck[0]["session_id"], "sess-wedged");
    assert_eq!(stuck[0]["attempt_count"], 627);
    assert_eq!(stuck[0]["dead_lettered"], true);
    assert_eq!(stuck[0]["last_failure_kind"], "mechanical");
    assert_eq!(stuck[0]["from_received_seq"], 26115);
    assert_eq!(stuck[0]["to_received_seq"], 26117);
}

/// The counterpart: a healthy store prints no stuck-run block at all, and its
/// JSON carries an empty list — the same "silent when there is nothing to say"
/// discipline the D-058 block next to it already follows.
#[tokio::test]
async fn stats_says_nothing_about_stuck_runs_on_a_healthy_store() {
    let (home, layout) = open_layout();
    {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        seed_observation_envelope(&state, "obs-1", "sess-ok").await;
        seed_consolidation_run(&state, "run-ok", "sess-ok", 1, 1, 1_000).await;
    }

    let output = run_cli(&home, home.path(), &["stats"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(
        !stdout(&output).contains("stuck runs"),
        "{}",
        stdout(&output)
    );

    let output = run_cli(&home, home.path(), &["stats", "--json"]);
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid json");
    assert_eq!(
        json["consolidation"]["stuck_runs"],
        serde_json::json!([]),
        "the key is always present, so a consumer never has to guess"
    );
}

// ---------------------------------------------------------------------------
// T21-08: the `memory.normalization` block (ADR-0010)
// ---------------------------------------------------------------------------

/// Record one normalization outcome over `text` — which must be the entry's
/// current text, since `upsert_normalization` refuses a stale write.
async fn seed_normalization(
    state: &StateDb,
    memory_id: &str,
    text: &str,
    status: NormalizationStatus,
    attempt_count: i64,
) {
    let id = memory_id.to_string();
    let sha = local_rag_core::hash::sha256_hex(text.as_bytes());
    let outcome = state
        .writer()
        .transaction(move |tx| {
            upsert_normalization(
                tx,
                &NormalizationWrite {
                    memory_id: &id,
                    status,
                    source_text_sha256: &sha,
                    normalized_text: match status {
                        NormalizationStatus::Ready => Some("the English variant"),
                        _ => None,
                    },
                    source_language: Some("ru"),
                    normalizer_model_id: Some("test-normalizer"),
                    prompt_version: Some(1),
                    normalizer_version: CURRENT_NORMALIZER_VERSION,
                    attempt_count,
                    last_error: match status {
                        NormalizationStatus::Failed => Some("answer was not one object"),
                        _ => None,
                    },
                    next_attempt_at: None,
                },
                3_000,
            )
        })
        .await
        .expect("seed normalization tx");
    assert_eq!(
        outcome,
        UpsertOutcome::Written,
        "fixture must actually land"
    );
}

fn normalization_block(text: &str) -> serde_json::Value {
    let value: serde_json::Value = serde_json::from_str(text).expect("valid json");
    value["memory"]["normalization"].clone()
}

#[test]
fn stats_on_an_empty_store_reports_an_empty_normalization_block() {
    let (home, _layout) = open_layout();
    let output = run_cli(&home, home.path(), &["stats", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let block = normalization_block(&stdout(&output));

    assert_eq!(block["counts_by_status"], serde_json::json!([]));
    assert_eq!(block["pending"], 0);
    assert_eq!(block["dead_letter"], 0);
    assert_eq!(block["normalizer_version"], CURRENT_NORMALIZER_VERSION);

    let human = stdout(&run_cli(&home, home.path(), &["stats"]));
    assert!(
        human.contains("memory normalization: none recorded"),
        "{human}"
    );
    assert!(human.contains("memory normalization pending: 0"), "{human}");
    // Silent when there is nothing to say, like D-058's and D-071's blocks.
    assert!(!human.contains("dead-letter"), "{human}");
}

#[tokio::test]
async fn stats_reports_a_partially_normalized_store() {
    let (home, layout) = open_layout();
    {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        seed_entry(
            &state,
            "mem-ru",
            MemoryKind::Fact,
            "запись по-русски",
            1_000,
        )
        .await;
        seed_entry(&state, "mem-en", MemoryKind::Fact, "an english note", 2_000).await;
        seed_entry(
            &state,
            "mem-new",
            MemoryKind::Fact,
            "ещё одна запись",
            3_000,
        )
        .await;
        seed_normalization(
            &state,
            "mem-ru",
            "запись по-русски",
            NormalizationStatus::Ready,
            1,
        )
        .await;
        seed_normalization(
            &state,
            "mem-en",
            "an english note",
            NormalizationStatus::Skipped,
            0,
        )
        .await;
    }

    let output = run_cli(&home, home.path(), &["stats", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let block = normalization_block(&stdout(&output));

    assert_eq!(
        block["counts_by_status"],
        serde_json::json!([
            {"status": "ready", "count": 1},
            {"status": "skipped", "count": 1},
        ]),
        "empty buckets are omitted, not reported as zero",
    );
    assert_eq!(block["pending"], 1, "only mem-new still lags");
    assert_eq!(block["dead_letter"], 0);

    let human = stdout(&run_cli(&home, home.path(), &["stats"]));
    assert!(human.contains("memory normalization  ready: 1"), "{human}");
    assert!(
        human.contains("memory normalization  skipped: 1"),
        "{human}"
    );
    assert!(human.contains("memory normalization pending: 1"), "{human}");
}

#[tokio::test]
async fn stats_reports_a_fully_normalized_store_as_zero_pending() {
    let (home, layout) = open_layout();
    {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        seed_entry(
            &state,
            "mem-ru",
            MemoryKind::Fact,
            "запись по-русски",
            1_000,
        )
        .await;
        seed_normalization(
            &state,
            "mem-ru",
            "запись по-русски",
            NormalizationStatus::Ready,
            1,
        )
        .await;
    }

    let block = normalization_block(&stdout(&run_cli(&home, home.path(), &["stats", "--json"])));
    assert_eq!(
        block["counts_by_status"],
        serde_json::json!([{"status": "ready", "count": 1}]),
    );
    assert_eq!(block["pending"], 0, "nothing left to translate");
    assert_eq!(block["dead_letter"], 0);
}

#[tokio::test]
async fn stats_reports_a_dead_lettered_entry_and_stops_counting_it_as_pending() {
    let (home, layout) = open_layout();
    {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        seed_entry(
            &state,
            "mem-ru",
            MemoryKind::Fact,
            "непереводимая запись",
            1_000,
        )
        .await;
        seed_normalization(
            &state,
            "mem-ru",
            "непереводимая запись",
            NormalizationStatus::Failed,
            MAX_NORMALIZATION_ATTEMPTS,
        )
        .await;
    }

    let block = normalization_block(&stdout(&run_cli(&home, home.path(), &["stats", "--json"])));
    assert_eq!(block["dead_letter"], 1);
    assert_eq!(
        block["pending"], 0,
        "no tick will offer it again under this normalizer",
    );

    let human = stdout(&run_cli(&home, home.path(), &["stats"]));
    assert!(
        human.contains("memory normalization dead-letter: 1 entry(ies)"),
        "{human}",
    );
    assert!(
        human.contains("keep using their original text"),
        "the line says what the consequence actually is: {human}",
    );
}
