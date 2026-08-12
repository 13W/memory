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
    Resolution, StateDb, WorktreeRootFacts, current_worktree_path, managed_worktrees,
    register_managed_worktree, set_managed_enabled, unregister_managed_worktree,
};

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
}

fn durable_rows(conn: &rusqlite::Connection) -> Result<Vec<ProjectRow>, String> {
    let managed = managed_worktrees(conn).map_err(|e| format!("could not list projects: {e}"))?;
    managed
        .into_iter()
        .map(|row| {
            let path = current_worktree_path(conn, &row.worktree_id)
                .map_err(|e| format!("could not read {}'s current path: {e}", row.worktree_id))?
                .unwrap_or_else(|| "(no current path)".to_string());
            Ok(ProjectRow {
                worktree_id: row.worktree_id,
                enabled: row.enabled,
                registered_at: row.registered_at,
                updated_at: row.updated_at,
                path,
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

    if json {
        let projects: Vec<serde_json::Value> = rows
            .iter()
            .map(|r| {
                serde_json::json!({
                    "worktree_id": r.worktree_id,
                    "enabled": r.enabled,
                    "registered_at": r.registered_at,
                    "updated_at": r.updated_at,
                    "path": r.path,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "projects": projects }))
                .expect("project list always serializes")
        );
        return ExitCode::SUCCESS;
    }

    if rows.is_empty() {
        println!("{BIN}: no managed projects");
        return ExitCode::SUCCESS;
    }
    for r in &rows {
        println!("{}  enabled={}  {}", r.worktree_id, r.enabled, r.path);
    }
    ExitCode::SUCCESS
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

    if json {
        let projects: Vec<serde_json::Value> = rows
            .iter()
            .map(|r| {
                let task = live_projects
                    .map(|p| live_task_for(p, &r.worktree_id).clone())
                    .unwrap_or(serde_json::Value::Null);
                serde_json::json!({
                    "worktree_id": r.worktree_id,
                    "enabled": r.enabled,
                    "registered_at": r.registered_at,
                    "updated_at": r.updated_at,
                    "path": r.path,
                    "task": task,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "daemon": daemon_label,
                "projects": projects,
            }))
            .expect("project status always serializes")
        );
        return ExitCode::SUCCESS;
    }

    println!("{BIN}: daemon {daemon_label}");
    if rows.is_empty() {
        println!("{BIN}: no managed projects");
        return ExitCode::SUCCESS;
    }
    for r in &rows {
        let task = live_projects.map(|p| live_task_for(p, &r.worktree_id));
        match task {
            Some(t) if !t.is_null() => {
                println!(
                    "{}  enabled={}  {}  last_generation={}  in_progress_since={}",
                    r.worktree_id,
                    r.enabled,
                    r.path,
                    t["last_generation_id"],
                    t["in_progress_since"],
                );
            }
            _ => {
                println!("{}  enabled={}  {}", r.worktree_id, r.enabled, r.path);
            }
        }
    }
    ExitCode::SUCCESS
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
