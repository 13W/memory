//! `T23-05`/`D-121`: a routed plan that names one entry twice applies, where
//! before it was guaranteed to fail its own version check mid-batch.
//!
//! This is the card's live-shape acceptance, offline. The owner's store held
//! eight runs failed with
//! `optimistic conflict: expected entry_version V, found V+1`, every one with
//! `found = expected + 1` and an op index of at least 3, and one of them still
//! parks a session with 1081 observations behind it. The second half of the
//! headline test reproduces that exact string from an un-collapsed plan built
//! through the very same `guard::materialize` calls `route` makes — an in-tree
//! control rather than a mutation nobody will re-run.
//!
//! Determinism: a temporary `LOCAL_RAG_HOME`, literal `now_ms`, seeded UUIDs,
//! a scripted generator. No clock, no network, no home directory.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use local_rag_core::config::DataPolicy;
use local_rag_core::identity::{Uuid, UuidSource, uuidv7_from};
use local_rag_core::paths::StoreLayout;
use local_rag_embed::{
    FinishReason, GenError, GenRequest, GenResponse, Generator, GeneratorEntry, GeneratorPool,
};
use local_rag_memory::budget::PromptBudget;
use local_rag_memory::{guard, router};
use local_rag_store::rusqlite::params;
use local_rag_store::{
    ConsolidationWindow, EvidenceKind, GeneratedOp, LEASE_DURATION_MS, MemoryKind, NewMemoryEntry,
    ProposedOperation, RunWindow, ScopeKind, StateDb, TrustLevel, WindowObservation,
    commit_apply_run, create_memory_entry, open_next_run,
};
use local_rag_test_support::TempHome;

const NO_BUDGET_LIMIT: u32 = u32::MAX;
// `T23-06`: real, non-zero reserves even though `Scripted::generate` ignores
// `max_tokens` — see `router.rs`'s own `NO_PROMPT_LIMIT` for why zero here
// would be a silent lie about what these tests exercise.
const NO_PROMPT_LIMIT: PromptBudget = PromptBudget {
    context_tokens: u32::MAX,
    answer_reserve_tokens: local_rag_memory::budget::ANSWER_RESERVE_TOKENS,
    retry_reserve_tokens: local_rag_memory::budget::ANSWER_RESERVE_TOKENS,
    system_tokens: 0,
    conflict_floor_tokens: 0,
    window_tokens: u32::MAX,
};

const EXISTING_TEXT: &str = "use pnpm";

struct SeqUuids(AtomicU64);

impl UuidSource for SeqUuids {
    fn next_uuid(&self) -> Uuid {
        uuidv7_from(2_000 + self.0.fetch_add(1, Ordering::Relaxed), [0xBB; 10])
    }
}

struct Scripted(Mutex<Vec<String>>);

impl Generator for Scripted {
    fn generate(&self, _req: GenRequest) -> Result<GenResponse, GenError> {
        match self.0.lock().expect("lock").pop() {
            Some(text) => Ok(GenResponse {
                text,
                finish_reason: FinishReason::Stop,
                tokens_generated: None,
            }),
            None => Err(GenError::permanent("scripted generator exhausted")),
        }
    }
}

fn pool_with(responses: Vec<&str>) -> GeneratorPool {
    GeneratorPool::new(vec![GeneratorEntry::local(
        "scripted",
        Arc::new(Scripted(Mutex::new(
            responses.into_iter().rev().map(str::to_string).collect(),
        ))),
    )])
}

fn open_state() -> (TempHome, StateDb) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    (home, db)
}

async fn seed_observations(db: &StateDb, ids: &[&str]) {
    let ids: Vec<String> = ids.iter().map(|s| (*s).to_string()).collect();
    db.writer()
        .transaction(move |tx| {
            for (i, id) in ids.iter().enumerate() {
                tx.execute(
                    "INSERT INTO observation_envelope \
                       (observation_id, source_event_id, payload_hash, event_type, \
                        evidence_kind, trust, session_id) \
                     VALUES (?1, ?2, 'deadbeef', 'Stop', 'user_statement', 'normal', 'sess-1')",
                    params![id, format!("evt-{i}")],
                )?;
            }
            Ok(())
        })
        .await
        .expect("seed observations");
}

