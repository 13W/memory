//! `local-rag stats [--json]` (spec 11 §6, D-025) — CLI mirror of the MCP
//! `stats` tool (`daemon/mcp/memory.rs`, T15-04): the same store-crate calls,
//! against this command's own opened `state.sqlite`/`cache.sqlite`, with no
//! `MemoryContext` (that type is daemon-only — built once at daemon startup,
//! not meaningful for a one-shot CLI invocation).

use std::process::ExitCode;

use local_rag_memory::recall as recall_pipeline;
use local_rag_store::{
    CURRENT_NORMALIZER_VERSION, NormalizationBacklog, NormalizationCountRow, RequestRoot,
    Resolution, STUCK_RUN_ATTEMPT_THRESHOLD, StuckRunRow, UnconsolidatableSession,
    consolidation_run_counts, generation_meta_for_worktree, memory_entry_counts,
    normalization_backlog, normalization_counts, observation_envelope_count,
    observations_applied_since, oldest_open_run_created_at, pending_candidate_counts,
    projection_state, resolve, store_instance_uuid, stuck_consolidation_runs,
    total_pending_backlog, unconsolidatable_sessions,
};

use local_rag::daemon::gitroot;

use super::freshness::{IndexFreshness, humanize_age};
use super::{block_on, fail, resolve_layout_and_config, system_now_ms};
use local_rag::indexing::{open_cache, open_state};

const BIN: &str = "local-rag";

/// Window (D-049, `[SPEC]`-chosen, not measured — same class of pick as
/// `LIVENESS_PROBE_TIMEOUT_MS`) over which recently-`applied` consolidation
/// runs are summed to estimate throughput/ETA. Long enough to smooth over a
/// single slow LLM call, short enough that a stalled consolidation-trigger
/// shows up as "no measurable throughput" within a few minutes, not stale
/// numbers from an hour ago.
const CONSOLIDATION_THROUGHPUT_WINDOW_MS: i64 = 5 * 60 * 1000;

#[derive(Debug, clap::Args)]
pub struct StatsArgs {
    /// Print the stats report as JSON instead of human-readable lines.
    #[arg(long)]
    json: bool,
}

/// Consolidation-backlog counts and a best-effort progress/ETA estimate
/// (D-049) — computed here (presentation), not in the store layer, from the
/// five new store-wide primitives. `progress_pct`/`eta_seconds` are honestly
/// `None` when unmeasurable (empty store, zero throughput, zero backlog)
/// rather than a fabricated number.
struct ConsolidationStats {
    runs_by_state: Vec<local_rag_store::RunCountRow>,
    pending_backlog_total: i64,
    progress_pct: Option<f64>,
    throughput_observations_per_min: f64,
    eta_seconds: Option<i64>,
    oldest_pending_run_created_at: Option<i64>,
    /// D-058: sessions permanently stuck in the floor case — an observation
    /// (or an already-halved window) that alone still overflows the model's
    /// context, so [`open_next_run`](local_rag_store::open_next_run)'s
    /// shrink-and-retry has no narrower window left to try. Empty in the
    /// overwhelmingly common case; every non-empty session here needs a
    /// human decision, not another retry.
    unconsolidatable: Vec<UnconsolidatableSession>,
    /// D-071: runs that keep failing or have been given up on. During the
    /// D-069 incident every number above this line looked healthy while one
    /// run was on its 627th attempt — this is the line that would have said
    /// so.
    stuck_runs: Vec<StuckRunRow>,
}

