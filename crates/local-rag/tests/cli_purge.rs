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
    CacheDb, EmbeddingKey, GLOBAL_SCOPE_OWNER_ID, MemoryKind, NewMemoryEntry, ScopeKind, StateDb,
    SubjectKind, create_memory_entry, insert_embedding,
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

/// Seed the `embedding_cache` row a real backfill would have written for
/// `memory_id`, under two representations, so `D-074`'s tests can prove a
/// purge takes **every** representation rather than one.
async fn seed_vectors(layout: &StoreLayout, state: &StateDb, memory_id: &str, text: &str) {
    let cache = open_cache_for_test(layout, state).await;
    let hash = local_rag_core::identity::domain::subject_memory_entry(memory_id, text);
    cache
        .writer()
        .transaction(move |tx| {
            for representation_id in ["rep-a", "rep-b"] {
                insert_embedding(
                    tx,
                    &EmbeddingKey {
                        subject_kind: SubjectKind::MemoryEntry,
                        subject_hash: hash.clone(),
                        representation_id: representation_id.to_string(),
                    },
                    2,
                    &[0.5, 0.5],
                    1_000,
                )?;
            }
            Ok(())
        })
        .await
        .expect("seed vectors");
}

/// A vector belonging to something that is not a memory entry — the control
/// for `purge --all`, which must not reach outside its own subject kind.
async fn seed_foreign_vector(layout: &StoreLayout, state: &StateDb) {
    let cache = open_cache_for_test(layout, state).await;
    cache
        .writer()
        .transaction(|tx| {
            insert_embedding(
                tx,
                &EmbeddingKey {
                    subject_kind: SubjectKind::ContentBlob,
                    subject_hash: "not-a-memory-subject".to_string(),
                    representation_id: "rep-a".to_string(),
                },
                2,
                &[0.25, 0.75],
                1_000,
            )
        })
        .await
        .expect("seed foreign vector");
}

async fn open_cache_for_test(layout: &StoreLayout, state: &StateDb) -> std::sync::Arc<CacheDb> {
    local_rag::indexing::open_cache(state, layout)
        .await
        .expect("open cache.sqlite")
}

/// How many `embedding_cache` rows exist for `memory_id`'s subject.
async fn vectors_for(layout: &StoreLayout, state: &StateDb, memory_id: &str, text: &str) -> i64 {
    let cache = open_cache_for_test(layout, state).await;
    let hash = local_rag_core::identity::domain::subject_memory_entry(memory_id, text);
    let read = cache.open_read().expect("cache read conn");
    read.query_row(
        "SELECT count(*) FROM embedding_cache WHERE subject_kind = 'memory_entry' \
         AND subject_hash = ?1",
        [hash],
        |r| r.get(0),
    )
    .expect("count vectors")
}

async fn total_vectors(layout: &StoreLayout, state: &StateDb, subject_kind: &str) -> i64 {
    let cache = open_cache_for_test(layout, state).await;
    let read = cache.open_read().expect("cache read conn");
    read.query_row(
        "SELECT count(*) FROM embedding_cache WHERE subject_kind = ?1",
        [subject_kind],
        |r| r.get(0),
    )
    .expect("count vectors by kind")
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
        stdout(&output).contains(
            "purged everything (1 memory entries, 1 sessions, 1 observations, 0 cached vectors removed)"
        ),
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

// ---------------------------------------------------------------------------
// D-074: the derived vector dies with the text it was derived from
// ---------------------------------------------------------------------------

const SEEDED_TEXT: &str = "some durable text";

#[tokio::test]
async fn purge_memory_removes_every_cached_vector_of_that_entry_and_no_others() {
    let (home, layout) = open_layout();
    let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
    seed_entry(&state, "mem-purged", 1_000).await;
    seed_entry(&state, "mem-kept", 1_000).await;
    seed_vectors(&layout, &state, "mem-purged", SEEDED_TEXT).await;
    seed_vectors(&layout, &state, "mem-kept", SEEDED_TEXT).await;
    assert_eq!(
        vectors_for(&layout, &state, "mem-purged", SEEDED_TEXT).await,
        2,
        "two representations seeded"
    );

    let output = run_cli(
        &home,
        &[
            "purge",
            "--memory",
            "mem-purged",
            "--expected-version",
            "1",
            "--yes",
        ],
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(
        stdout(&output).contains("2 cached vectors removed"),
        "the report counts exactly what went: {:?}",
        stdout(&output)
    );

    assert_eq!(
        vectors_for(&layout, &state, "mem-purged", SEEDED_TEXT).await,
        0,
        "the purged entry's vectors must not survive the only hard-delete path",
    );
    assert_eq!(
        vectors_for(&layout, &state, "mem-kept", SEEDED_TEXT).await,
        2,
        "another entry's vectors are none of this purge's business",
    );
}

/// The cost of deleting the vector before the entry, stated as a test rather
/// than left to be discovered: a purge that the state transaction refuses has
/// already dropped the vector. That is deliberate and it is the safe
/// direction — the next backfill recomputes it, whereas the opposite order
/// would leave a vector of private text that nothing can find, because its key
/// is derived from the text the purge deleted.
#[tokio::test]
async fn a_refused_purge_leaves_the_entry_and_costs_only_a_recomputable_vector() {
    let (home, layout) = open_layout();
    let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
    seed_entry(&state, "mem-stale", 1_000).await;
    seed_vectors(&layout, &state, "mem-stale", SEEDED_TEXT).await;

    let output = run_cli(
        &home,
        &[
            "purge",
            "--memory",
            "mem-stale",
            "--expected-version",
            "42",
            "--yes",
        ],
    );
    assert_ne!(output.status.code(), Some(0), "{output:?}");
    assert!(
        stderr(&output).contains("optimistic conflict"),
        "{:?}",
        stderr(&output)
    );

    assert!(
        inspect_memory_json(&home, "mem-stale").is_some(),
        "a refused purge must not remove the entry",
    );
    assert_eq!(
        vectors_for(&layout, &state, "mem-stale", SEEDED_TEXT).await,
        0,
        "the vector is gone, and that is the accepted cost of the safe order",
    );
}

#[tokio::test]
async fn purge_all_leaves_no_memory_vectors_and_does_not_reach_other_subject_kinds() {
    let (home, layout) = open_layout();
    let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
    seed_entry(&state, "mem-a", 1_000).await;
    seed_entry(&state, "mem-b", 1_000).await;
    seed_vectors(&layout, &state, "mem-a", SEEDED_TEXT).await;
    seed_vectors(&layout, &state, "mem-b", SEEDED_TEXT).await;
    seed_foreign_vector(&layout, &state).await;

    let output = run_cli(&home, &["purge", "--all", "--yes"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(
        stdout(&output).contains("4 cached vectors removed"),
        "{:?}",
        stdout(&output)
    );

    assert_eq!(
        total_vectors(&layout, &state, "memory_entry").await,
        0,
        "purge --all is exactly the operation after which no derived memory vector may remain",
    );
    assert_eq!(
        total_vectors(&layout, &state, "content_blob").await,
        1,
        "and it stays inside its own subject kind",
    );
}
