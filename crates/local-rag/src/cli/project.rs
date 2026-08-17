//! `local-rag project add|remove|enable|disable|list|status|reindex`
//! (spec 11 §6 `[SPEC surface]` + §8, T20-08) — the CLI half of daemon-managed
//! indexing (spec 03 §2.1's `managed_worktree` table, T20-01; the supervisor,
//! T20-05/T20-06; the `admin/*` verbs, T20-07).
//!
//! Every write here (`add`/`remove`/`enable`/`disable`) goes straight into
//! `state.sqlite` — the same no-`store.lock`, direct-`StateDb` access every
//! other command in this module tree already uses (`cli/mod.rs:23-31`) — so
//! this whole family works **without a live daemon**. A live daemon is only
//! ever *notified*, best-effort, via `admin/projects_reload`
//! ([`notify_daemon`]); it re-reads the table on its own slow backstop poll
//! regardless (spec 11 §8's "notify is a hint, the table is truth"). `status`
//! and `reindex` are the two verbs that genuinely need a live daemon
//! ([`call_admin`](local_rag::daemon::call_admin), T20-07) — both degrade to
//! an explicit "not running" answer rather than spawning one.

use std::path::PathBuf;
use std::process::ExitCode;

use local_rag::daemon::gitroot;
use local_rag::indexing::{open_state, register_new_managed_worktree, resolve_facts};
use local_rag_core::identity::{SystemUuidV7, Uuid, UuidSource};
use local_rag_core::paths::StoreLayout;
use local_rag_store::{
    Resolution, StateDb, WorktreeIndexingStatus, WorktreeRootFacts, all_worktree_ids,
    current_worktree_path, generation_meta_for_worktree, indexing_status, managed_worktrees,
    register_managed_worktree, set_managed_enabled, unregister_managed_worktree,
};

use super::freshness::{IndexFreshness, humanize_age};
use super::index::print_ambiguous;
use super::{block_on, fail, resolve_layout_and_config, system_now_ms};

const BIN: &str = "local-rag";

/// How long a `project status`/`project reindex` waits for the daemon's
/// `admin/*` answer. Chosen, not derived — the same class of one-shot admin
/// round trip `local-rag-tui::admin_client::CYCLE_TIMEOUT` already budgets
/// 2s for, and the same "picked and documented as chosen" precedent
/// `LIVENESS_PROBE_TIMEOUT_MS`/`MAX_CONCURRENT_STARTUP_RECONCILES` set.
#[cfg(unix)]
const ADMIN_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Debug, clap::Subcommand)]
pub enum ProjectCommand {
    /// Resolve (registering the worktree if it is new) and mark it managed.
    Add {
        /// Directory to manage.
        path: String,
    },
    /// Stop managing a worktree — the index itself is untouched.
    Remove {
        /// Directory to unmanage.
        path: String,
    },
    /// Resume background indexing for an already-enrolled project.
    Enable {
        /// Directory of the managed project.
        path: String,
    },
    /// Pause background indexing for an already-enrolled project.
    Disable {
        /// Directory of the managed project.
        path: String,
    },
    /// List every enrolled project (durable state only, no daemon required).
    List {
        /// Print the list as JSON instead of human-readable lines.
        #[arg(long)]
        json: bool,
    },
    /// Durable state plus the live supervisor's own view, if a daemon is running.
    Status {
        /// Print the status report as JSON instead of human-readable lines.
        #[arg(long)]
        json: bool,
    },
    /// Force an immediate reconcile for a managed project via the live daemon.
    Reindex {
        /// Directory to reconcile (defaults to the current directory).
        path: Option<String>,
    },
}

pub fn run(command: ProjectCommand) -> ExitCode {
    match command {
        ProjectCommand::Add { path } => run_add(path),
        ProjectCommand::Remove { path } => run_remove(path),
        ProjectCommand::Enable { path } => run_toggle(path, true),
        ProjectCommand::Disable { path } => run_toggle(path, false),
        ProjectCommand::List { json } => run_list(json),
        ProjectCommand::Status { json } => run_status(json),
        ProjectCommand::Reindex { path } => run_reindex(path),
    }
}

