//! The Server Settings screen (spec 11 §7, T18-07): a staged form over all six
//! `local_rag_core::config::Config` sections (`daemon`/`storage`/`models`/`index`/`spool`/
//! `memory`), flushed to `<config_dir>/config.toml` on `Ctrl+S` via [`local_rag_core::config::
//! Config::save`], with a follow-up prompt offering to call `local-rag restart` immediately (the
//! only way the new values take effect — there is no live config reload in the daemon, spec
//! carried over unchanged by this card).
//!
//! # Staged edits, not immediate-apply — unlike every other write screen
//!
//! T18-05's Memory mutations and T18-06's Repo Settings both apply each edit to `state.sqlite`
//! the moment it is confirmed, then re-read fresh data next frame. This screen instead holds a
//! working [`local_rag_core::config::Config`] copy inside [`ServerSettingsNav`] itself across
//! frames, mutated in memory as each field is edited, and written to disk only on `Ctrl+S` — the
//! card's own UX ("`Ctrl+S` → save" as one step covering the whole form, not per field) requires
//! it, and it would make no sense to write `config.toml` sixteen times for one sitting at the
//! keyboard.
//!
//! # No `StoreLayout` — this is the first screen not backed by `state.sqlite`
//!
//! Every earlier screen resolves `state.sqlite`/`cache.sqlite` through `StoreLayout`. Config
//! resolution is a separate concern (`local_rag_core::paths::config_dir`, a different resolver
//! with its own `LOCAL_RAG_HOME`-first precedence) — this module's `compute`/`execute` functions
//! take a plain `config_dir: &Path` instead, resolved once by `main.rs` alongside `StoreLayout`.
//!
//! # `handle_server_settings_key` takes no `data` parameter
//!
//! Every prior screen's `handle_*_key(nav, data, ev)` needs `data` because `data` is freshly
//! re-read from the store and may disagree with what `nav` last saw (e.g. a row count changing
//! between frames). Nothing here is re-read per frame — the working `Config` lives entirely on
//! `nav`, and the field list has a fixed, compile-time length ([`ALL_FIELDS`]) — so there is
//! nothing a separate `data` parameter would supply that `nav` does not already have. Kept as
//! `handle_server_settings_key(nav, ev)` rather than carrying an always-redundant parameter.
//!
//! # One free-text form for every field, including the `data_policy` enum
//!
//! Repo Settings dedicates a `p`/`P` cycling shortcut to its own `data_policy` field. This screen
//! deliberately does not special-case `models.data_policy` the same way: uniform free-text entry
//! (validated by [`DataPolicy::from_str_value`] on submit, same as every numeric field's own
//! `str::parse`) keeps all 16 fields behind one interaction model instead of two, at the minor
//! cost of typing the value instead of cycling it — simpler than a second, one-off widget.
//!
//! # Sibling-binary resolution, again
//!
//! [`execute_server_settings_action`]'s `Restart` action resolves `local-rag` next to the running
//! `local-rag-tui` binary (`std::env::current_exe()` → `.parent()` → `.join("local-rag")`) — the
//! same idiom `local-rag-proxy::connect::resolve_daemon_binary_path` and
//! `local-rag/src/cli/service.rs`'s own restart logic each already carry their own copy of, by
//! their own documented "each binary carries its own trivial copy" convention. Invoked
//! synchronously (`std::process::Command::status`, stdio redirected to `/dev/null`-equivalent) —
//! no `crate::rt::block_on` involved, since this is a child process wait, not an async database
//! call; the up-to-~30s blocking wait mirrors `local-rag restart`'s own internal timeout and is
//! acceptable for this card's minimal scope.

use std::path::Path;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use local_rag_core::DataPolicy;
use local_rag_core::config::Config;

use crate::keys::{is_ctrl, step};

/// One editable leaf of [`Config`]. Order here is the row order [`ALL_FIELDS`] presents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldId {
    DaemonIdleShutdownSecs,
    DaemonMaxOpenShards,
    DaemonLogLevel,
    StorageEmbeddingCacheBudgetMb,
    StoragePayloadTtlHours,
    StorageRetiredGenerationsKeep,
    StorageRetiredGenerationsTtlH,
    ModelsDefaultModelSpace,
    ModelsDataPolicy,
    IndexLanguages,
    IndexMaxFileSizeKb,
    SpoolDenyPaths,
    SpoolDenyTools,
    MemoryRecallTokenBudget,
    MemoryConsolidationBatchSize,
    MemoryConsolidationQueueThreshold,
}

