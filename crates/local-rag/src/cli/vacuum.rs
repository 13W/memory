//! `local-rag vacuum [--dry-run]` (spec 11 §6, `X-012`) — the operator's
//! reclamation pass.
//!
//! SQLite never hands a deleted page back to the filesystem on its own, so a
//! store whose GC has been doing its job keeps growing on disk anyway. Measured
//! on the owner's store before this command existed: 57 GB of file over roughly
//! 19.5 GB of live data, 66 % of it holes left by 1437 swept generations.
//!
//! # Why the heavy pass is a command and not a background job
//!
//! `VACUUM` rewrites the whole database: exclusive access, free disk for a
//! second copy of the live data, many minutes on a large store. A daemon doing
//! that at idle would hold `store.lock` for the duration and refuse every
//! request that arrived meanwhile — a worse defect than the bloat it cleared.
//! So the heavy pass runs here, deliberately, with the daemon stopped; the
//! daemon's own contribution is the cheap bounded chunk it takes at idle
//! (`daemon::lifecycle`'s `reclaim_one_chunk`), which only works *after* this
//! command has converted the store.
//!
//! # The conversion is the point, not a side effect
//!
//! `auto_vacuum` can only change while a database is empty or during a
//! `VACUUM`. This command's rewrite is therefore the one moment an existing
//! store can start being able to reclaim on its own, which is why it does both
//! in one connection (`StateWriter::vacuum`).

use std::process::ExitCode;
use std::time::Duration;

use local_rag_core::identity::{SystemUuidV7, UuidSource};
use local_rag_store::{AutoVacuum, DbSpace, RECLAIM_FREE_RATIO, RECLAIM_MIN_FILE_BYTES};

use super::{block_on, fail, resolve_layout_and_config};
use local_rag::indexing::open_state;

const BIN: &str = "local-rag";

#[derive(Debug, clap::Args)]
pub struct VacuumArgs {
    /// Report what the pass would reclaim without touching the store.
    #[arg(long)]
    dry_run: bool,
}

/// Human-readable bytes — the numbers here span kilobytes to tens of
/// gigabytes, and an operator deciding whether to spend twenty minutes should
/// not have to count digits.
fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[0])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn report(space: &DbSpace) {
    println!(
        "space: {} on disk, {} of it free ({:.1} %), auto_vacuum={}",
        human(space.file_bytes()),
        human(space.free_bytes()),
        space.free_ratio() * 100.0,
        space.auto_vacuum.as_str()
    );
}

/// Free bytes on the filesystem holding `path`, or `None` when the platform or
/// the call cannot say.
///
/// Answering "cannot tell" rather than guessing matters here: the caller uses
/// this to refuse a twenty-minute rewrite that would fail on a full disk, and a
/// fabricated number would either block a legitimate run or let a doomed one
/// start.
#[cfg(unix)]
fn free_disk_bytes(path: &std::path::Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;

    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `statvfs` reads the NUL-terminated path `c_path` owns for the
    // duration of the call and writes only into `stat`, which is a
    // correctly-sized, owned `libc::statvfs`. Zeroing it first is what makes
    // the failure path safe: on a non-zero return the struct is left as the
    // all-zero value and this function reports `None` rather than reading it.
    // Same shape and same justification as `local_rag_core::process::pid_exists`.
    #[allow(unsafe_code)]
    let (rc, stat) = unsafe {
        let mut stat: libc::statvfs = std::mem::zeroed();
        (libc::statvfs(c_path.as_ptr(), &mut stat), stat)
    };
    if rc != 0 {
        return None;
    }
    // `statvfs`'s field widths are platform-dependent (`u32` on some targets,
    // `u64` on macOS), so the cast is a no-op on this host and load-bearing on
    // another. Silenced rather than removed for exactly that reason.
    #[allow(clippy::unnecessary_cast)]
    let free = (stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64);
    Some(free)
}

#[cfg(not(unix))]
fn free_disk_bytes(_path: &std::path::Path) -> Option<u64> {
    None
}