/// Best-effort `admin/projects_reload` — never surfaced to the caller, in
/// either direction: `Unreachable` (no daemon running) is the common case,
/// not a failure, and any other outcome is not this CLI's job to report. The
/// durable `managed_worktree` write this always follows is already the
/// source of truth (spec 11 §8); a live daemon re-reads it on its own
/// backstop poll even if this notification is lost entirely.
#[cfg(unix)]
fn notify_daemon(layout: &StoreLayout) {
    let _ = local_rag::daemon::call_admin(
        &layout.socket_path(),
        ADMIN_CALL_TIMEOUT,
        "admin/projects_reload",
        None,
    );
}

#[cfg(not(unix))]
fn notify_daemon(_layout: &StoreLayout) {}

/// `gitroot::probe` (an inaccessible directory is a typed refusal) then
/// `resolve_facts` — the same two fallible steps `add`/`remove`/`enable`/
/// `disable` all start with, differing only in how they react to the
/// resulting [`Resolution`].
fn probe_and_resolve(
    layout: &StoreLayout,
    path: &str,
) -> Result<(std::sync::Arc<StateDb>, WorktreeRootFacts, Resolution), ExitCode> {
    let target = PathBuf::from(path);
    let Some(facts) = gitroot::probe(&target) else {
        return Err(fail(BIN, &format!("{path}: not an accessible directory")));
    };
    let state = open_state(layout).map_err(|e| fail(BIN, &e))?;
    let resolution = resolve_facts(&state, &facts).map_err(|e| fail(BIN, &e))?;
    Ok((state, facts, resolution))
}

fn run_add(path: String) -> ExitCode {
    let (layout, _config) = match resolve_layout_and_config() {
        Ok(v) => v,
        Err(e) => return fail(BIN, &e),
    };
    let (state, facts, resolution) = match probe_and_resolve(&layout, &path) {
        Ok(v) => v,
        Err(code) => return code,
    };
    let now_ms = system_now_ms();

    let worktree_id = match resolution {
        Resolution::Resolved { worktree_id, .. } => {
            let Ok(id) = worktree_id.parse::<Uuid>() else {
                return fail(BIN, "internal error: stored worktree id is not a UUID");
            };
            let id_str = id.to_string();
            if let Err(e) = block_on(
                state
                    .writer()
                    .transaction(move |tx| register_managed_worktree(tx, &id_str, now_ms)),
            ) {
                return fail(BIN, &format!("could not enroll worktree: {e}"));
            }
            id
        }
        Resolution::GlobalOnly => {
            let repo_id = SystemUuidV7.next_uuid();
            let worktree_id = SystemUuidV7.next_uuid();
            if let Err(e) = block_on(register_new_managed_worktree(
                &state,
                repo_id,
                worktree_id,
                &facts,
                now_ms,
            )) {
                return fail(BIN, &format!("could not register the worktree: {e}"));
            }
            worktree_id
        }
        Resolution::Ambiguous { candidates } => {
            print_ambiguous(&candidates);
            return ExitCode::FAILURE;
        }
    };

    println!("{BIN}: managing worktree {worktree_id} at {path}");
    notify_daemon(&layout);
    ExitCode::SUCCESS
}

fn run_remove(path: String) -> ExitCode {
    let (layout, _config) = match resolve_layout_and_config() {
        Ok(v) => v,
        Err(e) => return fail(BIN, &e),
    };
    let (state, _facts, resolution) = match probe_and_resolve(&layout, &path) {
        Ok(v) => v,
        Err(code) => return code,
    };

    let worktree_id = match resolution {
        Resolution::Resolved { worktree_id, .. } => worktree_id,
        Resolution::GlobalOnly => {
            return fail(
                BIN,
                &format!("{path}: not a known worktree; nothing to unmanage"),
            );
        }
        Resolution::Ambiguous { candidates } => {
            print_ambiguous(&candidates);
            return ExitCode::FAILURE;
        }
    };

    let id_for_tx = worktree_id.clone();
    let removed = match block_on(
        state
            .writer()
            .transaction(move |tx| unregister_managed_worktree(tx, &id_for_tx)),
    ) {
        Ok(removed) => removed,
        Err(e) => return fail(BIN, &format!("could not unmanage worktree: {e}")),
    };

    if removed {
        println!("{BIN}: no longer managing worktree {worktree_id} at {path}");
    } else {
        println!("{BIN}: worktree {worktree_id} at {path} is not managed (nothing to do)");
    }
    notify_daemon(&layout);
    ExitCode::SUCCESS
}