/// Every field, in display order — one row per [`Config`] leaf, six sections'-worth.
const ALL_FIELDS: [FieldId; 16] = [
    FieldId::DaemonIdleShutdownSecs,
    FieldId::DaemonMaxOpenShards,
    FieldId::DaemonLogLevel,
    FieldId::StorageEmbeddingCacheBudgetMb,
    FieldId::StoragePayloadTtlHours,
    FieldId::StorageRetiredGenerationsKeep,
    FieldId::StorageRetiredGenerationsTtlH,
    FieldId::ModelsDefaultModelSpace,
    FieldId::ModelsDataPolicy,
    FieldId::IndexLanguages,
    FieldId::IndexMaxFileSizeKb,
    FieldId::SpoolDenyPaths,
    FieldId::SpoolDenyTools,
    FieldId::MemoryRecallTokenBudget,
    FieldId::MemoryConsolidationBatchSize,
    FieldId::MemoryConsolidationQueueThreshold,
];

fn field_label(field: FieldId) -> &'static str {
    match field {
        FieldId::DaemonIdleShutdownSecs => "daemon.idle_shutdown_secs",
        FieldId::DaemonMaxOpenShards => "daemon.max_open_shards",
        FieldId::DaemonLogLevel => "daemon.log_level",
        FieldId::StorageEmbeddingCacheBudgetMb => "storage.embedding_cache_budget_mb",
        FieldId::StoragePayloadTtlHours => "storage.payload_ttl_hours",
        FieldId::StorageRetiredGenerationsKeep => "storage.retired_generations_keep",
        FieldId::StorageRetiredGenerationsTtlH => "storage.retired_generations_ttl_h",
        FieldId::ModelsDefaultModelSpace => "models.default_model_space",
        FieldId::ModelsDataPolicy => "models.data_policy",
        FieldId::IndexLanguages => "index.languages",
        FieldId::IndexMaxFileSizeKb => "index.max_file_size_kb",
        FieldId::SpoolDenyPaths => "spool.deny_paths",
        FieldId::SpoolDenyTools => "spool.deny_tools",
        FieldId::MemoryRecallTokenBudget => "memory.recall_token_budget",
        FieldId::MemoryConsolidationBatchSize => "memory.consolidation_batch_size",
        FieldId::MemoryConsolidationQueueThreshold => "memory.consolidation_queue_threshold",
    }
}

/// The field's current value, rendered exactly as [`apply_field_value`] expects to parse it back
/// — a `Vec<String>` field's `", "`-joined display round-trips through [`split_csv`].
fn field_display_value(config: &Config, field: FieldId) -> String {
    match field {
        FieldId::DaemonIdleShutdownSecs => config.daemon.idle_shutdown_secs.to_string(),
        FieldId::DaemonMaxOpenShards => config.daemon.max_open_shards.to_string(),
        FieldId::DaemonLogLevel => config.daemon.log_level.clone(),
        FieldId::StorageEmbeddingCacheBudgetMb => {
            config.storage.embedding_cache_budget_mb.to_string()
        }
        FieldId::StoragePayloadTtlHours => config.storage.payload_ttl_hours.to_string(),
        FieldId::StorageRetiredGenerationsKeep => {
            config.storage.retired_generations_keep.to_string()
        }
        FieldId::StorageRetiredGenerationsTtlH => {
            config.storage.retired_generations_ttl_h.to_string()
        }
        FieldId::ModelsDefaultModelSpace => config.models.default_model_space.clone(),
        FieldId::ModelsDataPolicy => config.models.data_policy.as_str().to_string(),
        FieldId::IndexLanguages => config.index.languages.join(", "),
        FieldId::IndexMaxFileSizeKb => config.index.max_file_size_kb.to_string(),
        FieldId::SpoolDenyPaths => config.spool.deny_paths.join(", "),
        FieldId::SpoolDenyTools => config.spool.deny_tools.join(", "),
        FieldId::MemoryRecallTokenBudget => config.memory.recall_token_budget.to_string(),
        FieldId::MemoryConsolidationBatchSize => config.memory.consolidation_batch_size.to_string(),
        FieldId::MemoryConsolidationQueueThreshold => {
            config.memory.consolidation_queue_threshold.to_string()
        }
    }
}

