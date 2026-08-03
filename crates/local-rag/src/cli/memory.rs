//! `local-rag memory list|approve|reject|edit|retract|merge|evidence` (spec
//! 11 §6, D-025). Thin CLI adapters over the exact same domain calls
//! `crates/local-rag/src/daemon/mcp/{memory,memory_write}.rs` already make
//! (T15-04/T15-05) — parse args, open a transaction/read connection against
//! `state.sqlite`, call a domain function, print the outcome. `memory
//! evidence` is the one "inspect"-shaped read this card keeps: it already had
//! a domain function (`memory_evidence_for`) and an MCP precedent
//! (`inspect_memory_evidence`) before this task: unlike the full
//! `local-rag inspect <observation|memory|generation> <id>` command (D-025,
//! deferred to T16-02), nothing new had to be built for it.
//!
//! `--candidates` on `memory list` is an as-built refinement of the one-line
//! spec sketch, the same kind T15-07 already made for `repo attach
//! --worktree`: `pending_memory_candidate` has no scope column (spec 03
//! §2.5), so listing candidates is a flag rather than a second subcommand
//! that would otherwise need its own scope-resolution dance for nothing.

use std::process::ExitCode;

use local_rag_memory::recall as recall_pipeline;
use local_rag_store::{
    Actor, CandidateState, EditMemoryOp, MemoryEntryRow, MemoryKind, MemoryOpError, MemoryState,
    MergeLoser, MergeMemoryOp, RequestRoot, RetractMemoryOp, ReviewError, ScopeKind, apply_edit,
    apply_merge, apply_retract, approve_candidate, list_candidates, list_memory_entries_for_scope,
    memory_evidence_for, reject_candidate, resolve,
};

use local_rag::daemon::gitroot;

use super::index::open_state;
use super::{EXIT_USAGE, block_on, fail, resolve_layout_and_config, system_now_ms};

const BIN: &str = "local-rag";

const USAGE: &str = "usage: local-rag memory list|approve|reject|edit|retract|merge|evidence …";

pub fn run(mut args: impl Iterator<Item = String>) -> ExitCode {
    match args.next().as_deref() {
        Some("list") => run_list(args),
        Some("approve") => run_approve(args),
        Some("reject") => run_reject(args),
        Some("edit") => run_edit(args),
        Some("retract") => run_retract(args),
        Some("merge") => run_merge(args),
        Some("evidence") => run_evidence(args),
        Some(other) => {
            eprintln!("{BIN} memory: unknown subcommand {other:?}\n{USAGE}");
            ExitCode::from(EXIT_USAGE)
        }
        None => {
            eprintln!("{BIN} memory: {USAGE}");
            ExitCode::from(EXIT_USAGE)
        }
    }
}

// ---------------------------------------------------------------------------
// shared error formatting — mirrors `daemon/mcp/memory_write.rs`'s
// `memory_op_error_envelope`/`review_error_envelope`, minus the MCP
// `ErrorEnvelope` wrapper this crate has no reason to build here.
// ---------------------------------------------------------------------------

fn memory_op_error_message(e: &MemoryOpError) -> String {
    match e {
        MemoryOpError::UnknownMemory => "no memory entry with that id".to_string(),
        // The card's own "expected_version surfaced" requirement: both
        // numbers must reach the caller, not just "conflict".
        MemoryOpError::OptimisticConflict { expected, actual } => {
            format!("optimistic conflict: expected version {expected}, actual version {actual}")
        }
        MemoryOpError::CanonicalKeyConflict => {
            "canonical_key already exists in this scope".to_string()
        }
        MemoryOpError::InvalidGlobalScopeOwner => {
            "global scope must use the singleton scope owner".to_string()
        }
        MemoryOpError::IllegalTransition(illegal) => illegal.to_string(),
        MemoryOpError::EntryTerminal => {
            "entry is in a terminal state and cannot be edited".to_string()
        }
        MemoryOpError::IncompatibleScope => {
            "merge survivor and loser have incompatible scopes".to_string()
        }
        MemoryOpError::EmptyMergeSet => "merge requires at least one loser".to_string(),
        MemoryOpError::ModelClaimOnlyProvenance => {
            "model-claim-only evidence cannot promote to this kind".to_string()
        }
    }
}