fn run_toggle(path: String, enabled: bool) -> ExitCode {
    let (layout, _config) = match resolve_layout_and_config() {
        Ok(v) => v,
        Err(e) => return fail(BIN, &e),
    };
    let (state, _facts, resolution) = match probe_and_resolve(&layout, &path) {
        Ok(v) => v,
        Err(code) => return code,
    };

    let worktree_id = match resolution {
        Resolution::Resolved { worktree_id, .. } => worktree_id,
        Resolution::GlobalOnly => {
            return fail(BIN, &format!("{path}: not a known worktree"));
        }
        Resolution::Ambiguous { candidates } => {
            print_ambiguous(&candidates);
            return ExitCode::FAILURE;
        }
    };

    let now_ms = system_now_ms();
    let id_for_tx = worktree_id.clone();
    let matched = match block_on(
        state
            .writer()
            .transaction(move |tx| set_managed_enabled(tx, &id_for_tx, enabled, now_ms)),
    ) {
        Ok(matched) => matched,
        Err(e) => return fail(BIN, &format!("could not update worktree: {e}")),
    };

    if !matched {
        return fail(
            BIN,
            &format!("worktree {worktree_id} at {path} is not a managed project"),
        );
    }

    let verb = if enabled { "enabled" } else { "disabled" };
    println!("{BIN}: {verb} worktree {worktree_id} at {path}");
    notify_daemon(&layout);
    ExitCode::SUCCESS
}

/// One durable row, joined with a human-readable path — the shape `list`
/// prints and `status` extends with a live `task`.
struct ProjectRow {
    worktree_id: String,
    enabled: bool,
    registered_at: i64,
    updated_at: i64,
    path: String,
    /// X-008: what this worktree's generation history says about its index —
    /// the age of what search actually serves, and whether newer work is built
    /// but stuck.
    freshness: IndexFreshness,
    /// X-008: the durable outcome of the last background cycle (X-006). Present
    /// even with no daemon running, which is the whole point of persisting it.
    indexing: Option<WorktreeIndexingStatus>,
}

impl ProjectRow {
    /// The per-row suffix both `list` and `status` append: index age, last
    /// durable success, failure streak, and a loud marker for stuck work.
    fn freshness_suffix(&self, now_ms: i64) -> String {
        let mut out = String::new();
        match &self.freshness.active {
            Some((_, number, created_ms)) => {
                out.push_str(&format!("  active=#{number}"));
                if let Some(ms) = created_ms {
                    out.push_str(&format!(" built {}", humanize_age(now_ms, *ms)));
                }
            }
            None => out.push_str("  active=(none — nothing indexed yet)"),
        }
        if let Some(status) = &self.indexing {
            if let Some(ms) = status.last_success_at {
                out.push_str(&format!("  last_success={}", humanize_age(now_ms, ms)));
            }
            if status.consecutive_failures > 0 {
                out.push_str(&format!("  failures={}", status.consecutive_failures));
            }
        }
        if !self.freshness.stuck_newer.is_empty() {
            let list: Vec<String> = self
                .freshness
                .stuck_newer
                .iter()
                .map(|s| format!("#{} {}", s.generation_number, s.state.as_str()))
                .collect();
            out.push_str(&format!(
                "  [STUCK: {} generation(s) newer than active, built but not serving: {}]",
                self.freshness.stuck_newer.len(),
                list.join(", "),
            ));
        }
        out
    }
}