/// Comma-separated free text into a `Vec<String>`: trims each entry, drops empty ones (so both
/// `""` and a trailing `,` mean "no entries" rather than one blank entry).
fn split_csv(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Parses `text` per `field`'s own type and, on success, mutates `config` in place. Never leaves
/// `config` partially mutated on `Err` — every arm either assigns once or returns before
/// assigning.
fn apply_field_value(config: &mut Config, field: FieldId, text: &str) -> Result<(), String> {
    fn parse_u64(text: &str) -> Result<u64, String> {
        text.trim()
            .parse()
            .map_err(|_| format!("{:?} is not a whole number", text.trim()))
    }
    fn parse_u32(text: &str) -> Result<u32, String> {
        text.trim()
            .parse()
            .map_err(|_| format!("{:?} is not a whole number", text.trim()))
    }
    fn parse_i64(text: &str) -> Result<i64, String> {
        text.trim()
            .parse()
            .map_err(|_| format!("{:?} is not a whole number", text.trim()))
    }

    match field {
        FieldId::DaemonIdleShutdownSecs => config.daemon.idle_shutdown_secs = parse_u64(text)?,
        FieldId::DaemonMaxOpenShards => config.daemon.max_open_shards = parse_u32(text)?,
        FieldId::DaemonLogLevel => config.daemon.log_level = text.trim().to_string(),
        FieldId::StorageEmbeddingCacheBudgetMb => {
            config.storage.embedding_cache_budget_mb = parse_u64(text)?
        }
        FieldId::StoragePayloadTtlHours => config.storage.payload_ttl_hours = parse_u64(text)?,
        FieldId::StorageRetiredGenerationsKeep => {
            config.storage.retired_generations_keep = parse_u32(text)?
        }
        FieldId::StorageRetiredGenerationsTtlH => {
            config.storage.retired_generations_ttl_h = parse_u64(text)?
        }
        FieldId::ModelsDefaultModelSpace => {
            config.models.default_model_space = text.trim().to_string()
        }
        FieldId::ModelsDataPolicy => {
            config.models.data_policy =
                DataPolicy::from_str_value(text.trim()).ok_or_else(|| {
                    format!(
                        "{:?} is not one of local_only | metadata_only_remote | \
                     allow_remote_with_redaction | allow_remote_full",
                        text.trim()
                    )
                })?
        }
        FieldId::IndexLanguages => config.index.languages = split_csv(text),
        FieldId::IndexMaxFileSizeKb => config.index.max_file_size_kb = parse_u64(text)?,
        FieldId::SpoolDenyPaths => config.spool.deny_paths = split_csv(text),
        FieldId::SpoolDenyTools => config.spool.deny_tools = split_csv(text),
        FieldId::MemoryRecallTokenBudget => config.memory.recall_token_budget = parse_u32(text)?,
        FieldId::MemoryConsolidationBatchSize => {
            config.memory.consolidation_batch_size = parse_i64(text)?
        }
        FieldId::MemoryConsolidationQueueThreshold => {
            config.memory.consolidation_queue_threshold = parse_i64(text)?
        }
    }
    Ok(())
}

/// Which state the screen is in, carrying the staged working [`Config`] (see the module doc's
/// "staged edits" section) through every frame until `Ctrl+S` flushes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerSettingsNav {
    FieldList {
        config: Config,
        selected: usize,
        status: Option<String>,
    },
    FieldForm {
        config: Config,
        selected: usize,
        field: FieldId,
        text: String,
        error: Option<String>,
    },
    SavedPrompt {
        config: Config,
        selected: usize,
    },
}

impl Default for ServerSettingsNav {
    /// `Config::default()`, not a real on-disk read — for tests and as a harmless starting shape;
    /// `main.rs`'s actual startup uses [`initial_nav`] instead, which does read the real file.
    fn default() -> Self {
        ServerSettingsNav::FieldList {
            config: Config::default(),
            selected: 0,
            status: None,
        }
    }
}

/// The real startup path: loads `<config_dir>/config.toml`. A missing file is
/// [`Config::default`] with no status (same as [`Config::load`]'s own contract); an unreadable or
/// invalid one still shows the form (defaults) but with an explanatory `status`, rather than
/// panicking or silently risking an overwrite of a file the user has not seen yet.
pub fn initial_nav(config_dir: &Path) -> ServerSettingsNav {
    match Config::load(config_dir) {
        Ok(config) => ServerSettingsNav::FieldList {
            config,
            selected: 0,
            status: None,
        },
        Err(e) => ServerSettingsNav::FieldList {
            config: Config::default(),
            selected: 0,
            status: Some(format!("config.toml unreadable, showing defaults: {e}")),
        },
    }
}

/// A fully-specified mutation, ready to run — the same split T18-05's `MemoryAction`/T18-06's
/// `RepoSettingsAction` established, built by [`handle_server_settings_key`], executed by
/// [`execute_server_settings_action`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerSettingsAction {
    Save { config: Config, selected: usize },
    Restart { config: Config, selected: usize },
}

/// `handle_server_settings_key`'s return: either a pure navigation update, or a fully-specified
/// action `run_app` must hand to [`execute_server_settings_action`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerSettingsKeyOutcome {
    Nav(ServerSettingsNav),
    Execute(ServerSettingsAction),
}

