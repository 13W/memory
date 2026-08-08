//! `local-rag watch` (spec 11 §6, spec 06 §1) — a standalone foreground
//! process that keeps one worktree's generation continuously fresh: the
//! filesystem watcher schedules reconciles, the debounced [`WorktreeReconciler`]
//! runs them, and every successfully built generation is embedded/activated/
//! materialized (the library pipeline's own [`project_generation`], T20-02)
//! before the next one is considered.
//!
//! # Why a standalone process, not daemon-IPC
//!
//! `local_rag_protocol` has no verb for "watch"; the daemon does not spawn a
//! [`spawn_watcher`]/[`WorktreeReconciler`] anywhere today (confirmed by grep
//! — every reference outside `crates/index` is this file). Adding a new
//! protocol message would be new, unrequested architectural surface just to
//! reach a primitive that already runs standalone; `TriggerKind::Manual`'s
//! own doc names `local-rag reindex` as "the manual force" — this module is
//! that same CLI's own always-on sibling, another direct caller of the
//! already-tested reconcile driver, not a daemon feature.

use std::process::ExitCode;

use local_rag::daemon::gitroot;
use local_rag::indexing::{
    IndexCtx, finish_index_ctx, open_state, project_generation, resolve_facts,
};
use local_rag_core::identity::Uuid;
use local_rag_core::redaction::Scanner;
use local_rag_index::reconcile::{
    ReconcileHandle, ScheduleConfig, TriggerKind, WorktreeReconciler, load_worktree_meta,
    spawn_reconciler, spawn_watcher,
};
use local_rag_store::Resolution;

use super::index::print_ambiguous;
use super::{block_on, fail, resolve_layout_and_config, system_now_ms};

const BIN: &str = "local-rag";

pub fn run_watch() -> ExitCode {
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
        // Installed before `finish_index_ctx` (which loads the ONNX model —
        // a real window of wall-clock time), not just before the select
        // loop: a SIGTERM/Ctrl-C arriving during that load must still be
        // observed, per `ShutdownSignal`'s own doc on why late installation
        // measurably flaked `tests/serve_subprocess.rs` for the daemon.
        let mut shutdown = local_rag::daemon::ShutdownSignal::install();
        let ctx = match finish_index_ctx(state, &layout, &config).await {
            Ok(ctx) => ctx,
            Err(e) => return fail(BIN, &e),
        };
        run_watch_loop(&ctx, worktree_id, &mut shutdown).await
    })
}

/// Drive the watch loop until SIGTERM/Ctrl-C: spawn the reconciler and the
/// live filesystem watcher, force one immediate cold-start reconcile, then
/// react to every subsequent success/failure until interrupted.
async fn run_watch_loop(
    ctx: &IndexCtx,
    worktree_id: Uuid,
    shutdown: &mut local_rag::daemon::ShutdownSignal,
) -> ExitCode {
    let case = gitroot::case_sensitivity();
    let meta = match load_worktree_meta(&ctx.state, &worktree_id.to_string(), case) {
        Ok(Some(meta)) => meta,
        Ok(None) => return fail(BIN, "the worktree vanished from the registry mid-run"),
        Err(e) => return fail(BIN, &format!("could not load worktree metadata: {e}")),
    };

    let reconciler = WorktreeReconciler::new(
        ctx.state.clone(),
        meta.clone(),
        ctx.classifier,
        Scanner::new(),
        ctx.uuids.clone(),
        ScheduleConfig::default(),
    );
    let ReconcileHandle {
        sender,
        join,
        mut failures,
        mut successes,
    } = spawn_reconciler(reconciler, 8);

    let watcher = match spawn_watcher(&meta.root, meta.is_git(), sender.clone()) {
        Ok(w) => w,
        Err(e) => {
            drop(sender);
            let _ = join.await;
            return fail(BIN, &format!("could not start the filesystem watcher: {e}"));
        }
    };

    // Cold start: reconcile once immediately rather than waiting for the
    // first real change or the periodic backstop.
    let _ = sender.send(TriggerKind::Startup).await;

    println!(
        "{BIN}: watching {} (worktree {worktree_id}); Ctrl-C to stop",
        meta.root.display()
    );

    let mut last_projected: Option<String> = None;

    loop {
        tokio::select! {
            _ = shutdown.wait() => {
                drop(watcher);
                drop(sender);
                let _ = join.await;
                // Flush: the shutdown-time reconcile the driver's own doc
                // promises ("any scheduled reconcile is flushed before
                // returning") may have published one more success after the
                // last time this loop observed the channel.
                let final_generation = successes.borrow().clone();
                if final_generation.is_some() && final_generation != last_projected {
                    project_and_report(ctx, worktree_id, final_generation).await;
                }
                println!("{BIN}: stopped watching");
                return ExitCode::SUCCESS;
            }
            changed = successes.changed() => {
                if changed.is_err() {
                    // The reconciler task ended on its own (should only
                    // happen once every sender is dropped, i.e. shutdown);
                    // fall through to the next select iteration, which will
                    // see Ctrl-C or loop harmlessly until it does.
                    continue;
                }
                let generation_id = successes.borrow().clone();
                if generation_id != last_projected {
                    project_and_report(ctx, worktree_id, generation_id.clone()).await;
                    last_projected = generation_id;
                }
            }
            changed = failures.changed() => {
                if changed.is_ok()
                    && let Some(f) = failures.borrow().clone()
                {
                    eprintln!(
                        "{BIN}: reconcile failed ({} consecutive): {}",
                        f.consecutive_failures, f.last_error
                    );
                }
            }
        }
    }
}

/// Project `generation_id` (embed → activate → materialize) and print the
/// outcome, or a diagnostic on failure — never fatal to the watch loop
/// itself, since a transient embedding failure must not stop watching for
/// the *next* change.
async fn project_and_report(ctx: &IndexCtx, worktree_id: Uuid, generation_id: Option<String>) {
    let Some(generation_id) = generation_id else {
        return;
    };
    let Ok(gid) = generation_id.parse::<Uuid>() else {
        eprintln!("{BIN}: internal error: generation id {generation_id} is not a UUID");
        return;
    };
    let now_ms = system_now_ms();
    match project_generation(ctx, worktree_id, gid, now_ms).await {
        Ok(outcome) => println!(
            "{BIN}: generation {generation_id} ready — embedded {} ({} reused); dense +{}/-{}; fts {} occurrences",
            outcome.backfill.embedded,
            outcome.backfill.reused,
            outcome.switch.upserted,
            outcome.switch.deleted,
            outcome.fts.occurrence_count,
        ),
        Err(e) => eprintln!("{BIN}: could not project generation {generation_id}: {e}"),
    }
}
