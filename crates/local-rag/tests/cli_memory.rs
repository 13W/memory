//! `local-rag memory list|approve|reject|edit|retract|merge|evidence`
//! acceptance tests (spec 11 §6, D-025), driving the real compiled binary —
//! mirrors `tests/cli_repo.rs`'s own `open_layout`/`run_cli`/seeding helpers
//! (duplicated here per this crate's established per-file-fixture
//! convention).
//!
//! None of these commands ever touch the embedder or a live daemon (no
//! `store.lock`, per `cli/mod.rs`'s own doc), so every test here runs
//! unconditionally against a plain `StateDb` seeded directly.

#![cfg(unix)]

use std::process::{Output, Stdio};

use local_rag_core::paths::StoreLayout;
use local_rag_store::{
    EvidenceKind, GLOBAL_SCOPE_OWNER_ID, MemoryKind, NewMemoryEntry, NewMemoryEvidence,
    ProposedOperation, ScopeKind, StateDb, create_memory_entry, insert_memory_evidence,
    propose_candidate,
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
    // A non-git cwd resolves to `GlobalOnly` (spec 02 §3.3) — every fixture
    // in this file seeds the global scope, so `memory list`/`stats` never
    // need a real worktree.
    cmd.current_dir(home.path());
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.output().expect("run local-rag")
}