fn review_error_message(e: &ReviewError) -> String {
    match e {
        ReviewError::UnknownCandidate => "no candidate with that id".to_string(),
        ReviewError::IllegalTransition(illegal) => illegal.to_string(),
        ReviewError::NotPending => "candidate is no longer pending".to_string(),
        ReviewError::InvalidProposedOperation(detail) => {
            format!("invalid proposed_operation: {detail}")
        }
        ReviewError::Materialization(e) => memory_op_error_message(e),
    }
}

fn print_memory_entry(row: &MemoryEntryRow) {
    let text_preview: String = row.text.chars().take(80).collect();
    println!(
        "{}  {}/{}  scope={}:{}  v{}  conf={:.2} imp={:.2}  {}",
        row.memory_id,
        row.kind.as_str(),
        row.state.as_str(),
        row.scope_kind.as_str(),
        row.scope_owner_id,
        row.entry_version,
        row.confidence,
        row.importance,
        text_preview,
    );
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

fn run_list(mut args: impl Iterator<Item = String>) -> ExitCode {
    let mut candidates_mode = false;
    let mut kind_filter: Option<MemoryKind> = None;
    let mut state_filter: Option<MemoryState> = None;
    let mut candidate_state_filter: Option<CandidateState> = None;
    let mut scope_filter: Option<ScopeKind> = None;
    let mut limit: i64 = 20;
    let mut offset: i64 = 0;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--candidates" => candidates_mode = true,
            "--kind" => match args.next().as_deref().and_then(MemoryKind::from_db) {
                Some(k) => kind_filter = Some(k),
                None => {
                    eprintln!(
                        "{BIN} memory list: --kind must be one of fact/decision/convention/procedure/task/question/hypothesis"
                    );
                    return ExitCode::from(EXIT_USAGE);
                }
            },
            "--state" => {
                let Some(raw) = args.next() else {
                    eprintln!("{BIN} memory list: --state needs a value");
                    return ExitCode::from(EXIT_USAGE);
                };
                if let Some(s) = MemoryState::from_db(&raw) {
                    state_filter = Some(s);
                } else if let Some(c) = CandidateState::from_db(&raw) {
                    candidate_state_filter = Some(c);
                } else {
                    eprintln!(
                        "{BIN} memory list: --state {raw:?} is not a valid memory or candidate state"
                    );
                    return ExitCode::from(EXIT_USAGE);
                }
            }
            "--scope" => match args.next().as_deref().and_then(ScopeKind::from_db) {
                Some(s) => scope_filter = Some(s),
                None => {
                    eprintln!(
                        "{BIN} memory list: --scope must be one of global/repository/worktree"
                    );
                    return ExitCode::from(EXIT_USAGE);
                }
            },
            "--limit" => match args.next().as_deref().and_then(|v| v.parse::<i64>().ok()) {
                Some(v) if v >= 1 => limit = v,
                _ => {
                    eprintln!("{BIN} memory list: --limit needs a positive integer");
                    return ExitCode::from(EXIT_USAGE);
                }
            },
            "--offset" => match args.next().as_deref().and_then(|v| v.parse::<i64>().ok()) {
                Some(v) if v >= 0 => offset = v,
                _ => {
                    eprintln!("{BIN} memory list: --offset needs a non-negative integer");
                    return ExitCode::from(EXIT_USAGE);
                }
            },
            other => {
                eprintln!("{BIN} memory list: unknown argument {other:?}");
                return ExitCode::from(EXIT_USAGE);
            }
        }
    }

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

    if candidates_mode {
        let rows = match list_candidates(&conn, candidate_state_filter, limit + 1, offset) {
            Ok(r) => r,
            Err(e) => return fail(BIN, &format!("could not list candidates: {e}")),
        };
        let has_more = rows.len() as i64 > limit;
        for row in rows.into_iter().take(limit as usize) {
            println!(
                "{}  {}  created_at={}",
                row.candidate_id,
                row.review_state.as_str(),
                row.created_at
            );
        }
        if has_more {
            println!(
                "(more candidates available; retry with --offset {})",
                offset + limit
            );
        }
        return ExitCode::SUCCESS;
    }

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
    let (scope_label, scopes) = recall_pipeline::scopes_for(&resolution);
    let scopes: Vec<(ScopeKind, String)> = match scope_filter {
        Some(wanted) => scopes.into_iter().filter(|(k, _)| *k == wanted).collect(),
        None => scopes,
    };

    let mut combined: Vec<MemoryEntryRow> = Vec::new();
    for (kind, owner) in &scopes {
        match list_memory_entries_for_scope(&conn, *kind, owner, kind_filter, state_filter) {
            Ok(rows) => combined.extend(rows),
            Err(e) => return fail(BIN, &format!("could not list memory entries: {e}")),
        }
    }
    combined.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.memory_id.cmp(&b.memory_id))
    });

    let total = combined.len();
    let offset_usize = usize::try_from(offset).unwrap_or(usize::MAX);
    let limit_usize = limit as usize;
    let has_more = total > offset_usize.saturating_add(limit_usize);

    println!("{BIN}: scope {scope_label}");
    for row in combined.into_iter().skip(offset_usize).take(limit_usize) {
        print_memory_entry(&row);
    }
    if has_more {
        println!(
            "(more entries available; retry with --offset {})",
            offset + limit
        );
    }
    ExitCode::SUCCESS
}