/// A global `decision` at `entry_version = 1`, then bumped to `3` by two
/// direct writes, so the version in the failure message is not 1 and the test
/// cannot pass by accident on a default.
async fn seed_entry_at_version_three(db: &StateDb, memory_id: &str) {
    let id = memory_id.to_string();
    db.writer()
        .transaction(move |tx| {
            let _ = create_memory_entry(
                tx,
                &NewMemoryEntry {
                    memory_id: &id,
                    kind: MemoryKind::Decision,
                    text: EXISTING_TEXT,
                    canonical_key: None,
                    scope_kind: ScopeKind::Global,
                    scope_owner_id: local_rag_store::GLOBAL_SCOPE_OWNER_ID,
                    confidence: 0.5,
                    importance: 0.5,
                    valid_from_tree: None,
                    last_verified_tree: None,
                    supersedes_id: None,
                },
                1_000,
            )?;
            tx.execute(
                "UPDATE memory_entry SET entry_version = 3 WHERE memory_id = ?1",
                params![id],
            )?;
            Ok(())
        })
        .await
        .expect("seed tx");
}

fn window(session_id: &str, ids: &[&str]) -> ConsolidationWindow {
    ConsolidationWindow {
        session_id: session_id.to_string(),
        from_received_seq: 1,
        to_received_seq: ids.len() as i64,
        observations: ids
            .iter()
            .enumerate()
            .map(|(i, id)| WindowObservation {
                observation_id: (*id).to_string(),
                received_seq: i as i64 + 1,
                event_type: "UserPromptSubmit".to_string(),
                evidence_kind: EvidenceKind::UserStatement,
                trust: TrustLevel::Normal,
                session_id: session_id.to_string(),
                repo_id: None,
                worktree_id: None,
                agent_id: None,
                commit_hash: None,
                short_evidence_excerpt: Some("we decided to use pnpm".to_string()),
                payload: None,
            })
            .collect(),
    }
}

/// The two `create` lines one window produces when the model proposes an
/// already-stored text twice — the shape `D-078`'s rewrite turns into two
/// reinforces of one entry.
fn duplicate_proposal_response() -> String {
    let line = |cite: &str| {
        format!(
            r#"{{"op":"create","kind":"decision","text":"{EXISTING_TEXT}","scope_kind":"global","confidence_signal":"high","importance_signal":"medium","cites":["{cite}"]}}"#
        )
    };
    format!("{}\n{}", line("o1"), line("o2"))
}

fn entry_version(db: &StateDb, memory_id: &str) -> i64 {
    db.open_read()
        .expect("read conn")
        .query_row(
            "SELECT entry_version FROM memory_entry WHERE memory_id = ?1",
            params![memory_id],
            |r| r.get(0),
        )
        .expect("entry exists")
}

fn evidence_ids(db: &StateDb, memory_id: &str) -> Vec<String> {
    let conn = db.open_read().expect("read conn");
    let mut stmt = conn
        .prepare(
            "SELECT observation_id FROM memory_evidence WHERE memory_id = ?1 \
             ORDER BY observation_id",
        )
        .expect("prepare");
    let rows = stmt
        .query_map(params![memory_id], |r| r.get::<_, String>(0))
        .expect("query");
    rows.map(|r| r.expect("row")).collect()
}

fn cursor(db: &StateDb, session_id: &str) -> Option<i64> {
    db.open_read()
        .expect("read conn")
        .query_row(
            "SELECT last_consolidated_received_seq FROM processing_cursor WHERE session_id = ?1",
            params![session_id],
            |r| r.get(0),
        )
        .ok()
}

async fn open_window(db: &StateDb, run_id: &str, session_id: &str) -> RunWindow {
    let (rid, sid) = (run_id.to_string(), session_id.to_string());
    let outcome = db
        .writer()
        .transaction(move |tx| {
            open_next_run(
                tx,
                &rid,
                &sid,
                20,
                local_rag_store::UNBOUNDED_WINDOW_CHARS,
                "v1",
                LEASE_DURATION_MS,
                1_000,
                "build-test",
            )
        })
        .await
        .expect("open tx");
    match outcome {
        local_rag_store::SnapshotOutcome::Opened(w) => w,
        other => panic!("expected an opened window, got {other:?}"),
    }
}

/// `T23-05`'s headline. Both halves use the same seeded store and the same
/// two raw ops; only the collapse differs.
#[tokio::test]
async fn a_plan_naming_one_entry_twice_applies_where_it_used_to_conflict() {
    let (_home, db) = open_state();
    let uuids = SeqUuids(AtomicU64::new(0));
    let memory_id = uuidv7_from(1_500, [0xAA; 10]).to_string();
    seed_observations(&db, &["o1", "o2"]).await;
    seed_entry_at_version_three(&db, &memory_id).await;

    let pool = pool_with(vec![&duplicate_proposal_response()]);
    let w = window("sess-1", &["o1", "o2"]);
    let ops = router::route(
        &db,
        &pool,
        DataPolicy::LocalOnly,
        &uuids,
        w.clone(),
        NO_BUDGET_LIMIT,
        NO_PROMPT_LIMIT,
    )
    .await
    .expect("routes cleanly");
    assert_eq!(
        ops.len(),
        1,
        "the plan is collapsed before it leaves: {ops:?}"
    );

    let run = open_window(&db, &uuidv7_from(1_600, [0xCC; 10]).to_string(), "sess-1").await;
    let report = commit_apply_run(
        &db,
        run,
        w.observations.clone(),
        1_000 + LEASE_DURATION_MS,
        ops,
        2_000,
    )
    .await
    .expect("the collapsed plan applies");

    assert_eq!(report.applied, 1);
    assert_eq!(
        entry_version(&db, &memory_id),
        4,
        "one window reinforces an entry once, not twice"
    );
    assert_eq!(
        evidence_ids(&db, &memory_id),
        vec!["o1".to_string(), "o2".to_string()],
        "and both proposals' citations survive the merge"
    );
    assert_eq!(cursor(&db, "sess-1"), Some(2), "the cursor advanced");
}

