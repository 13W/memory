//! `local-rag-tui` — terminal dashboard entry point (ADR-0008, spec 11 §7).
//!
//! T18-01 built the skeleton: the `ratatui`/`crossterm` event loop (raw mode + alternate screen
//! enter/restore, resize handling, a clean exit on panic) and this crate's dependency/dist wiring.
//! T18-02 added the first real screen — Status (`local_rag_tui::status`). T18-03 adds Repositories
//! (`local_rag_tui::repositories`) and this dashboard's first screen-switching scheme: digit keys
//! `1`..`SCREENS.len()` select a screen directly (see [`Screen`]/[`screen_for_key`]); `Enter`/
//! `Backspace` drill into/out of Repositories' own repo → worktree → worktree-detail navigation;
//! `q`/`Esc`/`Ctrl+C` always quit, unconditionally, at any screen or drill-down level. T18-05 adds
//! Memory mutations (`local_rag_tui::memory`'s own `MemoryKeyOutcome::{Nav, Execute}`) and this
//! loop's one narrow exception to "global keys are never delegated to the screen's own handler":
//! [`is_text_entry_key`], consulted only while `memory::captures_all_keys(&memory_nav)` — see that
//! function's own doc and `memory.rs`'s module doc for the full rationale.

use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use local_rag_core::paths::{StoreLayout, SystemEnv};
use local_rag_tui::memory::{self, MemoryNav};
use local_rag_tui::repositories::{self, RepositoriesNav};
use local_rag_tui::status::{compute_status_data, render_status};
use ratatui::DefaultTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

const BIN: &str = "local-rag-tui";

/// Which top-level screen is active. Position i (0-based) in [`SCREENS`] is selected by digit key
/// `i + 1` (see [`screen_for_key`]) — ADR-0008 names six screens total (Status, Logs, Memory,
/// Repositories, Repo Settings, Server Settings); each later T18-0N card appends one variant here
/// plus one [`SCREENS`] entry, no dispatcher rewrite. Digit keys were chosen over `Tab`-cycling for
/// direct addressability and because they never collide with the `Up`/`Down`/`Enter`/`Backspace`
/// keys Repositories/Memory need for their own drill-down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    Status,
    Repositories,
    Memory,
}

const SCREENS: [Screen; 3] = [Screen::Status, Screen::Repositories, Screen::Memory];

fn main() -> ExitCode {
    if matches!(
        std::env::args().nth(1).as_deref(),
        Some("version" | "--version" | "-V")
    ) {
        println!("{}", local_rag_core::version_line(BIN));
        return ExitCode::SUCCESS;
    }

    let layout = match StoreLayout::resolve(&SystemEnv) {
        Ok(layout) => layout,
        Err(e) => {
            eprintln!("{BIN}: could not resolve the store directory: {e}");
            return ExitCode::FAILURE;
        }
    };

    match ratatui::run(|terminal| run_app(terminal, &layout)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{BIN}: {err}");
            ExitCode::FAILURE
        }
    }
}

