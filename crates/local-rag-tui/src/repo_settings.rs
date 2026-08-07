//! The Repo Settings screen (spec 02 §3.2, 11 §7, T18-06): a `data_policy` form (4 fixed values,
//! "most restrictive wins") plus a generic `(key, value)` list, over
//! `crates/store/src/registry/settings.rs` — that backend shipped fully at T02-05 with **no**
//! existing caller anywhere in the workspace (no CLI command, no MCP tool); this screen is the
//! first production caller.
//!
//! # Repository selection is a picker list, not cwd-resolve
//!
//! Unlike Status/Memory (which silently resolve "the current repository" via
//! `gitroot::probe(cwd)`+`resolve()`), this screen opens on a flat list of every registered
//! repository (`all_repository_ids`+`current_path`, the same tandem `repositories.rs::
//! compute_repos_level` already uses) — a *settings* screen should let the user configure any
//! registered repository, not only the one the dashboard happens to be launched from, and
//! cwd-resolve's own `GlobalOnly`/`Ambiguous` branches have no good answer for a settings form
//! anyway. `Enter` descends into the selected repository's own settings.
//!
//! # No delete — the backend has none
//!
//! `set_repo_setting` is upsert-only; there is no `delete_repo_setting` anywhere in
//! `local_rag_store`. This screen never offers to remove a key, matching the backend's own
//! capability exactly — `e`/`E` edits an existing `(key, value)` pair in place (still an upsert
//! under the hood) and `n`/`N` adds a new one; both funnel through the same [`SettingForm`].
//!
//! # No confirm-modal — there is nothing to gate against
//!
//! T18-05's `retract_memory` confirm-modal is gated by the real MCP catalog's own
//! `annotations.destructiveHint`. `repo_settings`'s primitives have no MCP tool entry at all — no
//! catalog record exists to consult — so every mutation here (`p`/`P` cycling `data_policy`,
//! `Enter` submitting [`SettingForm`]) applies immediately on keypress, the same "no confirm
//! needed" shape T18-05's own non-destructive actions (`approve`/`reject`/`edit`/`merge`) already
//! have. Errors surface inline on [`RepoSettingsNav::RepoDetail`]/[`RepoSettingsNav::SettingForm`]'s
//! own `error` field (the lighter idiom T18-05's `MergeSelect` established), not a separate
//! dismissible banner — every mutation here returns to the exact screen it was triggered from, so
//! a second dismiss step would add friction without adding clarity.
//!
//! # `data_policy` cycling has no "unset" position
//!
//! `memory.rs`'s `cycle_option` legitimately cycles a filter through `None` ("off") because a
//! *read* filter can always be turned off. A `data_policy` *write* cannot express "unset" — there
//! is no delete — so [`cycle_data_policy`] only ever produces one of the 4 concrete values: from
//! an unset repository (`repo_data_policy` returned `None`), the first `p` writes
//! [`DataPolicy::LocalOnly`] (forward) or [`DataPolicy::AllowRemoteFull`] (backward) — `None` sits
//! at the wrap boundary between the two ends, the same mental model `cycle_option` uses, just
//! never reachable as an output. Not implemented by reusing `cycle_option` itself: that helper's
//! `None`-can-be-an-output semantics do not fit here, so this is a second, deliberately distinct
//! small function, not a shared one — genuine reuse would need a third occurrence of the *same*
//! semantics, the threshold `store_read.rs`'s own extraction already set at T18-04.
//!
//! `step` (list clamping) and `is_ctrl_x` (the T18-05 `SettingForm`/`EditForm`-style cancel
//! predicate) are third and second small-helper copies respectively (`repositories.rs`/`memory.rs`
//! already each have their own `step`; `memory.rs` already has its own `is_ctrl_x`) — deliberately
//! still not extracted, deferred by the same "wait for a genuine third occurrence of *identical*
//! code" convention, noted here for whoever hits the next occurrence.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use local_rag_core::DataPolicy;
use local_rag_core::paths::StoreLayout;
use local_rag_store::rusqlite::Connection;
use local_rag_store::{
    DATA_POLICY_KEY, all_repository_ids, current_path, repo_data_policy, repo_settings,
    set_repo_data_policy, set_repo_setting,
};

use crate::store_read::open_read_offline_safe;
use crate::store_write::open_write_offline_safe;

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

fn is_ctrl_x(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('x') | KeyCode::Char('X'))
        && key.modifiers.contains(KeyModifiers::CONTROL)
}

const DATA_POLICIES: [DataPolicy; 4] = [
    DataPolicy::LocalOnly,
    DataPolicy::MetadataOnlyRemote,
    DataPolicy::AllowRemoteWithRedaction,
    DataPolicy::AllowRemoteFull,
];

