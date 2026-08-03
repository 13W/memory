//! `local-rag export [--scope global|repository|worktree]` acceptance tests
//! (spec 11 §6, 12 §3, T16-02), driving the real compiled binary — mirrors
//! `tests/cli_memory.rs`'s own `open_layout`/`run_cli`/seeding helpers
//! (duplicated here per this crate's established per-file-fixture
//! convention).

#![cfg(unix)]

use std::process::{Output, Stdio};

use local_rag_core::paths::StoreLayout;
use local_rag_store::{
    EvidenceKind, GLOBAL_SCOPE_OWNER_ID, MemoryKind, NewMemoryEntry, NewMemoryEvidence, ScopeKind,
    StateDb, create_memory_entry, insert_memory_evidence,
};
use local_rag_test_support::TempHome;

fn open_layout() -> (TempHome, StoreLayout) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    (home, layout)
}

fn run_cli(home: &TempHome, args: &[&str]) -> Output {
    let mut cmd = home.command(env!("CARGO_BIN_EXE_local-rag"));
    cmd.args(args);
    // A non-git cwd resolves to `GlobalOnly` (spec 02 §3.3) -- every fixture
    // in this file seeds the global scope.
    cmd.current_dir(home.path());
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.output().expect("run local-rag")
}

async fn seed_entry(state: &StateDb, memory_id: &str, now_ms: i64) {
    let id = memory_id.to_string();
    state
        .writer()
        .transaction(move |tx| {
            create_memory_entry(
                tx,
                &NewMemoryEntry {
                    memory_id: &id,
                    kind: MemoryKind::Fact,
                    text: "some durable text",
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

async fn seed_observation(state: &StateDb, observation_id: &str, expires_at: i64) {
    let oid = observation_id.to_string();
    state
        .writer()
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO observation_envelope \
                   (observation_id, source_event_id, payload_hash, event_type, evidence_kind, \
                    trust, session_id) \
                 VALUES (?1, 'evt-1', 'deadbeef', 'Stop', 'user_statement', 'normal', 'sess-1')",
                [&oid],
            )?;
            tx.execute(
                "INSERT INTO observation_payload \
                   (observation_id, redacted_payload, byte_size, expires_at) \
                 VALUES (?1, ?2, 5, ?3)",
                local_rag_store::rusqlite::params![oid, b"hello".to_vec(), expires_at],
            )
        })
        .await
        .expect("seed observation envelope");
}

async fn seed_evidence(state: &StateDb, memory_id: &str, observation_id: &str) {
    let (mid, oid) = (memory_id.to_string(), observation_id.to_string());
    state
        .writer()
        .transaction(move |tx| {
            insert_memory_evidence(
                tx,
                &NewMemoryEvidence {
                    memory_id: &mid,
                    observation_id: &oid,
                    evidence_kind: EvidenceKind::UserStatement,
                    session_id: "sess-1",
                    agent_id: None,
                    commit_hash: None,
                },
            )
        })
        .await
        .expect("seed memory evidence");
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn export_without_flags_exports_every_resolvable_scope() {
    let (home, _layout) = open_layout();
    let output = run_cli(&home, &["export"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let value: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("valid json");
    assert_eq!(value, serde_json::json!([]), "empty store exports empty");
}

#[test]
fn export_with_an_unknown_scope_value_is_a_usage_error() {
    let (home, _layout) = open_layout();
    let output = run_cli(&home, &["export", "--scope", "bogus"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
}

#[tokio::test]
async fn export_scope_isolation_via_the_scope_flag() {
    let (home, layout) = open_layout();
    {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        seed_entry(&state, "mem-global", 1000).await;
    }

    let output = run_cli(&home, &["export", "--scope", "repository"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let value: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("valid json");
    assert_eq!(
        value,
        serde_json::json!([]),
        "a non-git cwd has no repository scope to isolate into: {value}"
    );

    let output = run_cli(&home, &["export", "--scope", "global"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let value: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("valid json");
    assert_eq!(value.as_array().expect("array").len(), 1);
    assert_eq!(value[0]["entry"]["memory_id"], "mem-global");
}

#[tokio::test]
async fn export_output_is_byte_identical_across_two_invocations_on_unchanged_state() {
    let (home, layout) = open_layout();
    {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        seed_entry(&state, "mem-stable", 1000).await;
    }

    let first = run_cli(&home, &["export"]);
    let second = run_cli(&home, &["export"]);
    assert_eq!(first.status.code(), Some(0), "{first:?}");
    assert_eq!(
        first.stdout, second.stdout,
        "identical state exports identically"
    );
}

#[tokio::test]
async fn export_reports_an_expired_payload_as_expired_not_present() {
    let (home, layout) = open_layout();
    {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        seed_entry(&state, "mem-with-evidence", 1000).await;
        // expires_at=1 is in the past relative to any real wall-clock read.
        seed_observation(&state, "obs-expired", 1).await;
        seed_evidence(&state, "mem-with-evidence", "obs-expired").await;
    }

    let output = run_cli(&home, &["export"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let value: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("valid json");
    let entry = value
        .as_array()
        .expect("array")
        .iter()
        .find(|e| e["entry"]["memory_id"] == "mem-with-evidence")
        .expect("seeded entry present");
    let evidence = entry["evidence"].as_array().expect("evidence array");
    assert_eq!(evidence.len(), 1);
    assert_eq!(evidence[0]["payload"]["status"], "expired");
}
