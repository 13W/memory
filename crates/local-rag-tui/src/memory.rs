//! The Memory screen (spec 11 §7, T18-04): browse memory entries/candidates with kind/state/scope
//! filters and pagination, drill into a selected entry's own detail + evidence. Read-only — none
//! of the primitives this screen calls touch a running daemon.
//!
//! Mirrors `status.rs`/`repositories.rs`'s own discipline: [`compute_memory_data`] does all the
//! I/O, [`render_memory`] does none, and the pure navigation transitions on [`MemoryNav`] touch
//! neither. `compute_entry_list`/`compute_candidate_list` transplant `local_rag::cli::memory::
//! run_list`'s two paths verbatim (that function is private to the `local-rag` binary target) —
//! including their pagination asymmetry: entries union every applicable scope in Rust, sort, then
//! `skip`/`take`; candidates pass `limit+1`/`offset` straight to SQL and truncate the extra row.
//!
//! # `EntryDetail` restores the exact prior list state, unlike `RepositoriesNav`
//!
//! `RepositoriesNav::ascend` only ever discards a single `selected: usize` on the way back up —
//! one keypress to redo. Here, ascending out of `EntryDetail` back to the list would otherwise
//! discard a multi-key filter/pagination setup (mode + four filters + offset) on every "peek at a
//! record's evidence and go back" — a materially worse regression than Repositories' own
//! single-index loss. `EntryDetail` therefore carries and restores the whole prior [`ListNav`]
//! verbatim.
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
//! # Keyboard scheme (first screen in this crate with filters/pagination)
//!
//! `Up`/`Down`/`Enter`/`Backspace` are reused with the same physical meaning Repositories already
//! established (move/descend/ascend) — safe because `main.rs` dispatches per-screen. New: `Tab`
//! toggles Entries ⇄ Candidates; `k`/`K`, `s`/`S`, `o`/`O` cycle the kind/state/scope filters
//! forward/backward (uppercase matched by literal char, the standard crossterm idiom for Shift+
//! letter, not a modifier check); `PageDown`/`PageUp` page the list. Deliberately reserves
//! `a`/`r`/`e`/`x`/`m` (approve/reject/edit/retract/merge mnemonics) for T18-05, which adds
//! mutation actions to this same screen next.

use std::path::Path;

use crossterm::event::{Event, KeyCode, KeyEventKind};
use local_rag::daemon::gitroot;
use local_rag_core::paths::StoreLayout;
use local_rag_memory::recall::scopes_for;
use local_rag_store::rusqlite::Connection;
use local_rag_store::{
    CandidateRow, CandidateState, MemoryEntryRow, MemoryKind, MemoryState, RequestRoot, ScopeKind,
    list_candidates, list_memory_entries_for_scope, memory_entry_by_id, memory_evidence_for,
    resolve,
};

use crate::store_read::open_read_offline_safe;

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

fn step(selected: usize, down: bool, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    if down {
        (selected + 1).min(len - 1)
    } else {
        selected.saturating_sub(1)
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryNav {
    List(ListNav),
    EntryDetail { memory_id: String, list: ListNav },
}

impl Default for MemoryNav {
    fn default() -> Self {
        MemoryNav::List(ListNav::default())
    }
}

impl MemoryNav {
    /// `Up`/`Down` — no-op at `EntryDetail` (nothing to move within).
    fn moved(&self, data: &MemoryScreenData, down: bool) -> Self {
        match self {
            MemoryNav::List(list) => {
                let len = match data {
                    MemoryScreenData::EntryList { rows, .. } => rows.len(),
                    MemoryScreenData::CandidateList { rows, .. } => rows.len(),
                    MemoryScreenData::Unavailable { .. } | MemoryScreenData::EntryDetail { .. } => {
                        return self.clone();
                    }
                };
                MemoryNav::List(ListNav {
                    selected: step(list.selected, down, len),
                    ..list.clone()
                })
            }
            MemoryNav::EntryDetail { .. } => self.clone(),
        }
    }

    /// `Enter` — descends into `EntryDetail` using the selected row's `memory_id`. A no-op in
    /// `Candidates` mode (the card names evidence/detail for "the selected **entry**" only), on an
    /// empty page, or already at `EntryDetail`.
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
            MemoryNav::List(_) => self.clone(),
        }
    }

    /// `Tab` — toggles `mode`, resetting `offset`/`selected` (the row-set changes completely) but
    /// preserving both modes' own filters. No-op at `EntryDetail`.
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
            MemoryNav::EntryDetail { .. } => self.clone(),
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
            MemoryNav::EntryDetail { .. } => self.clone(),
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
                    MemoryScreenData::Unavailable { .. } | MemoryScreenData::EntryDetail { .. } => {
                        return self.clone();
                    }
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
            MemoryNav::EntryDetail { .. } => self.clone(),
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
}

