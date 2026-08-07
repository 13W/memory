//! The Repositories screen (spec 11 §7, T18-03): browse every registered repository, drill down
//! into a repository's worktrees, then into one worktree's own detail (identity/mode + full path
//! history). Read-only — none of the primitives this screen calls touch a running daemon.
//!
//! Mirrors `status.rs`'s own discipline: [`compute_repositories_data`] does all the I/O (dispatched
//! per drill-down level, so an invisible level never spends a query), [`render_repositories`] does
//! none, and the pure navigation transitions ([`RepositoriesNav::descend`]/`ascend`/`moved`) touch
//! neither — hand-buildable and unit-testable with zero SQLite/rendering involved.
//!
//! # `path_history` vs `worktree_path_history`
//!
//! The T18-03 card names `path_history` for the worktree drill-down; that function
//! (`local_rag_store::path_history`) is actually repository-scoped (`Vec<PathObservation>`). The
//! worktree-scoped equivalent — what this drill-down level actually needs — is
//! `worktree_path_history` (`Vec<WorktreePathObservation>`, a different type: adds
//! `display_path`/`path_fingerprint`). Neither `cli/repo.rs` nor `cli/worktree.rs` calls either
//! function (both are flat `list`-only); this module is the first caller of a path-history
//! primitive. Corrected here and in the card text itself (`groups/18-tui-dashboard.md`) — a
//! planning-card function-name slip caught during implementation, not a normative behavior
//! mismatch, so no `D-NNN`.
//!
//! # Why durable reads never silently apply a pending migration
//!
//! Duplicated `status.rs`'s own `StateDb::diagnose_versions`-before-`StateDb::open` dance at first
//! (this screen is equally read-only) rather than sharing it, to keep this card from touching
//! T18-02's already-shipped, already-tested code a second time — with a note to revisit extraction
//! once a third screen needed the identical precaution. T18-04's Memory screen was that third
//! occurrence: the dance now lives in [`crate::store_read`], and this module calls it instead.

use crossterm::event::{Event, KeyCode, KeyEventKind};
use local_rag_core::paths::StoreLayout;
use local_rag_store::rusqlite::Connection;
use local_rag_store::{
    WorktreePathObservation, WorktreeSummary, all_repository_ids, current_path,
    current_worktree_path, worktree_path_history, worktree_summary, worktrees_of_repo,
};

use crate::store_read::open_read_offline_safe;

/// Which drill-down level is active, and enough identity to recompute that level's data next
/// frame. `selected` is the single source of truth for list position — never derived from
/// `ratatui::widgets::ListState` (see [`render_repositories`]'s own note on why).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepositoriesNav {
    Repos {
        selected: usize,
    },
    Worktrees {
        repo_id: String,
        selected: usize,
    },
    WorktreeDetail {
        repo_id: String,
        worktree_id: String,
    },
}

impl Default for RepositoriesNav {
    fn default() -> Self {
        RepositoriesNav::Repos { selected: 0 }
    }
}

impl RepositoriesNav {
    /// `Up`/`Down` (`down = false`/`true`) — clamps at both ends against the row count `data`
    /// actually has for the level `self` is on; a level/data mismatch (screen just switched away
    /// and back mid-transition) or an out-of-list level (`WorktreeDetail` has no list) is a no-op.
    fn moved(&self, data: &RepositoriesScreenData, down: bool) -> Self {
        match (self, data) {
            (RepositoriesNav::Repos { selected }, RepositoriesScreenData::Repos { rows, .. }) => {
                RepositoriesNav::Repos {
                    selected: step(*selected, down, rows.len()),
                }
            }
            (
                RepositoriesNav::Worktrees { repo_id, selected },
                RepositoriesScreenData::Worktrees { worktrees, .. },
            ) => RepositoriesNav::Worktrees {
                repo_id: repo_id.clone(),
                selected: step(*selected, down, worktrees.len()),
            },
            _ => self.clone(),
        }
    }

    /// `Enter` — descends one level using the row at `selected` in `data`; a no-op on an empty
    /// list, at the bottom (`WorktreeDetail`), or on a level/data mismatch.
    fn descend(&self, data: &RepositoriesScreenData) -> Self {
        match (self, data) {
            (RepositoriesNav::Repos { selected }, RepositoriesScreenData::Repos { rows, .. }) => {
                match rows.get(*selected) {
                    Some(row) => RepositoriesNav::Worktrees {
                        repo_id: row.repo_id.clone(),
                        selected: 0,
                    },
                    None => self.clone(),
                }
            }
            (
                RepositoriesNav::Worktrees { repo_id, selected },
                RepositoriesScreenData::Worktrees { worktrees, .. },
            ) => match worktrees.get(*selected) {
                Some(w) => RepositoriesNav::WorktreeDetail {
                    repo_id: repo_id.clone(),
                    worktree_id: w.worktree_id.clone(),
                },
                None => self.clone(),
            },
            _ => self.clone(),
        }
    }