/// The event loop itself. `ratatui::run` already installed raw mode, the alternate screen, and a
/// panic hook that restores both before delegating to the previously-installed hook (see its own
/// doc comment) — this function's own job is drawing the active screen's frame and routing input
/// until the user asks to quit; `ratatui::run` restores the terminal again on return here, panic
/// or not.
///
/// `data` is computed **before** reading the next event, in the same iteration it is drawn and
/// handled in — the same WYSIWYG discipline Status already had (recompute every iteration), now
/// also needed so `Enter` resolves "which row is selected" against the exact data just drawn, not
/// a stale prior frame's.
fn run_app(terminal: &mut DefaultTerminal, layout: &StoreLayout) -> std::io::Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut screen = Screen::Status;
    let mut repositories_nav = RepositoriesNav::default();
    let mut memory_nav = MemoryNav::default();

    loop {
        // `Terminal::draw` calls `Terminal::autoresize` on every call, so a terminal resize needs
        // no dedicated handling beyond looping back to `draw()` after any event — including
        // `Event::Resize` itself.
        let (ev, suppress_global) = match screen {
            Screen::Status => {
                let data = compute_status_data(
                    layout,
                    &cwd,
                    Duration::from_millis(local_rag::daemon::LIVENESS_PROBE_TIMEOUT_MS),
                );
                terminal.draw(|frame| render_status(frame, &data))?;
                (event::read()?, false)
            }
            Screen::Repositories => {
                let data = repositories::compute_repositories_data(layout, &repositories_nav);
                terminal.draw(|frame| repositories::render_repositories(frame, &data))?;
                let ev = event::read()?;
                // Global keys (quit, digit screen-switch) are never delegated to the screen's own
                // handler — checked below via `should_quit`/`screen_for_key` regardless, but
                // `repositories_nav` itself must not advance on e.g. a digit keypress.
                if !is_global_key(ev.clone()) {
                    repositories_nav =
                        repositories::handle_repositories_key(&repositories_nav, &data, ev.clone());
                }
                (ev, false)
            }
            Screen::Memory => {
                let data = memory::compute_memory_data(layout, &cwd, &memory_nav);
                terminal.draw(|frame| memory::render_memory(frame, &data))?;
                let ev = event::read()?;
                // T18-05's `EditForm` must receive bare `q`/digits as buffer content, not global
                // quit/screen-switch — the one narrow, documented exception to "global keys are
                // never delegated to the screen's own handler" (see `memory.rs`'s own module doc,
                // "The global-quit carve-out for text entry"). `Ctrl+C`/`Esc` are never produced as
                // typed content, so they are deliberately excluded and keep quitting unconditionally.
                let force_local = memory::captures_all_keys(&memory_nav) && is_text_entry_key(&ev);
                if force_local || !is_global_key(ev.clone()) {
                    match memory::handle_memory_key(&memory_nav, &data, ev.clone()) {
                        memory::MemoryKeyOutcome::Nav(next) => memory_nav = next,
                        memory::MemoryKeyOutcome::Execute(action) => {
                            memory_nav = memory::execute_memory_action(layout, action);
                        }
                    }
                }
                (ev, force_local)
            }
        };

        if suppress_global {
            continue;
        }
        if should_quit(ev.clone()) {
            return Ok(());
        }
        if let Some(next) = screen_for_key(ev) {
            screen = next;
        }
    }
}

/// `true` only for a bare (no `Ctrl`) literal `q` or ASCII digit key-press — the exact, narrow set
/// that `EditForm` must receive as buffer content rather than global quit/screen-switch. See
/// `run_app`'s own Memory-screen branch and `memory.rs`'s module doc for the full rationale.
fn is_text_entry_key(ev: &Event) -> bool {
    let Event::Key(key) = ev else {
        return false;
    };
    if key.kind != event::KeyEventKind::Press {
        return false;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return false;
    }
    matches!(key.code, KeyCode::Char(c) if c == 'q' || c.is_ascii_digit())
}

/// Digit keys `1`..`SCREENS.len()` jump directly to a screen; any other key (including `0` and
/// digits past the populated range) selects nothing. See [`Screen`]'s own doc for the rationale.
fn screen_for_key(ev: Event) -> Option<Screen> {
    let Event::Key(key) = ev else {
        return None;
    };
    if key.kind != event::KeyEventKind::Press {
        return None;
    }
    let KeyCode::Char(c) = key.code else {
        return None;
    };
    let digit = c.to_digit(10)?;
    if digit == 0 {
        return None;
    }
    SCREENS.get(digit as usize - 1).copied()
}

/// Keys `run_app` handles itself and never delegates to a screen's own handler — quit and digit
/// screen-switches — checked first so e.g. `1`/`2` never reach
/// [`repositories::handle_repositories_key`].
fn is_global_key(ev: Event) -> bool {
    should_quit(ev.clone()) || screen_for_key(ev).is_some()
}

