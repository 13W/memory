//! `local-rag repo list` / `local-rag repo attach` / `local-rag worktree
//! list` acceptance tests (spec 11 §6, spec 04 §7), driving the real
//! compiled binary — mirrors `tests/cli_index.rs`'s own `open_layout`/
//! `run_cli`/seeding helpers (duplicated here per this crate's established
//! per-file-fixture convention).
//!
//! None of these commands ever touch the embedder, so — unlike
//! `index`/`reindex`/`watch` — nothing here needs `ORT_DYLIB_PATH` or an
//! installed model; every test in this file runs unconditionally.

#![cfg(unix)]

use std::path::Path;
use std::process::{Command, Output, Stdio};

use local_rag::daemon::gitroot;
use local_rag_core::paths::StoreLayout;
use local_rag_store::{
    StateDb, WorktreeState, create_repository, create_worktree, insert_projection_state,
    observe_repository_path, observe_worktree_path, transition_worktree_state,
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

fn tokio_test_block_on<F: std::future::Future>(fut: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(fut)
}

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "local-rag-test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "local-rag-test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed");
}

/// Register one active `{repo, worktree}` at `path` (a fresh worktree,
/// current at `path`, in a brand-new repository). Uses the real
/// `gitroot::probe` so the seeded row matches byte for byte what the CLI's
/// own probe of the same directory computes.
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

/// Register one more active worktree at `path` under an *already-existing*
/// repository — the sibling of [`seed_active_repo_and_worktree`] for
/// scenarios needing two worktrees under the same `repo_id`.
async fn seed_sibling_worktree(
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
            create_worktree(tx, &worktree_id, &repo_id, facts.kind, 1_000)?;
            observe_worktree_path(
                tx,
                &worktree_id,
                &facts.observed_canonical_path,
                &facts.display_path,
                &facts.path_fingerprint,
                1_000,
            )?;
            insert_projection_state(tx, &worktree_id, 1_000)
        })
        .await
        .expect("seed sibling worktree");
}

async fn detach(layout: &StoreLayout, worktree_id: &str) {
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    let worktree_id = worktree_id.to_string();
    db.writer()
        .transaction(move |tx| {
            transition_worktree_state(tx, &worktree_id, WorktreeState::Detached, 2_000)
        })
        .await
        .expect("transition worktree state")
        .expect("legal transition");
}

async fn worktree_state(layout: &StoreLayout, worktree_id: &str) -> WorktreeState {
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    let conn = db.open_read().expect("read connection");
    local_rag_store::worktree_state(&conn, worktree_id)
        .expect("query worktree state")
        .expect("worktree exists")
}

