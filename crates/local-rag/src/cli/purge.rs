//! `local-rag purge --memory <id> --expected-version N|--session <id>|--all`
//! (spec 08 §3, 12 §3, T16-02) — the only hard-delete path, gated by an
//! explicit selector and an explicit `--yes` confirmation on every one of the
//! three modes (not just `--all`): purge is the one command in this CLI that
//! can make content actually disappear, so every mode gets the same
//! "compute-and-print what would happen, then require confirmation" shape,
//! not just the broadest one. `--memory` additionally requires
//! `--expected-version`, the same optimistic-concurrency contract `memory
//! edit`/`retract`/`merge` already give — a purge target the operator has not
//! freshly inspected should not be able to silently remove a row that changed
//! underneath them.

use std::process::ExitCode;

use local_rag_store::{
    PurgeMemoryError, SubjectKind, delete_all_memory_embeddings, delete_embeddings_for_subject,
    preview_purge_all, preview_purge_memory, preview_purge_session, purge_all, purge_memory,
    purge_session,
};

use super::{EXIT_USAGE, block_on, fail, resolve_layout_and_config, system_now_ms};
use local_rag::indexing::{open_cache, open_state};

const BIN: &str = "local-rag";

const USAGE: &str = "usage: local-rag purge --memory <id> --expected-version N --yes \
     | --session <id> --yes | --all --yes";

#[derive(Debug, clap::Args)]
pub struct PurgeArgs {
    #[arg(long = "memory")]
    memory_id: Option<String>,
    #[arg(long = "session")]
    session_id: Option<String>,
    #[arg(long)]
    all: bool,
    #[arg(long)]
    expected_version: Option<i64>,
    #[arg(long)]
    yes: bool,
}

pub fn run(args: PurgeArgs) -> ExitCode {
    let PurgeArgs {
        memory_id,
        session_id,
        all,
        expected_version,
        yes,
    } = args;

    let selector_count = [memory_id.is_some(), session_id.is_some(), all]
        .iter()
        .filter(|b| **b)
        .count();
    if selector_count != 1 {
        eprintln!("{BIN} purge: exactly one of --memory/--session/--all is required\n{USAGE}");
        return ExitCode::from(EXIT_USAGE);
    }

    let (layout, _config) = match resolve_layout_and_config() {
        Ok(v) => v,
        Err(e) => return fail(BIN, &e),
    };
    let state = match open_state(&layout) {
        Ok(s) => s,
        Err(e) => return fail(BIN, &e),
    };
    let now_ms = system_now_ms();

    if let Some(id) = memory_id {
        let Some(expected_version) = expected_version else {
            eprintln!("{BIN} purge: --expected-version is required with --memory");
            return ExitCode::from(EXIT_USAGE);
        };
        return run_purge_memory(&state, &layout, &id, expected_version, yes, now_ms);
    }
    if expected_version.is_some() {
        eprintln!("{BIN} purge: --expected-version is only valid with --memory");
        return ExitCode::from(EXIT_USAGE);
    }
    if let Some(id) = session_id {
        return run_purge_session(&state, &id, yes);
    }
    run_purge_all(&state, &layout, yes, now_ms)
}

/// Delete the `embedding_cache` rows a purge is about to orphan (`D-074`).
///
/// Runs **before** the `state.sqlite` transaction, which is the reverse of
/// this system's usual "state is the source of truth, the cache catches up".
/// That rule is right for derivation and wrong here: a cache row's key is
/// `H(memory_id, H(text))`, and the purge deletes the text, so after the state
/// commit there is nothing left to derive the key from and an orphaned vector
/// of private text becomes permanently unfindable. In this order the failure
/// window points the safe way — a crash leaves a vector the next backfill
/// recomputes, never one nothing can find.
///
/// Returns the number of rows removed, or an error message. A cache that
/// cannot be opened is fatal to the purge on purpose: silently purging the
/// entry while leaving its vector behind is precisely the defect `D-074`
/// records.
fn purge_vectors(
    state: &local_rag_store::StateDb,
    layout: &local_rag_core::paths::StoreLayout,
    subject_hash: Option<&str>,
) -> Result<u64, String> {
    let cache = block_on(open_cache(state, layout))?;
    let hash = subject_hash.map(str::to_string);
    block_on(async move {
        cache
            .writer()
            .transaction(move |tx| match hash.as_deref() {
                Some(h) => delete_embeddings_for_subject(tx, SubjectKind::MemoryEntry, h),
                None => delete_all_memory_embeddings(tx),
            })
            .await
    })
    .map_err(|e| format!("could not remove cached vectors: {e}"))
}

