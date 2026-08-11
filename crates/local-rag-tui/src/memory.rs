//! The Memory screen (spec 11 §7, T18-04 read paths + T18-05 mutations): browse memory
//! entries/candidates with kind/state/scope filters and pagination, drill into a selected entry's
//! own detail + evidence, and — as of T18-05 — approve/reject/edit/retract/merge through the same
//! primitives `cli/memory.rs` uses. Every read path is offline-safe (never applies a pending
//! migration); every write path opens through the identical offline-safe precaution before
//! touching `.writer()`.
//!
//! Mirrors `status.rs`/`repositories.rs`'s own discipline: [`compute_memory_data`] does all the
//! read I/O, [`render_memory`] does none, and [`handle_memory_key`] stays 100% pure (no I/O at
//! all, including no `state.sqlite` access) — it now returns [`MemoryKeyOutcome`] instead of a
//! bare [`MemoryNav`], so it can say "run this fully-specified mutation now"
//! ([`MemoryKeyOutcome::Execute`]) as well as "move to this nav state"
//! ([`MemoryKeyOutcome::Nav`]). [`execute_memory_action`] is the only function in this module that
//! ever touches `.writer()` — it mirrors `cli/memory.rs`'s five `run_*` functions literally:
//! `Actor::User`, `idempotency_key: None`, `evidence: &[]` for retract.
//!
//! # `EntryDetail` restores the exact prior list state, unlike `RepositoriesNav`
//!
//! `RepositoriesNav::ascend` only ever discards a single `selected: usize` on the way back up —
//! one keypress to redo. Here, ascending out of `EntryDetail` back to the list would otherwise
//! discard a multi-key filter/pagination setup (mode + four filters + offset) on every "peek at a
//! record's evidence and go back" — a materially worse regression than Repositories' own
//! single-index loss. `EntryDetail` therefore carries and restores the whole prior [`ListNav`]
//! verbatim. Every T18-05 mutation nav state (`EditForm`/`MergeSelect`/`ConfirmAction`/
//! `ActionResult`) carries its own `list: ListNav` for the same reason — cancelling a mutation or
//! dismissing its result returns to the exact list state the action started from.
//!
//! # Two typed filter fields, not one shared field like the CLI's `--state`
//!
//! `cli/memory.rs`'s `--state: Option<String>` is parsed against both `MemoryState::from_db`/
//! `CandidateState::from_db` — the right call for one free-text CLI flag, where parse-ambiguity
//! genuinely needs resolving. A TUI has an explicit `Tab` toggle instead of free text, so there is
//! no ambiguity to resolve; keeping `entry_state_filter`/`candidate_state_filter` as two always-
//! present `Option` fields means toggling `Tab` back and forth never discards either mode's own
//! filter setup.
//!
//! # Evidence panel shows bare `observation_id`s only
//!
//! The card names `memory_evidence_for` by name, and that function's own signature returns
//! `Vec<String>` — ids only, no text/source/time. The richer shape (`EvidenceSummary`/
//! `inspect_memory`) is `pub(crate)` inside `local_rag_store::privacy` except for `inspect_memory`
//! itself, which additionally pulls a full audit trail — a materially bigger read than this card's
//! literal text asks for, and `cli/memory.rs`'s own module doc already defers the full
//! `local-rag inspect` command to T16-02. Building an enriched evidence view here would be scope
//! creep past both the card text and its own CLI precedent.
//!
//! # Keyboard scheme
//!
//! `Up`/`Down`/`Enter`/`Backspace` are reused with the same physical meaning Repositories already
//! established (move/descend/ascend) — safe because `main.rs` dispatches per-screen. `Tab` toggles
//! Entries ⇄ Candidates; `k`/`K`, `s`/`S`, `o`/`O` cycle the kind/state/scope filters
//! forward/backward (uppercase matched by literal char, the standard crossterm idiom for Shift+
//! letter, not a modifier check); `PageDown`/`PageUp` page the list. T18-05 occupies the five keys
//! T18-04 reserved: `a`/`r` (approve/reject, `Candidates` mode only), `e`/`x`/`m` (edit/retract/
//! merge, `Entries` mode only) — all five trigger only from the `List` level, not `EntryDetail`
//! (see `start_edit`'s own doc for why). `x`/`Retract` and only `x`/`Retract` routes through a
//! confirm-modal (`ConfirmAction`), decided dynamically against `local_rag::daemon::mcp::catalog`'s
//! own `destructiveHint` annotation — never a hardcoded TUI-side list.
//!
//! # The global-quit carve-out for text entry (`main.rs::captures_all_keys`)
//!
//! `EditForm` is this crate's first free-text input surface — it must accept any printable
//! character, including the literal letter `q` and digits, as buffer content. `main.rs`'s own
//! `should_quit`/`screen_for_key`/`is_global_key` stay byte-identical (T18-01/T18-03's own
//! invariant: quit is unconditional, checked before any per-screen handler); [`captures_all_keys`]
//! is a narrow, separate predicate `run_app` consults to decide whether to even *reach*
//! those global checks for one keystroke while `nav` is `EditForm`. `Ctrl+C`/`Esc` are
//! deliberately excluded from the carve-out — neither is ever produced as ordinary typed content,
//! so both keep quitting unconditionally even mid-edit.

use std::path::Path;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use local_rag::daemon::gitroot;
use local_rag_core::paths::StoreLayout;
use local_rag_memory::recall::scopes_for;
use local_rag_store::rusqlite::Connection;
use local_rag_store::{
    Actor, ApproveCandidateOutcome, CandidateRow, CandidateState, EditMemoryOp, MemoryEntryRow,
    MemoryKind, MemoryOpError, MemoryOpOutcome, MemoryState, MergeLoser, MergeMemoryOp,
    RequestRoot, RetractMemoryOp, ReviewError, ScopeKind, apply_edit, apply_merge, apply_retract,
    approve_candidate, list_candidates, list_memory_entries_for_scope, memory_entry_by_id,
    memory_evidence_for, reject_candidate, resolve,
};

use crate::keys::{is_ctrl, step};
use crate::store_read::open_read_offline_safe;
use crate::store_write::open_write_offline_safe;

/// A page holds this many rows — a plain fixed constant, not adaptive to terminal size (this
/// crate's already-established taste, see `status.rs`/`repositories.rs`). Distinct from the MCP
/// wire protocol's `DEFAULT_LIST_LIMIT = 20` (`daemon/mcp/tools.rs`) — that constant governs a JSON
/// response, not a terminal viewport; 10 rows leaves room for a filter bar, list borders, and a
/// pagination footer in an 80×24 terminal.
const PAGE_SIZE: i64 = 10;

const MEMORY_KINDS: [MemoryKind; 7] = [
    MemoryKind::Fact,
    MemoryKind::Decision,
    MemoryKind::Convention,
    MemoryKind::Procedure,
    MemoryKind::Task,
    MemoryKind::Question,
    MemoryKind::Hypothesis,
];

const MEMORY_STATES: [MemoryState; 6] = [
    MemoryState::Active,
    MemoryState::Resolved,
    MemoryState::Retracted,
    MemoryState::Confirmed,
    MemoryState::Rejected,
    MemoryState::Superseded,
];

const SCOPE_KINDS: [ScopeKind; 3] = [
    ScopeKind::Global,
    ScopeKind::Repository,
    ScopeKind::Worktree,
];

const CANDIDATE_STATES: [CandidateState; 4] = [
    CandidateState::Pending,
    CandidateState::Approved,
    CandidateState::Rejected,
    CandidateState::Expired,
];

