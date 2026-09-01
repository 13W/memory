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
//!
//! # `list --candidates --grouped` and `dedup` (`T23-08`)
//!
//! `local-rag memory list --candidates` prints only id/state/created_at —
//! before `T23-08` an operator had no way to see that hundreds of pending
//! rows said the same thing. `--grouped` prints one line per exact-duplicate
//! group instead (`local_rag_store::pending_candidate_groups`), with a claim
//! preview.
//!
//! `local-rag memory dedup` is the write half: `--candidate <id>` folds
//! exactly the group the named candidate belongs to
//! (`local_rag_store::fold_pending_duplicates`); `--all` folds every group in
//! the pending queue (`local_rag_store::fold_all_pending_duplicates`).
//! Exactly one of the two is required — there is no default mode, so this
//! command can never touch a group its caller did not name. `--dry-run`
//! reports what would be folded without writing anything (the `gc`-style
//! flag: without it, the command applies — `--all` is itself the explicit
//! opt-in word for a full-queue fold).
//!
//! Folding only ever transitions an exact duplicate `pending -> rejected`
//! (the state [`reject_candidate`] already produces); it never approves
//! anything, and the survivor of every group stays `pending` for the
//! existing approve/reject/edit path, which remains the only way a
//! candidate becomes an entry (ADR-0014 Decision 2, `D-131`). This command
//! is deliberately not reachable from `local-rag gc` — see that command's
//! own module doc.

use std::process::ExitCode;

