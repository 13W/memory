//! `local-rag memory
//! list|approve|reject|edit|retract|confirm|refute|merge|rescope|evidence`
//! (spec 11 §6, D-025; `rescope` is X-009, `confirm`/`refute` are D-079). Thin CLI adapters over the exact same domain calls
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

use local_rag_core::identity::{SystemUuidV7, UuidSource};
use local_rag_memory::recall as recall_pipeline;
use local_rag_store::{
    Actor, CandidateState, ConfirmMemoryOp, EditMemoryOp, GLOBAL_SCOPE_OWNER_ID, MemoryEntryRow,
    MemoryKind, MemoryOpError, MemoryState, MergeLoser, MergeMemoryOp, RejectMemoryOp, RequestRoot,
    RetractMemoryOp, ReviewError, ScopeKind, SupersedeMemoryOp, apply_confirm, apply_edit,
    apply_merge, apply_reject, apply_retract, apply_supersede, approve_candidate, list_candidates,
    list_memory_entries_for_scope, memory_entry_by_id, memory_evidence_for, reject_candidate,
    resolve,
};

use local_rag::daemon::gitroot;

use super::{
    EXIT_USAGE, block_on, fail, parse_scope_kind, resolve_layout_and_config, system_now_ms,
};
use local_rag::indexing::open_state;

const BIN: &str = "local-rag";

fn parse_memory_kind(raw: &str) -> Result<MemoryKind, String> {
    MemoryKind::from_db(raw).ok_or_else(|| {
        "must be one of fact/decision/convention/procedure/task/question/hypothesis".to_string()
    })
}

#[derive(Debug, clap::Args)]
pub struct MemoryListArgs {
    /// List pending review candidates instead of durable memory entries.
    #[arg(long)]
    candidates: bool,
    #[arg(long, value_parser = parse_memory_kind)]
    kind: Option<MemoryKind>,
    /// A memory state (active/superseded/retracted/…) or, with
    /// `--candidates`, a candidate review state — the same free-text
    /// vocabulary this filter always accepted, since one flag covers both.
    #[arg(long)]
    state: Option<String>,
    #[arg(long, value_parser = parse_scope_kind)]
    scope: Option<ScopeKind>,
    #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(i64).range(1..))]
    limit: i64,
    #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(i64).range(0..))]
    offset: i64,
}

#[derive(Debug, clap::Subcommand)]
pub enum MemoryCommand {
    /// List durable memory entries (or, with `--candidates`, pending review candidates).
    List(MemoryListArgs),
    Approve {
        candidate_id: String,
    },
    /// Reject a pending review candidate (spec 04 §6).
    ///
    /// This is the candidate-review verb. To reject a durable hypothesis
    /// entry, see `refute`.
    Reject {
        candidate_id: String,
    },
    Edit {
        memory_id: String,
        #[arg(long)]
        expected_version: i64,
        #[arg(long)]
        text: Option<String>,
        #[arg(long)]
        importance: Option<f64>,
    },
    Retract {
        memory_id: String,
        #[arg(long)]
        expected_version: i64,
    },
    /// Confirm a hypothesis on strong evidence (`active` -> `confirmed`).
    ///
    /// Spec 04 §5. The entry keeps `kind=hypothesis` and stays eligible for
    /// recall as high trust; promotion to a `fact` is a separate, explicit
    /// `supersede`, not this command. Legal only for `hypothesis`.
    Confirm {
        memory_id: String,
        #[arg(long)]
        expected_version: i64,
    },
    /// Reject a hypothesis entry the evidence disproves (`active` ->
    /// `rejected`).
    ///
    /// Spec 04 §5. Terminal: recall stops showing the entry, which survives
    /// for review. Named `refute`, not `reject`, because `memory reject`
    /// already means "reject a pending review candidate" — a different
    /// table. Illegal once the hypothesis is confirmed; from there the only
    /// exit is `supersede`.
    Refute {
        memory_id: String,
        #[arg(long)]
        expected_version: i64,
    },
    Merge {
        /// `<memory_id>:<expected_version>`.
        #[arg(long)]
        survivor: String,
        /// `<memory_id>:<expected_version>`; repeat for multiple losers.
        #[arg(long = "loser")]
        losers: Vec<String>,
    },
    /// Move an entry into another scope by superseding it with an identical
    /// entry there (X-009).
    Rescope {
        memory_id: String,
        #[arg(long)]
        expected_version: i64,
        #[arg(long, value_parser = parse_scope_kind)]
        scope: ScopeKind,
        /// Directory whose registered worktree names the target repository or
        /// worktree (defaults to the current directory). Unused for `global`.
        #[arg(long)]
        root: Option<std::path::PathBuf>,
    },
    Evidence {
        memory_id: String,
    },
}