/// Advance `current` through `domain` (`None` sits before the first element and after the last —
/// cycling "off" is always reachable, matching `--kind`/`--state`/`--scope`'s own "no filter"
/// default). Generic over the four small `Copy` filter enums.
fn cycle_option<T: Copy + PartialEq>(current: Option<T>, domain: &[T], forward: bool) -> Option<T> {
    if domain.is_empty() {
        return None;
    }
    match current {
        None => Some(if forward {
            domain[0]
        } else {
            domain[domain.len() - 1]
        }),
        Some(cur) => match domain.iter().position(|d| *d == cur) {
            None => Some(domain[0]),
            Some(i) => {
                if forward {
                    domain.get(i + 1).copied()
                } else if i == 0 {
                    None
                } else {
                    Some(domain[i - 1])
                }
            }
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryMode {
    Entries,
    Candidates,
}

/// The list level's full state: active mode, all four filters (two of which are inert outside
/// their own mode — see module doc), pagination offset, and the selected row on the current page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListNav {
    pub mode: MemoryMode,
    pub kind_filter: Option<MemoryKind>,
    pub scope_filter: Option<ScopeKind>,
    pub entry_state_filter: Option<MemoryState>,
    pub candidate_state_filter: Option<CandidateState>,
    pub offset: i64,
    pub selected: usize,
}

impl Default for ListNav {
    fn default() -> Self {
        ListNav {
            mode: MemoryMode::Entries,
            kind_filter: None,
            scope_filter: None,
            entry_state_filter: None,
            candidate_state_filter: None,
            offset: 0,
            selected: 0,
        }
    }
}

/// Which `EditForm` buffer is focused — `Tab` toggles it, typed characters/`Backspace` route to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditField {
    Text,
    Importance,
}

/// A fully-specified mutation, ready to run — every variant carries the `list: ListNav` to return
/// to once it completes (success or cancel). Built by `handle_memory_key`'s `start_*`/form-submit
/// helpers, executed by [`execute_memory_action`]. `Edit.importance: f64` (not `Option<f64>`) —
/// see the module's own note in [`execute_memory_action`] for why always sending both fields is
/// behaviorally identical to the CLI's independently-optional ones.
#[derive(Debug, Clone, PartialEq)]
pub enum MemoryAction {
    Approve {
        candidate_id: String,
        list: ListNav,
    },
    Reject {
        candidate_id: String,
        list: ListNav,
    },
    Edit {
        memory_id: String,
        expected_version: i64,
        text: String,
        importance: f64,
        list: ListNav,
    },
    Retract {
        memory_id: String,
        expected_version: i64,
        list: ListNav,
    },
    Merge {
        survivor_id: String,
        survivor_expected_version: i64,
        losers: Vec<(String, i64)>,
        list: ListNav,
    },
}

impl MemoryAction {
    /// The MCP catalog tool name this action mirrors — what `gate` looks up in
    /// `local_rag::daemon::mcp::catalog()`'s own `destructiveHint` annotation.
    fn tool_name(&self) -> &'static str {
        match self {
            MemoryAction::Approve { .. } => "approve_memory_candidate",
            MemoryAction::Reject { .. } => "reject_memory_candidate",
            MemoryAction::Edit { .. } => "edit_memory",
            MemoryAction::Retract { .. } => "retract_memory",
            MemoryAction::Merge { .. } => "merge_memories",
        }
    }

    fn list(&self) -> &ListNav {
        match self {
            MemoryAction::Approve { list, .. }
            | MemoryAction::Reject { list, .. }
            | MemoryAction::Edit { list, .. }
            | MemoryAction::Retract { list, .. }
            | MemoryAction::Merge { list, .. } => list,
        }
    }

    /// A short human summary for `ConfirmAction`'s own prompt.
    fn describe(&self) -> String {
        match self {
            MemoryAction::Approve { candidate_id, .. } => {
                format!("approve candidate {candidate_id}")
            }
            MemoryAction::Reject { candidate_id, .. } => {
                format!("reject candidate {candidate_id}")
            }
            MemoryAction::Edit { memory_id, .. } => format!("edit memory entry {memory_id}"),
            MemoryAction::Retract { memory_id, .. } => format!("retract memory entry {memory_id}"),
            MemoryAction::Merge {
                survivor_id,
                losers,
                ..
            } => format!("merge {} loser(s) into {survivor_id}", losers.len()),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum MemoryNav {
    List(ListNav),
    EntryDetail {
        memory_id: String,
        list: ListNav,
    },
    /// A free-text edit of `memory_id`'s `text`/`importance` — see the module doc's own section on
    /// why this crate's global quit carve-out exists.
    EditForm {
        memory_id: String,
        expected_version: i64,
        field: EditField,
        text: String,
        importance: String,
        error: Option<String>,
        list: ListNav,
    },
    /// Picking a merge survivor + one-or-more losers from the same paginated entries query `List`
    /// itself uses. `survivor`/`losers` are materialized `(memory_id, entry_version)` pairs, not
    /// page-relative indices, so a pick made on one page survives `PageUp`/`PageDown` to another.
    MergeSelect {
        list: ListNav,
        cursor: usize,
        survivor: Option<(String, i64)>,
        losers: Vec<(String, i64)>,
        error: Option<String>,
    },
    /// Gates a `destructiveHint: true` action (today, exactly `retract_memory`) behind an explicit
    /// yes/no before `execute_memory_action` ever runs — see `gate`'s own doc.
    ConfirmAction {
        action: Box<MemoryAction>,
        list: ListNav,
    },
    /// A dismissible outcome banner — any key returns to `list`. `is_error` distinguishes a typed
    /// domain rejection (`MemoryOpError`/`ReviewError`, surfaced without panicking — the card's own
    /// requirement) from a successful mutation.
    ActionResult {
        message: String,
        is_error: bool,
        list: ListNav,
    },
}

impl Default for MemoryNav {
    fn default() -> Self {
        MemoryNav::List(ListNav::default())
    }
}

impl MemoryNav {
    /// `Up`/`Down` — no-op outside `List` (nothing to move within).
    fn moved(&self, data: &MemoryScreenData, down: bool) -> Self {
        match self {
            MemoryNav::List(list) => {
                let len = match data {
                    MemoryScreenData::EntryList { rows, .. } => rows.len(),
                    MemoryScreenData::CandidateList { rows, .. } => rows.len(),
                    _ => return self.clone(),
                };
                MemoryNav::List(ListNav {
                    selected: step(list.selected, down, len),
                    ..list.clone()
                })
            }
            _ => self.clone(),
        }
    }

    /// `Enter` — descends into `EntryDetail` using the selected row's `memory_id`. A no-op in
    /// `Candidates` mode (the card names evidence/detail for "the selected **entry**" only), on an
    /// empty page, or outside `List`.
    fn descend(&self, data: &MemoryScreenData) -> Self {
        match (self, data) {
            (MemoryNav::List(list), MemoryScreenData::EntryList { rows, .. })
                if list.mode == MemoryMode::Entries =>
            {
                match rows.get(list.selected) {
                    Some(row) => MemoryNav::EntryDetail {
                        memory_id: row.memory_id.clone(),
                        list: list.clone(),
                    },
                    None => self.clone(),
                }
            }
            _ => self.clone(),
        }
    }

    /// `Backspace` — restores the exact prior `ListNav` verbatim (see module doc). No-op already
    /// at `List`.
    fn ascend(&self) -> Self {
        match self {
            MemoryNav::EntryDetail { list, .. } => MemoryNav::List(list.clone()),
            _ => self.clone(),
        }
    }

    /// `Tab` — toggles `mode`, resetting `offset`/`selected` (the row-set changes completely) but
    /// preserving both modes' own filters. No-op outside `List`.
    fn toggle_mode(&self) -> Self {
        match self {
            MemoryNav::List(list) => {
                let mode = match list.mode {
                    MemoryMode::Entries => MemoryMode::Candidates,
                    MemoryMode::Candidates => MemoryMode::Entries,
                };
                MemoryNav::List(ListNav {
                    mode,
                    offset: 0,
                    selected: 0,
                    ..list.clone()
                })
            }
            _ => self.clone(),
        }
    }

    /// `k`/`K` — cycles `kind_filter`. No-op unless `mode == Entries` (`list_candidates` has no
    /// kind parameter at all — `pending_memory_candidate` has no kind column).
    fn cycle_kind(&self, forward: bool) -> Self {
        match self {
            MemoryNav::List(list) if list.mode == MemoryMode::Entries => MemoryNav::List(ListNav {
                kind_filter: cycle_option(list.kind_filter, &MEMORY_KINDS, forward),
                offset: 0,
                selected: 0,
                ..list.clone()
            }),
            _ => self.clone(),
        }
    }

    /// `s`/`S` — cycles `entry_state_filter` (Entries) or `candidate_state_filter` (Candidates)
    /// through that mode's own domain. Always applicable in both modes.
    fn cycle_state(&self, forward: bool) -> Self {
        match self {
            MemoryNav::List(list) => {
                let mut next = list.clone();
                match list.mode {
                    MemoryMode::Entries => {
                        next.entry_state_filter =
                            cycle_option(list.entry_state_filter, &MEMORY_STATES, forward);
                    }
                    MemoryMode::Candidates => {
                        next.candidate_state_filter =
                            cycle_option(list.candidate_state_filter, &CANDIDATE_STATES, forward);
                    }
                }
                next.offset = 0;
                next.selected = 0;
                MemoryNav::List(next)
            }
            _ => self.clone(),
        }
    }

    /// `o`/`O` — cycles `scope_filter`. No-op unless `mode == Entries` (`pending_memory_candidate`
    /// has no scope column, `cli/memory.rs`'s own module doc already states this).
    fn cycle_scope(&self, forward: bool) -> Self {
        match self {
            MemoryNav::List(list) if list.mode == MemoryMode::Entries => MemoryNav::List(ListNav {
                scope_filter: cycle_option(list.scope_filter, &SCOPE_KINDS, forward),
                offset: 0,
                selected: 0,
                ..list.clone()
            }),
            _ => self.clone(),
        }
    }

    /// `PageDown`/`PageUp` — `offset += PAGE_SIZE` / `offset -= PAGE_SIZE`, clamped (no-op past
    /// `has_more`/below `0`); `selected` resets (a fresh page's rows have nothing to do with the
    /// last page's selection index).
    fn paged(&self, data: &MemoryScreenData, forward: bool) -> Self {
        match self {
            MemoryNav::List(list) => {
                let has_more = match data {
                    MemoryScreenData::EntryList { has_more, .. }
                    | MemoryScreenData::CandidateList { has_more, .. } => *has_more,
                    _ => return self.clone(),
                };
                let offset = if forward {
                    if has_more {
                        list.offset + PAGE_SIZE
                    } else {
                        list.offset
                    }
                } else {
                    (list.offset - PAGE_SIZE).max(0)
                };
                MemoryNav::List(ListNav {
                    offset,
                    selected: 0,
                    ..list.clone()
                })
            }
            _ => self.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum MemoryScreenData {
    Unavailable {
        reason: String,
    },
    EntryList {
        scope_label: String,
        kind_filter: Option<MemoryKind>,
        state_filter: Option<MemoryState>,
        scope_filter: Option<ScopeKind>,
        rows: Vec<MemoryEntryRow>,
        total: usize,
        has_more: bool,
        selected: usize,
    },
    CandidateList {
        state_filter: Option<CandidateState>,
        rows: Vec<CandidateRow>,
        has_more: bool,
        selected: usize,
    },
    EntryDetail {
        entry: MemoryEntryRow,
        evidence_ids: Vec<String>,
    },
    EditForm {
        memory_id: String,
        expected_version: i64,
        field: EditField,
        text: String,
        importance: String,
        error: Option<String>,
    },
    MergeSelect {
        rows: Vec<MemoryEntryRow>,
        has_more: bool,
        cursor: usize,
        survivor: Option<(String, i64)>,
        losers: Vec<(String, i64)>,
        error: Option<String>,
    },
    ConfirmAction {
        description: String,
    },
    ActionResult {
        message: String,
        is_error: bool,
    },
}

/// The resolve→union-scopes→sort→paginate core `compute_entry_list`/`compute_merge_select` both
/// need — factored out so `MergeSelect` picks from the identical row set `EntryList` itself would
/// show for the same `ListNav`, not a second, possibly-diverging query.
fn fetch_entry_page(
    conn: &Connection,
    cwd: &Path,
    list: &ListNav,
) -> Result<(String, Vec<MemoryEntryRow>, usize, bool), String> {
    let facts = gitroot::probe(cwd);
    let resolution = resolve(
        conn,
        &RequestRoot {
            worktree_root: facts,
            repo_hint: None,
        },
    )
    .map_err(|e| format!("could not resolve worktree identity: {e}"))?;
    let (scope_label, scopes) = scopes_for(&resolution);
    let scopes: Vec<(ScopeKind, String)> = match list.scope_filter {
        Some(wanted) => scopes.into_iter().filter(|(k, _)| *k == wanted).collect(),
        None => scopes,
    };

    let mut combined: Vec<MemoryEntryRow> = Vec::new();
    for (kind, owner) in &scopes {
        let rows = list_memory_entries_for_scope(
            conn,
            *kind,
            owner,
            list.kind_filter,
            list.entry_state_filter,
        )
        .map_err(|e| format!("could not list memory entries: {e}"))?;
        combined.extend(rows);
    }
    // D-056: newest-first — the union has no SQL LIMIT/OFFSET of its own (comment above), so
    // page 1 (offset 0) is whichever end of this sort sits first. Oldest-first made the default
    // view show only rows that, by definition, never change, which live dogfooding showed reads
    // as "the screen is frozen" even while consolidation is actively producing new entries.
    combined.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| b.memory_id.cmp(&a.memory_id))
    });

    let total = combined.len();
    let offset_usize = usize::try_from(list.offset).unwrap_or(usize::MAX);
    let page_size_usize = PAGE_SIZE as usize;
    let has_more = total > offset_usize.saturating_add(page_size_usize);
    let rows: Vec<MemoryEntryRow> = combined
        .into_iter()
        .skip(offset_usize)
        .take(page_size_usize)
        .collect();

    Ok((scope_label, rows, total, has_more))
}

/// Transplants `cli/memory.rs::run_list`'s entries path verbatim via [`fetch_entry_page`].
fn compute_entry_list(conn: &Connection, cwd: &Path, list: &ListNav) -> MemoryScreenData {
    let (scope_label, rows, total, has_more) = match fetch_entry_page(conn, cwd, list) {
        Ok(v) => v,
        Err(reason) => return MemoryScreenData::Unavailable { reason },
    };
    let selected = if rows.is_empty() {
        0
    } else {
        list.selected.min(rows.len() - 1)
    };
    MemoryScreenData::EntryList {
        scope_label,
        kind_filter: list.kind_filter,
        state_filter: list.entry_state_filter,
        scope_filter: list.scope_filter,
        rows,
        total,
        has_more,
        selected,
    }
}

/// The `MergeSelect` nav's own data: the same paginated entries query `EntryList` uses, plus the
/// in-progress survivor/loser picks and `cursor` carried straight through from `nav`.
#[allow(clippy::too_many_arguments)]
fn compute_merge_select(
    conn: &Connection,
    cwd: &Path,
    list: &ListNav,
    cursor: usize,
    survivor: Option<(String, i64)>,
    losers: Vec<(String, i64)>,
    error: Option<String>,
) -> MemoryScreenData {
    let (_scope_label, rows, _total, has_more) = match fetch_entry_page(conn, cwd, list) {
        Ok(v) => v,
        Err(reason) => return MemoryScreenData::Unavailable { reason },
    };
    let cursor = if rows.is_empty() {
        0
    } else {
        cursor.min(rows.len() - 1)
    };
    MemoryScreenData::MergeSelect {
        rows,
        has_more,
        cursor,
        survivor,
        losers,
        error,
    }
}

/// Transplants `cli/memory.rs::run_list`'s candidates path verbatim: `limit+1`/`offset` straight
/// into SQL (`list_candidates` has real `LIMIT`/`OFFSET`), over-fetch-by-one to detect `has_more`
/// without a second `COUNT` query, then truncate.
fn compute_candidate_list(conn: &Connection, list: &ListNav) -> MemoryScreenData {
    let mut rows = match list_candidates(
        conn,
        list.candidate_state_filter,
        PAGE_SIZE + 1,
        list.offset,
    ) {
        Ok(r) => r,
        Err(e) => {
            return MemoryScreenData::Unavailable {
                reason: format!("could not list candidates: {e}"),
            };
        }
    };
    let has_more = rows.len() as i64 > PAGE_SIZE;
    rows.truncate(PAGE_SIZE as usize);
    let selected = if rows.is_empty() {
        0
    } else {
        list.selected.min(rows.len() - 1)
    };
    MemoryScreenData::CandidateList {
        state_filter: list.candidate_state_filter,
        rows,
        has_more,
        selected,
    }
}

/// Re-fetches by id via `memory_entry_by_id` rather than reusing the cached list row — the same
/// WYSIWYG-safe idiom `WorktreeDetail` already established (`worktree_summary` refetched by id,
/// not carried over from the `Worktrees` level). `Ok(None)` gives a correctly-typed "entry vanished
/// between frames" branch for free.
fn compute_entry_detail(conn: &Connection, memory_id: &str) -> MemoryScreenData {
    let entry = match memory_entry_by_id(conn, memory_id) {
        Ok(Some(e)) => e,
        Ok(None) => {
            return MemoryScreenData::Unavailable {
                reason: format!("memory entry {memory_id} no longer exists"),
            };
        }
        Err(e) => {
            return MemoryScreenData::Unavailable {
                reason: format!("could not read memory entry {memory_id}: {e}"),
            };
        }
    };
    let evidence_ids = match memory_evidence_for(conn, memory_id) {
        Ok(ids) => ids,
        Err(e) => {
            return MemoryScreenData::Unavailable {
                reason: format!("could not read evidence for {memory_id}: {e}"),
            };
        }
    };
    MemoryScreenData::EntryDetail {
        entry,
        evidence_ids,
    }
}

/// Compose everything — what `run_app` (and every test) actually calls. `EditForm`/`ConfirmAction`/
/// `ActionResult` are trivial passthroughs of `nav`'s own fields — the data they show was already
/// captured in `nav` when that state was entered, so no extra read happens; a stale
/// `expected_version` correctly surfaces as `OptimisticConflict` when the action actually executes,
/// the same non-preemptive-validation discipline `cli/memory.rs` itself already follows.
pub fn compute_memory_data(layout: &StoreLayout, cwd: &Path, nav: &MemoryNav) -> MemoryScreenData {
    let conn = match open_read_offline_safe(layout) {
        Ok(c) => c,
        Err(reason) => return MemoryScreenData::Unavailable { reason },
    };
    match nav {
        MemoryNav::List(list) => match list.mode {
            MemoryMode::Entries => compute_entry_list(&conn, cwd, list),
            MemoryMode::Candidates => compute_candidate_list(&conn, list),
        },
        MemoryNav::EntryDetail { memory_id, .. } => compute_entry_detail(&conn, memory_id),
        MemoryNav::EditForm {
            memory_id,
            expected_version,
            field,
            text,
            importance,
            error,
            ..
        } => MemoryScreenData::EditForm {
            memory_id: memory_id.clone(),
            expected_version: *expected_version,
            field: *field,
            text: text.clone(),
            importance: importance.clone(),
            error: error.clone(),
        },
        MemoryNav::MergeSelect {
            list,
            cursor,
            survivor,
            losers,
            error,
        } => compute_merge_select(
            &conn,
            cwd,
            list,
            *cursor,
            survivor.clone(),
            losers.clone(),
            error.clone(),
        ),
        MemoryNav::ConfirmAction { action, .. } => MemoryScreenData::ConfirmAction {
            description: action.describe(),
        },
        MemoryNav::ActionResult {
            message, is_error, ..
        } => MemoryScreenData::ActionResult {
            message: message.clone(),
            is_error: *is_error,
        },
    }
}

/// `handle_memory_key`'s return: either a pure navigation update, or a fully-specified mutation
/// `run_app` must hand to [`execute_memory_action`]. Keeps `handle_memory_key` itself 100% pure —
/// it never touches `state.sqlite`, it only ever *decides* that a mutation should run.
#[derive(Debug, Clone, PartialEq)]
pub enum MemoryKeyOutcome {
    Nav(MemoryNav),
    Execute(MemoryAction),
}

/// `true` only for [`MemoryNav::EditForm`] — the one nav state whose handler must receive every
/// keystroke verbatim (module doc's own section on the global-quit carve-out).
pub fn captures_all_keys(nav: &MemoryNav) -> bool {
    matches!(nav, MemoryNav::EditForm { .. })
}

/// `local_rag::daemon::mcp::catalog()`'s own `annotations.destructiveHint` for `tool_name` — the
/// single source of truth the card requires ("TUI reads this list as source of truth, not its
/// own"). `unwrap_or(true)`: an unrecognized/malformed catalog entry fails toward *requiring*
/// confirmation, never silently skipping it.
fn catalog_requires_confirmation(tool_name: &str) -> bool {
    let catalog = local_rag::daemon::mcp::catalog();
    catalog["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|t| t["name"] == tool_name)
        .and_then(|t| t["annotations"]["destructiveHint"].as_bool())
        .unwrap_or(true)
}

/// Every mutation trigger funnels through here: consult the real MCP catalog for `action`'s own
/// tool, and either gate it behind an explicit `ConfirmAction` or let it execute immediately. Today
/// this inserts a confirm step only for `retract_memory` (verified against the real `catalog()`,
/// never a mock — `daemon/mcp/tools.rs`'s own regression test holds that invariant catalog-wide),
/// and will automatically follow the catalog if it ever changes.
fn gate(action: MemoryAction) -> MemoryKeyOutcome {
    let list = action.list().clone();
    if catalog_requires_confirmation(action.tool_name()) {
        MemoryKeyOutcome::Nav(MemoryNav::ConfirmAction {
            action: Box::new(action),
            list,
        })
    } else {
        MemoryKeyOutcome::Execute(action)
    }
}

fn selected_candidate<'a>(list: &ListNav, data: &'a MemoryScreenData) -> Option<&'a CandidateRow> {
    match data {
        MemoryScreenData::CandidateList { rows, .. } => rows.get(list.selected),
        _ => None,
    }
}

fn selected_entry<'a>(list: &ListNav, data: &'a MemoryScreenData) -> Option<&'a MemoryEntryRow> {
    match data {
        MemoryScreenData::EntryList { rows, .. } => rows.get(list.selected),
        _ => None,
    }
}