/// How many registered worktrees are **not** enrolled in background indexing.
///
/// The number every empty-registry message needs: "no managed projects" alone
/// reads like a broken command, whereas "no managed projects, and here are the
/// N known worktrees none of which is enrolled" names the actual situation.
fn unenrolled_worktree_count(conn: &rusqlite::Connection) -> Result<usize, String> {
    let all = all_worktree_ids(conn).map_err(|e| format!("could not list worktrees: {e}"))?;
    let managed = managed_worktrees(conn).map_err(|e| format!("could not list projects: {e}"))?;
    Ok(all.len().saturating_sub(managed.len()))
}

/// The one line printed whenever background indexing is off for something —
/// kept in one place so `list` and `status` cannot drift apart on the wording,
/// and so the exact command to fix it always travels with the diagnosis.
fn print_enrollment_hint(count: usize) {
    if count == 0 {
        return;
    }
    println!(
        "{BIN}: {count} registered worktree(s) are NOT enrolled — background indexing does \
         nothing for them; run `local-rag project add <path>` to enroll one"
    );
}

fn durable_rows(conn: &rusqlite::Connection) -> Result<Vec<ProjectRow>, String> {
    let managed = managed_worktrees(conn).map_err(|e| format!("could not list projects: {e}"))?;
    managed
        .into_iter()
        .map(|row| {
            let path = current_worktree_path(conn, &row.worktree_id)
                .map_err(|e| format!("could not read {}'s current path: {e}", row.worktree_id))?
                .unwrap_or_else(|| "(no current path)".to_string());
            let generations = generation_meta_for_worktree(conn, &row.worktree_id)
                .map_err(|e| format!("could not read {}'s generations: {e}", row.worktree_id))?;
            let indexing = indexing_status(conn, &row.worktree_id).map_err(|e| {
                format!("could not read {}'s indexing status: {e}", row.worktree_id)
            })?;
            Ok(ProjectRow {
                worktree_id: row.worktree_id,
                enabled: row.enabled,
                registered_at: row.registered_at,
                updated_at: row.updated_at,
                path,
                freshness: IndexFreshness::from_generations(&generations),
                indexing,
            })
        })
        .collect()
}

fn run_list(json: bool) -> ExitCode {
    let (layout, _config) = match resolve_layout_and_config() {
        Ok(v) => v,
        Err(e) => return fail(BIN, &e),
    };
    let state = match open_state(&layout) {
        Ok(s) => s,
        Err(e) => return fail(BIN, &e),
    };
    let conn = match state.open_read() {
        Ok(c) => c,
        Err(e) => return fail(BIN, &format!("could not open state.sqlite: {e}")),
    };
    let rows = match durable_rows(&conn) {
        Ok(r) => r,
        Err(e) => return fail(BIN, &e),
    };

    let unenrolled = match unenrolled_worktree_count(&conn) {
        Ok(n) => n,
        Err(e) => return fail(BIN, &e),
    };
    let now_ms = system_now_ms();

    if json {
        let projects: Vec<serde_json::Value> = rows.iter().map(row_json).collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "projects": projects,
                "unenrolled_worktrees": unenrolled,
            }))
            .expect("project list always serializes")
        );
        return ExitCode::SUCCESS;
    }

    if rows.is_empty() {
        println!("{BIN}: no managed projects");
        print_enrollment_hint(unenrolled);
        return ExitCode::SUCCESS;
    }
    for r in &rows {
        println!(
            "{}  enabled={}  {}{}",
            r.worktree_id,
            r.enabled,
            r.path,
            r.freshness_suffix(now_ms),
        );
    }
    print_enrollment_hint(unenrolled);
    ExitCode::SUCCESS
}