/// The in-tree control: the same two raw ops, materialized exactly as `route`
/// materializes them but **not** collapsed, reproduce the live failure string
/// verbatim and commit nothing.
#[tokio::test]
async fn the_uncollapsed_plan_still_fails_with_the_live_optimistic_conflict() {
    let (_home, db) = open_state();
    let uuids = SeqUuids(AtomicU64::new(0));
    let memory_id = uuidv7_from(1_500, [0xAA; 10]).to_string();
    seed_observations(&db, &["o1", "o2"]).await;
    seed_entry_at_version_three(&db, &memory_id).await;

    let w = window("sess-1", &["o1", "o2"]);
    let raw = local_rag_memory::parse::parse_ops(&duplicate_proposal_response())
        .expect("two well-formed create ops");
    let by_id: HashMap<&str, &WindowObservation> = w
        .observations
        .iter()
        .map(|o| (o.observation_id.as_str(), o))
        .collect();
    let conn = db.open_read().expect("read conn");
    let uncollapsed: Vec<GeneratedOp> = raw
        .ops
        .into_iter()
        .map(|r| {
            guard::materialize(&conn, &by_id, &w.observations, &uuids, r).expect("materialize")
        })
        .collect();
    drop(conn);

    assert_eq!(uncollapsed.len(), 2, "this is the plan before the collapse");
    let targets: Vec<&str> = uncollapsed
        .iter()
        .filter_map(|op| match op {
            GeneratedOp::Materialize {
                operation: ProposedOperation::Reinforce { memory_id, .. },
                ..
            } => Some(memory_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        targets,
        vec![memory_id.as_str(), memory_id.as_str()],
        "D-078 rewrote both proposals into reinforces of one entry"
    );
    let versions: Vec<i64> = uncollapsed
        .iter()
        .filter_map(|op| match op {
            GeneratedOp::Materialize {
                operation:
                    ProposedOperation::Reinforce {
                        expected_version, ..
                    },
                ..
            } => Some(*expected_version),
            _ => None,
        })
        .collect();
    assert_eq!(
        versions,
        vec![3, 3],
        "and this is the defect itself: both captured the same snapshot, \
         because `guard::materialize` reads every version before any op applies"
    );

    let run = open_window(&db, &uuidv7_from(1_600, [0xCC; 10]).to_string(), "sess-1").await;
    let err = commit_apply_run(
        &db,
        run,
        w.observations.clone(),
        1_000 + LEASE_DURATION_MS,
        uncollapsed,
        2_000,
    )
    .await
    .expect_err("the uncollapsed plan cannot apply");

    assert_eq!(
        err.to_string(),
        "consolidation apply rejected: op 1: materialization failed: \
         optimistic conflict: expected entry_version 3, found 4",
        "the live string, `found = expected + 1`"
    );
    assert_eq!(
        entry_version(&db, &memory_id),
        3,
        "and the batch is all-or-nothing: nothing committed"
    );
    assert_eq!(cursor(&db, "sess-1"), None, "the cursor did not advance");
}

/// Why the fix is not "let a later op validate against the running version".
///
/// This is the rejected alternative, built by hand: two reinforces of one
/// entry whose versions are already correct in sequence (3 then 4), so the
/// optimistic check passes them both — exactly what a batch-local version
/// rebase inside `apply_run` would have achieved. They cite the same
/// observation, which on the owner's store is what 70 of 96 within-run op
/// pairs do, and `memory_evidence`'s `PRIMARY KEY (memory_id, observation_id)`
/// then fires on the second insert. `classify_apply_failure` makes a
/// constraint violation `Mechanical` on the **first** attempt — no retries at
/// all — so that fix would have traded a retryable failure for an immediately
/// permanent one.
#[tokio::test]
async fn passing_the_version_check_would_only_move_the_failure_to_the_evidence_key() {
    let (_home, db) = open_state();
    let memory_id = uuidv7_from(1_500, [0xAA; 10]).to_string();
    seed_observations(&db, &["o1", "o2"]).await;
    seed_entry_at_version_three(&db, &memory_id).await;

    let reinforce = |version: i64| GeneratedOp::Materialize {
        operation: ProposedOperation::Reinforce {
            memory_id: memory_id.clone(),
            expected_version: version,
            confidence: None,
        },
        // The same observation twice: the shape the measurement says is the
        // common one, not a contrived worst case.
        evidence_observation_ids: vec!["o1".to_string()],
    };

    let w = window("sess-1", &["o1", "o2"]);
    let run = open_window(&db, &uuidv7_from(1_600, [0xCC; 10]).to_string(), "sess-1").await;
    let err = commit_apply_run(
        &db,
        run,
        w.observations.clone(),
        1_000 + LEASE_DURATION_MS,
        vec![reinforce(3), reinforce(4)],
        2_000,
    )
    .await
    .expect_err("the second insert collides on the evidence primary key");

    assert!(
        err.to_string().contains("UNIQUE constraint failed")
            && err.to_string().contains("memory_evidence"),
        "the version check is passed and the evidence key is hit instead: {err}"
    );
    assert_eq!(
        entry_version(&db, &memory_id),
        3,
        "and, being a constraint violation, it still commits nothing"
    );
}

/// What `T23-05` does **not** fix, pinned so `G23` need not rediscover it.
///
/// A genuine outside writer — an MCP `edit_memory`, the normalization worker,
/// another session's run — moving the version between plan and apply is still
/// an optimistic conflict, and correctly so. What converges is re-planning,
/// not waiting: the second half replays the *cached* first plan and it fails
/// identically, then routes again and applies.
#[tokio::test]
async fn an_outside_writer_still_conflicts_and_only_re_planning_converges() {
    let (_home, db) = open_state();
    let uuids = SeqUuids(AtomicU64::new(0));
    let memory_id = uuidv7_from(1_500, [0xAA; 10]).to_string();
    seed_observations(&db, &["o1", "o2"]).await;
    seed_entry_at_version_three(&db, &memory_id).await;

    let w = window("sess-1", &["o1", "o2"]);
    let plan_at_v3 = router::route(
        &db,
        &pool_with(vec![&duplicate_proposal_response()]),
        DataPolicy::LocalOnly,
        &uuids,
        w.clone(),
        NO_BUDGET_LIMIT,
        NO_PROMPT_LIMIT,
    )
    .await
    .expect("routes cleanly");

    // The outside writer lands between plan and apply.
    let bumped = memory_id.clone();
    db.writer()
        .transaction(move |tx| {
            tx.execute(
                "UPDATE memory_entry SET entry_version = 4 WHERE memory_id = ?1",
                params![bumped],
            )?;
            Ok(())
        })
        .await
        .expect("out-of-band bump");

    // One run, retried in place: `open_next_run` refuses to open a second
    // window while a non-`applied` one exists, and a rejected apply leaves the
    // row `running` under the same lease — which is exactly how the daemon
    // retries, so the fixture matches production rather than working around it.
    let run = open_window(&db, &uuidv7_from(1_600, [0xCC; 10]).to_string(), "sess-1").await;
    let err = commit_apply_run(
        &db,
        run.clone(),
        w.observations.clone(),
        1_000 + LEASE_DURATION_MS,
        plan_at_v3.clone(),
        2_000,
    )
    .await
    .expect_err("a plan built at 3 cannot apply against 4");
    assert!(
        err.to_string()
            .contains("optimistic conflict: expected entry_version 3, found 4"),
        "the check the card puts out of scope still does its job: {err}"
    );

    // Replaying the same plan changes nothing — convergence is not a matter
    // of time.
    let again = commit_apply_run(
        &db,
        run.clone(),
        w.observations.clone(),
        1_000 + LEASE_DURATION_MS,
        plan_at_v3,
        3_000,
    )
    .await
    .expect_err("the identical plan fails identically");
    assert!(
        again
            .to_string()
            .contains("optimistic conflict: expected entry_version 3, found 4")
    );

    // Routing again reads the moved version and applies, which is what every
    // retry actually does.
    let fresh = router::route(
        &db,
        &pool_with(vec![&duplicate_proposal_response()]),
        DataPolicy::LocalOnly,
        &uuids,
        w.clone(),
        NO_BUDGET_LIMIT,
        NO_PROMPT_LIMIT,
    )
    .await
    .expect("routes cleanly");
    let report = commit_apply_run(
        &db,
        run,
        w.observations.clone(),
        1_000 + LEASE_DURATION_MS,
        fresh,
        4_000,
    )
    .await
    .expect("a re-planned window applies");
    assert_eq!(report.applied, 1);
    assert_eq!(entry_version(&db, &memory_id), 5);
}