// ---------------------------------------------------------------------------
// approve / reject
// ---------------------------------------------------------------------------

fn run_approve(mut args: impl Iterator<Item = String>) -> ExitCode {
    let Some(id) = args.next() else {
        eprintln!("{BIN} memory approve: usage: {BIN} memory approve <candidate_id>");
        return ExitCode::from(EXIT_USAGE);
    };
    if let Some(extra) = args.next() {
        eprintln!("{BIN} memory approve: unknown argument {extra:?}");
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
    let outcome = block_on({
        let id = id.clone();
        async move {
            state
                .writer()
                .transaction(move |tx| approve_candidate(tx, &id, now_ms))
                .await
        }
    });

    match outcome {
        Ok(Ok(local_rag_store::ApproveCandidateOutcome::Materialized(op_outcome))) => {
            println!(
                "{BIN}: approved {id} -> memory {} (entry_version {}, audit_id {})",
                op_outcome_memory_id(&op_outcome),
                op_outcome_entry_version(&op_outcome),
                op_outcome_audit_id(&op_outcome)
            );
            ExitCode::SUCCESS
        }
        Ok(Ok(local_rag_store::ApproveCandidateOutcome::AlreadyApproved)) => {
            println!("{BIN}: {id} was already approved");
            ExitCode::SUCCESS
        }
        Ok(Err(e)) => fail(BIN, &review_error_message(&e)),
        Err(e) => fail(BIN, &format!("could not approve {id}: {e}")),
    }
}

fn run_reject(mut args: impl Iterator<Item = String>) -> ExitCode {
    let Some(id) = args.next() else {
        eprintln!("{BIN} memory reject: usage: {BIN} memory reject <candidate_id>");
        return ExitCode::from(EXIT_USAGE);
    };
    if let Some(extra) = args.next() {
        eprintln!("{BIN} memory reject: unknown argument {extra:?}");
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
    let outcome = block_on({
        let id = id.clone();
        async move {
            state
                .writer()
                .transaction(move |tx| reject_candidate(tx, &id))
                .await
        }
    });

    match outcome {
        Ok(Ok(())) => {
            println!("{BIN}: rejected {id}");
            ExitCode::SUCCESS
        }
        Ok(Err(e)) => fail(BIN, &review_error_message(&e)),
        Err(e) => fail(BIN, &format!("could not reject {id}: {e}")),
    }
}

// ---------------------------------------------------------------------------
// edit / retract
// ---------------------------------------------------------------------------

fn run_edit(mut args: impl Iterator<Item = String>) -> ExitCode {
    let Some(id) = args.next() else {
        eprintln!(
            "{BIN} memory edit: usage: {BIN} memory edit <memory_id> --expected-version N [--text T] [--importance F]"
        );
        return ExitCode::from(EXIT_USAGE);
    };
    let mut expected_version: Option<i64> = None;
    let mut text: Option<String> = None;
    let mut importance: Option<f64> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--expected-version" => match args.next().as_deref().and_then(|v| v.parse().ok()) {
                Some(v) => expected_version = Some(v),
                None => {
                    eprintln!("{BIN} memory edit: --expected-version needs an integer");
                    return ExitCode::from(EXIT_USAGE);
                }
            },
            "--text" => match args.next() {
                Some(v) => text = Some(v),
                None => {
                    eprintln!("{BIN} memory edit: --text needs a value");
                    return ExitCode::from(EXIT_USAGE);
                }
            },
            "--importance" => match args.next().as_deref().and_then(|v| v.parse::<f64>().ok()) {
                Some(v) if (0.0..=1.0).contains(&v) => importance = Some(v),
                _ => {
                    eprintln!("{BIN} memory edit: --importance needs a number between 0 and 1");
                    return ExitCode::from(EXIT_USAGE);
                }
            },
            other => {
                eprintln!("{BIN} memory edit: unknown argument {other:?}");
                return ExitCode::from(EXIT_USAGE);
            }
        }
    }
    let Some(expected_version) = expected_version else {
        eprintln!("{BIN} memory edit: --expected-version is required");
        return ExitCode::from(EXIT_USAGE);
    };
    if text.is_none() && importance.is_none() {
        eprintln!("{BIN} memory edit: at least one of --text/--importance is required");
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
    let outcome = block_on({
        let id = id.clone();
        async move {
            state
                .writer()
                .transaction(move |tx| {
                    apply_edit(
                        tx,
                        &EditMemoryOp {
                            memory_id: &id,
                            expected_version,
                            text: text.as_deref(),
                            importance,
                            actor: Actor::User,
                            idempotency_key: None,
                        },
                        now_ms,
                    )
                })
                .await
        }
    });

    match outcome {
        Ok(Ok(op_outcome)) => {
            println!(
                "{BIN}: edited {id} -> entry_version {}, audit_id {}",
                op_outcome_entry_version(&op_outcome),
                op_outcome_audit_id(&op_outcome)
            );
            ExitCode::SUCCESS
        }
        Ok(Err(e)) => fail(BIN, &memory_op_error_message(&e)),
        Err(e) => fail(BIN, &format!("could not edit {id}: {e}")),
    }
}

