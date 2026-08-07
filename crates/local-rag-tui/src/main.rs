//! `local-rag-tui` — terminal dashboard entry point (ADR-0008, spec 11 §7).
//!
//! T18-01's own scope is the skeleton only: the `ratatui`/`crossterm` event loop (raw mode +
//! alternate screen enter/restore, resize handling, a clean exit on panic) and this crate's
//! dependency/dist wiring. No screen has real content yet — Status begins at T18-02.

use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use ratatui::DefaultTerminal;
use ratatui::widgets::Paragraph;
use std::process::ExitCode;

const BIN: &str = "local-rag-tui";

fn main() -> ExitCode {
    if matches!(
        std::env::args().nth(1).as_deref(),
        Some("version" | "--version" | "-V")
    ) {
        println!("{}", local_rag_core::version_line(BIN));
        return ExitCode::SUCCESS;
    }

    match ratatui::run(run_app) {
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
fn run_app(terminal: &mut DefaultTerminal) -> std::io::Result<()> {
    loop {
        // `Terminal::draw` calls `Terminal::autoresize` on every call, so a terminal resize needs
        // no dedicated handling beyond looping back to `draw()` after any event — including
        // `Event::Resize` itself.
        terminal.draw(|frame| {
            frame.render_widget(
                Paragraph::new("local-rag-tui — coming soon (press q to quit)"),
                frame.area(),
            );
        })?;

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
