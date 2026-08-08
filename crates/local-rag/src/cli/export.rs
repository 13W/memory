//! `local-rag export [--scope global|repository|worktree]` (spec 11 §6, 12
//! §3, T16-02) — a scoped, deterministic JSON dump of every memory entry
//! (plus its evidence and audit trail) in the caller-resolved scope set, via
//! `local_rag_store::privacy::export_scope`.
//!
//! Scope resolution is exactly `cli::memory::run_list`'s own: resolve cwd →
//! `resolve()` → `scopes_for()` → optional `--scope` narrowing. No new
//! `--repo-id`/`--worktree-id` flags — this reuses the one scope vocabulary
//! the CLI already has.

use std::process::ExitCode;

use local_rag_memory::recall as recall_pipeline;
use local_rag_store::{RequestRoot, ScopeKind, export_scope, resolve};

use local_rag::daemon::gitroot;

use super::inspect::memory_inspection_json;
use super::{fail, parse_scope_kind, resolve_layout_and_config, system_now_ms};
use local_rag::indexing::open_state;

const BIN: &str = "local-rag";

#[derive(Debug, clap::Args)]
pub struct ExportArgs {
    /// Narrow the export to one scope (global/repository/worktree); every
    /// scope this invocation resolves to by default.
    #[arg(long, value_parser = parse_scope_kind)]
    scope: Option<ScopeKind>,
}

pub fn run(args: ExportArgs) -> ExitCode {
    let scope_filter = args.scope;

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
    let (_scope_label, scopes) = recall_pipeline::scopes_for(&resolution);
    let scopes: Vec<(ScopeKind, String)> = match scope_filter {
        Some(wanted) => scopes.into_iter().filter(|(k, _)| *k == wanted).collect(),
        None => scopes,
    };

    let now_ms = system_now_ms();
    let exported = match export_scope(&conn, &scopes, now_ms) {
        Ok(v) => v,
        Err(e) => return fail(BIN, &format!("could not export: {e}")),
    };

    let value = serde_json::Value::Array(exported.iter().map(memory_inspection_json).collect());
    println!(
        "{}",
        serde_json::to_string_pretty(&value).expect("export result always serializes")
    );
    ExitCode::SUCCESS
}