fn run_retract(mut args: impl Iterator<Item = String>) -> ExitCode {
    let Some(id) = args.next() else {
        eprintln!(
            "{BIN} memory retract: usage: {BIN} memory retract <memory_id> --expected-version N"
        );
        return ExitCode::from(EXIT_USAGE);
    };
    let mut expected_version: Option<i64> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--expected-version" => match args.next().as_deref().and_then(|v| v.parse().ok()) {
                Some(v) => expected_version = Some(v),
                None => {
                    eprintln!("{BIN} memory retract: --expected-version needs an integer");
                    return ExitCode::from(EXIT_USAGE);
                }
            },
            other => {
                eprintln!("{BIN} memory retract: unknown argument {other:?}");
                return ExitCode::from(EXIT_USAGE);
            }
        }
    }
    let Some(expected_version) = expected_version else {
        eprintln!("{BIN} memory retract: --expected-version is required");
        return ExitCode::from(EXIT_USAGE);
    };

    let (layout, _config) = match resolve_layout_and_config() {
        Ok(v) => v,
        Err(e) => return fail(BIN, &e),
    };
    let state = match open_state(&layout) {
        Ok(s) => s,
        Err(e) => return fail(BIN, &e),
    };
    let now_ms = system_now_ms();
    let outcome = block_on({
        let id = id.clone();
        async move {
            state
                .writer()
                .transaction(move |tx| {
                    apply_retract(
                        tx,
                        &RetractMemoryOp {
                            memory_id: &id,
                            expected_version,
                            evidence: &[],
                            actor: Actor::User,
                            idempotency_key: None,
                        },
                        now_ms,
                    )
                })
                .await
        }
    });

    match outcome {
        Ok(Ok(op_outcome)) => {
            println!(
                "{BIN}: retracted {id} -> entry_version {}, audit_id {}",
                op_outcome_entry_version(&op_outcome),
                op_outcome_audit_id(&op_outcome)
            );
            ExitCode::SUCCESS
        }
        Ok(Err(e)) => fail(BIN, &memory_op_error_message(&e)),
        Err(e) => fail(BIN, &format!("could not retract {id}: {e}")),
    }
}

// ---------------------------------------------------------------------------
// merge
// ---------------------------------------------------------------------------

fn parse_id_version(spec: &str) -> Option<(String, i64)> {
    let (id, version) = spec.rsplit_once(':')?;
    let version: i64 = version.parse().ok()?;
    if id.is_empty() {
        return None;
    }
    Some((id.to_string(), version))
}

