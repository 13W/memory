//! `local-rag purge --memory <id>|--session <id>|--all` acceptance tests
//! (spec 08 §3, 12 §3, T16-02), driving the real compiled binary — mirrors
//! `tests/cli_memory.rs`'s own `open_layout`/`run_cli`/seeding helpers
//! (duplicated here per this crate's established per-file-fixture
//! convention).
//!
//! No failpoint/crash tests here -- the shared `Failpoints` registry is
//! process-local and cannot be armed across this file's subprocess boundary;
//! crash-rollback coverage lives entirely in
//! `crates/store/tests/privacy_purge.rs`, against the real domain functions
//! in-process.

#![cfg(unix)]

use std::process::{Output, Stdio};

use local_rag_core::paths::StoreLayout;
use local_rag_store::{
    GLOBAL_SCOPE_OWNER_ID, MemoryKind, NewMemoryEntry, ScopeKind, StateDb, create_memory_entry,
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

async fn seed_observation(state: &StateDb, observation_id: &str, session_id: &str) {
    let (oid, sess) = (observation_id.to_string(), session_id.to_string());
    state
        .writer()
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO observation_envelope \
                   (observation_id, source_event_id, payload_hash, event_type, evidence_kind, \
                    trust, session_id) \
                 VALUES (?1, 'evt-1', 'deadbeef', 'Stop', 'user_statement', 'normal', ?2)",
                local_rag_store::rusqlite::params![oid, sess],
            )
        })
        .await
        .expect("seed observation envelope");
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}
fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn inspect_memory_json(home: &TempHome, id: &str) -> Option<serde_json::Value> {
    let output = run_cli(home, &["inspect", "memory", id]);
    if output.status.code() != Some(0) {
        return None;
    }
    Some(serde_json::from_str(&stdout(&output)).expect("valid json"))
}

// ---------------------------------------------------------------------
// usage errors -- "explicit selector" half of the card's own test bullet
// ---------------------------------------------------------------------