/// `a`/`A` — approve the selected candidate. `Candidates` mode only, no-op otherwise/on an empty
/// page (never `ConfirmAction`-gated: `approve_memory_candidate` is `destructiveHint: false`).
fn start_approve(list: &ListNav, data: &MemoryScreenData) -> MemoryKeyOutcome {
    if list.mode != MemoryMode::Candidates {
        return MemoryKeyOutcome::Nav(MemoryNav::List(list.clone()));
    }
    match selected_candidate(list, data) {
        Some(row) => gate(MemoryAction::Approve {
            candidate_id: row.candidate_id.clone(),
            list: list.clone(),
        }),
        None => MemoryKeyOutcome::Nav(MemoryNav::List(list.clone())),
    }
}

/// `r`/`R` — reject the selected candidate. `Candidates` mode only.
fn start_reject(list: &ListNav, data: &MemoryScreenData) -> MemoryKeyOutcome {
    if list.mode != MemoryMode::Candidates {
        return MemoryKeyOutcome::Nav(MemoryNav::List(list.clone()));
    }
    match selected_candidate(list, data) {
        Some(row) => gate(MemoryAction::Reject {
            candidate_id: row.candidate_id.clone(),
            list: list.clone(),
        }),
        None => MemoryKeyOutcome::Nav(MemoryNav::List(list.clone())),
    }
}

/// `e`/`E` — open `EditForm` for the selected entry, pre-seeded with its current `text`/
/// `importance`. `Entries` mode only. Triggers only from `List`, not `EntryDetail` — `EntryDetail`'s
/// own `list: ListNav` already restores `selected` verbatim, so the round trip via `Backspace` then
/// `e` costs exactly one extra keypress, not a lost place; adding a second entry point would need a
/// list-vs-detail return-target type for no real workflow gain.
fn start_edit(list: &ListNav, data: &MemoryScreenData) -> MemoryKeyOutcome {
    if list.mode != MemoryMode::Entries {
        return MemoryKeyOutcome::Nav(MemoryNav::List(list.clone()));
    }
    match selected_entry(list, data) {
        Some(row) => MemoryKeyOutcome::Nav(MemoryNav::EditForm {
            memory_id: row.memory_id.clone(),
            expected_version: row.entry_version,
            field: EditField::Text,
            text: row.text.clone(),
            importance: format!("{:.2}", row.importance),
            error: None,
            list: list.clone(),
        }),
        None => MemoryKeyOutcome::Nav(MemoryNav::List(list.clone())),
    }
}

/// `x`/`X` — retract the selected entry. `Entries` mode only. Always `ConfirmAction`-gated in
/// practice (`retract_memory` is the one `destructiveHint: true` tool), decided dynamically by
/// `gate`, never hardcoded here.
fn start_retract(list: &ListNav, data: &MemoryScreenData) -> MemoryKeyOutcome {
    if list.mode != MemoryMode::Entries {
        return MemoryKeyOutcome::Nav(MemoryNav::List(list.clone()));
    }
    match selected_entry(list, data) {
        Some(row) => gate(MemoryAction::Retract {
            memory_id: row.memory_id.clone(),
            expected_version: row.entry_version,
            list: list.clone(),
        }),
        None => MemoryKeyOutcome::Nav(MemoryNav::List(list.clone())),
    }
}