/// `true` only for [`ServerSettingsNav::FieldForm`] — consulted by `main.rs`'s own text-entry
/// carve-out (`is_text_entry_key`), the same mechanism T18-05's `memory::captures_all_keys`
/// established.
pub fn captures_all_keys(nav: &ServerSettingsNav) -> bool {
    matches!(nav, ServerSettingsNav::FieldForm { .. })
}

fn handle_field_list_key(nav: &ServerSettingsNav, key: KeyEvent) -> ServerSettingsKeyOutcome {
    let ServerSettingsNav::FieldList {
        config,
        selected,
        status,
    } = nav
    else {
        return ServerSettingsKeyOutcome::Nav(nav.clone());
    };

    if is_ctrl(&key, 's') {
        return ServerSettingsKeyOutcome::Execute(ServerSettingsAction::Save {
            config: config.clone(),
            selected: *selected,
        });
    }

    match key.code {
        KeyCode::Up => ServerSettingsKeyOutcome::Nav(ServerSettingsNav::FieldList {
            config: config.clone(),
            selected: step(*selected, false, ALL_FIELDS.len()),
            status: status.clone(),
        }),
        KeyCode::Down => ServerSettingsKeyOutcome::Nav(ServerSettingsNav::FieldList {
            config: config.clone(),
            selected: step(*selected, true, ALL_FIELDS.len()),
            status: status.clone(),
        }),
        KeyCode::Enter => {
            let field = ALL_FIELDS[*selected];
            ServerSettingsKeyOutcome::Nav(ServerSettingsNav::FieldForm {
                text: field_display_value(config, field),
                config: config.clone(),
                selected: *selected,
                field,
                error: None,
            })
        }
        _ => ServerSettingsKeyOutcome::Nav(nav.clone()),
    }
}

/// Any unmodified printable char appends to `text` (including `q`/digits — the same global-quit
/// carve-out `EditForm`/`SettingForm` need), `Backspace` deletes from it, `Enter` validates+applies
/// via [`apply_field_value`], `Ctrl+X` cancels back to `FieldList` with `config` untouched.
fn handle_field_form_key(nav: &ServerSettingsNav, key: KeyEvent) -> ServerSettingsKeyOutcome {
    let ServerSettingsNav::FieldForm {
        config,
        selected,
        field,
        text,
        ..
    } = nav
    else {
        return ServerSettingsKeyOutcome::Nav(nav.clone());
    };

    if is_ctrl(&key, 'x') {
        return ServerSettingsKeyOutcome::Nav(ServerSettingsNav::FieldList {
            config: config.clone(),
            selected: *selected,
            status: None,
        });
    }

    match key.code {
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            let mut text = text.clone();
            text.push(c);
            ServerSettingsKeyOutcome::Nav(ServerSettingsNav::FieldForm {
                config: config.clone(),
                selected: *selected,
                field: *field,
                text,
                error: None,
            })
        }
        KeyCode::Backspace => {
            let mut text = text.clone();
            text.pop();
            ServerSettingsKeyOutcome::Nav(ServerSettingsNav::FieldForm {
                config: config.clone(),
                selected: *selected,
                field: *field,
                text,
                error: None,
            })
        }
        KeyCode::Enter => {
            let mut new_config = config.clone();
            match apply_field_value(&mut new_config, *field, text) {
                Ok(()) => ServerSettingsKeyOutcome::Nav(ServerSettingsNav::FieldList {
                    config: new_config,
                    selected: *selected,
                    status: None,
                }),
                Err(error) => ServerSettingsKeyOutcome::Nav(ServerSettingsNav::FieldForm {
                    config: config.clone(),
                    selected: *selected,
                    field: *field,
                    text: text.clone(),
                    error: Some(error),
                }),
            }
        }
        _ => ServerSettingsKeyOutcome::Nav(nav.clone()),
    }
}

/// `r`/`R` requests an immediate `local-rag restart`; any other key just dismisses the prompt.
fn handle_saved_prompt_key(nav: &ServerSettingsNav, key: KeyEvent) -> ServerSettingsKeyOutcome {
    let ServerSettingsNav::SavedPrompt { config, selected } = nav else {
        return ServerSettingsKeyOutcome::Nav(nav.clone());
    };
    match key.code {
        KeyCode::Char('r') | KeyCode::Char('R') => {
            ServerSettingsKeyOutcome::Execute(ServerSettingsAction::Restart {
                config: config.clone(),
                selected: *selected,
            })
        }
        _ => ServerSettingsKeyOutcome::Nav(ServerSettingsNav::FieldList {
            config: config.clone(),
            selected: *selected,
            status: Some("saved".to_string()),
        }),
    }
}

