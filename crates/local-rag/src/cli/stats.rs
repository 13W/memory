//! `local-rag stats [--json]` (spec 11 §6, D-025) — CLI mirror of the MCP
//! `stats` tool (`daemon/mcp/memory.rs`, T15-04): the same store-crate calls,
//! against this command's own opened `state.sqlite`/`cache.sqlite`, with no
//! `MemoryContext` (that type is daemon-only — built once at daemon startup,
//! not meaningful for a one-shot CLI invocation).

use std::process::ExitCode;

use local_rag_memory::recall as recall_pipeline;
use local_rag_store::{
    RequestRoot, Resolution, memory_entry_counts, pending_candidate_counts, projection_state,
    resolve, store_instance_uuid,
};

use local_rag::daemon::gitroot;

use super::{block_on, fail, resolve_layout_and_config};
use local_rag::indexing::{open_cache, open_state};

const BIN: &str = "local-rag";

#[derive(Debug, clap::Args)]
pub struct StatsArgs {
    /// Print the stats report as JSON instead of human-readable lines.
    #[arg(long)]
    json: bool,
}

pub fn run(args: StatsArgs) -> ExitCode {
    let json = args.json;

    let (layout, _config) = match resolve_layout_and_config() {
        Ok(v) => v,
        Err(e) => return fail(BIN, &e),
    };
    let state = match open_state(&layout) {
        Ok(s) => s,
        Err(e) => return fail(BIN, &e),
    };
    let cache = match block_on(open_cache(&state, &layout)) {
        Ok(c) => c,
        Err(e) => return fail(BIN, &e),
    };

    let conn = match state.open_read() {
        Ok(c) => c,
        Err(e) => return fail(BIN, &format!("could not open state.sqlite: {e}")),
    };

    let entries_by_kind_state = match memory_entry_counts(&conn) {
        Ok(v) => v,
        Err(e) => return fail(BIN, &format!("could not read memory counts: {e}")),
    };
    let pending_candidates_by_state = match pending_candidate_counts(&conn) {
        Ok(v) => v,
        Err(e) => return fail(BIN, &format!("could not read candidate counts: {e}")),
    };

    let target = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => {
            return fail(
                BIN,
                &format!("could not determine the current directory: {e}"),
            );
        }
    };
    let facts = gitroot::probe(&target);
    let resolution = match resolve(
        &conn,
        &RequestRoot {
            worktree_root: facts,
            repo_hint: None,
        },
    ) {
        Ok(r) => r,
        Err(e) => return fail(BIN, &format!("could not resolve worktree identity: {e}")),
    };
    let (scope_label, _scopes) = recall_pipeline::scopes_for(&resolution);

    let worktree = match &resolution {
        Resolution::Resolved {
            repo_id,
            worktree_id,
        } => match projection_state(&conn, worktree_id) {
            Ok(p) => Some((repo_id.clone(), worktree_id.clone(), p)),
            Err(e) => return fail(BIN, &format!("could not read projection state: {e}")),
        },
        Resolution::GlobalOnly | Resolution::Ambiguous { .. } => None,
    };

    let store_instance_uuid_value = match store_instance_uuid(&conn) {
        Ok(v) => v,
        Err(e) => return fail(BIN, &format!("could not read store instance id: {e}")),
    };

    if json {
        let worktree_json = worktree.as_ref().map(|(repo_id, worktree_id, projection)| {
            serde_json::json!({
                "repo_id": repo_id,
                "worktree_id": worktree_id,
                "active_generation_id": projection.as_ref().and_then(|p| p.active_generation_id.clone()),
                "active_model_space_id": projection.as_ref().and_then(|p| p.active_model_space_id.clone()),
                "projection_status": projection.as_ref().map(|p| p.status.as_str().to_string()),
                "projection_last_error": projection.as_ref().and_then(|p| p.last_error.clone()),
            })
        });
        let report = serde_json::json!({
            "memory": {
                "entries_by_kind_state": entries_by_kind_state.iter().map(|r| serde_json::json!({
                    "kind": r.kind.as_str(), "state": r.state.as_str(), "count": r.count,
                })).collect::<Vec<_>>(),
                "pending_candidates_by_state": pending_candidates_by_state.iter().map(|r| serde_json::json!({
                    "state": r.state.as_str(), "count": r.count,
                })).collect::<Vec<_>>(),
            },
            "scope": scope_label,
            "worktree": worktree_json,
            "store_instance_uuid": store_instance_uuid_value,
            "write_queues": {
                "state": {
                    "capacity": state.writer().queue_capacity(),
                    "available": state.writer().available_slots(),
                },
                "cache": {
                    "capacity": cache.writer().queue_capacity(),
                    "available": cache.writer().available_slots(),
                },
            },
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&report).expect("stats report always serializes")
        );
        return ExitCode::SUCCESS;
    }

    println!("{BIN}: scope {scope_label}");
    if entries_by_kind_state.is_empty() {
        println!("memory entries: none");
    } else {
        for row in &entries_by_kind_state {
            println!(
                "memory entries  {}/{}: {}",
                row.kind.as_str(),
                row.state.as_str(),
                row.count
            );
        }
    }
    if pending_candidates_by_state.is_empty() {
        println!("pending candidates: none");
    } else {
        for row in &pending_candidates_by_state {
            println!("pending candidates  {}: {}", row.state.as_str(), row.count);
        }
    }
    match &worktree {
        Some((repo_id, worktree_id, projection)) => {
            println!("worktree: repo {repo_id} / worktree {worktree_id}");
            match projection {
                Some(p) => println!(
                    "  active_generation={} active_model_space={} status={} last_error={}",
                    p.active_generation_id.as_deref().unwrap_or("(none)"),
                    p.active_model_space_id.as_deref().unwrap_or("(none)"),
                    p.status.as_str(),
                    p.last_error.as_deref().unwrap_or("(none)"),
                ),
                None => println!("  no projection state yet"),
            }
        }
        None => println!("worktree: (unresolved)"),
    }
    println!(
        "store_instance_uuid: {}",
        store_instance_uuid_value.as_deref().unwrap_or("(none)")
    );
    println!(
        "write queues: state {}/{} available, cache {}/{} available",
        state.writer().available_slots(),
        state.writer().queue_capacity(),
        cache.writer().available_slots(),
        cache.writer().queue_capacity(),
    );

    ExitCode::SUCCESS
}
