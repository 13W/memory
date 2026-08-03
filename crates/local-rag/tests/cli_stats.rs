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
    GLOBAL_SCOPE_OWNER_ID, MemoryKind, NewMemoryEntry, ProposedOperation, ScopeKind, StateDb,
    create_memory_entry, create_repository, create_worktree, insert_projection_state,
    observe_repository_path, observe_worktree_path, propose_candidate,
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
}

#[tokio::test]
async fn stats_reports_seeded_counts_and_the_resolved_worktree_block() {
    let (home, layout) = open_layout();
    {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        seed_entry(&state, "mem-a", MemoryKind::Fact, "a fact", 1_000).await;
        seed_entry(&state, "mem-b", MemoryKind::Task, "a task", 2_000).await;
        seed_candidate(&state, "cand-a", 1_000).await;
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
}