fn run_purge_memory(
    state: &local_rag_store::StateDb,
    layout: &local_rag_core::paths::StoreLayout,
    id: &str,
    expected_version: i64,
    yes: bool,
    now_ms: i64,
) -> ExitCode {
    let conn = match state.open_read() {
        Ok(c) => c,
        Err(e) => return fail(BIN, &format!("could not open state.sqlite: {e}")),
    };
    let preview = match preview_purge_memory(&conn, id) {
        Ok(p) => p,
        Err(e) => {
            return fail(
                BIN,
                &format!("could not preview purge for memory {id}: {e}"),
            );
        }
    };
    if !preview.exists {
        return fail(BIN, &format!("no memory entry with id {id}"));
    }
    // Read while the text is still there: after the purge the subject hash
    // cannot be derived at all (see `purge_vectors`).
    let subject_hash = match local_rag_store::memory_subject_hash(&conn, id) {
        Ok(h) => h,
        Err(e) => {
            return fail(
                BIN,
                &format!("could not read the subject of memory {id}: {e}"),
            );
        }
    };
    drop(conn);

    if !yes {
        println!(
            "{BIN} purge: would purge memory {id} v{} ({} evidence rows, {} descendant relinks); pass --yes to confirm",
            preview.current_version.unwrap_or_default(),
            preview.evidence_rows,
            preview.descendant_rows,
        );
        return ExitCode::from(EXIT_USAGE);
    }

    let vectors_removed = match purge_vectors(state, layout, subject_hash.as_deref()) {
        Ok(n) => n,
        Err(e) => return fail(BIN, &e),
    };

    let id_owned = id.to_string();
    let outcome = block_on(async {
        state
            .writer()
            .transaction(move |tx| purge_memory(tx, &id_owned, expected_version, now_ms))
            .await
    });
    match outcome {
        Ok(Ok(report)) => {
            println!(
                "{BIN}: purged memory {id} ({} evidence rows removed, {} descendants relinked, {} audit rows tombstoned, {} normalization rows removed, {vectors_removed} cached vectors removed)",
                report.evidence_rows_removed,
                report.descendants_relinked,
                report.audit_rows_tombstoned,
                report.normalization_rows_removed,
            );
            ExitCode::SUCCESS
        }
        Ok(Err(PurgeMemoryError::UnknownMemory)) => {
            fail(BIN, &format!("no memory entry with id {id}"))
        }
        Ok(Err(PurgeMemoryError::OptimisticConflict { expected, actual })) => fail(
            BIN,
            &format!("optimistic conflict: expected version {expected}, actual version {actual}"),
        ),
        Err(e) => fail(BIN, &format!("could not purge memory {id}: {e}")),
    }
}

fn run_purge_session(state: &local_rag_store::StateDb, id: &str, yes: bool) -> ExitCode {
    let conn = match state.open_read() {
        Ok(c) => c,
        Err(e) => return fail(BIN, &format!("could not open state.sqlite: {e}")),
    };
    let preview = match preview_purge_session(&conn, id) {
        Ok(p) => p,
        Err(e) => {
            return fail(
                BIN,
                &format!("could not preview purge for session {id}: {e}"),
            );
        }
    };
    drop(conn);

    if !yes {
        println!(
            "{BIN} purge: would purge session {id} ({} observations); pass --yes to confirm",
            preview.observations,
        );
        return ExitCode::from(EXIT_USAGE);
    }

    let id_owned = id.to_string();
    let outcome = block_on(async {
        state
            .writer()
            .transaction(move |tx| purge_session(tx, &id_owned))
            .await
    });
    match outcome {
        Ok(report) => {
            println!(
                "{BIN}: purged session {id} ({} observations, {} candidate_evidence rows, {} memory_evidence rows removed)",
                report.observations_purged,
                report.candidate_evidence_rows_removed,
                report.memory_evidence_rows_removed,
            );
            ExitCode::SUCCESS
        }
        Err(e) => fail(BIN, &format!("could not purge session {id}: {e}")),
    }
}

fn run_purge_all(
    state: &local_rag_store::StateDb,
    layout: &local_rag_core::paths::StoreLayout,
    yes: bool,
    now_ms: i64,
) -> ExitCode {
    let conn = match state.open_read() {
        Ok(c) => c,
        Err(e) => return fail(BIN, &format!("could not open state.sqlite: {e}")),
    };
    let preview = match preview_purge_all(&conn) {
        Ok(p) => p,
        Err(e) => return fail(BIN, &format!("could not preview purge --all: {e}")),
    };
    drop(conn);

    if !yes {
        println!(
            "{BIN} purge: would purge {} memory entries and {} sessions ({} observations); pass --yes to confirm",
            preview.memory_entries, preview.sessions, preview.observations,
        );
        return ExitCode::from(EXIT_USAGE);
    }

    // `None` means "every memory subject", not "the subjects of the entries
    // about to go": `purge --all` removes them all anyway, and the wholesale
    // form additionally takes any row that has already lost its entry. An
    // orphan is exactly what must not survive this operation.
    let vectors_removed = match purge_vectors(state, layout, None) {
        Ok(n) => n,
        Err(e) => return fail(BIN, &e),
    };

    let outcome = block_on(async {
        state
            .writer()
            .transaction(move |tx| purge_all(tx, now_ms))
            .await
    });
    match outcome {
        Ok(report) => {
            println!(
                "{BIN}: purged everything ({} memory entries, {} sessions, {} observations, {vectors_removed} cached vectors removed)",
                report.memory_entries_purged, report.sessions_purged, report.observations_purged,
            );
            ExitCode::SUCCESS
        }
        Err(e) => fail(BIN, &format!("could not purge --all: {e}")),
    }
}