#[test]
fn purge_without_a_selector_is_a_usage_error() {
    let (home, _layout) = open_layout();
    let output = run_cli(&home, &["purge", "--yes"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        stderr(&output).contains("exactly one of --memory/--session/--all"),
        "{output:?}"
    );
}

#[test]
fn purge_with_two_selectors_is_a_usage_error() {
    let (home, _layout) = open_layout();
    let output = run_cli(
        &home,
        &["purge", "--memory", "a", "--session", "b", "--yes"],
    );
    assert_eq!(output.status.code(), Some(2), "{output:?}");
}

#[test]
fn purge_memory_without_expected_version_is_a_usage_error() {
    let (home, _layout) = open_layout();
    let output = run_cli(&home, &["purge", "--memory", "some-id", "--yes"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(stderr(&output).contains("--expected-version"), "{output:?}");
}

#[test]
fn purge_session_rejects_an_expected_version_flag() {
    let (home, _layout) = open_layout();
    let output = run_cli(
        &home,
        &[
            "purge",
            "--session",
            "sess-1",
            "--expected-version",
            "1",
            "--yes",
        ],
    );
    assert_eq!(output.status.code(), Some(2), "{output:?}");
}

// ---------------------------------------------------------------------
// destructive purge requires --yes -- authorization UX
// ---------------------------------------------------------------------

#[tokio::test]
async fn purge_memory_without_yes_refuses_and_reports_the_would_be_deleted_count_with_no_mutation()
{
    let (home, layout) = open_layout();
    {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        seed_entry(&state, "mem-1", 1000).await;
    }

    let output = run_cli(
        &home,
        &["purge", "--memory", "mem-1", "--expected-version", "1"],
    );
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        stdout(&output).contains("would purge memory mem-1"),
        "{output:?}"
    );
    assert!(
        inspect_memory_json(&home, "mem-1").is_some(),
        "no mutation happened without --yes"
    );
}

#[tokio::test]
async fn purge_session_without_yes_refuses_with_no_mutation() {
    let (home, layout) = open_layout();
    {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        seed_observation(&state, "obs-1", "sess-target").await;
    }

    let output = run_cli(&home, &["purge", "--session", "sess-target"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        stdout(&output).contains("would purge session sess-target (1 observations)"),
        "{output:?}"
    );

    // Follow-up purge --all --yes still finds the untouched observation.
    let output = run_cli(&home, &["purge", "--all", "--yes"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(stdout(&output).contains("1 observations"), "{output:?}");
}

#[tokio::test]
async fn purge_all_without_yes_refuses_with_no_mutation() {
    let (home, layout) = open_layout();
    {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        seed_entry(&state, "mem-1", 1000).await;
    }

    let output = run_cli(&home, &["purge", "--all"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        stdout(&output).contains("would purge 1 memory entries"),
        "{output:?}"
    );
    assert!(
        inspect_memory_json(&home, "mem-1").is_some(),
        "no mutation happened without --yes"
    );
}

// ---------------------------------------------------------------------
// happy paths (with --yes)
// ---------------------------------------------------------------------

#[tokio::test]
async fn purge_memory_with_yes_and_correct_version_succeeds_then_inspect_returns_none() {
    let (home, layout) = open_layout();
    {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        seed_entry(&state, "mem-1", 1000).await;
    }

    let output = run_cli(
        &home,
        &[
            "purge",
            "--memory",
            "mem-1",
            "--expected-version",
            "1",
            "--yes",
        ],
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(
        stdout(&output).contains("purged memory mem-1"),
        "{output:?}"
    );
    assert!(inspect_memory_json(&home, "mem-1").is_none());
}

#[tokio::test]
async fn purge_memory_with_yes_and_stale_version_fails_and_surfaces_both_numbers() {
    let (home, layout) = open_layout();
    {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        seed_entry(&state, "mem-1", 1000).await;
    }

    let output = run_cli(
        &home,
        &[
            "purge",
            "--memory",
            "mem-1",
            "--expected-version",
            "99",
            "--yes",
        ],
    );
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let err = stderr(&output);
    assert!(err.contains("expected version 99"), "{err}");
    assert!(err.contains("actual version 1"), "{err}");
    assert!(
        inspect_memory_json(&home, "mem-1").is_some(),
        "no mutation happened on a stale version"
    );
}

#[tokio::test]
async fn purge_session_with_yes_succeeds_then_inspect_observation_returns_none() {
    let (home, layout) = open_layout();
    {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        seed_observation(&state, "obs-1", "sess-target").await;
    }

    let output = run_cli(&home, &["purge", "--session", "sess-target", "--yes"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(
        stdout(&output).contains("purged session sess-target"),
        "{output:?}"
    );

    let output = run_cli(&home, &["inspect", "observation", "obs-1"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
}

#[tokio::test]
async fn purge_all_with_yes_succeeds_then_memory_list_and_stats_report_empty() {
    let (home, layout) = open_layout();
    {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        seed_entry(&state, "mem-1", 1000).await;
        seed_observation(&state, "obs-1", "sess-1").await;
    }

    let output = run_cli(&home, &["purge", "--all", "--yes"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(
        stdout(&output)
            .contains("purged everything (1 memory entries, 1 sessions, 1 observations)"),
        "{output:?}"
    );

    // Cross-checked against the already-shipped memory list/stats commands.
    let output = run_cli(&home, &["memory", "list"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(
        !stdout(&output).contains("mem-1"),
        "the purged entry no longer appears in memory list: {output:?}"
    );

    let output = run_cli(&home, &["stats", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let value: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("valid json");
    assert_eq!(
        value["memory"]["entries_by_kind_state"],
        serde_json::json!([]),
        "{value}"
    );
}

#[tokio::test]
async fn purge_of_an_unknown_memory_id_with_yes_fails_with_exit_1() {
    let (home, _layout) = open_layout();
    let output = run_cli(
        &home,
        &[
            "purge",
            "--memory",
            "unknown",
            "--expected-version",
            "1",
            "--yes",
        ],
    );
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(stderr(&output).contains("no memory entry"), "{output:?}");
}