    /// `Backspace` — ascends one level, resetting the level-above's `selected` to `0` (no
    /// breadcrumb stack restoring the prior position — outside this card's own "drill-down
    /// navigation" scope, cheap to add later). A no-op already at `Repos`.
    fn ascend(&self) -> Self {
        match self {
            RepositoriesNav::Repos { .. } => self.clone(),
            RepositoriesNav::Worktrees { .. } => RepositoriesNav::Repos { selected: 0 },
            RepositoriesNav::WorktreeDetail { repo_id, .. } => RepositoriesNav::Worktrees {
                repo_id: repo_id.clone(),
                selected: 0,
            },
        }
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

/// One row of the `Repos` level — the repo-level trio the card names, composed exactly like
/// `cli/repo.rs::run_list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoRow {
    pub repo_id: String,
    pub current_path: Option<String>,
    pub worktree_count: usize,
}

/// Data for whichever level [`RepositoriesNav`] currently names. `Unavailable` covers both "store
/// not safely readable" (see module doc) and "the drilled-into row vanished between frames"
/// (`worktree_summary` returned `None`) — never a panic.
#[derive(Debug, Clone, PartialEq)]
pub enum RepositoriesScreenData {
    Unavailable {
        reason: String,
    },
    Repos {
        rows: Vec<RepoRow>,
        selected: usize,
    },
    Worktrees {
        repo_id: String,
        worktrees: Vec<WorktreeSummary>,
        selected: usize,
    },
    WorktreeDetail {
        summary: WorktreeSummary,
        current_path: Option<String>,
        history: Vec<WorktreePathObservation>,
    },
}

fn compute_repos_level(conn: &Connection, selected: usize) -> RepositoriesScreenData {
    let ids = match all_repository_ids(conn) {
        Ok(ids) => ids,
        Err(e) => {
            return RepositoriesScreenData::Unavailable {
                reason: format!("could not list repositories: {e}"),
            };
        }
    };
    let mut rows = Vec::with_capacity(ids.len());
    for repo_id in ids {
        let path = match current_path(conn, &repo_id) {
            Ok(p) => p,
            Err(e) => {
                return RepositoriesScreenData::Unavailable {
                    reason: format!("could not read {repo_id}'s current path: {e}"),
                };
            }
        };
        let worktree_count = match worktrees_of_repo(conn, &repo_id) {
            Ok(w) => w.len(),
            Err(e) => {
                return RepositoriesScreenData::Unavailable {
                    reason: format!("could not list {repo_id}'s worktrees: {e}"),
                };
            }
        };
        rows.push(RepoRow {
            repo_id,
            current_path: path,
            worktree_count,
        });
    }
    let selected = if rows.is_empty() {
        0
    } else {
        selected.min(rows.len() - 1)
    };
    RepositoriesScreenData::Repos { rows, selected }
}

fn compute_worktrees_level(
    conn: &Connection,
    repo_id: &str,
    selected: usize,
) -> RepositoriesScreenData {
    let worktrees = match worktrees_of_repo(conn, repo_id) {
        Ok(w) => w,
        Err(e) => {
            return RepositoriesScreenData::Unavailable {
                reason: format!("could not list {repo_id}'s worktrees: {e}"),
            };
        }
    };
    let selected = if worktrees.is_empty() {
        0
    } else {
        selected.min(worktrees.len() - 1)
    };
    RepositoriesScreenData::Worktrees {
        repo_id: repo_id.to_string(),
        worktrees,
        selected,
    }
}

fn compute_worktree_detail_level(conn: &Connection, worktree_id: &str) -> RepositoriesScreenData {
    let summary = match worktree_summary(conn, worktree_id) {
        Ok(Some(s)) => s,
        Ok(None) => {
            return RepositoriesScreenData::Unavailable {
                reason: format!("worktree {worktree_id} no longer exists"),
            };
        }
        Err(e) => {
            return RepositoriesScreenData::Unavailable {
                reason: format!("could not read worktree {worktree_id}: {e}"),
            };
        }
    };
    let current_path = match current_worktree_path(conn, worktree_id) {
        Ok(p) => p,
        Err(e) => {
            return RepositoriesScreenData::Unavailable {
                reason: format!("could not read {worktree_id}'s current path: {e}"),
            };
        }
    };
    let history = match worktree_path_history(conn, worktree_id) {
        Ok(h) => h,
        Err(e) => {
            return RepositoriesScreenData::Unavailable {
                reason: format!("could not read {worktree_id}'s path history: {e}"),
            };
        }
    };
    RepositoriesScreenData::WorktreeDetail {
        summary,
        current_path,
        history,
    }
}

/// Compose everything — what `run_app` (and every test) actually calls. Dispatches to exactly one
/// `compute_*_level` for the level `nav` names — an invisible level is never queried.
pub fn compute_repositories_data(
    layout: &StoreLayout,
    nav: &RepositoriesNav,
) -> RepositoriesScreenData {
    let conn = match open_read_offline_safe(layout) {
        Ok(c) => c,
        Err(reason) => return RepositoriesScreenData::Unavailable { reason },
    };
    match nav {
        RepositoriesNav::Repos { selected } => compute_repos_level(&conn, *selected),
        RepositoriesNav::Worktrees { repo_id, selected } => {
            compute_worktrees_level(&conn, repo_id, *selected)
        }
        RepositoriesNav::WorktreeDetail { worktree_id, .. } => {
            compute_worktree_detail_level(&conn, worktree_id)
        }
    }
}

/// `Up`/`Down`/`Enter`/`Backspace` — the only keys this screen's own handler recognizes; `run_app`
/// checks [`crate`]-level global keys (quit, digit screen-switch) first and never delegates them
/// here. Pure: no I/O, no render.
pub fn handle_repositories_key(
    nav: &RepositoriesNav,
    data: &RepositoriesScreenData,
    ev: Event,
) -> RepositoriesNav {
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
        _ => nav.clone(),
    }
}