#[test]
fn repo_list_on_a_fresh_store_reports_nothing_registered() {
    let (home, _layout) = open_layout();
    let output = run_cli(&home, home.path(), &["repo", "list"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("no repositories registered"),
        "{:?}",
        output.stdout
    );
}

#[test]
fn worktree_list_on_a_fresh_store_reports_nothing_registered() {
    let (home, _layout) = open_layout();
    let output = run_cli(&home, home.path(), &["worktree", "list"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("no worktrees registered"),
        "{:?}",
        output.stdout
    );
}

#[test]
fn repo_rejects_an_unknown_subcommand() {
    let (home, _layout) = open_layout();
    let output = run_cli(&home, home.path(), &["repo", "bogus"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
}

#[test]
fn worktree_rejects_an_unknown_subcommand() {
    let (home, _layout) = open_layout();
    let output = run_cli(&home, home.path(), &["worktree", "bogus"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
}

#[test]
fn repo_list_rejects_an_extra_argument() {
    let (home, _layout) = open_layout();
    let output = run_cli(&home, home.path(), &["repo", "list", "extra"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
}

#[test]
fn repo_attach_without_a_repo_id_is_a_usage_error() {
    let (home, _layout) = open_layout();
    let output = run_cli(&home, home.path(), &["repo", "attach"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
}

#[test]
fn repo_with_no_subcommand_is_a_usage_error() {
    let (home, _layout) = open_layout();
    let output = run_cli(&home, home.path(), &["repo"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
}

#[test]
fn worktree_with_no_subcommand_is_a_usage_error() {
    let (home, _layout) = open_layout();
    let output = run_cli(&home, home.path(), &["worktree"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
}

#[test]
fn worktree_list_rejects_an_extra_argument() {
    let (home, _layout) = open_layout();
    let output = run_cli(&home, home.path(), &["worktree", "list", "extra"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
}

#[test]
fn repo_attach_rejects_an_unknown_flag() {
    let (home, _layout) = open_layout();
    let output = run_cli(
        &home,
        home.path(),
        &["repo", "attach", "some-repo-id", "--bogus"],
    );
    assert_eq!(output.status.code(), Some(2), "{output:?}");
}

#[test]
fn repo_attach_rejects_a_path_flag_missing_its_value() {
    let (home, _layout) = open_layout();
    let output = run_cli(
        &home,
        home.path(),
        &["repo", "attach", "some-repo-id", "--path"],
    );
    assert_eq!(output.status.code(), Some(2), "{output:?}");
}

#[test]
fn repo_attach_rejects_a_worktree_flag_missing_its_value() {
    let (home, _layout) = open_layout();
    let output = run_cli(
        &home,
        home.path(),
        &["repo", "attach", "some-repo-id", "--worktree"],
    );
    assert_eq!(output.status.code(), Some(2), "{output:?}");
}

#[test]
fn repo_list_prints_every_repository_with_path_and_worktree_count() {
    let (home, layout) = open_layout();
    let a = home.join("repo-a");
    let b = home.join("repo-b");
    std::fs::create_dir_all(&a).expect("create repo-a");
    std::fs::create_dir_all(&b).expect("create repo-b");
    tokio_test_block_on(seed_active_repo_and_worktree(
        &layout,
        "10000000-0000-7000-8000-000000000001",
        "10000000-0000-7000-8000-000000000002",
        &a,
    ));
    tokio_test_block_on(seed_active_repo_and_worktree(
        &layout,
        "20000000-0000-7000-8000-000000000001",
        "20000000-0000-7000-8000-000000000002",
        &b,
    ));

    let output = run_cli(&home, home.path(), &["repo", "list"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("10000000-0000-7000-8000-000000000001")
            && stdout.contains("(1 worktree(s))"),
        "{stdout}"
    );
    assert!(
        stdout.contains("20000000-0000-7000-8000-000000000001"),
        "{stdout}"
    );
}

#[test]
fn worktree_list_prints_kind_and_state() {
    let (home, layout) = open_layout();
    let target = home.join("project");
    std::fs::create_dir_all(&target).expect("create target dir");
    tokio_test_block_on(seed_active_repo_and_worktree(
        &layout,
        "10000000-0000-7000-8000-000000000001",
        "10000000-0000-7000-8000-000000000002",
        &target,
    ));
    tokio_test_block_on(detach(&layout, "10000000-0000-7000-8000-000000000002"));

    let output = run_cli(&home, home.path(), &["worktree", "list"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("10000000-0000-7000-8000-000000000002"),
        "{stdout}"
    );
    assert!(stdout.contains("non_git"), "{stdout}");
    assert!(stdout.contains("detached"), "{stdout}");
}

#[test]
fn repo_attach_happy_path_reattaches_a_detached_worktree() {
    let (home, layout) = open_layout();
    let target = home.join("project");
    std::fs::create_dir_all(&target).expect("create target dir");
    let repo_id = "10000000-0000-7000-8000-000000000001";
    let worktree_id = "10000000-0000-7000-8000-000000000002";
    tokio_test_block_on(seed_active_repo_and_worktree(
        &layout,
        repo_id,
        worktree_id,
        &target,
    ));
    tokio_test_block_on(detach(&layout, worktree_id));
    assert_eq!(
        tokio_test_block_on(worktree_state(&layout, worktree_id)),
        WorktreeState::Detached
    );

    // No `--worktree`: the repo_id itself is the resolution hint, and this
    // path's own recorded fingerprint is its only (and therefore
    // auto-resolving) candidate.
    let output = run_cli(
        &home,
        home.path(),
        &[
            "repo",
            "attach",
            repo_id,
            "--path",
            target.to_str().unwrap(),
        ],
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(
        tokio_test_block_on(worktree_state(&layout, worktree_id)),
        WorktreeState::Active,
        "attach must reactivate the worktree"
    );
}

#[test]
fn repo_attach_with_explicit_worktree_flag_reattaches_directly() {
    let (home, layout) = open_layout();
    let target = home.join("project");
    std::fs::create_dir_all(&target).expect("create target dir");
    let repo_id = "10000000-0000-7000-8000-000000000001";
    let worktree_id = "10000000-0000-7000-8000-000000000002";
    tokio_test_block_on(seed_active_repo_and_worktree(
        &layout,
        repo_id,
        worktree_id,
        &target,
    ));
    tokio_test_block_on(detach(&layout, worktree_id));

    let output = run_cli(
        &home,
        home.path(),
        &[
            "repo",
            "attach",
            repo_id,
            "--path",
            target.to_str().unwrap(),
            "--worktree",
            worktree_id,
        ],
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(
        tokio_test_block_on(worktree_state(&layout, worktree_id)),
        WorktreeState::Active
    );
}

#[test]
fn repo_attach_worktree_flag_belonging_to_another_repo_is_refused() {
    let (home, layout) = open_layout();
    let target = home.join("project");
    std::fs::create_dir_all(&target).expect("create target dir");
    let owner_repo = "10000000-0000-7000-8000-000000000001";
    let worktree_id = "10000000-0000-7000-8000-000000000002";
    let other_repo = "20000000-0000-7000-8000-000000000001";
    tokio_test_block_on(seed_active_repo_and_worktree(
        &layout,
        owner_repo,
        worktree_id,
        &target,
    ));

    let output = run_cli(
        &home,
        home.path(),
        &[
            "repo",
            "attach",
            other_repo,
            "--path",
            target.to_str().unwrap(),
            "--worktree",
            worktree_id,
        ],
    );
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("belongs to repository"),
        "{:?}",
        output.stderr
    );
}

/// Two detached worktrees of one repository, discovered only through a
/// shared git remote fingerprint — the literal "attach ambiguity" scenario
/// the task card names, and `local_rag_store::registry::resolve`'s own doc
/// draws as its example of when even a `repo_hint` cannot pick automatically.
#[test]
fn repo_attach_ambiguous_candidates_requires_worktree_flag() {
    if !git_available() {
        eprintln!("SKIP: git not on PATH");
        return;
    }
    let (home, layout) = open_layout();
    let url = "https://example.invalid/org/repo.git";

    let clone_a = home.join("clone-a");
    std::fs::create_dir_all(&clone_a).expect("create clone-a");
    git(&clone_a, &["init", "-q"]);
    git(&clone_a, &["remote", "add", "origin", url]);

    let clone_b = home.join("clone-b");
    std::fs::create_dir_all(&clone_b).expect("create clone-b");
    git(&clone_b, &["init", "-q"]);
    git(&clone_b, &["remote", "add", "origin", url]);

    let repo_id = "10000000-0000-7000-8000-000000000001";
    let worktree_a = "10000000-0000-7000-8000-0000000000a1";
    let worktree_b = "10000000-0000-7000-8000-0000000000b1";
    tokio_test_block_on(seed_active_repo_and_worktree(
        &layout, repo_id, worktree_a, &clone_a,
    ));
    tokio_test_block_on(seed_sibling_worktree(
        &layout, repo_id, worktree_b, &clone_b,
    ));
    tokio_test_block_on(detach(&layout, worktree_a));
    tokio_test_block_on(detach(&layout, worktree_b));

    // A third clone of the same origin, never seen before: its path
    // fingerprint matches nothing, but its remote fingerprint matches both
    // detached worktrees above.
    let clone_c = home.join("clone-c");
    std::fs::create_dir_all(&clone_c).expect("create clone-c");
    git(&clone_c, &["init", "-q"]);
    git(&clone_c, &["remote", "add", "origin", url]);

    let output = run_cli(
        &home,
        home.path(),
        &[
            "repo",
            "attach",
            repo_id,
            "--path",
            clone_c.to_str().unwrap(),
        ],
    );
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--worktree"), "{stderr}");
    assert!(
        stderr.contains(worktree_a) && stderr.contains(worktree_b),
        "{stderr}"
    );
}
