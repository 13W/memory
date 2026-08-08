//! `local-rag index <path>` / `local-rag reindex` (spec 11 §6, spec 06 §1) —
//! the CLI half: resolve (or register) the worktree identity behind a path,
//! build the pipeline context, run it, and render the result as one stdout
//! summary line or a `{BIN}: {message}` failure with a non-zero exit code.
//!
//! The pipeline itself is `local_rag::indexing` (T20-02) — [`IndexCtx`],
//! [`index_worktree`], `project_generation`, [`open_state`]/`open_cache`/
//! [`finish_index_ctx`], [`register_new_worktree`]. It used to live in this
//! file, `pub(crate)` to a binary target; it does not any more, because it
//! has a second caller now (the daemon's per-worktree tasks, T20-05/T20-06)
//! that cannot link against a binary. What stays here is exactly what is
//! CLI-shaped: the `clap` argument type, `ExitCode`, `println!`/`eprintln!`,
//! and the `Ambiguous`-candidate listing ([`print_ambiguous`], shared with
//! `cli::watch`).

use std::path::PathBuf;
use std::process::ExitCode;

use local_rag::daemon::gitroot;
use local_rag::indexing::{
    IndexCtx, IndexError, finish_index_ctx, index_worktree, open_state, register_new_worktree,
    resolve_facts,
};
use local_rag_core::identity::{SystemUuidV7, Uuid, UuidSource};
use local_rag_core::redaction::Scanner;
use local_rag_index::reconcile::load_worktree_meta;
use local_rag_index::scan::StatCache;
use local_rag_store::{Candidate, Resolution};

use super::{block_on, fail, resolve_layout_and_config, system_now_ms};

const BIN: &str = "local-rag";

pub(crate) fn print_ambiguous(candidates: &[Candidate]) {
    eprintln!(
        "{BIN}: this path matches {} detached worktree(s) of more than one repository; \
         pick one with `local-rag repo attach <repo_id> --worktree <worktree_id>`:",
        candidates.len()
    );
    for c in candidates {
        eprintln!(
            "  repo {} worktree {} ({})",
            c.repo_id,
            c.worktree_id,
            c.kind.as_str()
        );
    }
}

/// Run the full pipeline for an already-resolved `worktree_id`, printing a
/// one-line summary on success.
async fn run_pipeline(ctx: &IndexCtx, worktree_id: Uuid, now_ms: i64) -> ExitCode {
    let case = gitroot::case_sensitivity();
    let meta = match load_worktree_meta(&ctx.state, &worktree_id.to_string(), case) {
        Ok(Some(meta)) => meta,
        Ok(None) => return fail(BIN, &IndexError::WorktreeVanished.to_string()),
        Err(e) => return fail(BIN, &IndexError::Meta(e).to_string()),
    };

    let mut stat_cache = StatCache::new();
    let scanner = Scanner::new();

    match index_worktree(
        ctx,
        &meta,
        &mut stat_cache,
        &ctx.classifier,
        &scanner,
        now_ms,
    )
    .await
    {
        Ok(outcome) => {
            println!(
                "{BIN}: indexed {} files ({} occurrences) into generation {}; \
                 embedded {} subjects ({} reused, {} repaired, {} failed); \
                 dense +{}/-{}; fts {} occurrences",
                outcome.reconcile.build.files_indexed,
                outcome.reconcile.build.occurrences,
                outcome.reconcile.build.generation_id,
                outcome.project.backfill.embedded,
                outcome.project.backfill.reused,
                outcome.project.backfill.repaired,
                outcome.project.backfill.failed,
                outcome.project.switch.upserted,
                outcome.project.switch.deleted,
                outcome.project.fts.occurrence_count,
            );
            ExitCode::SUCCESS
        }
        Err(e) => fail(BIN, &e.to_string()),
    }
}

#[derive(Debug, clap::Args)]
pub struct IndexArgs {
    /// Directory to index (registered as a new worktree if not already known).
    path: String,
}

pub fn run_index(args: IndexArgs) -> ExitCode {
    let (layout, config) = match resolve_layout_and_config() {
        Ok(v) => v,
        Err(e) => return fail(BIN, &e),
    };

    let path = PathBuf::from(&args.path);
    let Some(facts) = gitroot::probe(&path) else {
        return fail(BIN, &format!("{}: not an accessible directory", args.path));
    };

    let state = match open_state(&layout) {
        Ok(s) => s,
        Err(e) => return fail(BIN, &e),
    };
    let resolution = match resolve_facts(&state, &facts) {
        Ok(r) => r,
        Err(e) => return fail(BIN, &e),
    };
    let now_ms = system_now_ms();

    block_on(async {
        let worktree_id = match resolution {
            Resolution::Resolved { worktree_id, .. } => match worktree_id.parse::<Uuid>() {
                Ok(id) => id,
                Err(_) => return fail(BIN, "internal error: stored worktree id is not a UUID"),
            },
            Resolution::GlobalOnly => {
                let repo_id = SystemUuidV7.next_uuid();
                let worktree_id = SystemUuidV7.next_uuid();
                if let Err(e) =
                    register_new_worktree(&state, repo_id, worktree_id, &facts, now_ms).await
                {
                    return fail(BIN, &format!("could not register the worktree: {e}"));
                }
                worktree_id
            }
            Resolution::Ambiguous { candidates } => {
                print_ambiguous(&candidates);
                return ExitCode::FAILURE;
            }
        };

        let ctx = match finish_index_ctx(state, &layout, &config).await {
            Ok(ctx) => ctx,
            Err(e) => return fail(BIN, &e),
        };
        run_pipeline(&ctx, worktree_id, now_ms).await
    })
}

pub fn run_reindex() -> ExitCode {
    let (layout, config) = match resolve_layout_and_config() {
        Ok(v) => v,
        Err(e) => return fail(BIN, &e),
    };

    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => {
            return fail(
                BIN,
                &format!("could not determine the current directory: {e}"),
            );
        }
    };
    let Some(facts) = gitroot::probe(&cwd) else {
        return fail(BIN, "the current directory is not accessible");
    };

    let state = match open_state(&layout) {
        Ok(s) => s,
        Err(e) => return fail(BIN, &e),
    };
    let resolution = match resolve_facts(&state, &facts) {
        Ok(r) => r,
        Err(e) => return fail(BIN, &e),
    };
    let now_ms = system_now_ms();

    let worktree_id = match resolution {
        Resolution::Resolved { worktree_id, .. } => match worktree_id.parse::<Uuid>() {
            Ok(id) => id,
            Err(_) => return fail(BIN, "internal error: stored worktree id is not a UUID"),
        },
        Resolution::GlobalOnly => {
            return fail(
                BIN,
                "this path is not indexed yet; run `local-rag index <path>` first",
            );
        }
        Resolution::Ambiguous { candidates } => {
            print_ambiguous(&candidates);
            return ExitCode::FAILURE;
        }
    };

    block_on(async {
        let ctx = match finish_index_ctx(state, &layout, &config).await {
            Ok(ctx) => ctx,
            Err(e) => return fail(BIN, &e),
        };
        run_pipeline(&ctx, worktree_id, now_ms).await
    })
}