/// `m`/`M` — open `MergeSelect` over the same page `EntryList` is currently showing. `Entries`
/// mode only, no-op on an empty page.
fn start_merge(list: &ListNav, data: &MemoryScreenData) -> MemoryKeyOutcome {
    if list.mode != MemoryMode::Entries {
        return MemoryKeyOutcome::Nav(MemoryNav::List(list.clone()));
    }
    match data {
        MemoryScreenData::EntryList { rows, .. } if !rows.is_empty() => {
            MemoryKeyOutcome::Nav(MemoryNav::MergeSelect {
                list: list.clone(),
                cursor: list.selected.min(rows.len() - 1),
                survivor: None,
                losers: Vec::new(),
                error: None,
            })
        }
        _ => MemoryKeyOutcome::Nav(MemoryNav::List(list.clone())),
    }
}

/// The `Up`/`Down`/`Enter`/`Backspace`/`Tab`/`k`/`K`/`s`/`S`/`o`/`O`/`PageDown`/`PageUp` table
/// shared by `List` and `EntryDetail` — verbatim the pre-T18-05 `handle_memory_key` body, now a
/// helper so `List`'s own dispatch can layer the five new mutation-trigger keys in front of it.
fn navigate(nav: &MemoryNav, data: &MemoryScreenData, code: KeyCode) -> MemoryNav {
    match code {
        KeyCode::Up => nav.moved(data, false),
        KeyCode::Down => nav.moved(data, true),
        KeyCode::Enter => nav.descend(data),
        KeyCode::Backspace => nav.ascend(),
        KeyCode::Tab => nav.toggle_mode(),
        KeyCode::Char('k') => nav.cycle_kind(true),
        KeyCode::Char('K') => nav.cycle_kind(false),
        KeyCode::Char('s') => nav.cycle_state(true),
        KeyCode::Char('S') => nav.cycle_state(false),
        KeyCode::Char('o') => nav.cycle_scope(true),
        KeyCode::Char('O') => nav.cycle_scope(false),
        KeyCode::PageDown => nav.paged(data, true),
        KeyCode::PageUp => nav.paged(data, false),
        _ => nav.clone(),
    }
}

/// `EditForm`'s own dispatch: `Tab` switches field, any unmodified printable char appends to the
/// focused buffer, `Backspace` deletes from it, `Enter` validates+submits, `Ctrl+X` cancels. See
/// the module doc's own section on why plain `q`/digits must reach here as content, not quit.
fn handle_edit_form_key(nav: &MemoryNav, key: KeyEvent) -> MemoryKeyOutcome {
    let MemoryNav::EditForm {
        memory_id,
        expected_version,
        field,
        text,
        importance,
        list,
        ..
    } = nav
    else {
        return MemoryKeyOutcome::Nav(nav.clone());
    };

    if is_ctrl(&key, 'x') {
        return MemoryKeyOutcome::Nav(MemoryNav::List(list.clone()));
    }

    let with_error = |error: Option<String>, text: String, importance: String| {
        MemoryKeyOutcome::Nav(MemoryNav::EditForm {
            memory_id: memory_id.clone(),
            expected_version: *expected_version,
            field: *field,
            text,
            importance,
            error,
            list: list.clone(),
        })
    };

    match key.code {
        KeyCode::Tab => {
            let next_field = match field {
                EditField::Text => EditField::Importance,
                EditField::Importance => EditField::Text,
            };
            MemoryKeyOutcome::Nav(MemoryNav::EditForm {
                memory_id: memory_id.clone(),
                expected_version: *expected_version,
                field: next_field,
                text: text.clone(),
                importance: importance.clone(),
                error: None,
                list: list.clone(),
            })
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            let mut text = text.clone();
            let mut importance = importance.clone();
            match field {
                EditField::Text => text.push(c),
                EditField::Importance => importance.push(c),
            }
            with_error(None, text, importance)
        }
        KeyCode::Backspace => {
            let mut text = text.clone();
            let mut importance = importance.clone();
            match field {
                EditField::Text => {
                    text.pop();
                }
                EditField::Importance => {
                    importance.pop();
                }
            }
            with_error(None, text, importance)
        }
        KeyCode::Enter => match importance.trim().parse::<f64>() {
            Ok(v) if (0.0..=1.0).contains(&v) => gate(MemoryAction::Edit {
                memory_id: memory_id.clone(),
                expected_version: *expected_version,
                text: text.clone(),
                importance: v,
                list: list.clone(),
            }),
            _ => with_error(
                Some("importance must be a number between 0 and 1".to_string()),
                text.clone(),
                importance.clone(),
            ),
        },
        _ => MemoryKeyOutcome::Nav(nav.clone()),
    }
}

/// `MergeSelect`'s own dispatch: `Up`/`Down`/`PageUp`/`PageDown` browse the same paginated query
/// `List` uses; `Enter` sets the survivor (evicting it from `losers` first, keeping the two sets
/// disjoint); `Space` toggles loser membership; `m`/`M` executes once both are set, else leaves an
/// inline `error`; `Backspace`/`Ctrl+X` cancels back to `List`.
fn handle_merge_select_key(
    nav: &MemoryNav,
    data: &MemoryScreenData,
    key: KeyEvent,
) -> MemoryKeyOutcome {
    let MemoryNav::MergeSelect {
        list,
        cursor,
        survivor,
        losers,
        ..
    } = nav
    else {
        return MemoryKeyOutcome::Nav(nav.clone());
    };

    if is_ctrl(&key, 'x') {
        return MemoryKeyOutcome::Nav(MemoryNav::List(list.clone()));
    }

    let (rows_len, has_more) = match data {
        MemoryScreenData::MergeSelect { rows, has_more, .. } => (rows.len(), *has_more),
        _ => (0, false),
    };
    let current_row = || -> Option<(String, i64)> {
        match data {
            MemoryScreenData::MergeSelect { rows, .. } => rows
                .get(*cursor)
                .map(|r| (r.memory_id.clone(), r.entry_version)),
            _ => None,
        }
    };

    match key.code {
        KeyCode::Up => MemoryKeyOutcome::Nav(MemoryNav::MergeSelect {
            list: list.clone(),
            cursor: step(*cursor, false, rows_len),
            survivor: survivor.clone(),
            losers: losers.clone(),
            error: None,
        }),
        KeyCode::Down => MemoryKeyOutcome::Nav(MemoryNav::MergeSelect {
            list: list.clone(),
            cursor: step(*cursor, true, rows_len),
            survivor: survivor.clone(),
            losers: losers.clone(),
            error: None,
        }),
        KeyCode::PageDown => MemoryKeyOutcome::Nav(MemoryNav::MergeSelect {
            list: ListNav {
                offset: if has_more {
                    list.offset + PAGE_SIZE
                } else {
                    list.offset
                },
                ..list.clone()
            },
            cursor: 0,
            survivor: survivor.clone(),
            losers: losers.clone(),
            error: None,
        }),
        KeyCode::PageUp => MemoryKeyOutcome::Nav(MemoryNav::MergeSelect {
            list: ListNav {
                offset: (list.offset - PAGE_SIZE).max(0),
                ..list.clone()
            },
            cursor: 0,
            survivor: survivor.clone(),
            losers: losers.clone(),
            error: None,
        }),
        KeyCode::Enter => match current_row() {
            Some(picked) => {
                let mut losers = losers.clone();
                losers.retain(|l| l.0 != picked.0);
                MemoryKeyOutcome::Nav(MemoryNav::MergeSelect {
                    list: list.clone(),
                    cursor: *cursor,
                    survivor: Some(picked),
                    losers,
                    error: None,
                })
            }
            None => MemoryKeyOutcome::Nav(nav.clone()),
        },
        KeyCode::Char(' ') => match current_row() {
            Some(picked) if survivor.as_ref().map(|s| s.0.as_str()) != Some(picked.0.as_str()) => {
                let mut losers = losers.clone();
                if let Some(pos) = losers.iter().position(|l| l.0 == picked.0) {
                    losers.remove(pos);
                } else {
                    losers.push(picked);
                }
                MemoryKeyOutcome::Nav(MemoryNav::MergeSelect {
                    list: list.clone(),
                    cursor: *cursor,
                    survivor: survivor.clone(),
                    losers,
                    error: None,
                })
            }
            _ => MemoryKeyOutcome::Nav(nav.clone()),
        },
        KeyCode::Char('m') | KeyCode::Char('M') => match (survivor, losers.is_empty()) {
            (Some((survivor_id, survivor_expected_version)), false) => gate(MemoryAction::Merge {
                survivor_id: survivor_id.clone(),
                survivor_expected_version: *survivor_expected_version,
                losers: losers.clone(),
                list: list.clone(),
            }),
            _ => MemoryKeyOutcome::Nav(MemoryNav::MergeSelect {
                list: list.clone(),
                cursor: *cursor,
                survivor: survivor.clone(),
                losers: losers.clone(),
                error: Some(
                    "pick a survivor (Enter) and at least one loser (Space) first".to_string(),
                ),
            }),
        },
        KeyCode::Backspace => MemoryKeyOutcome::Nav(MemoryNav::List(list.clone())),
        _ => MemoryKeyOutcome::Nav(nav.clone()),
    }
}

/// `ConfirmAction`'s own dispatch: `Enter`/`y`/`Y` executes, `Backspace`/`n`/`N`/`Ctrl+X` cancels.
fn handle_confirm_action_key(nav: &MemoryNav, key: KeyEvent) -> MemoryKeyOutcome {
    let MemoryNav::ConfirmAction { action, list } = nav else {
        return MemoryKeyOutcome::Nav(nav.clone());
    };
    match key.code {
        KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
            MemoryKeyOutcome::Execute((**action).clone())
        }
        KeyCode::Backspace | KeyCode::Char('n') | KeyCode::Char('N') => {
            MemoryKeyOutcome::Nav(MemoryNav::List(list.clone()))
        }
        _ if is_ctrl(&key, 'x') => MemoryKeyOutcome::Nav(MemoryNav::List(list.clone())),
        _ => MemoryKeyOutcome::Nav(nav.clone()),
    }
}

/// The only keys this screen's own handler recognizes; `run_app` checks global keys (quit, digit
/// screen-switch) first — except while `captures_all_keys(nav)` and the pressed key is bare `q`/a
/// digit, per the module doc's own carve-out — and never delegates them here otherwise. Pure: no
/// I/O, no render, no `state.sqlite` access of any kind (including for the mutation gate — see
/// `gate`'s own doc on why the catalog lookup is legitimate here).
pub fn handle_memory_key(nav: &MemoryNav, data: &MemoryScreenData, ev: Event) -> MemoryKeyOutcome {
    let Event::Key(key) = ev else {
        return MemoryKeyOutcome::Nav(nav.clone());
    };
    if key.kind != KeyEventKind::Press {
        return MemoryKeyOutcome::Nav(nav.clone());
    }
    match nav {
        MemoryNav::List(list) => match key.code {
            KeyCode::Char('a') | KeyCode::Char('A') => start_approve(list, data),
            KeyCode::Char('r') | KeyCode::Char('R') => start_reject(list, data),
            KeyCode::Char('e') | KeyCode::Char('E') => start_edit(list, data),
            KeyCode::Char('x') | KeyCode::Char('X') => start_retract(list, data),
            KeyCode::Char('m') | KeyCode::Char('M') => start_merge(list, data),
            code => MemoryKeyOutcome::Nav(navigate(nav, data, code)),
        },
        MemoryNav::EntryDetail { .. } => MemoryKeyOutcome::Nav(navigate(nav, data, key.code)),
        MemoryNav::EditForm { .. } => handle_edit_form_key(nav, key),
        MemoryNav::MergeSelect { .. } => handle_merge_select_key(nav, data, key),
        MemoryNav::ConfirmAction { .. } => handle_confirm_action_key(nav, key),
        MemoryNav::ActionResult { list, .. } => {
            MemoryKeyOutcome::Nav(MemoryNav::List(list.clone()))
        }
    }
}

