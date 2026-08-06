//! `local-rag worktree list` (spec 11 §6).

use std::process::ExitCode;

use local_rag_store::{all_worktree_ids, current_worktree_path, worktree_summary};

use super::index::open_state;
use super::{fail, resolve_layout_and_config};

const BIN: &str = "local-rag";

#[derive(Debug, clap::Subcommand)]
pub enum WorktreeCommand {
    /// List every registered worktree.
    List,
}

pub fn run(command: WorktreeCommand) -> ExitCode {
    match command {
        WorktreeCommand::List => run_list(),
    }
}

fn run_list() -> ExitCode {
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

    let ids = match all_worktree_ids(&conn) {
        Ok(ids) => ids,
        Err(e) => return fail(BIN, &format!("could not list worktrees: {e}")),
    };
    if ids.is_empty() {
        println!("{BIN}: no worktrees registered yet");
        return ExitCode::SUCCESS;
    }
    for worktree_id in ids {
        let summary = match worktree_summary(&conn, &worktree_id) {
            Ok(Some(s)) => s,
            Ok(None) => continue, // deleted between the id listing and here
            Err(e) => {
                return fail(BIN, &format!("could not read worktree {worktree_id}: {e}"));
            }
        };
        let path = match current_worktree_path(&conn, &worktree_id) {
            Ok(Some(p)) => p,
            Ok(None) => "(no current path)".to_string(),
            Err(e) => {
                return fail(
                    BIN,
                    &format!("could not read {worktree_id}'s current path: {e}"),
                );
            }
        };
        println!(
            "{worktree_id}  repo {}  {}  {}  {path}",
            summary.repo_id,
            summary.kind.as_str(),
            summary.state.as_str(),
        );
    }
    ExitCode::SUCCESS
}