/// Pure render — no I/O, `TestBackend`-testable without a daemon or a store. First use of
/// `ratatui::widgets::List`/`ListState` in this workspace (`rg -n "ListState|widgets::List\b"`
/// found zero prior hits). `ListState` here is a one-shot rendering vessel built fresh from
/// `RepositoriesNav`'s own `selected` every frame — never a second, independently-mutated copy of
/// persisted state (`ListState::select_next`/`select_previous` only clamp at render time, which
/// would make an exact, render-independent unit test of a transition impossible — see
/// `RepositoriesNav::moved`'s own doc).
pub fn render_repositories(frame: &mut ratatui::Frame, data: &RepositoriesScreenData) {
    use ratatui::layout::{Constraint, Layout};
    use ratatui::text::Line;
    use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph, Row, Table};

    match data {
        RepositoriesScreenData::Unavailable { reason } => {
            frame.render_widget(
                Paragraph::new(reason.as_str()).block(Block::bordered().title("Repositories")),
                frame.area(),
            );
        }
        RepositoriesScreenData::Repos { rows, selected } => {
            let items: Vec<ListItem> = if rows.is_empty() {
                vec![ListItem::new("no repositories registered yet")]
            } else {
                rows.iter()
                    .map(|r| {
                        ListItem::new(format!(
                            "{}  {}  ({} worktree(s))",
                            r.repo_id,
                            r.current_path.as_deref().unwrap_or("(no current path)"),
                            r.worktree_count,
                        ))
                    })
                    .collect()
            };
            let list = List::new(items)
                .block(Block::bordered().title("Repositories"))
                .highlight_symbol("> ");
            let mut state =
                ListState::default().with_selected((!rows.is_empty()).then_some(*selected));
            frame.render_stateful_widget(list, frame.area(), &mut state);
        }
        RepositoriesScreenData::Worktrees {
            repo_id,
            worktrees,
            selected,
        } => {
            let items: Vec<ListItem> = if worktrees.is_empty() {
                vec![ListItem::new("no worktrees registered yet")]
            } else {
                worktrees
                    .iter()
                    .map(|w| {
                        ListItem::new(format!(
                            "{}  {}  {}",
                            w.worktree_id,
                            w.kind.as_str(),
                            w.state.as_str(),
                        ))
                    })
                    .collect()
            };
            let list = List::new(items)
                .block(Block::bordered().title(format!("Worktrees of {repo_id}")))
                .highlight_symbol("> ");
            let mut state =
                ListState::default().with_selected((!worktrees.is_empty()).then_some(*selected));
            frame.render_stateful_widget(list, frame.area(), &mut state);
        }
        RepositoriesScreenData::WorktreeDetail {
            summary,
            current_path,
            history,
        } => {
            let [detail_area, history_area] =
                Layout::vertical([Constraint::Length(6), Constraint::Min(0)]).areas(frame.area());
            let lines = vec![
                Line::from(format!("worktree_id: {}", summary.worktree_id)),
                Line::from(format!("repo_id: {}", summary.repo_id)),
                Line::from(format!("kind: {}", summary.kind.as_str())),
                Line::from(format!("state: {}", summary.state.as_str())),
                Line::from(format!(
                    "current_path: {}",
                    current_path.as_deref().unwrap_or("(none)")
                )),
            ];
            frame.render_widget(
                Paragraph::new(lines).block(Block::bordered().title("Worktree detail")),
                detail_area,
            );

            let mut rows = Vec::new();
            if history.is_empty() {
                rows.push(Row::new(["(no path history)".to_string(), String::new()]));
            }
            for h in history {
                rows.push(Row::new([
                    h.display_path.clone(),
                    if h.is_current {
                        "current".to_string()
                    } else {
                        "past".to_string()
                    },
                ]));
            }
            let table = Table::new(rows, [Constraint::Min(0), Constraint::Length(10)])
                .block(Block::bordered().title("Path history"));
            frame.render_widget(table, history_area);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn rendered_text(data: &RepositoriesScreenData) -> String {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test backend terminal");
        terminal
            .draw(|frame| render_repositories(frame, data))
            .expect("draw repositories screen");
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    fn repo_row(id: &str) -> RepoRow {
        RepoRow {
            repo_id: id.to_string(),
            current_path: None,
            worktree_count: 0,
        }
    }

    fn worktree_row(id: &str, repo_id: &str) -> WorktreeSummary {
        WorktreeSummary {
            worktree_id: id.to_string(),
            repo_id: repo_id.to_string(),
            kind: local_rag_store::WorktreeKind::Main,
            state: local_rag_store::WorktreeState::Active,
        }
    }

    #[test]
    fn renders_unavailable_reason() {
        let data = RepositoriesScreenData::Unavailable {
            reason: "store not yet initialized".to_string(),
        };
        assert!(rendered_text(&data).contains("not yet initialized"));
    }

    #[test]
    fn renders_empty_repos_list() {
        let data = RepositoriesScreenData::Repos {
            rows: vec![],
            selected: 0,
        };
        assert!(rendered_text(&data).contains("no repositories registered yet"));
    }

    #[test]
    fn renders_populated_repos_list() {
        let data = RepositoriesScreenData::Repos {
            rows: vec![RepoRow {
                repo_id: "repo-a".to_string(),
                current_path: Some("/repos/a".to_string()),
                worktree_count: 2,
            }],
            selected: 0,
        };
        let content = rendered_text(&data);
        assert!(content.contains("repo-a"), "{content}");
        assert!(content.contains("/repos/a"), "{content}");
        assert!(content.contains("2 worktree(s)"), "{content}");
    }

    #[test]
    fn renders_worktrees_including_a_detached_row() {
        let data = RepositoriesScreenData::Worktrees {
            repo_id: "repo-a".to_string(),
            worktrees: vec![
                worktree_row("wt-1", "repo-a"),
                WorktreeSummary {
                    worktree_id: "wt-2".to_string(),
                    repo_id: "repo-a".to_string(),
                    kind: local_rag_store::WorktreeKind::Linked,
                    state: local_rag_store::WorktreeState::Detached,
                },
            ],
            selected: 0,
        };
        let content = rendered_text(&data);
        assert!(content.contains("Worktrees of repo-a"), "{content}");
        assert!(content.contains("wt-2"), "{content}");
        assert!(content.contains("detached"), "{content}");
    }

    #[test]
    fn renders_worktree_detail_with_history() {
        let data = RepositoriesScreenData::WorktreeDetail {
            summary: worktree_row("wt-1", "repo-a"),
            current_path: Some("/repos/a".to_string()),
            history: vec![
                WorktreePathObservation {
                    observed_canonical_path: "/repos/a-old".to_string(),
                    display_path: "/repos/a-old".to_string(),
                    path_fingerprint: "fp-old".to_string(),
                    is_current: false,
                    first_seen_at: 1_000,
                    last_seen_at: 1_500,
                },
                WorktreePathObservation {
                    observed_canonical_path: "/repos/a".to_string(),
                    display_path: "/repos/a".to_string(),
                    path_fingerprint: "fp-new".to_string(),
                    is_current: true,
                    first_seen_at: 2_000,
                    last_seen_at: 2_000,
                },
            ],
        };
        let content = rendered_text(&data);
        assert!(content.contains("wt-1"), "{content}");
        assert!(content.contains("/repos/a-old"), "{content}");
        assert!(content.contains("current"), "{content}");
        assert!(content.contains("past"), "{content}");
    }

    #[test]
    fn down_and_up_clamp_at_both_ends() {
        let data = RepositoriesScreenData::Repos {
            rows: vec![repo_row("a"), repo_row("b")],
            selected: 0,
        };
        let nav = RepositoriesNav::Repos { selected: 0 };
        let nav = nav.moved(&data, true);
        assert_eq!(nav, RepositoriesNav::Repos { selected: 1 });
        let nav = nav.moved(&data, true);
        assert_eq!(nav, RepositoriesNav::Repos { selected: 1 });
        let nav = nav.moved(&data, false);
        assert_eq!(nav, RepositoriesNav::Repos { selected: 0 });
        let nav = nav.moved(&data, false);
        assert_eq!(nav, RepositoriesNav::Repos { selected: 0 });
    }

    #[test]
    fn enter_descends_repos_to_worktrees_to_detail() {
        let repos_data = RepositoriesScreenData::Repos {
            rows: vec![repo_row("repo-a")],
            selected: 0,
        };
        let nav = RepositoriesNav::Repos { selected: 0 }.descend(&repos_data);
        assert_eq!(
            nav,
            RepositoriesNav::Worktrees {
                repo_id: "repo-a".to_string(),
                selected: 0,
            }
        );

        let worktrees_data = RepositoriesScreenData::Worktrees {
            repo_id: "repo-a".to_string(),
            worktrees: vec![worktree_row("wt-1", "repo-a")],
            selected: 0,
        };
        let nav = nav.descend(&worktrees_data);
        assert_eq!(
            nav,
            RepositoriesNav::WorktreeDetail {
                repo_id: "repo-a".to_string(),
                worktree_id: "wt-1".to_string(),
            }
        );
    }

    #[test]
    fn enter_on_an_empty_list_or_at_worktree_detail_is_a_no_op() {
        let empty = RepositoriesScreenData::Repos {
            rows: vec![],
            selected: 0,
        };
        let nav = RepositoriesNav::Repos { selected: 0 };
        assert_eq!(nav.descend(&empty), nav);

        let detail_nav = RepositoriesNav::WorktreeDetail {
            repo_id: "r".to_string(),
            worktree_id: "w".to_string(),
        };
        let detail_data = RepositoriesScreenData::WorktreeDetail {
            summary: worktree_row("w", "r"),
            current_path: None,
            history: vec![],
        };
        assert_eq!(detail_nav.descend(&detail_data), detail_nav);
    }

    #[test]
    fn backspace_ascends_each_level_resetting_selection() {
        let wt_nav = RepositoriesNav::Worktrees {
            repo_id: "r".to_string(),
            selected: 3,
        };
        assert_eq!(wt_nav.ascend(), RepositoriesNav::Repos { selected: 0 });

        let detail_nav = RepositoriesNav::WorktreeDetail {
            repo_id: "r".to_string(),
            worktree_id: "w".to_string(),
        };
        assert_eq!(
            detail_nav.ascend(),
            RepositoriesNav::Worktrees {
                repo_id: "r".to_string(),
                selected: 0,
            }
        );

        let repos_nav = RepositoriesNav::Repos { selected: 0 };
        assert_eq!(repos_nav.ascend(), repos_nav);
    }

    #[test]
    fn handle_repositories_key_dispatches_and_ignores_key_release() {
        let data = RepositoriesScreenData::Repos {
            rows: vec![repo_row("a"), repo_row("b")],
            selected: 0,
        };
        let nav = RepositoriesNav::Repos { selected: 0 };

        let down = crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Down,
            crossterm::event::KeyModifiers::NONE,
        ));
        assert_eq!(
            handle_repositories_key(&nav, &data, down),
            RepositoriesNav::Repos { selected: 1 }
        );

        let mut release =
            crossterm::event::KeyEvent::new(KeyCode::Down, crossterm::event::KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        assert_eq!(
            handle_repositories_key(&nav, &data, Event::Key(release)),
            nav
        );
    }
}