/// The JSON shape of one row, shared by `list` and `status`. X-008 fields are
/// added alongside the T20-08 ones; no existing key is renamed or removed.
fn row_json(r: &ProjectRow) -> serde_json::Value {
    serde_json::json!({
        "worktree_id": r.worktree_id,
        "enabled": r.enabled,
        "registered_at": r.registered_at,
        "updated_at": r.updated_at,
        "path": r.path,
        "active_generation_number": r.freshness.active.as_ref().map(|(_, n, _)| *n),
        "active_generation_created_at": r.freshness.active.as_ref().and_then(|(_, _, ms)| *ms),
        "last_success_at": r.indexing.as_ref().and_then(|s| s.last_success_at),
        "last_attempt_at": r.indexing.as_ref().and_then(|s| s.last_attempt_at),
        "consecutive_failures": r.indexing.as_ref().map(|s| s.consecutive_failures),
        "last_error": r.indexing.as_ref().and_then(|s| s.last_error.clone()),
        "stuck_generations": r
            .freshness
            .stuck_newer
            .iter()
            .map(|s| serde_json::json!({
                "generation_id": s.generation_id,
                "generation_number": s.generation_number,
                "state": s.state.as_str(),
            }))
            .collect::<Vec<_>>(),
    })
}

/// The three states a live daemon answer can put `project status` in.
enum DaemonState {
    NotRunning,
    MigrationOnly,
    Running { projects: serde_json::Value },
}

#[cfg(unix)]
fn probe_daemon(layout: &StoreLayout) -> Result<DaemonState, String> {
    match local_rag::daemon::call_admin(
        &layout.socket_path(),
        ADMIN_CALL_TIMEOUT,
        "admin/projects_list",
        None,
    ) {
        Ok(value) if value["available"] == serde_json::Value::Bool(false) => {
            Ok(DaemonState::MigrationOnly)
        }
        Ok(value) => Ok(DaemonState::Running {
            projects: value["projects"].clone(),
        }),
        Err(local_rag::daemon::CallAdminError::Unreachable) => Ok(DaemonState::NotRunning),
        Err(e) => Err(format!("could not reach the daemon: {e}")),
    }
}

#[cfg(not(unix))]
fn probe_daemon(_layout: &StoreLayout) -> Result<DaemonState, String> {
    Ok(DaemonState::NotRunning)
}

fn live_task_for<'a>(projects: &'a serde_json::Value, worktree_id: &str) -> &'a serde_json::Value {
    static NULL: serde_json::Value = serde_json::Value::Null;
    projects
        .as_array()
        .and_then(|arr| arr.iter().find(|p| p["worktree_id"] == worktree_id))
        .map(|p| &p["task"])
        .unwrap_or(&NULL)
}