async fn seed_entry(state: &StateDb, memory_id: &str, text: &str, now_ms: i64) {
    let (id, text) = (memory_id.to_string(), text.to_string());
    state
        .writer()
        .transaction(move |tx| {
            create_memory_entry(
                tx,
                &NewMemoryEntry {
                    memory_id: &id,
                    kind: MemoryKind::Fact,
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

async fn seed_observation(state: &StateDb, observation_id: &str) {
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

async fn seed_candidate(state: &StateDb, candidate_id: &str, target_memory_id: &str, now_ms: i64) {
    let (cid, target) = (candidate_id.to_string(), target_memory_id.to_string());
    state
        .writer()
        .transaction(move |tx| {
            let op = ProposedOperation::Create {
                memory_id: target,
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
fn memory_without_a_subcommand_is_a_usage_error() {
    let (home, _layout) = open_layout();
    let output = run_cli(&home, &["memory"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
}

#[test]
fn memory_edit_without_expected_version_is_a_usage_error() {
    let (home, _layout) = open_layout();
    let output = run_cli(&home, &["memory", "edit", "some-id", "--text", "x"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(stderr(&output).contains("--expected-version"), "{output:?}");
}

#[test]
fn memory_edit_without_a_patch_field_is_a_usage_error() {
    let (home, _layout) = open_layout();
    let output = run_cli(
        &home,
        &["memory", "edit", "some-id", "--expected-version", "1"],
    );
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        stderr(&output).contains("--text/--importance"),
        "{output:?}"
    );
}

#[test]
fn memory_rejects_an_unknown_subcommand() {
    let (home, _layout) = open_layout();
    let output = run_cli(&home, &["memory", "bogus"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
}

// ---------------------------------------------------------------------
// list / edit / retract
// ---------------------------------------------------------------------

#[tokio::test]
async fn memory_list_shows_seeded_entries() {
    let (home, layout) = open_layout();
    {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        seed_entry(&state, "mem-a", "first fact", 1_000).await;
        seed_entry(&state, "mem-b", "second fact", 2_000).await;
    }

    let output = run_cli(&home, &["memory", "list"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let text = stdout(&output);
    assert!(text.contains("mem-a"), "{text}");
    assert!(text.contains("mem-b"), "{text}");
}

#[tokio::test]
async fn memory_edit_with_correct_expected_version_succeeds_and_bumps_version() {
    let (home, layout) = open_layout();
    {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        seed_entry(&state, "mem-edit", "original text", 1_000).await;
    }

    let output = run_cli(
        &home,
        &[
            "memory",
            "edit",
            "mem-edit",
            "--expected-version",
            "1",
            "--text",
            "updated text",
        ],
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(
        stdout(&output).contains("entry_version 2"),
        "{:?}",
        stdout(&output)
    );

    // The edit actually landed: a second edit against the now-stale v1
    // fails, proving the version really moved forward in the store.
    let output = run_cli(
        &home,
        &[
            "memory",
            "edit",
            "mem-edit",
            "--expected-version",
            "1",
            "--text",
            "stale edit",
        ],
    );
    assert_ne!(output.status.code(), Some(0), "{output:?}");
}

#[tokio::test]
async fn memory_edit_with_a_stale_expected_version_surfaces_both_numbers() {
    let (home, layout) = open_layout();
    {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        seed_entry(&state, "mem-stale", "text", 1_000).await;
    }

    let output = run_cli(
        &home,
        &[
            "memory",
            "edit",
            "mem-stale",
            "--expected-version",
            "99",
            "--text",
            "new text",
        ],
    );
    assert_ne!(output.status.code(), Some(0), "{output:?}");
    let err = stderr(&output);
    assert!(err.contains("expected version 99"), "{err}");
    assert!(err.contains("actual version 1"), "{err}");
}

#[tokio::test]
async fn memory_retract_is_not_delete_and_remains_visible_by_state_filter() {
    let (home, layout) = open_layout();
    {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        seed_entry(&state, "mem-retract", "text", 1_000).await;
    }

    let output = run_cli(
        &home,
        &[
            "memory",
            "retract",
            "mem-retract",
            "--expected-version",
            "1",
        ],
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");

    let output = run_cli(&home, &["memory", "list", "--state", "retracted"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(
        stdout(&output).contains("mem-retract"),
        "{:?}",
        stdout(&output)
    );
}

#[tokio::test]
async fn memory_merge_absorbs_a_loser_into_the_survivor() {
    let (home, layout) = open_layout();
    {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        seed_entry(&state, "mem-survivor", "keep me", 1_000).await;
        seed_entry(&state, "mem-loser", "absorb me", 2_000).await;
    }

    let output = run_cli(
        &home,
        &[
            "memory",
            "merge",
            "--survivor",
            "mem-survivor:1",
            "--loser",
            "mem-loser:1",
        ],
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");

    let output = run_cli(&home, &["memory", "list", "--state", "superseded"]);
    assert!(
        stdout(&output).contains("mem-loser"),
        "{:?}",
        stdout(&output)
    );
}

#[tokio::test]
async fn memory_evidence_lists_attached_observation_ids() {
    let (home, layout) = open_layout();
    {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        seed_entry(&state, "mem-evidenced", "text", 1_000).await;
        seed_observation(&state, "obs-1").await;
        seed_evidence(&state, "mem-evidenced", "obs-1").await;
    }

    let output = run_cli(&home, &["memory", "evidence", "mem-evidenced"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(stdout(&output).contains("obs-1"), "{:?}", stdout(&output));
}

#[test]
fn memory_evidence_of_an_unknown_id_reports_no_evidence() {
    let (home, _layout) = open_layout();
    let output = run_cli(&home, &["memory", "evidence", "unknown-id"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(
        stdout(&output).contains("no evidence"),
        "{:?}",
        stdout(&output)
    );
}

// ---------------------------------------------------------------------
// candidates: list --candidates / approve / reject
// ---------------------------------------------------------------------

#[tokio::test]
async fn memory_list_candidates_then_approve_and_reject() {
    let (home, layout) = open_layout();
    {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        seed_candidate(&state, "cand-approve", "mem-new-a", 1_000).await;
        seed_candidate(&state, "cand-reject", "mem-new-b", 2_000).await;
    }

    let output = run_cli(&home, &["memory", "list", "--candidates"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let text = stdout(&output);
    assert!(text.contains("cand-approve"), "{text}");
    assert!(text.contains("cand-reject"), "{text}");

    let output = run_cli(&home, &["memory", "approve", "cand-approve"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");

    let output = run_cli(&home, &["memory", "reject", "cand-reject"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");

    // Rejecting an already-terminal (approved) candidate is refused, not a
    // silent success.
    let output = run_cli(&home, &["memory", "reject", "cand-approve"]);
    assert_ne!(output.status.code(), Some(0), "{output:?}");
}