// ---------------------------------------------------------------------------
// Error/outcome translation — ported verbatim from `cli/memory.rs`'s own
// `memory_op_error_message`/`review_error_message`/`op_outcome_*` (that file is private to the
// `local-rag` binary target). The third occurrence of this exact match in the workspace —
// `daemon/mcp/memory_write.rs`'s `memory_op_error_envelope`/`review_error_envelope` is the second
// — not worth a shared crate: CLI/TUI want a plain `String`, MCP wants a JSON envelope, and
// generalizing over two genuinely different output shapes for three call sites buys nothing.
// ---------------------------------------------------------------------------

fn memory_op_error_message(e: &MemoryOpError) -> String {
    match e {
        MemoryOpError::UnknownMemory => "no memory entry with that id".to_string(),
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

fn op_outcome_memory_id(outcome: &MemoryOpOutcome) -> &str {
    match outcome {
        MemoryOpOutcome::Applied(r) | MemoryOpOutcome::Replayed(r) => &r.memory_id,
    }
}

fn op_outcome_entry_version(outcome: &MemoryOpOutcome) -> i64 {
    match outcome {
        MemoryOpOutcome::Applied(r) | MemoryOpOutcome::Replayed(r) => r.entry_version,
    }
}

fn op_outcome_audit_id(outcome: &MemoryOpOutcome) -> i64 {
    match outcome {
        MemoryOpOutcome::Applied(r) | MemoryOpOutcome::Replayed(r) => r.audit_id,
    }
}

fn system_now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The only function in this module that touches `.writer()` — mirrors `cli/memory.rs`'s five
/// `run_*` functions literally: `Actor::User`, `idempotency_key: None`, `evidence: &[]` for
/// retract. `Edit` always sends both `text`/`importance` as `Some` rather than reproducing
/// `EditMemoryOp`'s independent per-field nullability — behaviorally identical, since `apply_edit`
/// applies each field via `COALESCE(?, text)`/`COALESCE(?, importance)`: submitting a field's own
/// unchanged current value round-trips to the exact same stored value `None` would have left in
/// place, and every `apply_edit` call bumps `entry_version`/writes an `audit_event` regardless of
/// which fields actually changed — the CLI's own `--text`-only or `--importance`-only edit pays
/// that identical cost.
pub fn execute_memory_action(layout: &StoreLayout, action: MemoryAction) -> MemoryNav {
    let list = action.list().clone();
    let state = match open_write_offline_safe(layout) {
        Ok(s) => s,
        Err(reason) => {
            return MemoryNav::ActionResult {
                message: reason,
                is_error: true,
                list,
            };
        }
    };
    let now_ms = system_now_ms();

    let (message, is_error) = match action {
        MemoryAction::Approve { candidate_id, .. } => {
            let outcome = crate::rt::block_on({
                let candidate_id = candidate_id.clone();
                async move {
                    state
                        .writer()
                        .transaction(move |tx| approve_candidate(tx, &candidate_id, now_ms))
                        .await
                }
            });
            match outcome {
                Ok(Ok(ApproveCandidateOutcome::Materialized(op_outcome))) => (
                    format!(
                        "approved {candidate_id} -> memory {} (entry_version {}, audit_id {})",
                        op_outcome_memory_id(&op_outcome),
                        op_outcome_entry_version(&op_outcome),
                        op_outcome_audit_id(&op_outcome)
                    ),
                    false,
                ),
                Ok(Ok(ApproveCandidateOutcome::AlreadyApproved)) => {
                    (format!("{candidate_id} was already approved"), false)
                }
                Ok(Err(e)) => (review_error_message(&e), true),
                Err(e) => (format!("could not approve {candidate_id}: {e}"), true),
            }
        }
        MemoryAction::Reject { candidate_id, .. } => {
            let outcome = crate::rt::block_on({
                let candidate_id = candidate_id.clone();
                async move {
                    state
                        .writer()
                        .transaction(move |tx| reject_candidate(tx, &candidate_id))
                        .await
                }
            });
            match outcome {
                Ok(Ok(())) => (format!("rejected {candidate_id}"), false),
                Ok(Err(e)) => (review_error_message(&e), true),
                Err(e) => (format!("could not reject {candidate_id}: {e}"), true),
            }
        }
        MemoryAction::Edit {
            memory_id,
            expected_version,
            text,
            importance,
            ..
        } => {
            let outcome = crate::rt::block_on({
                let memory_id = memory_id.clone();
                let text = text.clone();
                async move {
                    state
                        .writer()
                        .transaction(move |tx| {
                            apply_edit(
                                tx,
                                &EditMemoryOp {
                                    memory_id: &memory_id,
                                    expected_version,
                                    text: Some(text.as_str()),
                                    importance: Some(importance),
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
                Ok(Ok(op_outcome)) => (
                    format!(
                        "edited {memory_id} -> entry_version {}, audit_id {}",
                        op_outcome_entry_version(&op_outcome),
                        op_outcome_audit_id(&op_outcome)
                    ),
                    false,
                ),
                Ok(Err(e)) => (memory_op_error_message(&e), true),
                Err(e) => (format!("could not edit {memory_id}: {e}"), true),
            }
        }
        MemoryAction::Retract {
            memory_id,
            expected_version,
            ..
        } => {
            let outcome = crate::rt::block_on({
                let memory_id = memory_id.clone();
                async move {
                    state
                        .writer()
                        .transaction(move |tx| {
                            apply_retract(
                                tx,
                                &RetractMemoryOp {
                                    memory_id: &memory_id,
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
                Ok(Ok(op_outcome)) => (
                    format!(
                        "retracted {memory_id} -> entry_version {}, audit_id {}",
                        op_outcome_entry_version(&op_outcome),
                        op_outcome_audit_id(&op_outcome)
                    ),
                    false,
                ),
                Ok(Err(e)) => (memory_op_error_message(&e), true),
                Err(e) => (format!("could not retract {memory_id}: {e}"), true),
            }
        }
        MemoryAction::Merge {
            survivor_id,
            survivor_expected_version,
            losers,
            ..
        } => {
            let outcome = crate::rt::block_on({
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
                Ok(Ok(op_outcome)) => (
                    format!(
                        "merged {} loser(s) into {survivor_id} -> entry_version {}, audit_id {}",
                        losers.len(),
                        op_outcome_entry_version(&op_outcome),
                        op_outcome_audit_id(&op_outcome)
                    ),
                    false,
                ),
                Ok(Err(e)) => (memory_op_error_message(&e), true),
                Err(e) => (format!("could not merge into {survivor_id}: {e}"), true),
            }
        }
    };

    MemoryNav::ActionResult {
        message,
        is_error,
        list,
    }
}

/// Pure render — no I/O, `TestBackend`-testable without a daemon or a store.
pub fn render_memory(frame: &mut ratatui::Frame, data: &MemoryScreenData) {
    use ratatui::layout::{Constraint, Layout};
    use ratatui::text::Line;
    use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph};

    match data {
        MemoryScreenData::Unavailable { reason } => {
            frame.render_widget(
                Paragraph::new(reason.as_str()).block(Block::bordered().title("Memory")),
                frame.area(),
            );
        }
        MemoryScreenData::EntryList {
            scope_label,
            kind_filter,
            state_filter,
            scope_filter,
            rows,
            total,
            has_more,
            selected,
        } => {
            let [filter_area, list_area, footer_area] = Layout::vertical([
                Constraint::Length(3),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .areas(frame.area());

            let filter_line = format!(
                "scope {scope_label}  kind={}  state={}  scope-filter={}",
                kind_filter.map(|k| k.as_str()).unwrap_or("(any)"),
                state_filter.map(|s| s.as_str()).unwrap_or("(any)"),
                scope_filter.map(|s| s.as_str()).unwrap_or("(any)"),
            );
            frame.render_widget(
                Paragraph::new(filter_line).block(Block::bordered().title("Memory — entries")),
                filter_area,
            );

            let items: Vec<ListItem> = if rows.is_empty() {
                vec![ListItem::new("no memory entries match these filters")]
            } else {
                rows.iter()
                    .map(|r| {
                        let preview: String = r.text.chars().take(60).collect();
                        ListItem::new(format!(
                            "{}  {}/{}  created_at={}  scope={}:{}  {}",
                            r.memory_id,
                            r.kind.as_str(),
                            r.state.as_str(),
                            r.created_at,
                            r.scope_kind.as_str(),
                            r.scope_owner_id,
                            preview,
                        ))
                    })
                    .collect()
            };
            let list = List::new(items).highlight_symbol("> ");
            let mut state =
                ListState::default().with_selected((!rows.is_empty()).then_some(*selected));
            frame.render_stateful_widget(list, list_area, &mut state);

            let footer = format!(
                "{total} total{}",
                if *has_more {
                    " (more available — PageDown)"
                } else {
                    ""
                }
            );
            frame.render_widget(Paragraph::new(footer), footer_area);
        }
        MemoryScreenData::CandidateList {
            state_filter,
            rows,
            has_more,
            selected,
        } => {
            let [filter_area, list_area, footer_area] = Layout::vertical([
                Constraint::Length(3),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .areas(frame.area());

            let filter_line = format!(
                "state={}",
                state_filter.map(|s| s.as_str()).unwrap_or("(any)")
            );
            frame.render_widget(
                Paragraph::new(filter_line).block(Block::bordered().title("Memory — candidates")),
                filter_area,
            );

            let items: Vec<ListItem> = if rows.is_empty() {
                vec![ListItem::new("no candidates match this filter")]
            } else {
                rows.iter()
                    .map(|r| {
                        ListItem::new(format!(
                            "{}  {}  created_at={}",
                            r.candidate_id,
                            r.review_state.as_str(),
                            r.created_at,
                        ))
                    })
                    .collect()
            };
            let list = List::new(items).highlight_symbol("> ");
            let mut state =
                ListState::default().with_selected((!rows.is_empty()).then_some(*selected));
            frame.render_stateful_widget(list, list_area, &mut state);

            let footer = if *has_more {
                "more candidates available — PageDown"
            } else {
                ""
            };
            frame.render_widget(Paragraph::new(footer), footer_area);
        }
        MemoryScreenData::EntryDetail {
            entry,
            evidence_ids,
        } => {
            let [detail_area, evidence_area] =
                Layout::vertical([Constraint::Length(9), Constraint::Min(0)]).areas(frame.area());

            let lines = vec![
                Line::from(format!("memory_id: {}", entry.memory_id)),
                Line::from(format!(
                    "kind/state: {}/{}",
                    entry.kind.as_str(),
                    entry.state.as_str()
                )),
                Line::from(format!(
                    "scope: {}:{}",
                    entry.scope_kind.as_str(),
                    entry.scope_owner_id
                )),
                Line::from(format!(
                    "confidence={:.2} importance={:.2} v{}",
                    entry.confidence, entry.importance, entry.entry_version
                )),
                Line::from(format!(
                    "created_at={} updated_at={}",
                    entry.created_at, entry.updated_at
                )),
                Line::from(entry.text.clone()),
            ];
            frame.render_widget(
                Paragraph::new(lines).block(Block::bordered().title("Entry detail")),
                detail_area,
            );

            let items: Vec<ListItem> = if evidence_ids.is_empty() {
                vec![ListItem::new("(no evidence)")]
            } else {
                evidence_ids
                    .iter()
                    .map(|id| ListItem::new(id.clone()))
                    .collect()
            };
            frame.render_widget(
                List::new(items).block(Block::bordered().title("Evidence")),
                evidence_area,
            );
        }
        MemoryScreenData::EditForm {
            memory_id,
            expected_version,
            field,
            text,
            importance,
            error,
        } => {
            let [text_area, importance_area, footer_area] = Layout::vertical([
                Constraint::Length(5),
                Constraint::Length(3),
                Constraint::Length(1),
            ])
            .areas(frame.area());

            let text_title = if *field == EditField::Text {
                "Text [editing]"
            } else {
                "Text"
            };
            frame.render_widget(
                Paragraph::new(text.as_str()).block(
                    Block::bordered()
                        .title(format!("{text_title} — {memory_id} v{expected_version}")),
                ),
                text_area,
            );

            let importance_title = if *field == EditField::Importance {
                "Importance [editing]"
            } else {
                "Importance"
            };
            frame.render_widget(
                Paragraph::new(importance.as_str())
                    .block(Block::bordered().title(importance_title)),
                importance_area,
            );

            let footer = error
                .as_deref()
                .unwrap_or("Tab: switch field  Enter: submit  Ctrl+X: cancel");
            frame.render_widget(Paragraph::new(footer), footer_area);
        }
        MemoryScreenData::MergeSelect {
            rows,
            has_more,
            cursor,
            survivor,
            losers,
            error,
        } => {
            let [list_area, footer_area] =
                Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(frame.area());

            let items: Vec<ListItem> = if rows.is_empty() {
                vec![ListItem::new("no memory entries to merge")]
            } else {
                rows.iter()
                    .map(|r| {
                        let tag = if survivor.as_ref().map(|s| s.0.as_str())
                            == Some(r.memory_id.as_str())
                        {
                            "[survivor]"
                        } else if losers.iter().any(|l| l.0 == r.memory_id) {
                            "[loser]"
                        } else {
                            ""
                        };
                        let preview: String = r.text.chars().take(40).collect();
                        ListItem::new(format!(
                            "{tag} {}  v{}  {}",
                            r.memory_id, r.entry_version, preview
                        ))
                    })
                    .collect()
            };
            let list = List::new(items)
                .block(Block::bordered().title("Merge — Enter: survivor  Space: loser  m: execute"))
                .highlight_symbol("> ");
            let mut state =
                ListState::default().with_selected((!rows.is_empty()).then_some(*cursor));
            frame.render_stateful_widget(list, list_area, &mut state);

            let footer = error.clone().unwrap_or_else(|| {
                format!(
                    "survivor={}  losers={}{}",
                    survivor.as_ref().map(|s| s.0.as_str()).unwrap_or("(none)"),
                    losers.len(),
                    if *has_more {
                        "  (more available — PageDown)"
                    } else {
                        ""
                    },
                )
            });
            frame.render_widget(Paragraph::new(footer), footer_area);
        }
        MemoryScreenData::ConfirmAction { description } => {
            frame.render_widget(
                Paragraph::new(format!("Confirm: {description}?  [y]es / [n]o"))
                    .block(Block::bordered().title("Confirm")),
                frame.area(),
            );
        }
        MemoryScreenData::ActionResult { message, is_error } => {
            let title = if *is_error { "Error" } else { "Done" };
            frame.render_widget(
                Paragraph::new(message.as_str()).block(Block::bordered().title(title)),
                frame.area(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn rendered_text(data: &MemoryScreenData) -> String {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test backend terminal");
        terminal
            .draw(|frame| render_memory(frame, data))
            .expect("draw memory screen");
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    fn entry_row(id: &str, kind: MemoryKind, state: MemoryState) -> MemoryEntryRow {
        MemoryEntryRow {
            memory_id: id.to_string(),
            kind,
            state,
            text: "some memory text".to_string(),
            canonical_key: None,
            scope_kind: ScopeKind::Global,
            scope_owner_id: local_rag_store::GLOBAL_SCOPE_OWNER_ID.to_string(),
            confidence: 0.5,
            importance: 0.5,
            valid_from_tree: None,
            last_verified_tree: None,
            supersedes_id: None,
            entry_version: 1,
            created_at: 1_000,
            updated_at: 1_000,
        }
    }

    fn candidate_row(id: &str, state: CandidateState) -> CandidateRow {
        CandidateRow {
            candidate_id: id.to_string(),
            proposed_operation: "{}".to_string(),
            conflicts: None,
            review_state: state,
            created_at: 1_000,
        }
    }

    fn press(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn press_ctrl(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::CONTROL))
    }

    fn nav_of(outcome: MemoryKeyOutcome) -> MemoryNav {
        match outcome {
            MemoryKeyOutcome::Nav(nav) => nav,
            MemoryKeyOutcome::Execute(action) => panic!("expected Nav, got Execute({action:?})"),
        }
    }

    fn execute_of(outcome: MemoryKeyOutcome) -> MemoryAction {
        match outcome {
            MemoryKeyOutcome::Execute(action) => action,
            MemoryKeyOutcome::Nav(nav) => panic!("expected Execute, got Nav({nav:?})"),
        }
    }

    // ---- render tests ----

    #[test]
    fn renders_unavailable_reason() {
        let data = MemoryScreenData::Unavailable {
            reason: "store not yet initialized".to_string(),
        };
        assert!(rendered_text(&data).contains("not yet initialized"));
    }

    #[test]
    fn renders_empty_entry_list() {
        let data = MemoryScreenData::EntryList {
            scope_label: "global".to_string(),
            kind_filter: None,
            state_filter: None,
            scope_filter: None,
            rows: vec![],
            total: 0,
            has_more: false,
            selected: 0,
        };
        assert!(rendered_text(&data).contains("no memory entries match these filters"));
    }

    #[test]
    fn renders_populated_entry_list_with_filter_bar() {
        let data = MemoryScreenData::EntryList {
            scope_label: "repo:repo-a".to_string(),
            kind_filter: Some(MemoryKind::Decision),
            state_filter: Some(MemoryState::Active),
            scope_filter: Some(ScopeKind::Repository),
            rows: vec![entry_row(
                "mem-1",
                MemoryKind::Decision,
                MemoryState::Active,
            )],
            total: 1,
            has_more: true,
            selected: 0,
        };
        let content = rendered_text(&data);
        assert!(content.contains("mem-1"), "{content}");
        assert!(content.contains("decision/active"), "{content}");
        assert!(content.contains("repo:repo-a"), "{content}");
        assert!(content.contains("more available"), "{content}");
        assert!(content.contains("created_at=1000"), "{content}");
    }

    #[test]
    fn renders_candidate_list() {
        let data = MemoryScreenData::CandidateList {
            state_filter: Some(CandidateState::Pending),
            rows: vec![candidate_row("cand-1", CandidateState::Pending)],
            has_more: false,
            selected: 0,
        };
        let content = rendered_text(&data);
        assert!(content.contains("cand-1"), "{content}");
        assert!(content.contains("pending"), "{content}");
    }

    #[test]
    fn renders_entry_detail_with_evidence() {
        let data = MemoryScreenData::EntryDetail {
            entry: entry_row("mem-1", MemoryKind::Fact, MemoryState::Active),
            evidence_ids: vec!["obs-1".to_string(), "obs-2".to_string()],
        };
        let content = rendered_text(&data);
        assert!(content.contains("mem-1"), "{content}");
        assert!(content.contains("obs-1"), "{content}");
        assert!(content.contains("obs-2"), "{content}");
        assert!(
            content.contains("created_at=1000") && content.contains("updated_at=1000"),
            "{content}"
        );
    }

    #[test]
    fn renders_entry_detail_with_no_evidence() {
        let data = MemoryScreenData::EntryDetail {
            entry: entry_row("mem-1", MemoryKind::Fact, MemoryState::Active),
            evidence_ids: vec![],
        };
        assert!(rendered_text(&data).contains("(no evidence)"));
    }

    #[test]
    fn renders_edit_form() {
        let data = MemoryScreenData::EditForm {
            memory_id: "mem-1".to_string(),
            expected_version: 3,
            field: EditField::Text,
            text: "some text".to_string(),
            importance: "0.50".to_string(),
            error: None,
        };
        let content = rendered_text(&data);
        assert!(content.contains("mem-1"), "{content}");
        assert!(content.contains("some text"), "{content}");
        assert!(content.contains("0.50"), "{content}");
        assert!(content.contains("editing"), "{content}");
    }

    #[test]
    fn renders_edit_form_error() {
        let data = MemoryScreenData::EditForm {
            memory_id: "mem-1".to_string(),
            expected_version: 3,
            field: EditField::Importance,
            text: "some text".to_string(),
            importance: "abc".to_string(),
            error: Some("importance must be a number between 0 and 1".to_string()),
        };
        assert!(rendered_text(&data).contains("importance must be a number"));
    }

    #[test]
    fn renders_merge_select_with_survivor_and_loser_tags() {
        let data = MemoryScreenData::MergeSelect {
            rows: vec![
                entry_row("mem-a", MemoryKind::Fact, MemoryState::Active),
                entry_row("mem-b", MemoryKind::Fact, MemoryState::Active),
            ],
            has_more: false,
            cursor: 0,
            survivor: Some(("mem-a".to_string(), 1)),
            losers: vec![("mem-b".to_string(), 1)],
            error: None,
        };
        let content = rendered_text(&data);
        assert!(content.contains("survivor"), "{content}");
        assert!(content.contains("loser"), "{content}");
        assert!(content.contains("mem-a"), "{content}");
        assert!(content.contains("mem-b"), "{content}");
    }

    #[test]
    fn renders_confirm_action() {
        let data = MemoryScreenData::ConfirmAction {
            description: "retract memory entry mem-1".to_string(),
        };
        assert!(rendered_text(&data).contains("retract memory entry mem-1"));
    }

    #[test]
    fn renders_action_result_success_and_error() {
        let success = MemoryScreenData::ActionResult {
            message: "rejected cand-1".to_string(),
            is_error: false,
        };
        assert!(rendered_text(&success).contains("rejected cand-1"));

        let failure = MemoryScreenData::ActionResult {
            message: "optimistic conflict: expected version 1, actual version 2".to_string(),
            is_error: true,
        };
        assert!(rendered_text(&failure).contains("optimistic conflict"));
    }

    // ---- navigation unit tests ----

    #[test]
    fn down_and_up_clamp_at_both_ends() {
        let data = MemoryScreenData::EntryList {
            scope_label: "global".to_string(),
            kind_filter: None,
            state_filter: None,
            scope_filter: None,
            rows: vec![
                entry_row("a", MemoryKind::Fact, MemoryState::Active),
                entry_row("b", MemoryKind::Fact, MemoryState::Active),
            ],
            total: 2,
            has_more: false,
            selected: 0,
        };
        let nav = MemoryNav::default();
        let nav = nav.moved(&data, true);
        assert_eq!(
            nav,
            MemoryNav::List(ListNav {
                selected: 1,
                ..ListNav::default()
            })
        );
        let nav = nav.moved(&data, true);
        assert_eq!(
            nav,
            MemoryNav::List(ListNav {
                selected: 1,
                ..ListNav::default()
            })
        );
        let nav = nav.moved(&data, false);
        assert_eq!(
            nav,
            MemoryNav::List(ListNav {
                selected: 0,
                ..ListNav::default()
            })
        );
    }

    #[test]
    fn enter_descends_to_entry_detail_and_backspace_restores_list_verbatim() {
        let list = ListNav {
            kind_filter: Some(MemoryKind::Decision),
            offset: 20,
            selected: 0,
            ..ListNav::default()
        };
        let data = MemoryScreenData::EntryList {
            scope_label: "global".to_string(),
            kind_filter: list.kind_filter,
            state_filter: None,
            scope_filter: None,
            rows: vec![entry_row(
                "mem-1",
                MemoryKind::Decision,
                MemoryState::Active,
            )],
            total: 1,
            has_more: false,
            selected: 0,
        };
        let nav = MemoryNav::List(list.clone()).descend(&data);
        assert_eq!(
            nav,
            MemoryNav::EntryDetail {
                memory_id: "mem-1".to_string(),
                list: list.clone(),
            }
        );
        assert_eq!(nav.ascend(), MemoryNav::List(list));
    }

    #[test]
    fn enter_is_a_no_op_in_candidates_mode_or_on_empty_list() {
        let candidates_data = MemoryScreenData::CandidateList {
            state_filter: None,
            rows: vec![candidate_row("cand-1", CandidateState::Pending)],
            has_more: false,
            selected: 0,
        };
        let candidates_nav = MemoryNav::List(ListNav {
            mode: MemoryMode::Candidates,
            ..ListNav::default()
        });
        assert_eq!(candidates_nav.descend(&candidates_data), candidates_nav);

        let empty_data = MemoryScreenData::EntryList {
            scope_label: "global".to_string(),
            kind_filter: None,
            state_filter: None,
            scope_filter: None,
            rows: vec![],
            total: 0,
            has_more: false,
            selected: 0,
        };
        let entries_nav = MemoryNav::default();
        assert_eq!(entries_nav.descend(&empty_data), entries_nav);
    }

    #[test]
    fn tab_toggles_mode_and_preserves_both_modes_filters() {
        let nav = MemoryNav::List(ListNav {
            kind_filter: Some(MemoryKind::Fact),
            candidate_state_filter: Some(CandidateState::Pending),
            offset: 10,
            selected: 3,
            ..ListNav::default()
        });
        let toggled = nav.toggle_mode();
        match &toggled {
            MemoryNav::List(list) => {
                assert_eq!(list.mode, MemoryMode::Candidates);
                assert_eq!(list.kind_filter, Some(MemoryKind::Fact));
                assert_eq!(list.candidate_state_filter, Some(CandidateState::Pending));
                assert_eq!(list.offset, 0);
                assert_eq!(list.selected, 0);
            }
            other => panic!("expected List, got {other:?}"),
        }
        let back = toggled.toggle_mode();
        match back {
            MemoryNav::List(list) => assert_eq!(list.mode, MemoryMode::Entries),
            other => panic!("expected List, got {other:?}"),
        }
    }

    #[test]
    fn cycle_kind_wraps_through_none_and_is_a_no_op_outside_entries_mode() {
        let nav = MemoryNav::default();
        let nav = nav.cycle_kind(true);
        assert_eq!(
            nav,
            MemoryNav::List(ListNav {
                kind_filter: Some(MemoryKind::Fact),
                ..ListNav::default()
            })
        );
        let nav = MemoryNav::List(ListNav {
            kind_filter: Some(MemoryKind::Hypothesis),
            ..ListNav::default()
        })
        .cycle_kind(true);
        assert_eq!(nav, MemoryNav::default());

        let candidates_nav = MemoryNav::List(ListNav {
            mode: MemoryMode::Candidates,
            ..ListNav::default()
        });
        assert_eq!(candidates_nav.cycle_kind(true), candidates_nav);
    }

    #[test]
    fn cycle_state_picks_the_right_domain_per_mode() {
        let entries_nav = MemoryNav::default().cycle_state(true);
        assert_eq!(
            entries_nav,
            MemoryNav::List(ListNav {
                entry_state_filter: Some(MemoryState::Active),
                ..ListNav::default()
            })
        );

        let candidates_nav = MemoryNav::List(ListNav {
            mode: MemoryMode::Candidates,
            ..ListNav::default()
        })
        .cycle_state(true);
        assert_eq!(
            candidates_nav,
            MemoryNav::List(ListNav {
                mode: MemoryMode::Candidates,
                candidate_state_filter: Some(CandidateState::Pending),
                ..ListNav::default()
            })
        );
    }

    #[test]
    fn cycle_scope_is_a_no_op_outside_entries_mode() {
        let candidates_nav = MemoryNav::List(ListNav {
            mode: MemoryMode::Candidates,
            ..ListNav::default()
        });
        assert_eq!(candidates_nav.cycle_scope(true), candidates_nav);

        let nav = MemoryNav::default().cycle_scope(true);
        assert_eq!(
            nav,
            MemoryNav::List(ListNav {
                scope_filter: Some(ScopeKind::Global),
                ..ListNav::default()
            })
        );
    }

    #[test]
    fn paged_no_ops_past_has_more_and_below_zero() {
        let no_more = MemoryScreenData::EntryList {
            scope_label: "global".to_string(),
            kind_filter: None,
            state_filter: None,
            scope_filter: None,
            rows: vec![],
            total: 0,
            has_more: false,
            selected: 0,
        };
        let nav = MemoryNav::default();
        assert_eq!(nav.paged(&no_more, true), nav);
        assert_eq!(nav.paged(&no_more, false), nav);

        let has_more = MemoryScreenData::EntryList {
            scope_label: "global".to_string(),
            kind_filter: None,
            state_filter: None,
            scope_filter: None,
            rows: vec![],
            total: 0,
            has_more: true,
            selected: 0,
        };
        let nav = nav.paged(&has_more, true);
        assert_eq!(
            nav,
            MemoryNav::List(ListNav {
                offset: PAGE_SIZE,
                ..ListNav::default()
            })
        );
    }

    #[test]
    fn handle_memory_key_dispatches_and_ignores_key_release() {
        let data = MemoryScreenData::EntryList {
            scope_label: "global".to_string(),
            kind_filter: None,
            state_filter: None,
            scope_filter: None,
            rows: vec![
                entry_row("a", MemoryKind::Fact, MemoryState::Active),
                entry_row("b", MemoryKind::Fact, MemoryState::Active),
            ],
            total: 2,
            has_more: false,
            selected: 0,
        };
        let nav = MemoryNav::default();

        assert_eq!(
            nav_of(handle_memory_key(&nav, &data, press(KeyCode::Down))),
            MemoryNav::List(ListNav {
                selected: 1,
                ..ListNav::default()
            })
        );

        let mut release = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        assert_eq!(
            nav_of(handle_memory_key(&nav, &data, Event::Key(release))),
            nav
        );
    }

    // ---- T18-05 mutation-trigger tests ----

    #[test]
    fn a_and_r_trigger_approve_and_reject_directly_in_candidates_mode() {
        let list = ListNav {
            mode: MemoryMode::Candidates,
            ..ListNav::default()
        };
        let data = MemoryScreenData::CandidateList {
            state_filter: None,
            rows: vec![candidate_row("cand-1", CandidateState::Pending)],
            has_more: false,
            selected: 0,
        };
        let nav = MemoryNav::List(list);

        let approve = execute_of(handle_memory_key(&nav, &data, press(KeyCode::Char('a'))));
        assert_eq!(
            approve,
            MemoryAction::Approve {
                candidate_id: "cand-1".to_string(),
                list: match &nav {
                    MemoryNav::List(l) => l.clone(),
                    _ => unreachable!(),
                },
            }
        );

        let reject = execute_of(handle_memory_key(&nav, &data, press(KeyCode::Char('r'))));
        assert!(
            matches!(reject, MemoryAction::Reject { candidate_id, .. } if candidate_id == "cand-1")
        );
    }

    #[test]
    fn a_and_r_are_no_ops_in_entries_mode() {
        let nav = MemoryNav::default();
        let data = MemoryScreenData::EntryList {
            scope_label: "global".to_string(),
            kind_filter: None,
            state_filter: None,
            scope_filter: None,
            rows: vec![entry_row("a", MemoryKind::Fact, MemoryState::Active)],
            total: 1,
            has_more: false,
            selected: 0,
        };
        assert_eq!(
            nav_of(handle_memory_key(&nav, &data, press(KeyCode::Char('a')))),
            nav
        );
        assert_eq!(
            nav_of(handle_memory_key(&nav, &data, press(KeyCode::Char('r')))),
            nav
        );
    }

    #[test]
    fn e_opens_edit_form_seeded_with_current_text_and_importance() {
        let mut row = entry_row("mem-1", MemoryKind::Fact, MemoryState::Active);
        row.text = "hello world".to_string();
        row.importance = 0.75;
        row.entry_version = 4;
        let data = MemoryScreenData::EntryList {
            scope_label: "global".to_string(),
            kind_filter: None,
            state_filter: None,
            scope_filter: None,
            rows: vec![row],
            total: 1,
            has_more: false,
            selected: 0,
        };
        let nav = MemoryNav::default();
        let next = nav_of(handle_memory_key(&nav, &data, press(KeyCode::Char('e'))));
        assert_eq!(
            next,
            MemoryNav::EditForm {
                memory_id: "mem-1".to_string(),
                expected_version: 4,
                field: EditField::Text,
                text: "hello world".to_string(),
                importance: "0.75".to_string(),
                error: None,
                list: ListNav::default(),
            }
        );
    }

    #[test]
    fn edit_form_typing_appends_including_q_and_digits_backspace_deletes() {
        let nav = MemoryNav::EditForm {
            memory_id: "mem-1".to_string(),
            expected_version: 1,
            field: EditField::Text,
            text: String::new(),
            importance: "0.50".to_string(),
            error: None,
            list: ListNav::default(),
        };
        let data = MemoryScreenData::EditForm {
            memory_id: "mem-1".to_string(),
            expected_version: 1,
            field: EditField::Text,
            text: String::new(),
            importance: "0.50".to_string(),
            error: None,
        };

        let nav = nav_of(handle_memory_key(&nav, &data, press(KeyCode::Char('q'))));
        let nav = nav_of(handle_memory_key(&nav, &data, press(KeyCode::Char('7'))));
        match &nav {
            MemoryNav::EditForm { text, .. } => assert_eq!(text, "q7"),
            other => panic!("expected EditForm, got {other:?}"),
        }

        let nav = nav_of(handle_memory_key(&nav, &data, press(KeyCode::Backspace)));
        match nav {
            MemoryNav::EditForm { text, .. } => assert_eq!(text, "q"),
            other => panic!("expected EditForm, got {other:?}"),
        }
    }

    #[test]
    fn edit_form_tab_switches_field() {
        let nav = MemoryNav::EditForm {
            memory_id: "mem-1".to_string(),
            expected_version: 1,
            field: EditField::Text,
            text: String::new(),
            importance: "0.50".to_string(),
            error: None,
            list: ListNav::default(),
        };
        let data = MemoryScreenData::EditForm {
            memory_id: "mem-1".to_string(),
            expected_version: 1,
            field: EditField::Text,
            text: String::new(),
            importance: "0.50".to_string(),
            error: None,
        };
        let next = nav_of(handle_memory_key(&nav, &data, press(KeyCode::Tab)));
        match next {
            MemoryNav::EditForm { field, .. } => assert_eq!(field, EditField::Importance),
            other => panic!("expected EditForm, got {other:?}"),
        }
    }

    #[test]
    fn edit_form_enter_with_bad_importance_stays_with_error() {
        let nav = MemoryNav::EditForm {
            memory_id: "mem-1".to_string(),
            expected_version: 1,
            field: EditField::Importance,
            text: "text".to_string(),
            importance: "2".to_string(),
            error: None,
            list: ListNav::default(),
        };
        let data = MemoryScreenData::EditForm {
            memory_id: "mem-1".to_string(),
            expected_version: 1,
            field: EditField::Importance,
            text: "text".to_string(),
            importance: "2".to_string(),
            error: None,
        };
        let next = nav_of(handle_memory_key(&nav, &data, press(KeyCode::Enter)));
        match next {
            MemoryNav::EditForm { error: Some(e), .. } => {
                assert!(e.contains("between 0 and 1"), "{e}");
            }
            other => panic!("expected EditForm with error, got {other:?}"),
        }
    }

    #[test]
    fn edit_form_enter_with_valid_importance_executes_directly_no_confirm() {
        let nav = MemoryNav::EditForm {
            memory_id: "mem-1".to_string(),
            expected_version: 2,
            field: EditField::Importance,
            text: "new text".to_string(),
            importance: "0.9".to_string(),
            error: None,
            list: ListNav::default(),
        };
        let data = MemoryScreenData::EditForm {
            memory_id: "mem-1".to_string(),
            expected_version: 2,
            field: EditField::Importance,
            text: "new text".to_string(),
            importance: "0.9".to_string(),
            error: None,
        };
        let action = execute_of(handle_memory_key(&nav, &data, press(KeyCode::Enter)));
        assert_eq!(
            action,
            MemoryAction::Edit {
                memory_id: "mem-1".to_string(),
                expected_version: 2,
                text: "new text".to_string(),
                importance: 0.9,
                list: ListNav::default(),
            }
        );
    }

    #[test]
    fn edit_form_ctrl_x_cancels_to_list() {
        let nav = MemoryNav::EditForm {
            memory_id: "mem-1".to_string(),
            expected_version: 1,
            field: EditField::Text,
            text: "abc".to_string(),
            importance: "0.5".to_string(),
            error: None,
            list: ListNav::default(),
        };
        let data = MemoryScreenData::EditForm {
            memory_id: "mem-1".to_string(),
            expected_version: 1,
            field: EditField::Text,
            text: "abc".to_string(),
            importance: "0.5".to_string(),
            error: None,
        };
        assert_eq!(
            nav_of(handle_memory_key(
                &nav,
                &data,
                press_ctrl(KeyCode::Char('x'))
            )),
            MemoryNav::List(ListNav::default())
        );
    }

    #[test]
    fn x_on_a_selected_entry_opens_confirm_action_using_the_real_catalog() {
        let data = MemoryScreenData::EntryList {
            scope_label: "global".to_string(),
            kind_filter: None,
            state_filter: None,
            scope_filter: None,
            rows: vec![entry_row("mem-1", MemoryKind::Fact, MemoryState::Active)],
            total: 1,
            has_more: false,
            selected: 0,
        };
        let nav = MemoryNav::default();
        let next = nav_of(handle_memory_key(&nav, &data, press(KeyCode::Char('x'))));
        match next {
            MemoryNav::ConfirmAction { action, .. } => {
                assert!(matches!(*action, MemoryAction::Retract { .. }));
            }
            other => panic!("expected ConfirmAction, got {other:?}"),
        }
    }

    #[test]
    fn confirm_action_enter_executes_backspace_cancels() {
        let action = MemoryAction::Retract {
            memory_id: "mem-1".to_string(),
            expected_version: 1,
            list: ListNav::default(),
        };
        let nav = MemoryNav::ConfirmAction {
            action: Box::new(action.clone()),
            list: ListNav::default(),
        };
        let data = MemoryScreenData::ConfirmAction {
            description: "retract memory entry mem-1".to_string(),
        };

        assert_eq!(
            execute_of(handle_memory_key(&nav, &data, press(KeyCode::Enter))),
            action
        );
        assert_eq!(
            nav_of(handle_memory_key(&nav, &data, press(KeyCode::Backspace))),
            MemoryNav::List(ListNav::default())
        );
        assert_eq!(
            nav_of(handle_memory_key(&nav, &data, press(KeyCode::Char('n')))),
            MemoryNav::List(ListNav::default())
        );
    }

    #[test]
    fn edit_and_merge_never_reach_confirm_action() {
        // edit_memory / merge_memories are both destructiveHint: false in the real catalog — gate()
        // must return Execute directly for both, never wrapping them in ConfirmAction.
        let edit_action = MemoryAction::Edit {
            memory_id: "mem-1".to_string(),
            expected_version: 1,
            text: "t".to_string(),
            importance: 0.5,
            list: ListNav::default(),
        };
        assert_eq!(
            gate(edit_action.clone()),
            MemoryKeyOutcome::Execute(edit_action)
        );

        let merge_action = MemoryAction::Merge {
            survivor_id: "mem-1".to_string(),
            survivor_expected_version: 1,
            losers: vec![("mem-2".to_string(), 1)],
            list: ListNav::default(),
        };
        assert_eq!(
            gate(merge_action.clone()),
            MemoryKeyOutcome::Execute(merge_action)
        );
    }

    #[test]
    fn merge_select_enter_sets_survivor_space_toggles_loser_m_executes() {
        let list = ListNav::default();
        let data = MemoryScreenData::MergeSelect {
            rows: vec![
                entry_row("mem-a", MemoryKind::Fact, MemoryState::Active),
                entry_row("mem-b", MemoryKind::Fact, MemoryState::Active),
            ],
            has_more: false,
            cursor: 0,
            survivor: None,
            losers: vec![],
            error: None,
        };
        let nav = MemoryNav::MergeSelect {
            list: list.clone(),
            cursor: 0,
            survivor: None,
            losers: vec![],
            error: None,
        };

        // m with nothing picked yet -> stays with an error, not Execute.
        let after_m = nav_of(handle_memory_key(&nav, &data, press(KeyCode::Char('m'))));
        match after_m {
            MemoryNav::MergeSelect { error: Some(_), .. } => {}
            other => panic!("expected MergeSelect with error, got {other:?}"),
        }

        // Enter on row 0 (mem-a) sets it as survivor.
        let nav = nav_of(handle_memory_key(&nav, &data, press(KeyCode::Enter)));
        let nav = nav_of(handle_memory_key(&nav, &data, press(KeyCode::Down)));
        // Space on row 1 (mem-b) toggles it as a loser.
        let nav = nav_of(handle_memory_key(&nav, &data, press(KeyCode::Char(' '))));
        match &nav {
            MemoryNav::MergeSelect {
                survivor: Some((sid, _)),
                losers,
                ..
            } => {
                assert_eq!(sid, "mem-a");
                assert_eq!(losers.len(), 1);
                assert_eq!(losers[0].0, "mem-b");
            }
            other => panic!("expected MergeSelect with survivor+loser, got {other:?}"),
        }

        let data_with_picks = MemoryScreenData::MergeSelect {
            rows: vec![
                entry_row("mem-a", MemoryKind::Fact, MemoryState::Active),
                entry_row("mem-b", MemoryKind::Fact, MemoryState::Active),
            ],
            has_more: false,
            cursor: 1,
            survivor: Some(("mem-a".to_string(), 1)),
            losers: vec![("mem-b".to_string(), 1)],
            error: None,
        };
        let action = execute_of(handle_memory_key(
            &nav,
            &data_with_picks,
            press(KeyCode::Char('m')),
        ));
        assert_eq!(
            action,
            MemoryAction::Merge {
                survivor_id: "mem-a".to_string(),
                survivor_expected_version: 1,
                losers: vec![("mem-b".to_string(), 1)],
                list,
            }
        );
    }

    #[test]
    fn merge_select_backspace_and_ctrl_x_cancel_to_list() {
        let nav = MemoryNav::MergeSelect {
            list: ListNav::default(),
            cursor: 0,
            survivor: Some(("mem-a".to_string(), 1)),
            losers: vec![("mem-b".to_string(), 1)],
            error: None,
        };
        let data = MemoryScreenData::MergeSelect {
            rows: vec![entry_row("mem-a", MemoryKind::Fact, MemoryState::Active)],
            has_more: false,
            cursor: 0,
            survivor: Some(("mem-a".to_string(), 1)),
            losers: vec![("mem-b".to_string(), 1)],
            error: None,
        };
        assert_eq!(
            nav_of(handle_memory_key(&nav, &data, press(KeyCode::Backspace))),
            MemoryNav::List(ListNav::default())
        );
        assert_eq!(
            nav_of(handle_memory_key(
                &nav,
                &data,
                press_ctrl(KeyCode::Char('x'))
            )),
            MemoryNav::List(ListNav::default())
        );
    }

    #[test]
    fn action_result_dismisses_on_any_key() {
        let nav = MemoryNav::ActionResult {
            message: "rejected cand-1".to_string(),
            is_error: false,
            list: ListNav::default(),
        };
        let data = MemoryScreenData::ActionResult {
            message: "rejected cand-1".to_string(),
            is_error: false,
        };
        assert_eq!(
            nav_of(handle_memory_key(&nav, &data, press(KeyCode::Char('z')))),
            MemoryNav::List(ListNav::default())
        );
    }

    #[test]
    fn captures_all_keys_is_true_only_for_edit_form() {
        assert!(!captures_all_keys(&MemoryNav::default()));
        assert!(!captures_all_keys(&MemoryNav::EntryDetail {
            memory_id: "m".to_string(),
            list: ListNav::default(),
        }));
        assert!(captures_all_keys(&MemoryNav::EditForm {
            memory_id: "m".to_string(),
            expected_version: 1,
            field: EditField::Text,
            text: String::new(),
            importance: String::new(),
            error: None,
            list: ListNav::default(),
        }));
    }
}