fn run_status(json: bool) -> ExitCode {
    let (layout, _config) = match resolve_layout_and_config() {
        Ok(v) => v,
        Err(e) => return fail(BIN, &e),
    };
    let state = match open_state(&layout) {
        Ok(s) => s,
        Err(e) => return fail(BIN, &e),
    };
    let conn = match state.open_read() {
        Ok(c) => c,
        Err(e) => return fail(BIN, &format!("could not open state.sqlite: {e}")),
    };
    let rows = match durable_rows(&conn) {
        Ok(r) => r,
        Err(e) => return fail(BIN, &e),
    };
    let daemon_state = match probe_daemon(&layout) {
        Ok(s) => s,
        Err(e) => return fail(BIN, &e),
    };

    let (daemon_label, live_projects) = match &daemon_state {
        DaemonState::NotRunning => ("not_running", None),
        DaemonState::MigrationOnly => ("migration_only", None),
        DaemonState::Running { projects } => ("running", Some(projects)),
    };

    let unenrolled = match unenrolled_worktree_count(&conn) {
        Ok(n) => n,
        Err(e) => return fail(BIN, &e),
    };
    // X-008: whether *this* directory is enrolled is the question a human
    // standing in a project actually has, and no verb answered it before.
    let here = current_directory_enrollment(&layout, &rows);
    let now_ms = system_now_ms();

    if json {
        let projects: Vec<serde_json::Value> = rows
            .iter()
            .map(|r| {
                let task = live_projects
                    .map(|p| live_task_for(p, &r.worktree_id).clone())
                    .unwrap_or(serde_json::Value::Null);
                let mut value = row_json(r);
                value["task"] = task;
                value
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "daemon": daemon_label,
                "projects": projects,
                "unenrolled_worktrees": unenrolled,
                "current_directory": match &here {
                    CurrentDirectory::Managed { worktree_id } =>
                        serde_json::json!({ "managed": true, "worktree_id": worktree_id }),
                    CurrentDirectory::KnownButUnmanaged { worktree_id } =>
                        serde_json::json!({ "managed": false, "worktree_id": worktree_id }),
                    CurrentDirectory::NotAWorktree =>
                        serde_json::json!({ "managed": false, "worktree_id": null }),
                },
            }))
            .expect("project status always serializes")
        );
        return ExitCode::SUCCESS;
    }

    println!("{BIN}: daemon {daemon_label}");
    if rows.is_empty() {
        println!("{BIN}: no managed projects");
    }
    for r in &rows {
        let task = live_projects.map(|p| live_task_for(p, &r.worktree_id));
        let suffix = r.freshness_suffix(now_ms);
        match task {
            Some(t) if !t.is_null() => {
                println!(
                    "{}  enabled={}  {}{}  in_progress_since={}",
                    r.worktree_id, r.enabled, r.path, suffix, t["in_progress_since"],
                );
            }
            _ => {
                println!(
                    "{}  enabled={}  {}{}",
                    r.worktree_id, r.enabled, r.path, suffix,
                );
            }
        }
    }
    match &here {
        CurrentDirectory::Managed { .. } => {}
        CurrentDirectory::KnownButUnmanaged { worktree_id } => println!(
            "{BIN}: background indexing is OFF for this worktree ({worktree_id}) — \
             run `local-rag project add .` to enroll it"
        ),
        CurrentDirectory::NotAWorktree => println!(
            "{BIN}: this directory is not a registered worktree — background indexing is OFF; \
             run `local-rag project add .` to index it"
        ),
    }
    print_enrollment_hint(unenrolled);
    ExitCode::SUCCESS
}

/// Where the process's current directory stands relative to the managed
/// registry.
enum CurrentDirectory {
    Managed { worktree_id: String },
    KnownButUnmanaged { worktree_id: String },
    NotAWorktree,
}

/// Resolve the current directory through the same `gitroot::probe` →
/// `resolve_facts` pair every write verb uses, then classify it against the
/// rows already read.
///
/// Never fails the command: this is an extra diagnostic line, so an
/// inaccessible directory or an ambiguous resolution simply reports
/// [`CurrentDirectory::NotAWorktree`] rather than turning a successful `status`
/// into an error.
fn current_directory_enrollment(layout: &StoreLayout, rows: &[ProjectRow]) -> CurrentDirectory {
    let Ok((_state, _facts, resolution)) = probe_and_resolve(layout, ".") else {
        return CurrentDirectory::NotAWorktree;
    };
    let Resolution::Resolved { worktree_id, .. } = resolution else {
        return CurrentDirectory::NotAWorktree;
    };
    if rows.iter().any(|r| r.worktree_id == worktree_id) {
        CurrentDirectory::Managed { worktree_id }
    } else {
        CurrentDirectory::KnownButUnmanaged { worktree_id }
    }
}

fn run_reindex(path: Option<String>) -> ExitCode {
    let (layout, _config) = match resolve_layout_and_config() {
        Ok(v) => v,
        Err(e) => return fail(BIN, &e),
    };
    let target = match path.clone() {
        Some(p) => p,
        None => match std::env::current_dir() {
            Ok(cwd) => cwd.display().to_string(),
            Err(e) => {
                return fail(
                    BIN,
                    &format!("could not determine the current directory: {e}"),
                );
            }
        },
    };
    let (_state, _facts, resolution) = match probe_and_resolve(&layout, &target) {
        Ok(v) => v,
        Err(code) => return code,
    };

    let worktree_id = match resolution {
        Resolution::Resolved { worktree_id, .. } => worktree_id,
        Resolution::GlobalOnly => {
            return fail(
                BIN,
                &format!("{target}: not indexed yet; run `local-rag index {target}` first"),
            );
        }
        Resolution::Ambiguous { candidates } => {
            print_ambiguous(&candidates);
            return ExitCode::FAILURE;
        }
    };

    match reconcile_now(&layout, &worktree_id) {
        Ok(()) => {
            println!("{BIN}: reconcile triggered for worktree {worktree_id}");
            ExitCode::SUCCESS
        }
        Err(ReconcileNowFailure::NotRunning) => fail(
            BIN,
            "the daemon is not running; run `local-rag reindex` instead",
        ),
        Err(ReconcileNowFailure::NotManaged) => fail(
            BIN,
            &format!(
                "worktree {worktree_id} is not currently managed by the daemon; run \
                 `local-rag project add {target}` first"
            ),
        ),
        Err(ReconcileNowFailure::Other(msg)) => fail(BIN, &msg),
    }
}