pub fn run(args: VacuumArgs) -> ExitCode {
    let (layout, _config) = match resolve_layout_and_config() {
        Ok(v) => v,
        Err(e) => return fail(BIN, &e),
    };

    // Take the store lock for the whole pass, exactly as a daemon would.
    //
    // A point-in-time "is a daemon running?" check is not enough, and that was
    // learned the hard way rather than reasoned: on the machine this command
    // was written for, the MCP proxy brings a daemon back within seconds of
    // `local-rag stop`, so a rewrite that takes minutes would be racing a
    // daemon that starts halfway through it. Holding the lock is what makes
    // "with the daemon stopped" true for the duration instead of true for an
    // instant — and it reuses the very machinery `D-084` built for daemon
    // exclusivity rather than inventing a second, weaker one.
    // Stop the daemon here rather than telling the operator to, and the reason
    // is measured. `local-rag stop` followed by `local-rag vacuum` loses the
    // store: on this machine the MCP proxy brought a daemon back **403 ms**
    // after `daemon stopped`, while two CLI invocations are seconds apart —
    // process startup alone dwarfs the window. Inside one process the gap
    // between "it is gone" and "the lock is mine" is microseconds, which is the
    // difference between a usable command and one that always refuses.
    match super::service::stop_running_daemon(&layout) {
        super::service::StopOutcome::NotRunning => {}
        super::service::StopOutcome::Stopped => {
            println!("vacuum: stopped the running daemon — it will come back on the next request")
        }
        super::service::StopOutcome::TimedOut { pid } => {
            return fail(
                BIN,
                &format!("daemon (pid {pid}) did not stop; `VACUUM` needs the store to itself"),
            );
        }
    }

    let guard = match acquire_store_lock(&layout) {
        Ok(g) => g,
        Err(msg) => return fail(BIN, &msg),
    };

    let outcome = run_locked(&layout, args.dry_run);
    guard.release(&layout);
    outcome
}

/// The pass itself, with the store lock already held by the caller.
#[cfg(unix)]
fn run_locked(layout: &local_rag_core::paths::StoreLayout, dry_run: bool) -> ExitCode {
    let state = match open_state(layout) {
        Ok(v) => v,
        Err(e) => return fail(BIN, &e),
    };

    let before = match block_on(
        state
            .writer()
            .read_transaction(|tx| local_rag_store::db_space(tx)),
    ) {
        Ok(v) => v,
        Err(e) => return fail(BIN, &e.to_string()),
    };
    report(&before);

    // The threshold governs the *advice* (`doctor`, `stats`), never this
    // command: someone who typed `vacuum` asked for one, and answering "I
    // decided not to" would be the surprising choice. Say so and continue.
    if !local_rag_store::should_reclaim(&before, RECLAIM_MIN_FILE_BYTES, RECLAIM_FREE_RATIO) {
        println!("vacuum: below the threshold that would advise this, running anyway as asked");
    }
    if before.auto_vacuum != AutoVacuum::Incremental {
        // Worth saying out loud: this run is also the one moment an existing
        // store can gain the ability to reclaim at idle.
        println!("vacuum: this store predates incremental reclamation and will be converted");
    }

    if dry_run {
        println!(
            "vacuum: would reclaim about {} (dry run, nothing changed)",
            human(before.free_bytes())
        );
        return ExitCode::SUCCESS;
    }

    // The rewrite builds a second copy of the *live* data beside the original,
    // so this is the number that decides whether it can finish.
    let live_bytes = before.file_bytes().saturating_sub(before.free_bytes());
    if let Some(free) = free_disk_bytes(
        layout
            .state_db()
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| layout.root().to_path_buf())
            .as_path(),
    ) {
        if free < live_bytes {
            return fail(
                BIN,
                &format!(
                    "not enough free disk: the rewrite needs about {} beside the current file and \
                     {} is available",
                    human(live_bytes),
                    human(free)
                ),
            );
        }
    } else {
        println!(
            "vacuum: free disk could not be read; the rewrite needs about {} beside the current \
             file",
            human(live_bytes)
        );
    }

    println!("vacuum: rewriting — this can take many minutes on a large store");
    if let Err(e) = block_on(state.writer().vacuum()) {
        return fail(BIN, &e.to_string());
    }

    let after = match block_on(
        state
            .writer()
            .read_transaction(|tx| local_rag_store::db_space(tx)),
    ) {
        Ok(v) => v,
        Err(e) => return fail(BIN, &e.to_string()),
    };
    report(&after);
    println!(
        "vacuum: reclaimed {}",
        human(before.file_bytes().saturating_sub(after.file_bytes()))
    );
    ExitCode::SUCCESS
}

/// Take `store.lock` for this pass, or explain why we cannot.
///
/// The same `acquire` the daemon itself uses (`D-084`), so "who owns this
/// store" has one answer and one implementation rather than two dialects. The
/// handover budget is zero on purpose: a daemon holding the store right now is
/// a reason to stop and tell the operator, not to wait ten seconds hoping it
/// leaves.
#[cfg(unix)]
fn acquire_store_lock(
    layout: &local_rag_core::paths::StoreLayout,
) -> Result<local_rag::daemon::StoreLockGuard, String> {
    let instance_uuid = SystemUuidV7.next_uuid().to_string();
    local_rag::daemon::acquire(
        layout,
        &instance_uuid,
        std::process::id(),
        local_rag_core::VERSION,
        super::system_now_ms(),
        Duration::ZERO,
    )
    .map_err(|e| {
        format!(
            "could not take the store for this pass ({e}); `VACUUM` needs it to itself — run \
             `local-rag stop` first"
        )
    })
}