/// The only keys this screen's own handler recognizes; `run_app` checks global keys first —
/// except while `captures_all_keys(nav)` and the pressed key is bare `q`/a digit, the same
/// carve-out `memory.rs`/`repo_settings.rs` established. Pure: no I/O, no render.
pub fn handle_server_settings_key(nav: &ServerSettingsNav, ev: Event) -> ServerSettingsKeyOutcome {
    let Event::Key(key) = ev else {
        return ServerSettingsKeyOutcome::Nav(nav.clone());
    };
    if key.kind != KeyEventKind::Press {
        return ServerSettingsKeyOutcome::Nav(nav.clone());
    }
    match nav {
        ServerSettingsNav::FieldList { .. } => handle_field_list_key(nav, key),
        ServerSettingsNav::FieldForm { .. } => handle_field_form_key(nav, key),
        ServerSettingsNav::SavedPrompt { .. } => handle_saved_prompt_key(nav, key),
    }
}

/// Resolve `local-rag` next to the running `local-rag-tui` binary and run `restart`, synchronously
/// (see the module doc's "sibling-binary resolution" section). stdio is redirected away from this
/// process's own — inherited stdio would corrupt the TUI's raw-mode alternate screen.
fn restart_daemon() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("could not resolve own binary: {e}"))?;
    let dir = exe
        .parent()
        .ok_or_else(|| "the running binary has no parent directory".to_string())?;
    let name = if cfg!(windows) {
        "local-rag.exe"
    } else {
        "local-rag"
    };
    let candidate = dir.join(name);
    if !candidate.is_file() {
        return Err(format!(
            "no sibling {name} binary at {}",
            candidate.display()
        ));
    }

    use std::process::{Command, Stdio};
    let status = Command::new(candidate)
        .arg("restart")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("could not run local-rag restart: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("local-rag restart exited with {status}"))
    }
}

/// The only function in this module that touches the filesystem or spawns a process — mirrors
/// T18-05/T18-06's own `execute_*_action` shape.
pub fn execute_server_settings_action(
    config_dir: &Path,
    action: ServerSettingsAction,
) -> ServerSettingsNav {
    match action {
        ServerSettingsAction::Save { config, selected } => match config.save(config_dir) {
            Ok(()) => ServerSettingsNav::SavedPrompt { config, selected },
            Err(e) => ServerSettingsNav::FieldList {
                config,
                selected,
                status: Some(format!("save failed: {e}")),
            },
        },
        ServerSettingsAction::Restart { config, selected } => {
            let status = match restart_daemon() {
                Ok(()) => "local-rag restart: ok".to_string(),
                Err(e) => format!("local-rag restart failed: {e}"),
            };
            ServerSettingsNav::FieldList {
                config,
                selected,
                status: Some(status),
            }
        }
    }
}

/// What [`render_server_settings`] draws — pure derivation from [`ServerSettingsNav`], no I/O
/// (unlike every prior screen's own `ScreenData`, nothing here is re-read from a store; it is
/// already in memory on `nav`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerSettingsScreenData {
    FieldList {
        rows: Vec<(String, String)>,
        selected: usize,
        status: Option<String>,
    },
    FieldForm {
        label: String,
        text: String,
        error: Option<String>,
    },
    SavedPrompt,
}

/// Compose everything — what `run_app` (and every test) actually calls.
pub fn compute_server_settings_data(nav: &ServerSettingsNav) -> ServerSettingsScreenData {
    match nav {
        ServerSettingsNav::FieldList {
            config,
            selected,
            status,
        } => ServerSettingsScreenData::FieldList {
            rows: ALL_FIELDS
                .iter()
                .map(|f| (field_label(*f).to_string(), field_display_value(config, *f)))
                .collect(),
            selected: (*selected).min(ALL_FIELDS.len().saturating_sub(1)),
            status: status.clone(),
        },
        ServerSettingsNav::FieldForm {
            field, text, error, ..
        } => ServerSettingsScreenData::FieldForm {
            label: field_label(*field).to_string(),
            text: text.clone(),
            error: error.clone(),
        },
        ServerSettingsNav::SavedPrompt { .. } => ServerSettingsScreenData::SavedPrompt,
    }
}