enum ReconcileNowFailure {
    NotRunning,
    NotManaged,
    Other(String),
}

#[cfg(unix)]
fn reconcile_now(layout: &StoreLayout, worktree_id: &str) -> Result<(), ReconcileNowFailure> {
    let params = serde_json::json!({ "worktree_id": worktree_id });
    match local_rag::daemon::call_admin(
        &layout.socket_path(),
        ADMIN_CALL_TIMEOUT,
        "admin/reconcile_now",
        Some(params),
    ) {
        Ok(_) => Ok(()),
        Err(local_rag::daemon::CallAdminError::Unreachable) => Err(ReconcileNowFailure::NotRunning),
        Err(local_rag::daemon::CallAdminError::JsonRpcError { code: -32602, .. }) => {
            Err(ReconcileNowFailure::NotManaged)
        }
        Err(e) => Err(ReconcileNowFailure::Other(e.to_string())),
    }
}

#[cfg(not(unix))]
fn reconcile_now(_layout: &StoreLayout, _worktree_id: &str) -> Result<(), ReconcileNowFailure> {
    Err(ReconcileNowFailure::NotRunning)
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use crate::cli::Cli;

    fn parse(args: &[&str]) -> super::ProjectCommand {
        let mut full = vec!["local-rag"];
        full.extend_from_slice(args);
        match Cli::try_parse_from(full).expect("valid arguments").command {
            crate::cli::Command::Project { command } => command,
            other => panic!("expected Command::Project, got {other:?}"),
        }
    }

    #[test]
    fn add_parses_its_path() {
        match parse(&["project", "add", "/tmp/repo"]) {
            super::ProjectCommand::Add { path } => assert_eq!(path, "/tmp/repo"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn remove_parses_its_path() {
        match parse(&["project", "remove", "/tmp/repo"]) {
            super::ProjectCommand::Remove { path } => assert_eq!(path, "/tmp/repo"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn enable_parses_its_path() {
        match parse(&["project", "enable", "/tmp/repo"]) {
            super::ProjectCommand::Enable { path } => assert_eq!(path, "/tmp/repo"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn disable_parses_its_path() {
        match parse(&["project", "disable", "/tmp/repo"]) {
            super::ProjectCommand::Disable { path } => assert_eq!(path, "/tmp/repo"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn list_parses_the_json_flag() {
        match parse(&["project", "list", "--json"]) {
            super::ProjectCommand::List { json } => assert!(json),
            other => panic!("{other:?}"),
        }
        match parse(&["project", "list"]) {
            super::ProjectCommand::List { json } => assert!(!json),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn status_parses_the_json_flag() {
        match parse(&["project", "status", "--json"]) {
            super::ProjectCommand::Status { json } => assert!(json),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn reindex_path_is_optional() {
        match parse(&["project", "reindex"]) {
            super::ProjectCommand::Reindex { path } => assert_eq!(path, None),
            other => panic!("{other:?}"),
        }
        match parse(&["project", "reindex", "/tmp/repo"]) {
            super::ProjectCommand::Reindex { path } => {
                assert_eq!(path.as_deref(), Some("/tmp/repo"))
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn add_rejects_an_unknown_flag() {
        let result = Cli::try_parse_from(["local-rag", "project", "add", "--bogus"]);
        assert!(result.is_err(), "{result:?}");
    }
}