fn compute_consolidation_stats(
    conn: &rusqlite::Connection,
    total_observations: i64,
    now_ms: i64,
) -> rusqlite::Result<ConsolidationStats> {
    let runs_by_state = consolidation_run_counts(conn)?;
    let pending_backlog_total = total_pending_backlog(conn)?;
    let applied_recently =
        observations_applied_since(conn, now_ms - CONSOLIDATION_THROUGHPUT_WINDOW_MS)?;
    let throughput_observations_per_min =
        applied_recently as f64 / (CONSOLIDATION_THROUGHPUT_WINDOW_MS as f64 / 60_000.0);
    let progress_pct = if total_observations > 0 {
        Some(
            (total_observations - pending_backlog_total) as f64 / total_observations as f64 * 100.0,
        )
    } else {
        None
    };
    let eta_seconds = if throughput_observations_per_min > 0.0 && pending_backlog_total > 0 {
        Some((pending_backlog_total as f64 / throughput_observations_per_min * 60.0) as i64)
    } else {
        None
    };
    let oldest_pending_run_created_at = oldest_open_run_created_at(conn)?;
    let unconsolidatable = unconsolidatable_sessions(conn, local_rag_core::BUILD_ID)?;
    let stuck_runs =
        stuck_consolidation_runs(conn, local_rag_core::BUILD_ID, STUCK_RUN_ATTEMPT_THRESHOLD)?;
    Ok(ConsolidationStats {
        runs_by_state,
        pending_backlog_total,
        progress_pct,
        throughput_observations_per_min,
        eta_seconds,
        oldest_pending_run_created_at,
        unconsolidatable,
        stuck_runs,
    })
}

/// One [`StuckRunRow`] as a single human-readable line (D-071) — the run, its
/// session, the window it covers, how many times it has failed, whether
/// anything will ever retry it, and the failure the store already recorded.
/// T21-08: the `memory.normalization` block, in the exact shape the MCP
/// `stats` tool serializes — both surfaces render the same two store reads, and
/// a test asserts the two blocks are equal on one store.
///
/// Deliberately store-state only: whether the worker is switched on, and why it
/// might be stopped, is `doctor`'s job. `stats` answers "where is this store",
/// `doctor` answers "why is it there".
fn normalization_json(
    by_status: &[NormalizationCountRow],
    backlog: &NormalizationBacklog,
) -> serde_json::Value {
    serde_json::json!({
        "counts_by_status": by_status.iter().map(|r| serde_json::json!({
            "status": r.status.as_str(), "count": r.count,
        })).collect::<Vec<_>>(),
        "pending": backlog.pending,
        "dead_letter": backlog.dead_letter,
        "normalizer_version": CURRENT_NORMALIZER_VERSION,
    })
}

/// The human half of the block above. The dead-letter line follows the same
/// discipline as D-058's and D-071's: silent when zero, so a healthy store
/// gains no noise, and impossible to miss when not.
fn print_normalization(by_status: &[NormalizationCountRow], backlog: &NormalizationBacklog) {
    if by_status.is_empty() {
        println!("memory normalization: none recorded");
    } else {
        for row in by_status {
            println!(
                "memory normalization  {}: {}",
                row.status.as_str(),
                row.count
            );
        }
    }
    println!("memory normalization pending: {}", backlog.pending);
    if backlog.dead_letter > 0 {
        println!(
            "memory normalization dead-letter: {} entry(ies) given up on under normalizer v{} \
             — they keep using their original text",
            backlog.dead_letter, CURRENT_NORMALIZER_VERSION,
        );
    }
}