pub fn run(command: MemoryCommand) -> ExitCode {
    match command {
        MemoryCommand::List(args) => run_list(args),
        MemoryCommand::Approve { candidate_id } => run_approve(candidate_id),
        MemoryCommand::Reject { candidate_id } => run_reject(candidate_id),
        MemoryCommand::Edit {
            memory_id,
            expected_version,
            text,
            importance,
        } => run_edit(memory_id, expected_version, text, importance),
        MemoryCommand::Retract {
            memory_id,
            expected_version,
        } => run_retract(memory_id, expected_version),
        MemoryCommand::Confirm {
            memory_id,
            expected_version,
        } => run_confirm(memory_id, expected_version),
        MemoryCommand::Refute {
            memory_id,
            expected_version,
        } => run_refute(memory_id, expected_version),
        MemoryCommand::Merge { survivor, losers } => run_merge(survivor, losers),
        MemoryCommand::Rescope {
            memory_id,
            expected_version,
            scope,
            root,
        } => run_rescope(memory_id, expected_version, scope, root),
        MemoryCommand::Evidence { memory_id } => run_evidence(memory_id),
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

fn run_list(args: MemoryListArgs) -> ExitCode {
    let MemoryListArgs {
        candidates: candidates_mode,
        kind: kind_filter,
        state: state_raw,
        scope: scope_filter,
        limit,
        offset,
    } = args;

    let mut state_filter: Option<MemoryState> = None;
    let mut candidate_state_filter: Option<CandidateState> = None;
    if let Some(raw) = state_raw {
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

fn run_approve(id: String) -> ExitCode {
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

fn run_reject(id: String) -> ExitCode {
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

fn run_edit(
    id: String,
    expected_version: i64,
    text: Option<String>,
    importance: Option<f64>,
) -> ExitCode {
    if let Some(v) = importance
        && !(0.0..=1.0).contains(&v)
    {
        eprintln!("{BIN} memory edit: --importance needs a number between 0 and 1");
        return ExitCode::from(EXIT_USAGE);
    }
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

/// `memory rescope` (X-009): move an entry into another scope by superseding
/// it with an identical entry there.
///
/// Scope is not editable — `edit`'s patch is `text`/`importance` only (spec 08
/// §3), and for good reason: an entry's scope is half of its
/// `(scope_kind, scope_owner_id, canonical_key)` identity. `supersede` is the
/// op that *does* take a new scope, so this command is a thin adapter over it
/// rather than a new store primitive, and the move stays inside the ledger:
/// one transaction, ordinary audit rows, the old entry preserved as
/// `superseded` with the successor's `supersedes_id` pointing back at it.
///
/// Evidence rows are deliberately not copied onto the successor — they stay on
/// the superseded original, which the `supersedes_id` chain keeps reachable
/// (`memory evidence <old_id>` still answers). Duplicating them would double-
/// count the same observations as independent support.
fn run_rescope(
    id: String,
    expected_version: i64,
    scope: ScopeKind,
    root: Option<std::path::PathBuf>,
) -> ExitCode {
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

    let Some(entry) = (match memory_entry_by_id(&conn, &id) {
        Ok(e) => e,
        Err(e) => return fail(BIN, &format!("could not read {id}: {e}")),
    }) else {
        return fail(BIN, &format!("unknown memory entry {id}"));
    };

    let owner_id = match scope {
        ScopeKind::Global => GLOBAL_SCOPE_OWNER_ID.to_string(),
        ScopeKind::Repository | ScopeKind::Worktree => {
            let target = match root {
                Some(p) => p,
                None => match std::env::current_dir() {
                    Ok(cwd) => cwd,
                    Err(e) => {
                        return fail(
                            BIN,
                            &format!("could not determine the current directory: {e}"),
                        );
                    }
                },
            };
            let resolution = match resolve(
                &conn,
                &RequestRoot {
                    worktree_root: gitroot::probe(&target),
                    repo_hint: None,
                },
            ) {
                Ok(r) => r,
                Err(e) => return fail(BIN, &format!("could not resolve worktree identity: {e}")),
            };
            match (&resolution, scope) {
                (local_rag_store::Resolution::Resolved { repo_id, .. }, ScopeKind::Repository) => {
                    repo_id.clone()
                }
                (
                    local_rag_store::Resolution::Resolved { worktree_id, .. },
                    ScopeKind::Worktree,
                ) => worktree_id.clone(),
                // Never a silent fallback to `global` — that is exactly the
                // silent degradation D-064 removed from `remember`.
                _ => {
                    return fail(
                        BIN,
                        &format!(
                            "{} does not resolve to a registered worktree, so there is no \
                             {} scope to move {id} into — index it first (`local-rag index \
                             <path>`)",
                            target.display(),
                            scope.as_str()
                        ),
                    );
                }
            }
        }
    };

    if entry.scope_kind == scope && entry.scope_owner_id == owner_id {
        println!(
            "{BIN}: {id} is already {} scope (owner {owner_id}); nothing to do",
            scope.as_str()
        );
        return ExitCode::SUCCESS;
    }
    drop(conn);

    let new_memory_id = SystemUuidV7.next_uuid().to_string();
    let now_ms = system_now_ms();
    let outcome = block_on({
        let id = id.clone();
        let new_memory_id = new_memory_id.clone();
        async move {
            state
                .writer()
                .transaction(move |tx| {
                    apply_supersede(
                        tx,
                        &SupersedeMemoryOp {
                            old_memory_id: &id,
                            old_expected_version: expected_version,
                            new_memory_id: &new_memory_id,
                            new_kind: entry.kind,
                            new_text: &entry.text,
                            new_canonical_key: entry.canonical_key.as_deref(),
                            new_scope_kind: scope,
                            new_scope_owner_id: &owner_id,
                            new_confidence: entry.confidence,
                            new_importance: entry.importance,
                            new_valid_from_tree: entry.valid_from_tree.as_deref(),
                            new_last_verified_tree: entry.last_verified_tree.as_deref(),
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
                "{BIN}: {id} superseded by {new_memory_id} in {} scope -> entry_version {}, \
                 audit_id {}",
                scope.as_str(),
                op_outcome_entry_version(&op_outcome),
                op_outcome_audit_id(&op_outcome)
            );
            ExitCode::SUCCESS
        }
        Ok(Err(e)) => fail(BIN, &memory_op_error_message(&e)),
        Err(e) => fail(BIN, &format!("could not rescope {id}: {e}")),
    }
}

fn run_retract(id: String, expected_version: i64) -> ExitCode {
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
// confirm / refute (D-079)
//
// The `hypothesis` machine's own two verbs. Same shape as `run_retract`, which
// they sit next to deliberately: whichever kind an entry has, the CLI's
// state-moving commands are one domain call in one transaction with an
// `--expected-version` precondition, and nothing else.
// ---------------------------------------------------------------------------

fn run_confirm(id: String, expected_version: i64) -> ExitCode {
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
                    apply_confirm(
                        tx,
                        &ConfirmMemoryOp {
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
                "{BIN}: confirmed {id} -> entry_version {}, audit_id {}",
                op_outcome_entry_version(&op_outcome),
                op_outcome_audit_id(&op_outcome)
            );
            ExitCode::SUCCESS
        }
        Ok(Err(e)) => fail(BIN, &memory_op_error_message(&e)),
        Err(e) => fail(BIN, &format!("could not confirm {id}: {e}")),
    }
}

fn run_refute(id: String, expected_version: i64) -> ExitCode {
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
                    apply_reject(
                        tx,
                        &RejectMemoryOp {
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
                "{BIN}: rejected {id} -> entry_version {}, audit_id {}",
                op_outcome_entry_version(&op_outcome),
                op_outcome_audit_id(&op_outcome)
            );
            ExitCode::SUCCESS
        }
        Ok(Err(e)) => fail(BIN, &memory_op_error_message(&e)),
        Err(e) => fail(BIN, &format!("could not reject {id}: {e}")),
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

fn run_merge(survivor: String, losers: Vec<String>) -> ExitCode {
    let Some((survivor_id, survivor_expected_version)) = parse_id_version(&survivor) else {
        eprintln!("{BIN} memory merge: --survivor needs <memory_id>:<expected_version>");
        return ExitCode::from(EXIT_USAGE);
    };
    if losers.is_empty() {
        eprintln!("{BIN} memory merge: at least one --loser is required");
        return ExitCode::from(EXIT_USAGE);
    }
    let mut parsed_losers: Vec<(String, i64)> = Vec::with_capacity(losers.len());
    for loser in &losers {
        let Some(v) = parse_id_version(loser) else {
            eprintln!("{BIN} memory merge: --loser needs <memory_id>:<expected_version>");
            return ExitCode::from(EXIT_USAGE);
        };
        parsed_losers.push(v);
    }
    let losers = parsed_losers;

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

fn run_evidence(id: String) -> ExitCode {
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
