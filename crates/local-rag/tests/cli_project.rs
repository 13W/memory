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

/// Whether a CLI run failed *only* because the daemon did not answer inside
/// `cli::project::ADMIN_CALL_TIMEOUT` (2 s).
///
/// D-091: under `nextest`'s process-per-test parallelism that budget is
/// exceeded reliably on a loaded machine — the gate's own run failed here at
/// `2.084s` while the same test alone finishes in `1.32s`. A daemon that has
/// not answered yet is not a daemon that answered wrong, and the caller below
/// already owns a 120 s deadline for exactly this kind of waiting.
///
/// Deliberately narrow: it matches this one message, so any other non-zero
/// exit still fails on the spot instead of hiding until the deadline runs out.
fn is_transient_admin_timeout(out: &Output) -> bool {
    out.status.code() == Some(1)
        && String::from_utf8_lossy(&out.stderr).contains("the daemon did not answer in time")
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
    // Declared without a value: every path through the loop body assigns it
    // before the deadline check reads it, and seeding it with a `None` nobody
    // reads is a warning under the workspace's `-D warnings`.
    let mut last: String;
    let json = loop {
        let status = run_cli(&home, &["project", "status", "--json"]);
        // D-091: a 2 s admin round trip that did not land yet is part of the
        // cold start this loop exists to wait out, not a verdict about it.
        if !is_transient_admin_timeout(&status) {
            assert_eq!(status.status.code(), Some(0), "{status:?}");
            let json: serde_json::Value =
                serde_json::from_slice(&status.stdout).expect("valid json on stdout");
            if json["daemon"] == "running" && !json["projects"][0]["task"].is_null() {
                break json;
            }
            last = json.to_string();
        } else {
            last = format!("{status:?}");
        }
        if Instant::now() >= deadline {
            let _ = daemon.kill();
            let _ = daemon.wait();
            panic!("project status never reported a live task within 120s: {last}");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(json["projects"][0]["worktree_id"].as_str().is_some());

    // Same tolerance, same reason (D-091): this call had none at all, and it
    // runs under the same 2 s budget the loop above just waited out.
    let reindex = loop {
        let out = run_cli(&home, &["project", "reindex", path]);
        if !is_transient_admin_timeout(&out) {
            break out;
        }
        if Instant::now() >= deadline {
            let _ = daemon.kill();
            let _ = daemon.wait();
            panic!("project reindex never reached the daemon within the deadline: {out:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
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

// ---------------------------------------------------------------------------
// D-096 — `project coverage`
// ---------------------------------------------------------------------------

/// Seed an `active` generation over `worktree_id` holding exactly the given
/// members and skips.
///
/// Direct seeding rather than a real `local-rag index` run on purpose: the whole
/// pipeline needs an installed embedding model (`cli_index.rs` gates on one),
/// while what `D-096` added is the arithmetic between the tree and the two
/// membership tables — which is fully determined by those rows. One
/// `file_revision` is minted per member because `generation_file` has a foreign
/// key to it; `skipped_file` deliberately has none (a skipped file never gets a
/// `source_blob`, spec 12 §5).
async fn seed_active_generation(
    layout: &StoreLayout,
    worktree_id: &str,
    generation_id: &str,
    members: &[&str],
    skips: &[(&str, local_rag_store::SkipReason)],
) {
    use local_rag_store::{
        GenerationState, NewFileRevision, NewlineStyle, SourceCompression, allocate_generation,
        insert_file_revision, insert_generation_file, insert_skipped_file, transition_generation,
    };

    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    let (worktree_id, generation_id) = (worktree_id.to_string(), generation_id.to_string());
    let members: Vec<String> = members.iter().map(|m| (*m).to_string()).collect();
    let skips: Vec<(String, local_rag_store::SkipReason)> =
        skips.iter().map(|(p, r)| ((*p).to_string(), *r)).collect();

    db.writer()
        .transaction(move |tx| {
            allocate_generation(tx, &worktree_id, &generation_id, 1_800_000_000_000)?;
            for (i, path) in members.iter().enumerate() {
                let revision_id = format!("019f0000-0000-7000-8000-00000000{i:04}");
                let content_hash = format!("hash{i:060}");
                insert_file_revision(
                    tx,
                    &NewFileRevision {
                        file_revision_id: &revision_id,
                        content_hash: &content_hash,
                        parser_fingerprint: "chunk=1;grammar=t@1;lang=rust;norm=1;queries=1",
                        source_blob: b"fn a() {}\n",
                        compression: SourceCompression::None,
                        source_encoding: "utf-8",
                        newline_style: NewlineStyle::Lf,
                        source_size: 10,
                    },
                    1_800_000_000_000,
                )?;
                insert_generation_file(tx, &generation_id, path, path, &revision_id)?;
            }
            for (path, reason) in &skips {
                insert_skipped_file(tx, &generation_id, path, *reason, None)?;
            }
            transition_generation(tx, &generation_id, GenerationState::ProjectionReady)?
                .expect("building → projection_ready");
            transition_generation(tx, &generation_id, GenerationState::Active)?
                .expect("projection_ready → active");
            Ok(())
        })
        .await
        .expect("seed active generation")
}

/// The live-store measurement that found `D-096`, reproduced as a test: a tree
/// holding files the generation put in neither membership table, and a command
/// that says so with a number and an extension histogram.
#[test]
fn coverage_names_the_files_the_generation_accounted_for_and_the_ones_it_did_not() {
    let (home, layout) = open_layout();
    let target = home.join("repo");
    std::fs::create_dir_all(target.join("docs")).expect("create docs");
    std::fs::create_dir_all(target.join("deploy")).expect("create deploy");
    // Two indexed, one skipped, four the generation never saw.
    std::fs::write(target.join("a.rs"), b"fn a() {}\n").expect("write a.rs");
    std::fs::write(target.join("b.rs"), b"fn b() {}\n").expect("write b.rs");
    std::fs::write(target.join("blob.rs"), b"fn c() {}\0").expect("write blob.rs");
    std::fs::write(target.join("docs/one.md"), b"# one\n").expect("write one.md");
    std::fs::write(target.join("docs/two.md"), b"# two\n").expect("write two.md");
    std::fs::write(target.join("deploy/values.yaml"), b"image: x\n").expect("write values.yaml");
    std::fs::write(target.join("Makefile"), b"all:\n").expect("write Makefile");

    let add = run_cli(&home, &["project", "add", target.to_str().unwrap()]);
    assert_eq!(add.status.code(), Some(0), "{add:?}");

    let worktree_id = {
        let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
        let conn = db.open_read().expect("read");
        local_rag_store::all_worktree_ids(&conn).expect("ids")[0].clone()
    };
    tokio_test_block_on(seed_active_generation(
        &layout,
        &worktree_id,
        "019f1111-1111-7111-8111-111111111111",
        &["a.rs", "b.rs"],
        &[("blob.rs", local_rag_store::SkipReason::Binary)],
    ));

    let out = run_cli(&home, &["project", "coverage", target.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let text = stdout(&out);

    // The durable half, in the one wording every command shares.
    assert!(text.contains("2 indexed, 1 skipped (1 binary)"), "{text}");
    // The measured half: four files in neither table, grouped by extension.
    assert!(
        text.contains("4 file(s) in the tree are in NEITHER table"),
        "{text}"
    );
    assert!(text.contains("md 2"), "{text}");
    assert!(text.contains("yaml 1"), "{text}");
    assert!(text.contains("(none) 1"), "{text}");
    // Examples name actual paths, so the diagnosis is actionable.
    assert!(text.contains("docs/one.md"), "{text}");
    assert!(text.contains("deploy/values.yaml"), "{text}");
}

/// A tree whose every file is accounted for says so plainly — the state
/// `D-098` is required to make permanent, and the one a passing report must not
/// confuse with "I did not look".
#[test]
fn coverage_on_a_fully_accounted_tree_says_so() {
    let (home, layout) = open_layout();
    let target = home.join("repo");
    std::fs::create_dir_all(&target).expect("create target");
    std::fs::write(target.join("a.rs"), b"fn a() {}\n").expect("write a.rs");
    std::fs::write(target.join("blob.rs"), b"fn c() {}\0").expect("write blob.rs");

    let add = run_cli(&home, &["project", "add", target.to_str().unwrap()]);
    assert_eq!(add.status.code(), Some(0), "{add:?}");
    let worktree_id = {
        let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
        let conn = db.open_read().expect("read");
        local_rag_store::all_worktree_ids(&conn).expect("ids")[0].clone()
    };
    tokio_test_block_on(seed_active_generation(
        &layout,
        &worktree_id,
        "019f2222-2222-7222-8222-222222222222",
        &["a.rs"],
        &[("blob.rs", local_rag_store::SkipReason::Binary)],
    ));

    let out = run_cli(&home, &["project", "coverage", target.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let text = stdout(&out);
    assert!(
        text.contains("every file in the tree is accounted for"),
        "{text}"
    );
    assert!(!text.contains("NEITHER table"), "{text}");
}

/// `coverage` is a diagnostic, and a diagnostic that mutates what it measures
/// is worthless. Asserted against the tables it reports on rather than
/// `PRAGMA data_version`: **every** command in this family moves that counter,
/// because `StateDb::open` runs the migration framework (spec 02 §4.1's
/// open → migrate → serve) and its bookkeeping is a write — measured, not
/// assumed, by watching `project list` move it too.
#[test]
fn coverage_writes_nothing() {
    let (home, layout) = open_layout();
    let target = home.join("repo");
    std::fs::create_dir_all(&target).expect("create target");
    std::fs::write(target.join("a.rs"), b"fn a() {}\n").expect("write a.rs");
    std::fs::write(target.join("notes.md"), b"# hi\n").expect("write notes.md");

    let add = run_cli(&home, &["project", "add", target.to_str().unwrap()]);
    assert_eq!(add.status.code(), Some(0), "{add:?}");
    let worktree_id = {
        let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
        let conn = db.open_read().expect("read");
        local_rag_store::all_worktree_ids(&conn).expect("ids")[0].clone()
    };
    tokio_test_block_on(seed_active_generation(
        &layout,
        &worktree_id,
        "019f3333-3333-7333-8333-333333333333",
        &["a.rs"],
        &[],
    ));

    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    let before = index_state_digest(&db);

    let out = run_cli(&home, &["project", "coverage", target.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(stdout(&out).contains("NEITHER table"), "{out:?}");

    let after = index_state_digest(&db);
    assert_eq!(
        before, after,
        "`project coverage` changed the index state it reports on"
    );
}

/// Every row of the tables a coverage report reads, as one comparable string.
fn index_state_digest(db: &StateDb) -> String {
    let conn = db.open_read().expect("read");
    let mut out = String::new();
    for sql in [
        "SELECT generation_id, worktree_id, generation_number, state FROM generation \
         ORDER BY generation_id",
        "SELECT generation_id, normalized_path, display_path, file_revision_id \
         FROM generation_file ORDER BY generation_id, normalized_path",
        "SELECT generation_id, normalized_path, reason FROM skipped_file \
         ORDER BY generation_id, normalized_path",
        "SELECT worktree_id, enabled FROM managed_worktree ORDER BY worktree_id",
    ] {
        let mut stmt = conn.prepare(sql).expect("prepare");
        let mut rows = stmt.query([]).expect("query");
        while let Some(row) = rows.next().expect("row") {
            for i in 0..row.as_ref().column_count() {
                out.push_str(&format!("{:?}|", row.get_ref(i).expect("column")));
            }
            out.push('\n');
        }
        out.push_str("--\n");
    }
    out
}

/// A registered-but-never-indexed worktree is a bootstrap state, not a fault:
/// the command says what is true and exits zero, the same way `doctor` refuses
/// to call "never indexed" a problem.
#[test]
fn coverage_without_an_active_generation_is_not_an_error() {
    let (home, _layout) = open_layout();
    let target = home.join("repo");
    std::fs::create_dir_all(&target).expect("create target");
    std::fs::write(target.join("a.rs"), b"fn a() {}\n").expect("write a.rs");

    let add = run_cli(&home, &["project", "add", target.to_str().unwrap()]);
    assert_eq!(add.status.code(), Some(0), "{add:?}");

    let out = run_cli(&home, &["project", "coverage", target.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(
        stdout(&out).contains("no active generation — nothing is indexed yet"),
        "{out:?}"
    );
}

/// The JSON shape is machine-readable and carries both halves, so a script can
/// gate on the gap rather than grepping prose.
#[test]
fn coverage_json_carries_both_halves() {
    let (home, layout) = open_layout();
    let target = home.join("repo");
    std::fs::create_dir_all(&target).expect("create target");
    std::fs::write(target.join("a.rs"), b"fn a() {}\n").expect("write a.rs");
    std::fs::write(target.join("notes.md"), b"# hi\n").expect("write notes.md");
    std::fs::write(target.join("more.md"), b"# hi\n").expect("write more.md");

    let add = run_cli(&home, &["project", "add", target.to_str().unwrap()]);
    assert_eq!(add.status.code(), Some(0), "{add:?}");
    let worktree_id = {
        let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
        let conn = db.open_read().expect("read");
        local_rag_store::all_worktree_ids(&conn).expect("ids")[0].clone()
    };
    tokio_test_block_on(seed_active_generation(
        &layout,
        &worktree_id,
        "019f4444-4444-7444-8444-444444444444",
        &["a.rs"],
        &[],
    ));

    let out = run_cli(
        &home,
        &["project", "coverage", target.to_str().unwrap(), "--json"],
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid JSON");
    assert_eq!(v["indexed_files"], 1);
    assert_eq!(v["skipped_files"], 0);
    assert_eq!(v["unaccounted_files"], 2);
    assert_eq!(v["unaccounted_by_extension"][0]["extension"], "md");
    assert_eq!(v["unaccounted_by_extension"][0]["files"], 2);
    assert_eq!(v["active_generation_number"], 1);
}

/// `project list` is the command a user runs when they suspect indexing is
/// wrong, and until `D-096` it answered only "how old". Now the same line
/// answers "how much", in the wording `doctor` and `coverage` share.
#[test]
fn list_reports_the_coverage_of_the_generation_it_serves() {
    let (home, layout) = open_layout();
    let target = home.join("repo");
    std::fs::create_dir_all(&target).expect("create target");
    std::fs::write(target.join("a.rs"), b"fn a() {}\n").expect("write a.rs");

    let add = run_cli(&home, &["project", "add", target.to_str().unwrap()]);
    assert_eq!(add.status.code(), Some(0), "{add:?}");
    let worktree_id = {
        let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
        let conn = db.open_read().expect("read");
        local_rag_store::all_worktree_ids(&conn).expect("ids")[0].clone()
    };
    tokio_test_block_on(seed_active_generation(
        &layout,
        &worktree_id,
        "019f5555-5555-7555-8555-555555555555",
        &["a.rs", "b.rs"],
        &[
            ("c.rs", local_rag_store::SkipReason::Secret),
            ("d.rs", local_rag_store::SkipReason::Secret),
            ("e.png", local_rag_store::SkipReason::Binary),
        ],
    ));

    let list = run_cli(&home, &["project", "list"]);
    assert_eq!(list.status.code(), Some(0), "{list:?}");
    let text = stdout(&list);
    assert!(text.contains("active=#1"), "{text}");
    assert!(
        text.contains("2 indexed, 3 skipped (2 secret, 1 binary)"),
        "{text}"
    );

    let json_out = run_cli(&home, &["project", "list", "--json"]);
    let json: serde_json::Value =
        serde_json::from_str(&stdout(&json_out)).expect("list --json is valid JSON");
    let row = &json["projects"][0];
    assert_eq!(row["indexed_files"], 2);
    assert_eq!(row["skipped_files"], 3);
    assert_eq!(row["skipped_by_reason"]["secret"], 2);
    assert_eq!(row["skipped_by_reason"]["binary"], 1);
}
