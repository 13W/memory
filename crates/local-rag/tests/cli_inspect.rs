//! `local-rag inspect <observation|memory|generation> <id>` acceptance tests
//! (spec 11 §6, 12 §3, T16-02), driving the real compiled binary — mirrors
//! `tests/cli_memory.rs`'s own `open_layout`/`run_cli`/seeding helpers
//! (duplicated here per this crate's established per-file-fixture
//! convention).

#![cfg(unix)]

use std::process::{Output, Stdio};

use local_rag_core::paths::StoreLayout;
use local_rag_store::{
    GLOBAL_SCOPE_OWNER_ID, MemoryKind, NewMemoryEntry, NormalizationStatus, NormalizationWrite,
    ScopeKind, StateDb, UpsertOutcome, WorktreeKind, allocate_generation, create_memory_entry,
    create_repository, create_worktree, upsert_normalization,
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

async fn seed_observation(state: &StateDb, observation_id: &str) {
    let oid = observation_id.to_string();
    state
        .writer()
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO observation_envelope \
                   (observation_id, source_event_id, payload_hash, event_type, evidence_kind, \
                    trust, session_id, short_evidence_excerpt) \
                 VALUES (?1, 'evt-1', 'deadbeef', 'Stop', 'user_statement', 'normal', 'sess-1', \
                         'short excerpt')",
                [&oid],
            )?;
            tx.execute(
                "INSERT INTO observation_path (observation_id, normalized_path) \
                 VALUES (?1, 'src/a.rs')",
                [&oid],
            )?;
            tx.execute(
                "INSERT INTO observation_payload \
                   (observation_id, redacted_payload, byte_size, expires_at) \
                 VALUES (?1, ?2, 5, 9999999999999)",
                local_rag_store::rusqlite::params![oid, b"hello".to_vec()],
            )
        })
        .await
        .expect("seed observation envelope");
}

async fn seed_generation(state: &StateDb, repo_id: &str, worktree_id: &str, generation_id: &str) {
    let (repo, wt, genr) = (
        repo_id.to_string(),
        worktree_id.to_string(),
        generation_id.to_string(),
    );
    state
        .writer()
        .transaction(move |tx| {
            create_repository(tx, &repo, None, 1000)?;
            create_worktree(tx, &wt, &repo, WorktreeKind::Main, 1000)?;
            allocate_generation(tx, &wt, &genr, 1000).map(|_| ())
        })
        .await
        .expect("seed generation");
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}
fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

// ---------------------------------------------------------------------
// usage errors
// ---------------------------------------------------------------------

#[test]
fn inspect_without_a_kind_is_a_usage_error() {
    let (home, _layout) = open_layout();
    let output = run_cli(&home, &["inspect"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
}

#[test]
fn inspect_without_an_id_is_a_usage_error() {
    let (home, _layout) = open_layout();
    let output = run_cli(&home, &["inspect", "memory"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
}

#[test]
fn inspect_with_an_unknown_kind_is_a_usage_error() {
    let (home, _layout) = open_layout();
    let output = run_cli(&home, &["inspect", "bogus", "some-id"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let err = stderr(&output);
    assert!(err.contains("observation"), "{output:?}");
    assert!(err.contains("memory"), "{output:?}");
    assert!(err.contains("generation"), "{output:?}");
}

// ---------------------------------------------------------------------
// happy paths
// ---------------------------------------------------------------------

#[tokio::test]
async fn inspect_observation_prints_the_seeded_fields() {
    let (home, layout) = open_layout();
    {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        seed_observation(&state, "obs-1").await;
    }

    let output = run_cli(&home, &["inspect", "observation", "obs-1"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let value: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("valid json");
    assert_eq!(value["observation_id"], "obs-1");
    assert_eq!(value["session_id"], "sess-1");
    assert_eq!(value["paths"], serde_json::json!(["src/a.rs"]));
    assert_eq!(value["payload"]["status"], "present");
    assert_eq!(value["payload"]["text"], "hello");
}

#[tokio::test]
async fn inspect_memory_prints_entry_evidence_and_audit() {
    let (home, layout) = open_layout();
    {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        seed_entry(&state, "mem-1", 1000).await;
    }

    let output = run_cli(&home, &["inspect", "memory", "mem-1"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let value: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("valid json");
    assert_eq!(value["entry"]["memory_id"], "mem-1");
    assert_eq!(value["entry"]["kind"], "fact");
    assert_eq!(value["entry"]["entry_version"], 1);
    assert_eq!(value["evidence"], serde_json::json!([]));
    assert_eq!(value["audit_trail"], serde_json::json!([]));
    assert_eq!(
        value["normalization"],
        serde_json::Value::Null,
        "an entry that was never normalized reports the key as null, not omitted",
    );
}

/// T21-07: the English variant and its provenance are part of what `inspect`
/// prints — including the translated text itself, since `export` reuses this
/// renderer and exists to show everything the store holds.
#[tokio::test]
async fn inspect_memory_prints_the_translation_and_its_provenance() {
    let (home, layout) = open_layout();
    {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        seed_entry(&state, "mem-1", 1000).await;
        let sha = local_rag_core::hash::sha256_hex(b"some durable text");
        let outcome = state
            .writer()
            .transaction(move |tx| {
                upsert_normalization(
                    tx,
                    &NormalizationWrite {
                        memory_id: "mem-1",
                        status: NormalizationStatus::Ready,
                        source_text_sha256: &sha,
                        normalized_text: Some("the English variant"),
                        source_language: Some("ru"),
                        normalizer_model_id: Some("test-normalizer"),
                        prompt_version: Some(1),
                        normalizer_version: 1,
                        attempt_count: 1,
                        last_error: None,
                        next_attempt_at: None,
                    },
                    2000,
                )
            })
            .await
            .expect("seed normalization tx");
        assert_eq!(outcome, UpsertOutcome::Written);
    }

    let output = run_cli(&home, &["inspect", "memory", "mem-1"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let value: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("valid json");
    assert_eq!(value["normalization"]["status"], "ready");
    assert_eq!(
        value["normalization"]["normalized_text"],
        "the English variant"
    );
    assert_eq!(value["normalization"]["source_language"], "ru");
    assert_eq!(
        value["normalization"]["normalizer_model_id"],
        "test-normalizer"
    );
    assert_eq!(value["normalization"]["normalizer_version"], 1);
    assert_eq!(
        value["entry"]["text"], "some durable text",
        "the canonical text is untouched next to it",
    );
}

#[tokio::test]
async fn inspect_generation_prints_the_seeded_fields() {
    let (home, layout) = open_layout();
    {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        seed_generation(&state, "repo-1", "wt-1", "gen-1").await;
    }

    let output = run_cli(&home, &["inspect", "generation", "gen-1"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let value: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("valid json");
    assert_eq!(value["generation_id"], "gen-1");
    assert_eq!(value["worktree_id"], "wt-1");
    assert_eq!(value["generation_number"], 1);
    assert_eq!(value["state"], "building");
}

#[test]
fn inspect_of_an_unknown_id_fails_with_exit_1() {
    let (home, _layout) = open_layout();
    let output = run_cli(&home, &["inspect", "memory", "unknown"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(stderr(&output).contains("no memory entry"), "{output:?}");
}