use local_rag_core::identity::{SystemUuidV7, UuidSource};
use local_rag_memory::recall as recall_pipeline;
use local_rag_store::{
    Actor, CANDIDATE_EXPIRY_MS, CandidateState, ConfirmMemoryOp, DedupTriageReport, EditMemoryOp,
    FoldDuplicatesOutcome, FoldRetained, GLOBAL_SCOPE_OWNER_ID, MemoryEntryRow, MemoryKind,
    MemoryOpError, MemoryState, MergeLoser, MergeMemoryOp, ProposedOperation, RejectMemoryOp,
    RequestRoot, RetractMemoryOp, ReviewError, ScopeKind, SupersedeMemoryOp, apply_confirm,
    apply_edit, apply_merge, apply_reject, apply_retract, apply_supersede, approve_candidate,
    candidate_state, fold_all_pending_duplicates, fold_pending_duplicates, list_candidates,
    list_memory_entries_for_scope, memory_entry_by_id, memory_evidence_for, pending_candidate_ages,
    pending_candidate_groups, reject_candidate, resolve,
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
    /// With `--candidates`: one line per exact-duplicate group instead of
    /// one line per candidate (`T23-08`).
    #[arg(long, requires = "candidates")]
    grouped: bool,
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
    /// Fold exact-duplicate pending candidates (`T23-08`, ADR-0014
    /// Decision 2). Exactly one of `--candidate`/`--all` is required — see
    /// the module doc.
    Dedup {
        /// Fold the exact-duplicate group this candidate belongs to (any
        /// member of the group may be named, not necessarily the group's
        /// oldest).
        #[arg(long, conflicts_with = "all")]
        candidate: Option<String>,
        /// Fold every exact-duplicate group in the pending queue.
        #[arg(long)]
        all: bool,
        /// Report what would be folded without writing anything.
        #[arg(long)]
        dry_run: bool,
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
        MemoryCommand::Dedup {
            candidate,
            all,
            dry_run,
        } => run_dedup(candidate, all, dry_run),
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

/// An 80-character preview of what a `pending_memory_candidate` row's
/// `proposed_operation` JSON claims — `T23-08`'s reason `list --candidates
/// --grouped` exists at all: plain `list --candidates` never shows this, so
/// there was previously no way to see that two rows said the same thing.
/// Unparsable JSON says so rather than panicking; `pending_candidate_groups`
/// itself already excludes such rows from every group, so this only ever
/// fires for a row this preview was handed out of band.
fn candidate_claim_preview(proposed_operation_json: &str) -> String {
    let preview80 = |s: &str| -> String { s.chars().take(80).collect() };
    match serde_json::from_str::<ProposedOperation>(proposed_operation_json) {
        Ok(ProposedOperation::Create { text, .. }) => preview80(&text),
        Ok(ProposedOperation::Reinforce { memory_id, .. }) => format!("reinforce {memory_id}"),
        Ok(ProposedOperation::Resolve { memory_id, .. }) => format!("resolve {memory_id}"),
        Ok(ProposedOperation::Retract { memory_id, .. }) => format!("retract {memory_id}"),
        Ok(ProposedOperation::Supersede {
            old_memory_id,
            new_text,
            ..
        }) => format!("supersede {old_memory_id} -> {}", preview80(&new_text)),
        Err(_) => "<unparsable proposed_operation>".to_string(),
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
        grouped,
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

    if candidates_mode && grouped {
        // `T23-08`: grouping is store-wide (no `review_state` filter to
        // apply — `pending_candidate_groups` only ever sees `pending` rows)
        // and the point is precisely to show what plain `list --candidates`
        // cannot: how many rows say the same thing.
        let groups = match pending_candidate_groups(&conn) {
            Ok(g) => g,
            Err(e) => return fail(BIN, &format!("could not group candidates: {e}")),
        };
        let distinct = groups.len();
        let total: usize = groups.iter().map(|g| 1 + g.duplicate_ids.len()).sum();
        let largest = groups
            .iter()
            .map(|g| 1 + g.duplicate_ids.len())
            .max()
            .unwrap_or(0);

        let offset_usize = usize::try_from(offset).unwrap_or(usize::MAX);
        let limit_usize = limit as usize;
        let has_more = groups.len() > offset_usize.saturating_add(limit_usize);

        for group in groups.iter().skip(offset_usize).take(limit_usize) {
            println!(
                "{}x  {}  created_at={}  {}",
                1 + group.duplicate_ids.len(),
                group.survivor.candidate_id,
                group.survivor.created_at,
                candidate_claim_preview(&group.survivor.proposed_operation),
            );
        }
        if has_more {
            println!(
                "(more groups available; retry with --offset {})",
                offset + limit
            );
        }
        println!("{total} pending over {distinct} distinct claim(s) (largest group: {largest}x)");
        return ExitCode::SUCCESS;
    }

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
// dedup (`T23-08`, ADR-0014 Decision 2) — see the module doc.
// ---------------------------------------------------------------------------

fn fold_retained_message(reason: &FoldRetained) -> &'static str {
    match reason {
        FoldRetained::ConflictsDiffer => "its conflicts differ from the survivor's",
        FoldRetained::UnparsableProposal => "its proposed_operation no longer parses",
        FoldRetained::SurvivorNoLongerPending => {
            "the survivor moved out of pending before this could apply"
        }
    }
}

fn print_fold_outcome(id: &str, outcome: &FoldDuplicatesOutcome) {
    if outcome.folded.is_empty() {
        println!(
            "{BIN}: {id} has no exact duplicates — nothing to fold; use `memory approve`/`memory \
             reject` to decide it"
        );
    } else {
        println!(
            "{BIN}: folded {} duplicate(s) of {id} into survivor {} (dedup key {}), linked {} \
             evidence row(s)",
            outcome.folded.len(),
            outcome.survivor_candidate_id,
            outcome.key.as_str(),
            outcome.evidence_linked,
        );
        for twin in &outcome.folded {
            println!("  rejected {twin}");
        }
    }
    for (twin, reason) in &outcome.retained {
        println!("  retained {twin}: {}", fold_retained_message(reason));
    }
}

fn print_triage_report(report: &DedupTriageReport) {
    let verb = if report.dry_run {
        "would reject"
    } else {
        "rejected"
    };
    println!(
        "{BIN}: {verb} {} duplicate candidate(s) across {} group(s) — {} distinct claim(s) \
         {} pending",
        report.folded.len(),
        report.groups_folded,
        report.distinct_claims_after,
        if report.dry_run {
            "would remain"
        } else {
            "remain"
        },
    );
    if !report.dry_run {
        println!(
            "  linked {} evidence row(s) onto survivors",
            report.evidence_linked
        );
    }
    if !report.retained.is_empty() {
        println!("  {} row(s) retained, not folded:", report.retained.len());
        for (twin, reason) in &report.retained {
            println!("    {twin}: {}", fold_retained_message(reason));
        }
    }
}

/// `CANDIDATE_EXPIRY_MS - 7 days`, in milliseconds: `local-rag gc`'s own
/// expiry sweep threshold minus a week of headroom — a fold makes every
/// surviving candidate the *oldest* of its group (`fold_pending_duplicates`'
/// tie-break), so a reduction here can, without warning, hand a routine `gc`
/// run a queue's worth of rows to expire (`T23-08`'s own design note).
const NEAR_EXPIRY_WARNING_MS: i64 = CANDIDATE_EXPIRY_MS - 7 * 24 * 60 * 60 * 1_000;

fn warn_if_pending_near_expiry(state: &local_rag_store::StateDb) {
    let Ok(conn) = state.open_read() else {
        return;
    };
    let Ok(ages) = pending_candidate_ages(&conn) else {
        return;
    };
    let now_ms = system_now_ms();
    let near_expiry = ages
        .iter()
        .filter(|(_, created_at)| now_ms.saturating_sub(*created_at) >= NEAR_EXPIRY_WARNING_MS)
        .count();
    if near_expiry > 0 {
        eprintln!(
            "{BIN}: note — {near_expiry} pending candidate(s) are within 7 days of the 30-day \
             expiry budget; `local-rag gc` would expire them. Review them before running gc."
        );
    }
}

fn run_dedup(candidate: Option<String>, all: bool, dry_run: bool) -> ExitCode {
    if candidate.is_none() && !all {
        eprintln!("{BIN} memory dedup: one of --candidate <id> or --all is required");
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

    if let Some(id) = candidate {
        if dry_run {
            let conn = match state.open_read() {
                Ok(c) => c,
                Err(e) => return fail(BIN, &format!("could not open state.sqlite: {e}")),
            };
            match candidate_state(&conn, &id) {
                Ok(Some(CandidateState::Pending)) => {}
                Ok(Some(_)) => return fail(BIN, &review_error_message(&ReviewError::NotPending)),
                Ok(None) => {
                    return fail(BIN, &review_error_message(&ReviewError::UnknownCandidate));
                }
                Err(e) => return fail(BIN, &format!("could not read {id}: {e}")),
            }
            let groups = match pending_candidate_groups(&conn) {
                Ok(g) => g,
                Err(e) => return fail(BIN, &format!("could not group candidates: {e}")),
            };
            let Some(group) = groups
                .iter()
                .find(|g| g.survivor.candidate_id == id || g.duplicate_ids.contains(&id))
            else {
                // `pending_candidate_groups` never invents a group for a row
                // it could not parse — same rule as a real fold.
                return fail(
                    BIN,
                    "invalid proposed_operation: candidate could not be grouped",
                );
            };
            if group.duplicate_ids.is_empty() {
                println!(
                    "{BIN}: {id} has no exact duplicates — nothing to fold; use `memory \
                     approve`/`memory reject` to decide it"
                );
            } else {
                println!(
                    "{BIN}: would fold {} duplicate(s) into survivor {} (dedup key {})",
                    group.duplicate_ids.len(),
                    group.survivor.candidate_id,
                    group.key.as_str(),
                );
            }
            return ExitCode::SUCCESS;
        }

        let now_ms = system_now_ms();
        let outcome = block_on({
            let id = id.clone();
            async move {
                state
                    .writer()
                    .transaction(move |tx| fold_pending_duplicates(tx, &id, now_ms))
                    .await
            }
        });
        return match outcome {
            Ok(Ok(outcome)) => {
                print_fold_outcome(&id, &outcome);
                ExitCode::SUCCESS
            }
            Ok(Err(e)) => fail(BIN, &review_error_message(&e)),
            Err(e) => fail(BIN, &format!("could not fold duplicates of {id}: {e}")),
        };
    }

    // `--all`.
    let now_ms = system_now_ms();
    let report = block_on(fold_all_pending_duplicates(&state, now_ms, dry_run));
    match report {
        Ok(report) => {
            print_triage_report(&report);
            if !dry_run {
                warn_if_pending_near_expiry(&state);
            }
            ExitCode::SUCCESS
        }
        Err(e) => fail(BIN, &format!("could not fold duplicate candidates: {e}")),
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