/// `q`/`Esc`, or `Ctrl+C` — always quits, unconditionally, regardless of which screen or
/// drill-down level is active. T18-03 gives Repositories its own `Enter`/`Backspace` drill
/// in/out keys instead of overloading `Esc`, specifically so quit never has to become
/// context-sensitive and this function's own contract (and its 5 tests) stays untouched.
fn should_quit(ev: Event) -> bool {
    let Event::Key(key) = ev else {
        return false;
    };
    if key.kind != event::KeyEventKind::Press {
        return false;
    }
    matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
        || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyEventKind, KeyEventState};

    fn press(code: KeyCode) -> Event {
        Event::Key(KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })
    }

    #[test]
    fn q_and_esc_quit() {
        assert!(should_quit(press(KeyCode::Char('q'))));
        assert!(should_quit(press(KeyCode::Esc)));
    }

    #[test]
    fn ctrl_c_quits() {
        let mut ev = press(KeyCode::Char('c'));
        if let Event::Key(key) = &mut ev {
            key.modifiers = KeyModifiers::CONTROL;
        }
        assert!(should_quit(ev));
    }

    #[test]
    fn plain_c_does_not_quit() {
        assert!(!should_quit(press(KeyCode::Char('c'))));
    }

    #[test]
    fn key_release_does_not_quit() {
        let mut ev = press(KeyCode::Char('q'));
        if let Event::Key(key) = &mut ev {
            key.kind = KeyEventKind::Release;
        }
        assert!(!should_quit(ev));
    }

    #[test]
    fn non_key_events_do_not_quit() {
        assert!(!should_quit(Event::FocusGained));
    }

    #[test]
    fn digit_keys_select_screens() {
        assert_eq!(
            screen_for_key(press(KeyCode::Char('1'))),
            Some(Screen::Status)
        );
        assert_eq!(
            screen_for_key(press(KeyCode::Char('2'))),
            Some(Screen::Repositories)
        );
        assert_eq!(
            screen_for_key(press(KeyCode::Char('3'))),
            Some(Screen::Memory)
        );
    }

    #[test]
    fn digit_zero_and_out_of_range_select_nothing() {
        assert_eq!(screen_for_key(press(KeyCode::Char('0'))), None);
        assert_eq!(screen_for_key(press(KeyCode::Char('9'))), None);
    }

    #[test]
    fn navigation_keys_are_not_global() {
        assert!(!is_global_key(press(KeyCode::Up)));
        assert!(!is_global_key(press(KeyCode::Down)));
        assert!(!is_global_key(press(KeyCode::Enter)));
        assert!(!is_global_key(press(KeyCode::Backspace)));
    }

    #[test]
    fn quit_and_digit_keys_are_global() {
        assert!(is_global_key(press(KeyCode::Char('q'))));
        assert!(is_global_key(press(KeyCode::Esc)));
        assert!(is_global_key(press(KeyCode::Char('1'))));
        assert!(is_global_key(press(KeyCode::Char('2'))));
    }

    #[test]
    fn is_text_entry_key_accepts_bare_q_and_digits_only() {
        assert!(is_text_entry_key(&press(KeyCode::Char('q'))));
        assert!(is_text_entry_key(&press(KeyCode::Char('7'))));
        assert!(!is_text_entry_key(&press(KeyCode::Char('a'))));
        assert!(!is_text_entry_key(&press(KeyCode::Esc)));
    }

    #[test]
    fn is_text_entry_key_rejects_control_modified_and_released_keys() {
        let mut ctrl_q = press(KeyCode::Char('q'));
        if let Event::Key(key) = &mut ctrl_q {
            key.modifiers = KeyModifiers::CONTROL;
        }
        assert!(!is_text_entry_key(&ctrl_q));

        let mut released = press(KeyCode::Char('q'));
        if let Event::Key(key) = &mut released {
            key.kind = KeyEventKind::Release;
        }
        assert!(!is_text_entry_key(&released));

        assert!(!is_text_entry_key(&Event::FocusGained));
    }
}