/// See the module doc's own section on why `None` is a wrap boundary, never an output.
fn cycle_data_policy(current: Option<DataPolicy>, forward: bool) -> DataPolicy {
    let len = DATA_POLICIES.len();
    match current {
        None => {
            if forward {
                DATA_POLICIES[0]
            } else {
                DATA_POLICIES[len - 1]
            }
        }
        Some(c) => {
            let idx = DATA_POLICIES.iter().position(|d| *d == c).unwrap_or(0);
            let next_idx = if forward {
                (idx + 1) % len
            } else {
                (idx + len - 1) % len
            };
            DATA_POLICIES[next_idx]
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingField {
    Key,
    Value,
}

/// One row of the repo picker — the same trio `repositories.rs::RepoRow` uses, minus
/// `worktree_count` (irrelevant to a settings screen).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoRow {
    pub repo_id: String,
    pub current_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoSettingsNav {
    RepoList {
        selected: usize,
    },
    RepoDetail {
        repo_id: String,
        selected: usize,
        error: Option<String>,
    },
    SettingForm {
        repo_id: String,
        key: String,
        value: String,
        field: SettingField,
        error: Option<String>,
        list_selected: usize,
    },
}

impl Default for RepoSettingsNav {
    fn default() -> Self {
        RepoSettingsNav::RepoList { selected: 0 }
    }
}

impl RepoSettingsNav {
    /// `Up`/`Down` — no-op outside `RepoList`/`RepoDetail` (nothing to move within `SettingForm`).
    fn moved(&self, data: &RepoSettingsScreenData, down: bool) -> Self {
        match self {
            RepoSettingsNav::RepoList { selected } => {
                let len = match data {
                    RepoSettingsScreenData::RepoList { rows, .. } => rows.len(),
                    _ => return self.clone(),
                };
                RepoSettingsNav::RepoList {
                    selected: step(*selected, down, len),
                }
            }
            RepoSettingsNav::RepoDetail {
                repo_id,
                selected,
                error,
            } => {
                let len = match data {
                    RepoSettingsScreenData::RepoDetail { settings, .. } => settings.len(),
                    _ => return self.clone(),
                };
                RepoSettingsNav::RepoDetail {
                    repo_id: repo_id.clone(),
                    selected: step(*selected, down, len),
                    error: error.clone(),
                }
            }
            RepoSettingsNav::SettingForm { .. } => self.clone(),
        }
    }

    /// `Enter` at `RepoList` — descends into `RepoDetail` for the selected repository. No-op
    /// elsewhere or on an empty list.
    fn descend(&self, data: &RepoSettingsScreenData) -> Self {
        match (self, data) {
            (
                RepoSettingsNav::RepoList { selected },
                RepoSettingsScreenData::RepoList { rows, .. },
            ) => match rows.get(*selected) {
                Some(row) => RepoSettingsNav::RepoDetail {
                    repo_id: row.repo_id.clone(),
                    selected: 0,
                    error: None,
                },
                None => self.clone(),
            },
            _ => self.clone(),
        }
    }

    /// `Backspace` at `RepoDetail` — ascends to `RepoList`, resetting `selected` (the same simpler
    /// "no breadcrumb restore" precedent `RepositoriesNav::ascend` already established — this list
    /// has no filters/pagination to lose, so the cost of resetting is one extra keypress, not a
    /// meaningful regression). No-op at `RepoList`; `SettingForm`'s own cancel is handled directly
    /// in [`handle_setting_form_key`], not here (its `Backspace` deletes a character instead).
    fn ascend(&self) -> Self {
        match self {
            RepoSettingsNav::RepoDetail { .. } => RepoSettingsNav::RepoList { selected: 0 },
            _ => self.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoSettingsScreenData {
    Unavailable {
        reason: String,
    },
    RepoList {
        rows: Vec<RepoRow>,
        selected: usize,
    },
    RepoDetail {
        repo_id: String,
        data_policy: Option<DataPolicy>,
        /// `repo_settings(conn, repo_id)` minus the `data_policy` key — shown separately above.
        settings: Vec<(String, String)>,
        selected: usize,
        error: Option<String>,
    },
    SettingForm {
        repo_id: String,
        key: String,
        value: String,
        field: SettingField,
        error: Option<String>,
    },
}

fn compute_repo_list(conn: &Connection, selected: usize) -> RepoSettingsScreenData {
    let ids = match all_repository_ids(conn) {
        Ok(ids) => ids,
        Err(e) => {
            return RepoSettingsScreenData::Unavailable {
                reason: format!("could not list repositories: {e}"),
            };
        }
    };
    let mut rows = Vec::with_capacity(ids.len());
    for repo_id in ids {
        let path = match current_path(conn, &repo_id) {
            Ok(p) => p,
            Err(e) => {
                return RepoSettingsScreenData::Unavailable {
                    reason: format!("could not read {repo_id}'s current path: {e}"),
                };
            }
        };
        rows.push(RepoRow {
            repo_id,
            current_path: path,
        });
    }
    let selected = if rows.is_empty() {
        0
    } else {
        selected.min(rows.len() - 1)
    };
    RepoSettingsScreenData::RepoList { rows, selected }
}

/// Re-fetches `data_policy`/settings fresh by `repo_id` every frame (WYSIWYG, the same idiom every
/// other screen in this dashboard already follows) — never JOINs against `repository`, so a
/// `repo_id` that has since vanished shows an empty result here rather than an error; the
/// corresponding write later surfaces that as a plain FK `ConstraintViolation`, not a panic.
fn compute_repo_detail(
    conn: &Connection,
    repo_id: &str,
    selected: usize,
    error: Option<String>,
) -> RepoSettingsScreenData {
    let data_policy = match repo_data_policy(conn, repo_id) {
        Ok(p) => p,
        Err(e) => {
            return RepoSettingsScreenData::Unavailable {
                reason: format!("could not read {repo_id}'s data_policy: {e}"),
            };
        }
    };
    let all_settings = match repo_settings(conn, repo_id) {
        Ok(s) => s,
        Err(e) => {
            return RepoSettingsScreenData::Unavailable {
                reason: format!("could not read {repo_id}'s settings: {e}"),
            };
        }
    };
    let settings: Vec<(String, String)> = all_settings
        .into_iter()
        .filter(|(k, _)| k != DATA_POLICY_KEY)
        .collect();
    let selected = if settings.is_empty() {
        0
    } else {
        selected.min(settings.len() - 1)
    };
    RepoSettingsScreenData::RepoDetail {
        repo_id: repo_id.to_string(),
        data_policy,
        settings,
        selected,
        error,
    }
}

/// Compose everything — what `run_app` (and every test) actually calls.
pub fn compute_repo_settings_data(
    layout: &StoreLayout,
    nav: &RepoSettingsNav,
) -> RepoSettingsScreenData {
    let conn = match open_read_offline_safe(layout) {
        Ok(c) => c,
        Err(reason) => return RepoSettingsScreenData::Unavailable { reason },
    };
    match nav {
        RepoSettingsNav::RepoList { selected } => compute_repo_list(&conn, *selected),
        RepoSettingsNav::RepoDetail {
            repo_id,
            selected,
            error,
        } => compute_repo_detail(&conn, repo_id, *selected, error.clone()),
        RepoSettingsNav::SettingForm {
            repo_id,
            key,
            value,
            field,
            error,
            ..
        } => RepoSettingsScreenData::SettingForm {
            repo_id: repo_id.clone(),
            key: key.clone(),
            value: value.clone(),
            field: *field,
            error: error.clone(),
        },
    }
}

/// A fully-specified mutation, ready to run — mirrors T18-05's `MemoryAction`. Built by
/// `handle_repo_settings_key`'s own key handlers, executed by [`execute_repo_settings_action`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoSettingsAction {
    SetDataPolicy {
        repo_id: String,
        policy: DataPolicy,
        list_selected: usize,
    },
    SetSetting {
        repo_id: String,
        key: String,
        value: String,
        list_selected: usize,
    },
}

/// `handle_repo_settings_key`'s return: either a pure navigation update, or a fully-specified
/// mutation `run_app` must hand to [`execute_repo_settings_action`] — the same split T18-05's
/// `MemoryKeyOutcome` established, for the same reason: keeps the key handler itself free of I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoSettingsKeyOutcome {
    Nav(RepoSettingsNav),
    Execute(RepoSettingsAction),
}

/// `true` only for [`RepoSettingsNav::SettingForm`] — consulted by `main.rs`'s own text-entry
/// carve-out (`is_text_entry_key`), the same mechanism T18-05's `memory::captures_all_keys`
/// established for `EditForm`.
pub fn captures_all_keys(nav: &RepoSettingsNav) -> bool {
    matches!(nav, RepoSettingsNav::SettingForm { .. })
}

fn handle_repo_list_key(
    nav: &RepoSettingsNav,
    data: &RepoSettingsScreenData,
    code: KeyCode,
) -> RepoSettingsKeyOutcome {
    match code {
        KeyCode::Up => RepoSettingsKeyOutcome::Nav(nav.moved(data, false)),
        KeyCode::Down => RepoSettingsKeyOutcome::Nav(nav.moved(data, true)),
        KeyCode::Enter => RepoSettingsKeyOutcome::Nav(nav.descend(data)),
        _ => RepoSettingsKeyOutcome::Nav(nav.clone()),
    }
}

/// `Up`/`Down` move the generic-settings selection; `Backspace` ascends; `p`/`P` cycles
/// `data_policy` and executes immediately; `e`/`E` opens `SettingForm` pre-filled from the selected
/// row; `n`/`N` opens it empty (add a new key). See the module doc for why neither `p` nor `Enter`
/// in `SettingForm` needs a confirm-modal.
fn handle_repo_detail_key(
    nav: &RepoSettingsNav,
    data: &RepoSettingsScreenData,
    code: KeyCode,
) -> RepoSettingsKeyOutcome {
    let RepoSettingsNav::RepoDetail {
        repo_id, selected, ..
    } = nav
    else {
        return RepoSettingsKeyOutcome::Nav(nav.clone());
    };
    match code {
        KeyCode::Up => RepoSettingsKeyOutcome::Nav(nav.moved(data, false)),
        KeyCode::Down => RepoSettingsKeyOutcome::Nav(nav.moved(data, true)),
        KeyCode::Backspace => RepoSettingsKeyOutcome::Nav(nav.ascend()),
        KeyCode::Char('p') | KeyCode::Char('P') => {
            let forward = code == KeyCode::Char('p');
            let current = match data {
                RepoSettingsScreenData::RepoDetail { data_policy, .. } => *data_policy,
                _ => None,
            };
            let policy = cycle_data_policy(current, forward);
            RepoSettingsKeyOutcome::Execute(RepoSettingsAction::SetDataPolicy {
                repo_id: repo_id.clone(),
                policy,
                list_selected: *selected,
            })
        }
        KeyCode::Char('e') | KeyCode::Char('E') => match data {
            RepoSettingsScreenData::RepoDetail { settings, .. } => match settings.get(*selected) {
                Some((key, value)) => RepoSettingsKeyOutcome::Nav(RepoSettingsNav::SettingForm {
                    repo_id: repo_id.clone(),
                    key: key.clone(),
                    value: value.clone(),
                    field: SettingField::Key,
                    error: None,
                    list_selected: *selected,
                }),
                None => RepoSettingsKeyOutcome::Nav(nav.clone()),
            },
            _ => RepoSettingsKeyOutcome::Nav(nav.clone()),
        },
        KeyCode::Char('n') | KeyCode::Char('N') => {
            RepoSettingsKeyOutcome::Nav(RepoSettingsNav::SettingForm {
                repo_id: repo_id.clone(),
                key: String::new(),
                value: String::new(),
                field: SettingField::Key,
                error: None,
                list_selected: *selected,
            })
        }
        _ => RepoSettingsKeyOutcome::Nav(nav.clone()),
    }
}

/// `Tab` switches focus, any unmodified printable char appends to the focused buffer (including
/// `q`/digits — the same global-quit carve-out `EditForm` needed), `Backspace` deletes from it,
/// `Enter` validates (`key` must be non-empty — `PRIMARY KEY(repo_id, key)` allows an empty string,
/// but there is no sane use for one) and submits, `Ctrl+X` cancels back to `RepoDetail`.
fn handle_setting_form_key(nav: &RepoSettingsNav, key_event: KeyEvent) -> RepoSettingsKeyOutcome {
    let RepoSettingsNav::SettingForm {
        repo_id,
        key,
        value,
        field,
        list_selected,
        ..
    } = nav
    else {
        return RepoSettingsKeyOutcome::Nav(nav.clone());
    };

    if is_ctrl_x(&key_event) {
        return RepoSettingsKeyOutcome::Nav(RepoSettingsNav::RepoDetail {
            repo_id: repo_id.clone(),
            selected: *list_selected,
            error: None,
        });
    }

    let with = |key: String, value: String, error: Option<String>| {
        RepoSettingsKeyOutcome::Nav(RepoSettingsNav::SettingForm {
            repo_id: repo_id.clone(),
            key,
            value,
            field: *field,
            error,
            list_selected: *list_selected,
        })
    };

    match key_event.code {
        KeyCode::Tab => {
            let next_field = match field {
                SettingField::Key => SettingField::Value,
                SettingField::Value => SettingField::Key,
            };
            RepoSettingsKeyOutcome::Nav(RepoSettingsNav::SettingForm {
                repo_id: repo_id.clone(),
                key: key.clone(),
                value: value.clone(),
                field: next_field,
                error: None,
                list_selected: *list_selected,
            })
        }
        KeyCode::Char(c) if !key_event.modifiers.contains(KeyModifiers::CONTROL) => {
            let mut key = key.clone();
            let mut value = value.clone();
            match field {
                SettingField::Key => key.push(c),
                SettingField::Value => value.push(c),
            }
            with(key, value, None)
        }
        KeyCode::Backspace => {
            let mut key = key.clone();
            let mut value = value.clone();
            match field {
                SettingField::Key => {
                    key.pop();
                }
                SettingField::Value => {
                    value.pop();
                }
            }
            with(key, value, None)
        }
        KeyCode::Enter => {
            if key.trim().is_empty() {
                with(
                    key.clone(),
                    value.clone(),
                    Some("key must not be empty".to_string()),
                )
            } else {
                RepoSettingsKeyOutcome::Execute(RepoSettingsAction::SetSetting {
                    repo_id: repo_id.clone(),
                    key: key.clone(),
                    value: value.clone(),
                    list_selected: *list_selected,
                })
            }
        }
        _ => RepoSettingsKeyOutcome::Nav(nav.clone()),
    }
}

/// The only keys this screen's own handler recognizes; `run_app` checks global keys first — except
/// while `captures_all_keys(nav)` and the pressed key is bare `q`/a digit, the same carve-out
/// `memory.rs` established. Pure: no I/O, no render.
pub fn handle_repo_settings_key(
    nav: &RepoSettingsNav,
    data: &RepoSettingsScreenData,
    ev: Event,
) -> RepoSettingsKeyOutcome {
    let Event::Key(key) = ev else {
        return RepoSettingsKeyOutcome::Nav(nav.clone());
    };
    if key.kind != KeyEventKind::Press {
        return RepoSettingsKeyOutcome::Nav(nav.clone());
    }
    match nav {
        RepoSettingsNav::RepoList { .. } => handle_repo_list_key(nav, data, key.code),
        RepoSettingsNav::RepoDetail { .. } => handle_repo_detail_key(nav, data, key.code),
        RepoSettingsNav::SettingForm { .. } => handle_setting_form_key(nav, key),
    }
}

/// The only function in this module that touches `.writer()` — mirrors T18-05's
/// `execute_memory_action` shape, minus any `Actor`/`now_ms`/idempotency plumbing: neither
/// `set_repo_setting` nor `set_repo_data_policy` takes any of those (confirmed by their own exact
/// signatures — this primitive has no audit trail, unlike the memory-op engine). Also a flatter
/// result shape than `execute_memory_action`'s own: `apply_edit`/`apply_retract`/etc. each return a
/// *double*-nested `rusqlite::Result<Result<Outcome, MemoryOpError>>` (a real typed domain error
/// distinct from raw SQLite failure), but `set_repo_setting`/`set_repo_data_policy` return a single
/// `rusqlite::Result<()>` — no separate domain-error type exists for this primitive, so
/// `StateWriter::transaction` collapses everything (including an unknown `repo_id`'s FK
/// `ConstraintViolation`) into one `Result<(), WriteError>`.
pub fn execute_repo_settings_action(
    layout: &StoreLayout,
    action: RepoSettingsAction,
) -> RepoSettingsNav {
    let (repo_id, list_selected) = match &action {
        RepoSettingsAction::SetDataPolicy {
            repo_id,
            list_selected,
            ..
        } => (repo_id.clone(), *list_selected),
        RepoSettingsAction::SetSetting {
            repo_id,
            list_selected,
            ..
        } => (repo_id.clone(), *list_selected),
    };
    let state = match open_write_offline_safe(layout) {
        Ok(s) => s,
        Err(reason) => {
            return RepoSettingsNav::RepoDetail {
                repo_id,
                selected: list_selected,
                error: Some(reason),
            };
        }
    };

    let result = match action {
        RepoSettingsAction::SetDataPolicy {
            repo_id, policy, ..
        } => crate::rt::block_on({
            let repo_id = repo_id.clone();
            async move {
                state
                    .writer()
                    .transaction(move |tx| set_repo_data_policy(tx, &repo_id, policy))
                    .await
            }
        }),
        RepoSettingsAction::SetSetting {
            repo_id,
            key,
            value,
            ..
        } => crate::rt::block_on({
            let repo_id = repo_id.clone();
            let key = key.clone();
            let value = value.clone();
            async move {
                state
                    .writer()
                    .transaction(move |tx| set_repo_setting(tx, &repo_id, &key, &value))
                    .await
            }
        }),
    };

    let error = match result {
        Ok(()) => None,
        Err(e) => Some(format!("could not save setting: {e}")),
    };
    RepoSettingsNav::RepoDetail {
        repo_id,
        selected: list_selected,
        error,
    }
}

/// Pure render — no I/O, `TestBackend`-testable without a daemon or a store.
pub fn render_repo_settings(frame: &mut ratatui::Frame, data: &RepoSettingsScreenData) {
    use ratatui::layout::{Constraint, Layout};
    use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph};

    match data {
        RepoSettingsScreenData::Unavailable { reason } => {
            frame.render_widget(
                Paragraph::new(reason.as_str()).block(Block::bordered().title("Repo Settings")),
                frame.area(),
            );
        }
        RepoSettingsScreenData::RepoList { rows, selected } => {
            let items: Vec<ListItem> = if rows.is_empty() {
                vec![ListItem::new("no repositories registered yet")]
            } else {
                rows.iter()
                    .map(|r| {
                        ListItem::new(format!(
                            "{}  {}",
                            r.repo_id,
                            r.current_path.as_deref().unwrap_or("(no current path)"),
                        ))
                    })
                    .collect()
            };
            let list = List::new(items)
                .block(Block::bordered().title("Repo Settings"))
                .highlight_symbol("> ");
            let mut state =
                ListState::default().with_selected((!rows.is_empty()).then_some(*selected));
            frame.render_stateful_widget(list, frame.area(), &mut state);
        }
        RepoSettingsScreenData::RepoDetail {
            repo_id,
            data_policy,
            settings,
            selected,
            error,
        } => {
            let [policy_area, list_area, footer_area] = Layout::vertical([
                Constraint::Length(4),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .areas(frame.area());

            let policy_line = format!(
                "data_policy: {} — most restrictive of global and repo wins (spec 02 §3.2); p/P cycles",
                data_policy
                    .map(|p| p.as_str())
                    .unwrap_or("(unset — inherits global)"),
            );
            frame.render_widget(
                Paragraph::new(policy_line)
                    .block(Block::bordered().title(format!("Repo Settings — {repo_id}"))),
                policy_area,
            );

            let items: Vec<ListItem> = if settings.is_empty() {
                vec![ListItem::new("no generic settings — n: add one")]
            } else {
                settings
                    .iter()
                    .map(|(k, v)| ListItem::new(format!("{k} = {v}")))
                    .collect()
            };
            let list = List::new(items)
                .block(Block::bordered().title("Settings — e: edit selected, n: add new"))
                .highlight_symbol("> ");
            let mut state =
                ListState::default().with_selected((!settings.is_empty()).then_some(*selected));
            frame.render_stateful_widget(list, list_area, &mut state);

            let footer = error.as_deref().unwrap_or("");
            frame.render_widget(Paragraph::new(footer), footer_area);
        }
        RepoSettingsScreenData::SettingForm {
            repo_id,
            key,
            value,
            field,
            error,
        } => {
            let [key_area, value_area, footer_area] = Layout::vertical([
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Length(1),
            ])
            .areas(frame.area());

            let key_title = if *field == SettingField::Key {
                "Key [editing]"
            } else {
                "Key"
            };
            frame.render_widget(
                Paragraph::new(key.as_str())
                    .block(Block::bordered().title(format!("{key_title} — {repo_id}"))),
                key_area,
            );

            let value_title = if *field == SettingField::Value {
                "Value [editing]"
            } else {
                "Value"
            };
            frame.render_widget(
                Paragraph::new(value.as_str()).block(Block::bordered().title(value_title)),
                value_area,
            );

            let footer = error
                .as_deref()
                .unwrap_or("Tab: switch field  Enter: save  Ctrl+X: cancel");
            frame.render_widget(Paragraph::new(footer), footer_area);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn rendered_text(data: &RepoSettingsScreenData) -> String {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test backend terminal");
        terminal
            .draw(|frame| render_repo_settings(frame, data))
            .expect("draw repo settings screen");
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    fn press(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn press_ctrl(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::CONTROL))
    }

    fn nav_of(outcome: RepoSettingsKeyOutcome) -> RepoSettingsNav {
        match outcome {
            RepoSettingsKeyOutcome::Nav(nav) => nav,
            RepoSettingsKeyOutcome::Execute(action) => {
                panic!("expected Nav, got Execute({action:?})")
            }
        }
    }

    fn execute_of(outcome: RepoSettingsKeyOutcome) -> RepoSettingsAction {
        match outcome {
            RepoSettingsKeyOutcome::Execute(action) => action,
            RepoSettingsKeyOutcome::Nav(nav) => panic!("expected Execute, got Nav({nav:?})"),
        }
    }

    fn repo_row(id: &str) -> RepoRow {
        RepoRow {
            repo_id: id.to_string(),
            current_path: None,
        }
    }

    // ---- render tests ----

    #[test]
    fn renders_unavailable_reason() {
        let data = RepoSettingsScreenData::Unavailable {
            reason: "store not yet initialized".to_string(),
        };
        assert!(rendered_text(&data).contains("not yet initialized"));
    }

    #[test]
    fn renders_empty_repo_list() {
        let data = RepoSettingsScreenData::RepoList {
            rows: vec![],
            selected: 0,
        };
        assert!(rendered_text(&data).contains("no repositories registered yet"));
    }

    #[test]
    fn renders_populated_repo_list() {
        let data = RepoSettingsScreenData::RepoList {
            rows: vec![RepoRow {
                repo_id: "repo-a".to_string(),
                current_path: Some("/repos/a".to_string()),
            }],
            selected: 0,
        };
        let content = rendered_text(&data);
        assert!(content.contains("repo-a"), "{content}");
        assert!(content.contains("/repos/a"), "{content}");
    }

    #[test]
    fn renders_repo_detail_with_and_without_data_policy() {
        let unset = RepoSettingsScreenData::RepoDetail {
            repo_id: "repo-a".to_string(),
            data_policy: None,
            settings: vec![],
            selected: 0,
            error: None,
        };
        let content = rendered_text(&unset);
        assert!(content.contains("unset"), "{content}");
        assert!(content.contains("no generic settings"), "{content}");

        let set = RepoSettingsScreenData::RepoDetail {
            repo_id: "repo-a".to_string(),
            data_policy: Some(DataPolicy::LocalOnly),
            settings: vec![("default_model_space".to_string(), "fast".to_string())],
            selected: 0,
            error: Some("could not save setting: boom".to_string()),
        };
        let content = rendered_text(&set);
        assert!(content.contains("local_only"), "{content}");
        assert!(content.contains("default_model_space = fast"), "{content}");
        assert!(content.contains("could not save setting"), "{content}");
    }

    #[test]
    fn renders_setting_form() {
        let data = RepoSettingsScreenData::SettingForm {
            repo_id: "repo-a".to_string(),
            key: "default_model_space".to_string(),
            value: "fast".to_string(),
            field: SettingField::Value,
            error: None,
        };
        let content = rendered_text(&data);
        assert!(content.contains("default_model_space"), "{content}");
        assert!(content.contains("fast"), "{content}");
        assert!(content.contains("editing"), "{content}");
    }

    // ---- navigation unit tests ----

    #[test]
    fn down_and_up_clamp_in_repo_list() {
        let data = RepoSettingsScreenData::RepoList {
            rows: vec![repo_row("a"), repo_row("b")],
            selected: 0,
        };
        let nav = RepoSettingsNav::default();
        let nav = nav.moved(&data, true);
        assert_eq!(nav, RepoSettingsNav::RepoList { selected: 1 });
        let nav = nav.moved(&data, true);
        assert_eq!(nav, RepoSettingsNav::RepoList { selected: 1 });
        let nav = nav.moved(&data, false);
        assert_eq!(nav, RepoSettingsNav::RepoList { selected: 0 });
    }

    #[test]
    fn enter_descends_and_backspace_ascends_resetting_selection() {
        let list_data = RepoSettingsScreenData::RepoList {
            rows: vec![repo_row("repo-a")],
            selected: 0,
        };
        let nav = RepoSettingsNav::default().descend(&list_data);
        assert_eq!(
            nav,
            RepoSettingsNav::RepoDetail {
                repo_id: "repo-a".to_string(),
                selected: 0,
                error: None,
            }
        );
        assert_eq!(nav.ascend(), RepoSettingsNav::RepoList { selected: 0 });
    }

    #[test]
    fn enter_is_a_no_op_on_an_empty_list() {
        let empty = RepoSettingsScreenData::RepoList {
            rows: vec![],
            selected: 0,
        };
        let nav = RepoSettingsNav::default();
        assert_eq!(nav.descend(&empty), nav);
    }

    #[test]
    fn cycle_data_policy_wraps_both_directions_from_unset_and_from_a_value() {
        assert_eq!(cycle_data_policy(None, true), DataPolicy::LocalOnly);
        assert_eq!(cycle_data_policy(None, false), DataPolicy::AllowRemoteFull);
        assert_eq!(
            cycle_data_policy(Some(DataPolicy::LocalOnly), true),
            DataPolicy::MetadataOnlyRemote
        );
        assert_eq!(
            cycle_data_policy(Some(DataPolicy::AllowRemoteFull), true),
            DataPolicy::LocalOnly
        );
        assert_eq!(
            cycle_data_policy(Some(DataPolicy::LocalOnly), false),
            DataPolicy::AllowRemoteFull
        );
    }

    #[test]
    fn p_on_repo_detail_executes_set_data_policy_directly_no_confirm() {
        let nav = RepoSettingsNav::RepoDetail {
            repo_id: "repo-a".to_string(),
            selected: 0,
            error: None,
        };
        let data = RepoSettingsScreenData::RepoDetail {
            repo_id: "repo-a".to_string(),
            data_policy: None,
            settings: vec![],
            selected: 0,
            error: None,
        };
        let action = execute_of(handle_repo_settings_key(
            &nav,
            &data,
            press(KeyCode::Char('p')),
        ));
        assert_eq!(
            action,
            RepoSettingsAction::SetDataPolicy {
                repo_id: "repo-a".to_string(),
                policy: DataPolicy::LocalOnly,
                list_selected: 0,
            }
        );
    }

    #[test]
    fn e_on_a_selected_row_opens_setting_form_prefilled() {
        let nav = RepoSettingsNav::RepoDetail {
            repo_id: "repo-a".to_string(),
            selected: 0,
            error: None,
        };
        let data = RepoSettingsScreenData::RepoDetail {
            repo_id: "repo-a".to_string(),
            data_policy: None,
            settings: vec![("k1".to_string(), "v1".to_string())],
            selected: 0,
            error: None,
        };
        let next = nav_of(handle_repo_settings_key(
            &nav,
            &data,
            press(KeyCode::Char('e')),
        ));
        assert_eq!(
            next,
            RepoSettingsNav::SettingForm {
                repo_id: "repo-a".to_string(),
                key: "k1".to_string(),
                value: "v1".to_string(),
                field: SettingField::Key,
                error: None,
                list_selected: 0,
            }
        );
    }

    #[test]
    fn e_with_no_rows_is_a_no_op() {
        let nav = RepoSettingsNav::RepoDetail {
            repo_id: "repo-a".to_string(),
            selected: 0,
            error: None,
        };
        let data = RepoSettingsScreenData::RepoDetail {
            repo_id: "repo-a".to_string(),
            data_policy: None,
            settings: vec![],
            selected: 0,
            error: None,
        };
        assert_eq!(
            nav_of(handle_repo_settings_key(
                &nav,
                &data,
                press(KeyCode::Char('e'))
            )),
            nav
        );
    }

    #[test]
    fn n_always_opens_an_empty_setting_form() {
        let nav = RepoSettingsNav::RepoDetail {
            repo_id: "repo-a".to_string(),
            selected: 0,
            error: None,
        };
        let data = RepoSettingsScreenData::RepoDetail {
            repo_id: "repo-a".to_string(),
            data_policy: None,
            settings: vec![("k1".to_string(), "v1".to_string())],
            selected: 0,
            error: None,
        };
        let next = nav_of(handle_repo_settings_key(
            &nav,
            &data,
            press(KeyCode::Char('n')),
        ));
        assert_eq!(
            next,
            RepoSettingsNav::SettingForm {
                repo_id: "repo-a".to_string(),
                key: String::new(),
                value: String::new(),
                field: SettingField::Key,
                error: None,
                list_selected: 0,
            }
        );
    }

    #[test]
    fn setting_form_typing_appends_including_q_and_digits_backspace_deletes() {
        let nav = RepoSettingsNav::SettingForm {
            repo_id: "repo-a".to_string(),
            key: String::new(),
            value: String::new(),
            field: SettingField::Key,
            error: None,
            list_selected: 0,
        };
        let data = RepoSettingsScreenData::SettingForm {
            repo_id: "repo-a".to_string(),
            key: String::new(),
            value: String::new(),
            field: SettingField::Key,
            error: None,
        };
        let nav = nav_of(handle_repo_settings_key(
            &nav,
            &data,
            press(KeyCode::Char('q')),
        ));
        let nav = nav_of(handle_repo_settings_key(
            &nav,
            &data,
            press(KeyCode::Char('7')),
        ));
        match &nav {
            RepoSettingsNav::SettingForm { key, .. } => assert_eq!(key, "q7"),
            other => panic!("expected SettingForm, got {other:?}"),
        }
        let nav = nav_of(handle_repo_settings_key(
            &nav,
            &data,
            press(KeyCode::Backspace),
        ));
        match nav {
            RepoSettingsNav::SettingForm { key, .. } => assert_eq!(key, "q"),
            other => panic!("expected SettingForm, got {other:?}"),
        }
    }

    #[test]
    fn setting_form_tab_switches_field() {
        let nav = RepoSettingsNav::SettingForm {
            repo_id: "repo-a".to_string(),
            key: String::new(),
            value: String::new(),
            field: SettingField::Key,
            error: None,
            list_selected: 0,
        };
        let data = RepoSettingsScreenData::SettingForm {
            repo_id: "repo-a".to_string(),
            key: String::new(),
            value: String::new(),
            field: SettingField::Key,
            error: None,
        };
        let next = nav_of(handle_repo_settings_key(&nav, &data, press(KeyCode::Tab)));
        match next {
            RepoSettingsNav::SettingForm { field, .. } => assert_eq!(field, SettingField::Value),
            other => panic!("expected SettingForm, got {other:?}"),
        }
    }

    #[test]
    fn setting_form_enter_with_empty_key_stays_with_error() {
        let nav = RepoSettingsNav::SettingForm {
            repo_id: "repo-a".to_string(),
            key: String::new(),
            value: "v1".to_string(),
            field: SettingField::Value,
            error: None,
            list_selected: 0,
        };
        let data = RepoSettingsScreenData::SettingForm {
            repo_id: "repo-a".to_string(),
            key: String::new(),
            value: "v1".to_string(),
            field: SettingField::Value,
            error: None,
        };
        let next = nav_of(handle_repo_settings_key(&nav, &data, press(KeyCode::Enter)));
        match next {
            RepoSettingsNav::SettingForm { error: Some(e), .. } => {
                assert!(e.contains("must not be empty"), "{e}");
            }
            other => panic!("expected SettingForm with error, got {other:?}"),
        }
    }

    #[test]
    fn setting_form_enter_with_a_key_executes_directly_no_confirm() {
        let nav = RepoSettingsNav::SettingForm {
            repo_id: "repo-a".to_string(),
            key: "k1".to_string(),
            value: "v1".to_string(),
            field: SettingField::Value,
            error: None,
            list_selected: 2,
        };
        let data = RepoSettingsScreenData::SettingForm {
            repo_id: "repo-a".to_string(),
            key: "k1".to_string(),
            value: "v1".to_string(),
            field: SettingField::Value,
            error: None,
        };
        let action = execute_of(handle_repo_settings_key(&nav, &data, press(KeyCode::Enter)));
        assert_eq!(
            action,
            RepoSettingsAction::SetSetting {
                repo_id: "repo-a".to_string(),
                key: "k1".to_string(),
                value: "v1".to_string(),
                list_selected: 2,
            }
        );
    }

    #[test]
    fn setting_form_ctrl_x_cancels_to_repo_detail() {
        let nav = RepoSettingsNav::SettingForm {
            repo_id: "repo-a".to_string(),
            key: "k1".to_string(),
            value: "v1".to_string(),
            field: SettingField::Key,
            error: None,
            list_selected: 3,
        };
        let data = RepoSettingsScreenData::SettingForm {
            repo_id: "repo-a".to_string(),
            key: "k1".to_string(),
            value: "v1".to_string(),
            field: SettingField::Key,
            error: None,
        };
        assert_eq!(
            nav_of(handle_repo_settings_key(
                &nav,
                &data,
                press_ctrl(KeyCode::Char('x'))
            )),
            RepoSettingsNav::RepoDetail {
                repo_id: "repo-a".to_string(),
                selected: 3,
                error: None,
            }
        );
    }

    #[test]
    fn handle_repo_settings_key_ignores_key_release() {
        let nav = RepoSettingsNav::default();
        let data = RepoSettingsScreenData::RepoList {
            rows: vec![repo_row("a")],
            selected: 0,
        };
        let mut release = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        assert_eq!(
            nav_of(handle_repo_settings_key(&nav, &data, Event::Key(release))),
            nav
        );
    }

    #[test]
    fn captures_all_keys_is_true_only_for_setting_form() {
        assert!(!captures_all_keys(&RepoSettingsNav::default()));
        assert!(!captures_all_keys(&RepoSettingsNav::RepoDetail {
            repo_id: "repo-a".to_string(),
            selected: 0,
            error: None,
        }));
        assert!(captures_all_keys(&RepoSettingsNav::SettingForm {
            repo_id: "repo-a".to_string(),
            key: String::new(),
            value: String::new(),
            field: SettingField::Key,
            error: None,
            list_selected: 0,
        }));
    }
}