/// Transplants `cli/memory.rs::run_list`'s entries path verbatim: resolve worktree identity,
/// union every applicable scope (narrowed by `scope_filter`), sort by `(created_at, memory_id)`,
/// then paginate in Rust — `list_memory_entries_for_scope` itself has no `LIMIT`/`OFFSET` (it
/// cannot: pagination has to slice the union of scopes, not any one scope's own rows).
fn compute_entry_list(conn: &Connection, cwd: &Path, list: &ListNav) -> MemoryScreenData {
    let facts = gitroot::probe(cwd);
    let resolution = match resolve(
        conn,
        &RequestRoot {
            worktree_root: facts,
            repo_hint: None,
        },
    ) {
        Ok(r) => r,
        Err(e) => {
            return MemoryScreenData::Unavailable {
                reason: format!("could not resolve worktree identity: {e}"),
            };
        }
    };
    let (scope_label, scopes) = scopes_for(&resolution);
    let scopes: Vec<(ScopeKind, String)> = match list.scope_filter {
        Some(wanted) => scopes.into_iter().filter(|(k, _)| *k == wanted).collect(),
        None => scopes,
    };

    let mut combined: Vec<MemoryEntryRow> = Vec::new();
    for (kind, owner) in &scopes {
        match list_memory_entries_for_scope(
            conn,
            *kind,
            owner,
            list.kind_filter,
            list.entry_state_filter,
        ) {
            Ok(rows) => combined.extend(rows),
            Err(e) => {
                return MemoryScreenData::Unavailable {
                    reason: format!("could not list memory entries: {e}"),
                };
            }
        }
    }
    combined.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.memory_id.cmp(&b.memory_id))
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

/// Compose everything — what `run_app` (and every test) actually calls.
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
    }
}

/// The only keys this screen's own handler recognizes; `run_app` checks global keys (quit, digit
/// screen-switch) first and never delegates them here. Pure: no I/O, no render.
pub fn handle_memory_key(nav: &MemoryNav, data: &MemoryScreenData, ev: Event) -> MemoryNav {
    let Event::Key(key) = ev else {
        return nav.clone();
    };
    if key.kind != KeyEventKind::Press {
        return nav.clone();
    }
    match key.code {
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
                            "{}  {}/{}  scope={}:{}  {}",
                            r.memory_id,
                            r.kind.as_str(),
                            r.state.as_str(),
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
                Layout::vertical([Constraint::Length(8), Constraint::Min(0)]).areas(frame.area());

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
    }

    #[test]
    fn renders_entry_detail_with_no_evidence() {
        let data = MemoryScreenData::EntryDetail {
            entry: entry_row("mem-1", MemoryKind::Fact, MemoryState::Active),
            evidence_ids: vec![],
        };
        assert!(rendered_text(&data).contains("(no evidence)"));
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

        let down = Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Down,
            crossterm::event::KeyModifiers::NONE,
        ));
        assert_eq!(
            handle_memory_key(&nav, &data, down),
            MemoryNav::List(ListNav {
                selected: 1,
                ..ListNav::default()
            })
        );

        let mut release =
            crossterm::event::KeyEvent::new(KeyCode::Down, crossterm::event::KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        assert_eq!(handle_memory_key(&nav, &data, Event::Key(release)), nav);
    }
}