/// Pure render — no I/O, `TestBackend`-testable without a daemon or a store.
pub fn render_server_settings(frame: &mut ratatui::Frame, data: &ServerSettingsScreenData) {
    use ratatui::layout::{Constraint, Layout};
    use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph};

    match data {
        ServerSettingsScreenData::FieldList {
            rows,
            selected,
            status,
        } => {
            let [list_area, footer_area] =
                Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(frame.area());

            let items: Vec<ListItem> = rows
                .iter()
                .map(|(label, value)| ListItem::new(format!("{label} = {value}")))
                .collect();
            let list = List::new(items)
                .block(Block::bordered().title("Server Settings — Enter: edit, Ctrl+S: save"))
                .highlight_symbol("> ");
            let mut state = ListState::default().with_selected(Some(*selected));
            frame.render_stateful_widget(list, list_area, &mut state);

            frame.render_widget(Paragraph::new(status.as_deref().unwrap_or("")), footer_area);
        }
        ServerSettingsScreenData::FieldForm { label, text, error } => {
            let [field_area, footer_area] =
                Layout::vertical([Constraint::Length(3), Constraint::Length(1)])
                    .areas(frame.area());

            frame.render_widget(
                Paragraph::new(text.as_str()).block(Block::bordered().title(label.as_str())),
                field_area,
            );
            let footer = error
                .as_deref()
                .unwrap_or("Enter: save field  Ctrl+X: cancel");
            frame.render_widget(Paragraph::new(footer), footer_area);
        }
        ServerSettingsScreenData::SavedPrompt => {
            frame.render_widget(
                Paragraph::new(
                    "Saved. Takes effect after `local-rag restart`.\n\n\
                     r: restart now   any other key: back",
                )
                .block(Block::bordered().title("Server Settings — saved")),
                frame.area(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventState, KeyModifiers};

    fn press(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn ctrl_press(c: char) -> Event {
        Event::Key(KeyEvent {
            code: KeyCode::Char(c),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })
    }

    // ---- field_display_value / apply_field_value ----------------------------

    #[test]
    fn every_numeric_field_round_trips_through_display_and_apply() {
        let mut config = Config::default();
        for (field, value) in [
            (FieldId::DaemonIdleShutdownSecs, "42"),
            (FieldId::DaemonMaxOpenShards, "3"),
            (FieldId::StorageEmbeddingCacheBudgetMb, "4096"),
            (FieldId::StoragePayloadTtlHours, "24"),
            (FieldId::StorageRetiredGenerationsKeep, "5"),
            (FieldId::StorageRetiredGenerationsTtlH, "12"),
            (FieldId::IndexMaxFileSizeKb, "2048"),
            (FieldId::MemoryRecallTokenBudget, "3000"),
            (FieldId::MemoryConsolidationBatchSize, "5"),
            (FieldId::MemoryConsolidationQueueThreshold, "30"),
        ] {
            apply_field_value(&mut config, field, value).expect("valid number applies");
            assert_eq!(field_display_value(&config, field), value);
        }
    }

    #[test]
    fn string_fields_round_trip_and_are_trimmed() {
        let mut config = Config::default();
        apply_field_value(&mut config, FieldId::DaemonLogLevel, "  debug  ").unwrap();
        assert_eq!(
            field_display_value(&config, FieldId::DaemonLogLevel),
            "debug"
        );

        apply_field_value(&mut config, FieldId::ModelsDefaultModelSpace, "fast").unwrap();
        assert_eq!(
            field_display_value(&config, FieldId::ModelsDefaultModelSpace),
            "fast"
        );
    }

    #[test]
    fn vec_fields_round_trip_through_comma_joined_display() {
        let mut config = Config::default();
        apply_field_value(&mut config, FieldId::IndexLanguages, "python, go , rust").unwrap();
        assert_eq!(config.index.languages, vec!["python", "go", "rust"]);
        assert_eq!(
            field_display_value(&config, FieldId::IndexLanguages),
            "python, go, rust"
        );

        apply_field_value(&mut config, FieldId::SpoolDenyPaths, "secrets,.env,").unwrap();
        assert_eq!(config.spool.deny_paths, vec!["secrets", ".env"]);
    }

    #[test]
    fn an_empty_vec_field_clears_to_no_entries() {
        let mut config = Config::default();
        config.spool.deny_tools = vec!["Bash".to_string()];
        apply_field_value(&mut config, FieldId::SpoolDenyTools, "").unwrap();
        assert!(config.spool.deny_tools.is_empty());
    }

    #[test]
    fn data_policy_round_trips_and_rejects_bogus_values() {
        let mut config = Config::default();
        apply_field_value(&mut config, FieldId::ModelsDataPolicy, "allow_remote_full").unwrap();
        assert_eq!(config.models.data_policy, DataPolicy::AllowRemoteFull);
        assert_eq!(
            field_display_value(&config, FieldId::ModelsDataPolicy),
            "allow_remote_full"
        );

        let err = apply_field_value(&mut config, FieldId::ModelsDataPolicy, "send_it_all")
            .expect_err("bogus policy is rejected");
        assert!(err.contains("send_it_all"), "{err}");
        // Rejected input must not have mutated the field.
        assert_eq!(config.models.data_policy, DataPolicy::AllowRemoteFull);
    }

    #[test]
    fn every_numeric_field_rejects_non_numeric_text() {
        let mut config = Config::default();
        for field in [
            FieldId::DaemonIdleShutdownSecs,
            FieldId::DaemonMaxOpenShards,
            FieldId::StorageEmbeddingCacheBudgetMb,
            FieldId::StoragePayloadTtlHours,
            FieldId::StorageRetiredGenerationsKeep,
            FieldId::StorageRetiredGenerationsTtlH,
            FieldId::IndexMaxFileSizeKb,
            FieldId::MemoryRecallTokenBudget,
            FieldId::MemoryConsolidationBatchSize,
            FieldId::MemoryConsolidationQueueThreshold,
        ] {
            let err = apply_field_value(&mut config, field, "not-a-number")
                .expect_err(&format!("{field:?} must reject non-numeric text"));
            assert!(err.contains("not-a-number"), "{err}");
        }
    }

    #[test]
    fn field_labels_are_unique_and_cover_every_field() {
        let labels: Vec<&str> = ALL_FIELDS.iter().map(|f| field_label(*f)).collect();
        let mut sorted = labels.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            labels.len(),
            "duplicate field label: {labels:?}"
        );
        assert_eq!(labels.len(), 16);
    }

    // ---- handle_server_settings_key ------------------------------------------

    fn list_nav(config: Config, selected: usize) -> ServerSettingsNav {
        ServerSettingsNav::FieldList {
            config,
            selected,
            status: None,
        }
    }

    #[test]
    fn field_list_up_down_clamp_via_shared_step() {
        let nav = list_nav(Config::default(), 0);
        let ServerSettingsKeyOutcome::Nav(ServerSettingsNav::FieldList { selected, .. }) =
            handle_server_settings_key(&nav, press(KeyCode::Up))
        else {
            panic!("expected Nav(FieldList)");
        };
        assert_eq!(selected, 0, "clamped at the top");

        let ServerSettingsKeyOutcome::Nav(ServerSettingsNav::FieldList { selected, .. }) =
            handle_server_settings_key(&nav, press(KeyCode::Down))
        else {
            panic!("expected Nav(FieldList)");
        };
        assert_eq!(selected, 1);
    }

    #[test]
    fn enter_on_field_list_opens_a_prefilled_field_form() {
        let mut config = Config::default();
        config.daemon.log_level = "info".to_string();
        let nav = list_nav(config, 2); // DaemonLogLevel
        let ServerSettingsKeyOutcome::Nav(ServerSettingsNav::FieldForm {
            field, text, error, ..
        }) = handle_server_settings_key(&nav, press(KeyCode::Enter))
        else {
            panic!("expected Nav(FieldForm)");
        };
        assert_eq!(field, FieldId::DaemonLogLevel);
        assert_eq!(text, "info");
        assert_eq!(error, None);
    }

    #[test]
    fn ctrl_s_on_field_list_emits_a_save_action() {
        let config = Config::default();
        let nav = list_nav(config.clone(), 5);
        let outcome = handle_server_settings_key(&nav, ctrl_press('s'));
        assert_eq!(
            outcome,
            ServerSettingsKeyOutcome::Execute(ServerSettingsAction::Save {
                config,
                selected: 5,
            })
        );
    }

    #[test]
    fn field_form_text_editing_appends_and_backspaces() {
        let nav = ServerSettingsNav::FieldForm {
            config: Config::default(),
            selected: 0,
            field: FieldId::DaemonLogLevel,
            text: "in".to_string(),
            error: None,
        };
        let ServerSettingsKeyOutcome::Nav(ServerSettingsNav::FieldForm { text, .. }) =
            handle_server_settings_key(&nav, press(KeyCode::Char('q')))
        else {
            panic!("expected Nav(FieldForm)");
        };
        assert_eq!(text, "inq", "bare q must be typable, not treated as quit");

        let nav_with_text = ServerSettingsNav::FieldForm {
            config: Config::default(),
            selected: 0,
            field: FieldId::DaemonLogLevel,
            text: "inq".to_string(),
            error: None,
        };
        let ServerSettingsKeyOutcome::Nav(ServerSettingsNav::FieldForm { text, .. }) =
            handle_server_settings_key(&nav_with_text, press(KeyCode::Backspace))
        else {
            panic!("expected Nav(FieldForm)");
        };
        assert_eq!(text, "in");
    }

    #[test]
    fn field_form_enter_with_a_valid_value_applies_and_returns_to_the_list() {
        let nav = ServerSettingsNav::FieldForm {
            config: Config::default(),
            selected: 1,
            field: FieldId::DaemonMaxOpenShards,
            text: "16".to_string(),
            error: None,
        };
        let ServerSettingsKeyOutcome::Nav(ServerSettingsNav::FieldList {
            config, selected, ..
        }) = handle_server_settings_key(&nav, press(KeyCode::Enter))
        else {
            panic!("expected Nav(FieldList)");
        };
        assert_eq!(config.daemon.max_open_shards, 16);
        assert_eq!(selected, 1);
    }

    #[test]
    fn field_form_enter_with_an_invalid_value_stays_in_the_form_with_an_error() {
        let nav = ServerSettingsNav::FieldForm {
            config: Config::default(),
            selected: 1,
            field: FieldId::DaemonMaxOpenShards,
            text: "not-a-number".to_string(),
            error: None,
        };
        let ServerSettingsKeyOutcome::Nav(ServerSettingsNav::FieldForm { config, error, .. }) =
            handle_server_settings_key(&nav, press(KeyCode::Enter))
        else {
            panic!("expected Nav(FieldForm)");
        };
        assert_eq!(config.daemon.max_open_shards, 8, "unchanged from default");
        assert!(error.is_some());
    }

    #[test]
    fn ctrl_x_on_field_form_cancels_without_mutating_config() {
        let original = Config::default();
        let nav = ServerSettingsNav::FieldForm {
            config: original.clone(),
            selected: 1,
            field: FieldId::DaemonMaxOpenShards,
            text: "999".to_string(),
            error: None,
        };
        let outcome = handle_server_settings_key(&nav, ctrl_press('x'));
        assert_eq!(
            outcome,
            ServerSettingsKeyOutcome::Nav(ServerSettingsNav::FieldList {
                config: original,
                selected: 1,
                status: None,
            })
        );
    }

    #[test]
    fn saved_prompt_r_emits_a_restart_action() {
        let config = Config::default();
        let nav = ServerSettingsNav::SavedPrompt {
            config: config.clone(),
            selected: 3,
        };
        let outcome = handle_server_settings_key(&nav, press(KeyCode::Char('r')));
        assert_eq!(
            outcome,
            ServerSettingsKeyOutcome::Execute(ServerSettingsAction::Restart {
                config,
                selected: 3,
            })
        );
    }

    #[test]
    fn saved_prompt_any_other_key_dismisses_back_to_the_list() {
        let config = Config::default();
        let nav = ServerSettingsNav::SavedPrompt {
            config: config.clone(),
            selected: 3,
        };
        let outcome = handle_server_settings_key(&nav, press(KeyCode::Esc));
        assert_eq!(
            outcome,
            ServerSettingsKeyOutcome::Nav(ServerSettingsNav::FieldList {
                config,
                selected: 3,
                status: Some("saved".to_string()),
            })
        );
    }

    #[test]
    fn captures_all_keys_is_true_only_for_field_form() {
        assert!(!captures_all_keys(&ServerSettingsNav::default()));
        assert!(captures_all_keys(&ServerSettingsNav::FieldForm {
            config: Config::default(),
            selected: 0,
            field: FieldId::DaemonLogLevel,
            text: String::new(),
            error: None,
        }));
        assert!(!captures_all_keys(&ServerSettingsNav::SavedPrompt {
            config: Config::default(),
            selected: 0,
        }));
    }

    // ---- compute_server_settings_data / render --------------------------------

    #[test]
    fn compute_field_list_lists_all_sixteen_rows_in_order() {
        let nav = list_nav(Config::default(), 0);
        let ServerSettingsScreenData::FieldList { rows, .. } = compute_server_settings_data(&nav)
        else {
            panic!("expected FieldList");
        };
        assert_eq!(rows.len(), 16);
        assert_eq!(rows[0].0, "daemon.idle_shutdown_secs");
        assert_eq!(rows[15].0, "memory.consolidation_queue_threshold");
    }

    fn rendered_text(data: &ServerSettingsScreenData) -> String {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test backend terminal");
        terminal
            .draw(|frame| render_server_settings(frame, data))
            .expect("draw server settings screen");
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn render_field_list_shows_a_row_label() {
        let nav = list_nav(Config::default(), 0);
        let data = compute_server_settings_data(&nav);
        let text = rendered_text(&data);
        assert!(text.contains("daemon.idle_shutdown_secs"), "{text}");
    }

    #[test]
    fn render_saved_prompt_mentions_restart() {
        let text = rendered_text(&ServerSettingsScreenData::SavedPrompt);
        assert!(text.contains("restart"), "{text}");
    }
}