fn run_merge(mut args: impl Iterator<Item = String>) -> ExitCode {
    let mut survivor: Option<(String, i64)> = None;
    let mut losers: Vec<(String, i64)> = Vec::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--survivor" => match args.next().as_deref().and_then(parse_id_version) {
                Some(v) => survivor = Some(v),
                None => {
                    eprintln!(
                        "{BIN} memory merge: --survivor needs <memory_id>:<expected_version>"
                    );
                    return ExitCode::from(EXIT_USAGE);
                }
            },
            "--loser" => match args.next().as_deref().and_then(parse_id_version) {
                Some(v) => losers.push(v),
                None => {
                    eprintln!("{BIN} memory merge: --loser needs <memory_id>:<expected_version>");
                    return ExitCode::from(EXIT_USAGE);
                }
            },
            other => {
                eprintln!("{BIN} memory merge: unknown argument {other:?}");
                return ExitCode::from(EXIT_USAGE);
            }
        }
    }
    let Some((survivor_id, survivor_expected_version)) = survivor else {
        eprintln!(
            "{BIN} memory merge: usage: {BIN} memory merge --survivor <id>:<version> --loser <id>:<version> [--loser ...]"
        );
        return ExitCode::from(EXIT_USAGE);
    };
    if losers.is_empty() {
        eprintln!("{BIN} memory merge: at least one --loser is required");
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
    let outcome = block_on({
        let survivor_id = survivor_id.clone();
        let losers = losers.clone();
        async move {
            state
                .writer()
                .transaction(move |tx| {
                    let loser_structs: Vec<MergeLoser<'_>> = losers
                        .iter()
                        .map(|(id, expected_version)| MergeLoser {
                            memory_id: id,
                            expected_version: *expected_version,
                        })
                        .collect();
                    apply_merge(
                        tx,
                        &MergeMemoryOp {
                            survivor_id: &survivor_id,
                            survivor_expected_version,
                            losers: &loser_structs,
                            actor: Actor::User,
                            idempotency_key: None,
                        },
                        now_ms,
                    )
                })
                .await
        }
    });

    match outcome {
        Ok(Ok(op_outcome)) => {
            println!(
                "{BIN}: merged {} loser(s) into {survivor_id} -> entry_version {}, audit_id {}",
                losers.len(),
                op_outcome_entry_version(&op_outcome),
                op_outcome_audit_id(&op_outcome)
            );
            ExitCode::SUCCESS
        }
        Ok(Err(e)) => fail(BIN, &memory_op_error_message(&e)),
        Err(e) => fail(BIN, &format!("could not merge into {survivor_id}: {e}")),
    }
}

// ---------------------------------------------------------------------------
// evidence
// ---------------------------------------------------------------------------

fn run_evidence(mut args: impl Iterator<Item = String>) -> ExitCode {
    let Some(id) = args.next() else {
        eprintln!("{BIN} memory evidence: usage: {BIN} memory evidence <memory_id>");
        return ExitCode::from(EXIT_USAGE);
    };
    if let Some(extra) = args.next() {
        eprintln!("{BIN} memory evidence: unknown argument {extra:?}");
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
    let conn = match state.open_read() {
        Ok(c) => c,
        Err(e) => return fail(BIN, &format!("could not open state.sqlite: {e}")),
    };
    let ids = match memory_evidence_for(&conn, &id) {
        Ok(v) => v,
        Err(e) => return fail(BIN, &format!("could not read evidence for {id}: {e}")),
    };
    if ids.is_empty() {
        println!("{BIN}: {id} has no evidence");
    } else {
        for observation_id in ids {
            println!("{observation_id}");
        }
    }
    ExitCode::SUCCESS
}

// ---------------------------------------------------------------------------
// MemoryOpOutcome field access — `Applied`/`Replayed` both carry the same
// `MemoryOpResult`; small helpers keep the call sites above from repeating
// the match.
// ---------------------------------------------------------------------------

fn op_outcome_memory_id(outcome: &local_rag_store::MemoryOpOutcome) -> &str {
    match outcome {
        local_rag_store::MemoryOpOutcome::Applied(r)
        | local_rag_store::MemoryOpOutcome::Replayed(r) => &r.memory_id,
    }
}

fn op_outcome_entry_version(outcome: &local_rag_store::MemoryOpOutcome) -> i64 {
    match outcome {
        local_rag_store::MemoryOpOutcome::Applied(r)
        | local_rag_store::MemoryOpOutcome::Replayed(r) => r.entry_version,
    }
}

fn op_outcome_audit_id(outcome: &local_rag_store::MemoryOpOutcome) -> i64 {
    match outcome {
        local_rag_store::MemoryOpOutcome::Applied(r)
        | local_rag_store::MemoryOpOutcome::Replayed(r) => r.audit_id,
    }
}
