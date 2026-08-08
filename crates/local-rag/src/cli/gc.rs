//! `local-rag gc [--dry-run]` (spec 11 §6, D-025) — the first production
//! wiring of six already-built, already-tested sweeps: orphan shard dirs
//! (T06-03), expired detached/removing shard dirs (D-007), unreferenced
//! model-space shard dirs (D-011), dead spool sessions (T13-05), expired
//! `observation_payload` rows (T13-05), and stale `pending_memory_candidate`
//! rows (T14-05). Every sweep already lives in `local_rag_store::housekeeping`
//! / `local_rag_store::observation::run_payload_ttl_sweep`, public, dry-run
//! capable, and unit/integration tested — this command adds no new domain
//! logic, only sequencing and a report. No confirmation prompt: every sweep
//! here is already-established, already-gated retention/GC behavior (specs
//! 05 §8, 07 §6, 12 §3), not the "destructive purge" class that
//! confirmation requirement is aimed at (D-025 leaves `purge` to T16-02).

use std::process::ExitCode;

use local_rag_store::{
    CANDIDATE_EXPIRY_MS, SHARD_DESTROY_GRACE_MS, SPOOL_SESSION_ABSENCE_MS,
    run_candidate_expiry_sweep, run_expired_shard_sweep, run_orphan_shard_sweep,
    run_payload_ttl_sweep, run_spool_session_sweep, run_unreferenced_space_sweep,
};

use super::{block_on, fail, resolve_layout_and_config, system_now_ms};
use local_rag::indexing::open_state;

const BIN: &str = "local-rag";

#[derive(Debug, clap::Args)]
pub struct GcArgs {
    /// Report what each sweep would remove without removing it.
    #[arg(long)]
    dry_run: bool,
}

pub fn run(args: GcArgs) -> ExitCode {
    let dry_run = args.dry_run;

    let (layout, _config) = match resolve_layout_and_config() {
        Ok(v) => v,
        Err(e) => return fail(BIN, &e),
    };
    let state = match open_state(&layout) {
        Ok(s) => s,
        Err(e) => return fail(BIN, &e),
    };
    let now_ms = system_now_ms();
    let verb = if dry_run { "would remove" } else { "removed" };

    let orphan = match run_orphan_shard_sweep(&state, &layout, dry_run) {
        Ok(r) => r,
        Err(e) => return fail(BIN, &format!("orphan shard sweep failed: {e}")),
    };
    println!(
        "orphan shard dirs: {verb} {}, retained {}",
        orphan.removed.len(),
        orphan.retained
    );

    let expired =
        match run_expired_shard_sweep(&state, &layout, now_ms, SHARD_DESTROY_GRACE_MS, dry_run) {
            Ok(r) => r,
            Err(e) => return fail(BIN, &format!("expired shard sweep failed: {e}")),
        };
    println!(
        "expired shard dirs: {verb} {}, retained {}",
        expired.removed.len(),
        expired.retained
    );

    let unreferenced = match run_unreferenced_space_sweep(&state, &layout, dry_run) {
        Ok(r) => r,
        Err(e) => return fail(BIN, &format!("unreferenced model-space sweep failed: {e}")),
    };
    println!(
        "unreferenced model-space dirs: {verb} {}, retained {}",
        unreferenced.removed.len(),
        unreferenced.retained
    );

    let (spool, payload, candidates) = block_on(async {
        let spool =
            run_spool_session_sweep(&state, &layout, now_ms, SPOOL_SESSION_ABSENCE_MS, dry_run)
                .await;
        let payload = run_payload_ttl_sweep(&state, now_ms, dry_run).await;
        let candidates =
            run_candidate_expiry_sweep(&state, now_ms, CANDIDATE_EXPIRY_MS, dry_run).await;
        (spool, payload, candidates)
    });

    let spool = match spool {
        Ok(r) => r,
        Err(e) => return fail(BIN, &format!("spool session sweep failed: {e}")),
    };
    println!(
        "dead spool sessions: {verb} {}, retained {}",
        spool.removed.len(),
        spool.retained
    );

    let payload = match payload {
        Ok(r) => r,
        Err(e) => return fail(BIN, &format!("payload TTL sweep failed: {e}")),
    };
    println!(
        "expired observation payloads: {verb} {}, retained {} (total envelopes {})",
        payload.payload_removed, payload.payload_retained, payload.total_envelopes
    );

    let candidates = match candidates {
        Ok(r) => r,
        Err(e) => return fail(BIN, &format!("candidate expiry sweep failed: {e}")),
    };
    println!(
        "stale pending candidates: {} {}, retained {}",
        if dry_run { "would expire" } else { "expired" },
        candidates.expired.len(),
        candidates.retained
    );

    ExitCode::SUCCESS
}
