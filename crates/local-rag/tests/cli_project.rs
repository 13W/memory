//! `local-rag project add|remove|enable|disable|list|status|reindex`
//! acceptance tests (spec 11 §6 + §8, T20-08), driving the real compiled
//! binary — mirrors `tests/cli_repo.rs`'s own `open_layout`/`run_cli` shapes
//! (duplicated here per this crate's established per-file-fixture
//! convention), plus `tests/cli_service.rs`'s `spawn_serve`/`wait_until_ready`
//! for the one scenario that needs a genuine live daemon.
//!
//! `add`/`remove`/`enable`/`disable`/`list` never touch the embedder or a
//! live daemon — like `cli_repo.rs`, those run unconditionally, no
//! `ORT_DYLIB_PATH`/model install required. The live-daemon scenario needs
//! no real embedder either: T20-05's own "no installed model silently skips
//! projection this tick" behavior means a worktree task starts and is
//! watch-registered (so `admin/projects_list` reports a non-null `task`)
//! regardless of embedder readiness — only an actually-completed generation
//! would need one, and this file never waits for one.

#![cfg(unix)]

use std::path::Path;
use std::process::{Child, Output, Stdio};
use std::time::{Duration, Instant};

use local_rag::daemon::gitroot;
use local_rag_core::paths::StoreLayout;
use local_rag_store::{
    StateDb, create_repository, create_worktree, insert_projection_state, observe_repository_path,
    observe_worktree_path,
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
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.output().expect("run local-rag")
}

fn spawn_serve(home: &TempHome) -> Child {
    let mut cmd = home.command(env!("CARGO_BIN_EXE_local-rag"));
    cmd.arg("serve");
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.spawn().expect("spawn local-rag serve")
}