fn describe_stuck_run(r: &StuckRunRow) -> String {
    let verdict = if r.dead_lettered {
        "dead-lettered on this build — only a rebuild retries it"
    } else {
        "still retrying"
    };
    format!(
        "run {} session {} received_seq {}..={} — {} attempt(s), {}{}",
        r.run_id,
        r.session_id,
        r.from_received_seq,
        r.to_received_seq,
        r.attempt_count,
        verdict,
        match (&r.last_failure_kind, &r.last_failure_reason) {
            (Some(kind), Some(reason)) => format!(" ({kind}): {reason}"),
            (Some(kind), None) => format!(" ({kind})"),
            (None, Some(reason)) => format!(": {reason}"),
            (None, None) => String::new(),
        }
    )
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
    // T21-08: the normalization axis (ADR-0010) — what has been normalized,
    // what still lags, and what the worker has given up on. Two store reads,
    // the same two the MCP `stats` tool makes, so both surfaces cannot drift.
    let normalization_by_status = match normalization_counts(&conn) {
        Ok(v) => v,
        Err(e) => return fail(BIN, &format!("could not read normalization counts: {e}")),
    };
    let normalization =
        match normalization_backlog(&conn, CURRENT_NORMALIZER_VERSION, system_now_ms()) {
            Ok(v) => v,
            Err(e) => return fail(BIN, &format!("could not read normalization backlog: {e}")),
        };

    // D-049: the observations pillar (`01-overview.md` §5-9) and the
    // consolidation backlog/progress — store-wide, same as the memory
    // counts above, previously unreported by `stats()` entirely.
    let observations_total = match observation_envelope_count(&conn) {
        Ok(v) => v,
        Err(e) => return fail(BIN, &format!("could not read observation count: {e}")),
    };
    let consolidation =
        match compute_consolidation_stats(&conn, observations_total, system_now_ms()) {
            Ok(v) => v,
            Err(e) => return fail(BIN, &format!("could not read consolidation stats: {e}")),
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
            Ok(p) => {
                // X-008: `active_generation_id` alone never answered "is this
                // index current?" — the age of that generation does.
                let generations =
                    generation_meta_for_worktree(&conn, worktree_id).unwrap_or_default();
                Some((
                    repo_id.clone(),
                    worktree_id.clone(),
                    p,
                    IndexFreshness::from_generations(&generations),
                ))
            }
            Err(e) => return fail(BIN, &format!("could not read projection state: {e}")),
        },
        Resolution::GlobalOnly | Resolution::Ambiguous { .. } => None,
    };

    let store_instance_uuid_value = match store_instance_uuid(&conn) {
        Ok(v) => v,
        Err(e) => return fail(BIN, &format!("could not read store instance id: {e}")),
    };

    if json {
        let worktree_json = worktree
            .as_ref()
            .map(|(repo_id, worktree_id, projection, freshness)| {
            serde_json::json!({
                "repo_id": repo_id,
                "worktree_id": worktree_id,
                "active_generation_id": projection.as_ref().and_then(|p| p.active_generation_id.clone()),
                "active_model_space_id": projection.as_ref().and_then(|p| p.active_model_space_id.clone()),
                "projection_status": projection.as_ref().map(|p| p.status.as_str().to_string()),
                "projection_last_error": projection.as_ref().and_then(|p| p.last_error.clone()),
                "active_generation_number": freshness.active.as_ref().map(|(_, n, _)| *n),
                "active_generation_created_at": freshness.active.as_ref().and_then(|(_, _, ms)| *ms),
                "stuck_generations": freshness
                    .stuck_newer
                    .iter()
                    .map(|s| serde_json::json!({
                        "generation_number": s.generation_number,
                        "state": s.state.as_str(),
                    }))
                    .collect::<Vec<_>>(),
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
                "normalization": normalization_json(&normalization_by_status, &normalization),
            },
            "observations": {
                "total": observations_total,
            },
            "consolidation": {
                "runs_by_state": consolidation.runs_by_state.iter().map(|r| serde_json::json!({
                    "state": r.state.as_str(), "count": r.count,
                })).collect::<Vec<_>>(),
                "pending_backlog_total": consolidation.pending_backlog_total,
                "progress_pct": consolidation.progress_pct,
                "throughput_observations_per_min": consolidation.throughput_observations_per_min,
                "eta_seconds": consolidation.eta_seconds,
                "oldest_pending_run_created_at": consolidation.oldest_pending_run_created_at,
                "unconsolidatable_sessions": consolidation.unconsolidatable.iter().map(|s| serde_json::json!({
                    "session_id": s.session_id,
                    "dead_letter_run_id": s.dead_letter_run_id,
                    "from_received_seq": s.from_received_seq,
                    "to_received_seq": s.to_received_seq,
                })).collect::<Vec<_>>(),
                "stuck_runs": consolidation.stuck_runs.iter().map(|r| serde_json::json!({
                    "run_id": r.run_id,
                    "session_id": r.session_id,
                    "attempt_count": r.attempt_count,
                    "dead_lettered": r.dead_lettered,
                    "last_failure_kind": r.last_failure_kind,
                    "last_failure_reason": r.last_failure_reason,
                    "from_received_seq": r.from_received_seq,
                    "to_received_seq": r.to_received_seq,
                })).collect::<Vec<_>>(),
            },
            "scope": scope_label,
            "worktree": worktree_json,
            "store_instance_uuid": store_instance_uuid_value,
            // `longest_hold_ms` (D-094): how long one queued transaction has held
            // the connection at most. Seconds here mean a caller is starving
            // every other process's writer for that long — the shape D-094 had,
            // which nothing reported at the time.
            "write_queues": {
                "state": {
                    "capacity": state.writer().queue_capacity(),
                    "available": state.writer().available_slots(),
                    "longest_hold_ms": state.writer().longest_hold_ms(),
                },
                "cache": {
                    "capacity": cache.writer().queue_capacity(),
                    "available": cache.writer().available_slots(),
                    "longest_hold_ms": cache.writer().longest_hold_ms(),
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
    print_normalization(&normalization_by_status, &normalization);
    println!("observations: {observations_total} total");
    if consolidation.runs_by_state.is_empty() {
        println!("consolidation runs: none");
    } else {
        for row in &consolidation.runs_by_state {
            println!("consolidation runs  {}: {}", row.state.as_str(), row.count);
        }
    }
    println!(
        "consolidation pending backlog: {}",
        consolidation.pending_backlog_total
    );
    match consolidation.progress_pct {
        Some(pct) => println!("consolidation progress: {pct:.1}%"),
        None => println!("consolidation progress: unknown (no observations yet)"),
    }
    println!(
        "consolidation throughput: {:.1} observations/min (last {} min)",
        consolidation.throughput_observations_per_min,
        CONSOLIDATION_THROUGHPUT_WINDOW_MS / 60_000,
    );
    match consolidation.eta_seconds {
        Some(secs) => println!("consolidation eta: {secs}s"),
        None => println!("consolidation eta: unknown (no measurable throughput)"),
    }
    match consolidation.oldest_pending_run_created_at {
        Some(ms) => println!("consolidation oldest pending run created_at: {ms}"),
        None => println!("consolidation oldest pending run: none (fully caught up)"),
    }
    // D-058: silent when empty — the overwhelmingly common case — so this
    // never adds noise to a healthy store; every line printed here names a
    // session that needs a human, not another retry.
    if !consolidation.unconsolidatable.is_empty() {
        println!(
            "consolidation unconsolidatable: {} session(s) — needs manual review, \
             will not resolve on its own",
            consolidation.unconsolidatable.len()
        );
        for s in &consolidation.unconsolidatable {
            println!(
                "  session {} received_seq {}..={} — dead-letter run {}",
                s.session_id, s.from_received_seq, s.to_received_seq, s.dead_letter_run_id
            );
        }
    }
    // D-071: same discipline as the D-058 block above — silent on a healthy
    // store, and every line printed here names a run that is either being
    // retried without converging or has been given up on entirely.
    if !consolidation.stuck_runs.is_empty() {
        println!(
            "consolidation stuck runs: {} — retried without converging, or dead-lettered",
            consolidation.stuck_runs.len()
        );
        for r in &consolidation.stuck_runs {
            println!("  {}", describe_stuck_run(r));
        }
    }
    match &worktree {
        Some((repo_id, worktree_id, projection, freshness)) => {
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
            // X-008: how old the served index actually is, and whether newer
            // work was built and dropped.
            let now_ms = system_now_ms();
            match &freshness.active {
                Some((_, number, Some(created_ms))) => println!(
                    "  index age: generation #{number} built {}",
                    humanize_age(now_ms, *created_ms),
                ),
                Some((_, number, None)) => {
                    println!("  index age: generation #{number}, build time unknown")
                }
                None => println!("  index age: nothing active — nothing is being served"),
            }
            for s in &freshness.stuck_newer {
                println!(
                    "  STUCK: generation #{} is {} but never became active",
                    s.generation_number,
                    s.state.as_str(),
                );
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
