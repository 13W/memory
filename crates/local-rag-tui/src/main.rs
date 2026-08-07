//! `local-rag-tui` — terminal dashboard entry point (ADR-0008, spec 11 §7).
//!
//! T18-01 built the skeleton: the `ratatui`/`crossterm` event loop (raw mode + alternate screen
//! enter/restore, resize handling, a clean exit on panic) and this crate's dependency/dist wiring.
//! T18-02 adds the first real screen — Status (`local_rag_tui::status`) — replacing the
//! placeholder paragraph below.

use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use local_rag_core::paths::{StoreLayout, SystemEnv};
use local_rag_tui::status::{compute_status_data, render_status};
use ratatui::DefaultTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

const BIN: &str = "local-rag-tui";

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
/// doc comment) — this function's only job is drawing frames and reacting to input until the user
/// asks to quit; `ratatui::run` restores the terminal again on return here, panic or not.
fn run_app(terminal: &mut DefaultTerminal, layout: &StoreLayout) -> std::io::Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    loop {
        // `Terminal::draw` calls `Terminal::autoresize` on every call, so a terminal resize needs
        // no dedicated handling beyond looping back to `draw()` after any event — including
        // `Event::Resize` itself. Recomputing on every event (not just a real keymap change) keeps
        // the screen live without a tick timer/async — cheap when the daemon is dead (file reads
        // only) and bounded by `LIVENESS_PROBE_TIMEOUT_MS` when it is alive.
        let data = compute_status_data(
            layout,
            &cwd,
            Duration::from_millis(local_rag::daemon::LIVENESS_PROBE_TIMEOUT_MS),
        );
        terminal.draw(|frame| render_status(frame, &data))?;

        if should_quit(event::read()?) {
            return Ok(());
        }
    }
}

/// `q`/`Esc`, or `Ctrl+C` — deliberately not any other binding; no screen exists yet to reserve
/// one for (T18-02+ extends this once a real keymap exists).
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
}