fn wait_until_ready(layout: &StoreLayout, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(bytes) = std::fs::read(layout.store_lock())
            && let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes)
            && json.get("ready").and_then(|v| v.as_bool()) == Some(true)
        {
            return;
        }
        if Instant::now() >= deadline {
            panic!("store.lock did not become ready within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn tokio_test_block_on<F: std::future::Future>(fut: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(fut)
}

/// Register a plain (non-managed) `{repo, worktree}` at `path` — the same
/// seeding `tests/cli_repo.rs::seed_active_repo_and_worktree` does, without
/// ever enrolling it in `managed_worktree`. Used to exercise `enable`/
/// `disable`/`remove` on a worktree that is known but never `project add`-ed.
async fn seed_unmanaged_worktree(
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
        .expect("seed unmanaged worktree")
}

#[test]
fn add_registers_and_manages_a_new_path() {
    let (home, _layout) = open_layout();
    let target = home.join("repo");
    std::fs::create_dir_all(&target).expect("create target dir");

    let output = run_cli(&home, &["project", "add", target.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let out = stdout(&output);
    assert!(out.contains("managing worktree"), "{out}");
    assert!(out.contains(target.to_str().unwrap()), "{out}");

    let list = run_cli(&home, &["project", "list"]);
    assert_eq!(list.status.code(), Some(0), "{list:?}");
    let list_out = stdout(&list);
    assert!(list_out.contains("enabled=true"), "{list_out}");
    assert!(list_out.contains(target.to_str().unwrap()), "{list_out}");
}

/// The card's own literal test scenario: add → list → disable → list → remove.
#[test]
fn round_trip_add_list_disable_list_remove() {
    let (home, _layout) = open_layout();
    let target = home.join("repo");
    std::fs::create_dir_all(&target).expect("create target dir");
    let path = target.to_str().unwrap();

    assert_eq!(
        run_cli(&home, &["project", "add", path]).status.code(),
        Some(0)
    );

    let after_add = run_cli(&home, &["project", "list"]);
    assert!(
        stdout(&after_add).contains("enabled=true"),
        "{:?}",
        after_add
    );

    assert_eq!(
        run_cli(&home, &["project", "disable", path]).status.code(),
        Some(0)
    );

    let after_disable = run_cli(&home, &["project", "list"]);
    assert!(
        stdout(&after_disable).contains("enabled=false"),
        "{:?}",
        after_disable
    );

    assert_eq!(
        run_cli(&home, &["project", "remove", path]).status.code(),
        Some(0)
    );

    let after_remove = run_cli(&home, &["project", "list"]);
    assert!(
        stdout(&after_remove).contains("no managed projects"),
        "{:?}",
        after_remove
    );
}

#[test]
fn add_rejects_a_nonexistent_path() {
    let (home, _layout) = open_layout();
    let missing = home.join("does-not-exist");

    let output = run_cli(&home, &["project", "add", missing.to_str().unwrap()]);
    assert_ne!(output.status.code(), Some(0), "{output:?}");
    assert!(
        stderr(&output).contains("not an accessible directory"),
        "{:?}",
        output
    );
}

/// None of the durable-only verbs must attempt to spawn or wait for a
/// daemon. `store.lock` is created by exactly one thing in this whole
/// codebase — `local-rag serve`'s own startup (spec 02 §4.1 step 1) — so its
/// continued absence after every verb runs is direct, non-flaky proof that
/// none of them ever spawned one (a wall-clock bound would only prove
/// "fast enough", and is exactly the kind of assertion that flakes under
/// this machine's own well-documented shared-load jitter).
#[test]
fn commands_never_spawn_or_wait_for_a_daemon() {
    let (home, layout) = open_layout();
    let target = home.join("repo");
    std::fs::create_dir_all(&target).expect("create target dir");
    let path = target.to_str().unwrap();

    for args in [
        vec!["project", "add", path],
        vec!["project", "list"],
        vec!["project", "disable", path],
        vec!["project", "enable", path],
        vec!["project", "remove", path],
    ] {
        run_cli(&home, &args);
        assert!(
            !layout.store_lock().exists(),
            "{args:?} left a store.lock behind, suggesting it spawned a daemon"
        );
    }
}

#[test]
fn list_and_status_json_are_stable_and_valid() {
    let (home, _layout) = open_layout();
    let target = home.join("repo");
    std::fs::create_dir_all(&target).expect("create target dir");
    run_cli(&home, &["project", "add", target.to_str().unwrap()]);

    let list = run_cli(&home, &["project", "list", "--json"]);
    assert_eq!(list.status.code(), Some(0), "{list:?}");
    let list_json: serde_json::Value =
        serde_json::from_slice(&list.stdout).expect("valid json on stdout");
    let projects = list_json["projects"].as_array().expect("projects array");
    assert_eq!(projects.len(), 1);
    for key in [
        "worktree_id",
        "enabled",
        "registered_at",
        "updated_at",
        "path",
    ] {
        assert!(projects[0].get(key).is_some(), "missing {key}: {list_json}");
    }

    let status = run_cli(&home, &["project", "status", "--json"]);
    assert_eq!(status.status.code(), Some(0), "{status:?}");
    let status_json: serde_json::Value =
        serde_json::from_slice(&status.stdout).expect("valid json on stdout");
    assert_eq!(status_json["daemon"], "not_running");
    let status_projects = status_json["projects"].as_array().expect("projects array");
    assert_eq!(status_projects.len(), 1);
    assert!(status_projects[0]["task"].is_null());
}

#[test]
fn reindex_without_a_daemon_hints_at_plain_reindex() {
    let (home, _layout) = open_layout();
    let target = home.join("repo");
    std::fs::create_dir_all(&target).expect("create target dir");
    let path = target.to_str().unwrap();
    assert_eq!(
        run_cli(&home, &["project", "add", path]).status.code(),
        Some(0)
    );

    let output = run_cli(&home, &["project", "reindex", path]);
    assert_ne!(output.status.code(), Some(0), "{output:?}");
    assert!(
        stderr(&output).contains("local-rag reindex"),
        "{:?}",
        output
    );
}

#[test]
fn enable_disable_remove_on_a_path_never_added_are_typed_refusals() {
    let (home, _layout) = open_layout();
    let target = home.join("repo");
    std::fs::create_dir_all(&target).expect("create target dir");
    let path = target.to_str().unwrap();

    // Never even indexed: `GlobalOnly`.
    for verb in ["enable", "disable", "remove"] {
        let output = run_cli(&home, &["project", verb, path]);
        assert_ne!(output.status.code(), Some(0), "{verb}: {output:?}");
        assert!(
            stderr(&output).contains("not a known worktree"),
            "{verb}: {:?}",
            output
        );
    }
}

#[test]
fn enable_disable_on_a_known_but_unmanaged_worktree_are_typed_refusals() {
    let (home, layout) = open_layout();
    let target = home.join("repo");
    std::fs::create_dir_all(&target).expect("create target dir");
    let path = target.to_str().unwrap();
    // Indexed and known (`Resolved`), but never `project add`-ed.
    tokio_test_block_on(seed_unmanaged_worktree(
        &layout,
        "10000000-0000-7000-8000-000000000001",
        "10000000-0000-7000-8000-000000000002",
        &target,
    ));

    for verb in ["enable", "disable"] {
        let output = run_cli(&home, &["project", verb, path]);
        assert_ne!(output.status.code(), Some(0), "{verb}: {output:?}");
        assert!(
            stderr(&output).contains("not a managed project"),
            "{verb}: {:?}",
            output
        );
    }

    // `remove` on an unmanaged-but-known worktree is idempotent success, not
    // a refusal — matching `unregister_managed_worktree`'s own doc.
    let removed = run_cli(&home, &["project", "remove", path]);
    assert_eq!(removed.status.code(), Some(0), "{removed:?}");
    assert!(stdout(&removed).contains("is not managed"), "{:?}", removed);
}

/// The one scenario that needs a genuine live daemon: `status`/`reindex`
/// through the real `admin/projects_list`/`admin/reconcile_now` verbs
/// (T20-07) over a real subprocess `local-rag serve`.
#[test]
fn status_and_reindex_work_through_a_live_daemon() {
    let (home, layout) = open_layout();
    let target = home.join("repo");
    std::fs::create_dir_all(&target).expect("create target dir");
    let path = target.to_str().unwrap();
    assert_eq!(
        run_cli(&home, &["project", "add", path]).status.code(),
        Some(0)
    );

    let mut daemon = spawn_serve(&home);
    wait_until_ready(&layout, Duration::from_secs(20));

    // The supervisor's own worktree task must finish starting (including its
    // `spawn_watcher` cold start, which this machine's own documented
    // FSEvents contention can stretch well past a naive bound — T20-07's own
    // `admin_indexing.rs` precedent) before `task` turns non-null.
    let deadline = Instant::now() + Duration::from_secs(120);
    let json = loop {
        let status = run_cli(&home, &["project", "status", "--json"]);
        assert_eq!(status.status.code(), Some(0), "{status:?}");
        let json: serde_json::Value =
            serde_json::from_slice(&status.stdout).expect("valid json on stdout");
        if json["daemon"] == "running" && !json["projects"][0]["task"].is_null() {
            break json;
        }
        if Instant::now() >= deadline {
            let _ = daemon.kill();
            let _ = daemon.wait();
            panic!("project status never reported a live task within 120s: {json}");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(json["projects"][0]["worktree_id"].as_str().is_some());

    let reindex = run_cli(&home, &["project", "reindex", path]);
    assert_eq!(reindex.status.code(), Some(0), "{reindex:?}");
    assert!(
        stdout(&reindex).contains("reconcile triggered"),
        "{:?}",
        reindex
    );

    let _ = daemon.kill();
    let _ = daemon.wait();
}

// ---------------------------------------------------------------------------
// X-008: the empty registry and the current directory both say so out loud
// ---------------------------------------------------------------------------

/// Before X-008 an empty registry printed one line — `no managed projects` —
/// which reads identically whether the feature is off or the command is broken.
/// It must now also say how many known worktrees are sitting unenrolled, and
/// name the command that fixes it.
#[tokio::test]
async fn list_on_an_empty_registry_reports_the_unenrolled_worktrees_and_the_fix() {
    let (home, layout) = open_layout();
    let dir = home.join("wt-a");
    std::fs::create_dir_all(&dir).expect("create worktree dir");
    seed_unmanaged_worktree(
        &layout,
        "018f0000-0000-7000-8000-0000000000a1",
        "018f0000-0000-7000-8000-0000000000b1",
        &dir,
    )
    .await;

    let out = run_cli(&home, &["project", "list"]);
    assert!(out.status.success(), "list must succeed: {out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("no managed projects"),
        "the original line stays: {stdout}"
    );
    assert!(
        stdout.contains("1 registered worktree(s) are NOT enrolled"),
        "the count of unenrolled worktrees must appear: {stdout}"
    );
    assert!(
        stdout.contains("local-rag project add"),
        "the fix must travel with the diagnosis: {stdout}"
    );
}

/// The question a human standing in a project actually has. `status` answers it
/// for the current directory specifically, not just for the registry at large.
#[tokio::test]
async fn status_says_background_indexing_is_off_for_the_current_directory() {
    let (home, layout) = open_layout();
    let dir = home.join("wt-here");
    std::fs::create_dir_all(&dir).expect("create worktree dir");
    seed_unmanaged_worktree(
        &layout,
        "018f0000-0000-7000-8000-0000000000a2",
        "018f0000-0000-7000-8000-0000000000b2",
        &dir,
    )
    .await;

    let mut cmd = home.command(env!("CARGO_BIN_EXE_local-rag"));
    cmd.args(["project", "status"]);
    cmd.current_dir(&dir); // stand inside the seeded worktree
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let out = cmd.output().expect("run local-rag project status");

    assert!(out.status.success(), "status must succeed: {out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("background indexing is OFF for this worktree"),
        "the current directory's own state must be named: {stdout}"
    );
    assert!(
        stdout.contains("local-rag project add ."),
        "and the exact command to enroll it: {stdout}"
    );
}

/// Enrolling flips that line off — the same command must not keep nagging once
/// the project is actually managed.
#[tokio::test]
async fn status_stops_warning_once_the_current_directory_is_enrolled() {
    let (home, layout) = open_layout();
    let dir = home.join("wt-managed");
    std::fs::create_dir_all(&dir).expect("create worktree dir");
    seed_unmanaged_worktree(
        &layout,
        "018f0000-0000-7000-8000-0000000000a3",
        "018f0000-0000-7000-8000-0000000000b3",
        &dir,
    )
    .await;

    let added = run_cli(
        &home,
        &["project", "add", dir.to_str().expect("utf-8 path")],
    );
    assert!(added.status.success(), "add must succeed: {added:?}");

    let mut cmd = home.command(env!("CARGO_BIN_EXE_local-rag"));
    cmd.args(["project", "status"]);
    cmd.current_dir(&dir);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let out = cmd.output().expect("run local-rag project status");

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("background indexing is OFF for this worktree"),
        "an enrolled directory must not be warned about: {stdout}"
    );
    assert!(
        !stdout.contains("are NOT enrolled"),
        "and with every worktree enrolled there is nothing left to nag about: {stdout}"
    );
}

/// `--json` gains the X-008 fields without renaming or dropping T20-08's.
#[tokio::test]
async fn list_json_carries_both_the_original_and_the_new_fields() {
    let (home, layout) = open_layout();
    let dir = home.join("wt-json");
    std::fs::create_dir_all(&dir).expect("create worktree dir");
    seed_unmanaged_worktree(
        &layout,
        "018f0000-0000-7000-8000-0000000000a4",
        "018f0000-0000-7000-8000-0000000000b4",
        &dir,
    )
    .await;
    run_cli(
        &home,
        &["project", "add", dir.to_str().expect("utf-8 path")],
    );

    let out = run_cli(&home, &["project", "list", "--json"]);
    assert!(out.status.success(), "list --json must succeed: {out:?}");
    let json: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("list --json emits valid JSON");
    let row = &json["projects"][0];
    for original in [
        "worktree_id",
        "enabled",
        "registered_at",
        "updated_at",
        "path",
    ] {
        assert!(
            !row[original].is_null(),
            "T20-08's {original:?} must survive: {json}"
        );
    }
    assert!(
        json["unenrolled_worktrees"].is_number(),
        "the unenrolled count is part of the machine-readable answer: {json}"
    );
    assert!(
        row.get("stuck_generations").is_some() && row.get("active_generation_number").is_some(),
        "X-008's fields must be present even when empty/null: {json}"
    );
}
