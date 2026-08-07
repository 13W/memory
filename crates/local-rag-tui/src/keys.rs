//! Shared crossterm key-handling helpers used by every write-capable screen's own pure
//! `handle_*_key` — extracted here at T18-07 once a third (byte-identical) occurrence of `step`
//! appeared (`repositories.rs`, `memory.rs`, `repo_settings.rs` each carried their own copy),
//! this crate's own documented "wait for a genuine third occurrence" convention (see
//! `repo_settings.rs`'s prior doc comment, which invoked and deferred it twice before).
//!
//! `is_ctrl_x` (duplicated in `memory.rs`/`repo_settings.rs` for their own cancel chord) is
//! generalized to `is_ctrl(key, c)` here rather than relocated as-is: T18-07's own Server
//! Settings screen needs a second control chord (`Ctrl+S`, save) alongside the existing
//! `Ctrl+X` (cancel), and a hard-coded `is_ctrl_x` cannot serve both without a near-duplicate
//! `is_ctrl_s` sibling — the shared helper takes the target char instead.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Move `selected` one row up (`down = false`) or down (`down = true`), clamped to
/// `[0, len - 1]`. `len == 0` always yields `0`.
pub(crate) fn step(selected: usize, down: bool, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    if down {
        (selected + 1).min(len - 1)
    } else {
        selected.saturating_sub(1)
    }
}

/// `true` for `Ctrl+<c>`, case-insensitively (crossterm reports the shifted letter's own case,
/// not a separate shift flag, for `Ctrl`-chords on most terminals).
pub(crate) fn is_ctrl(key: &KeyEvent, c: char) -> bool {
    matches!(key.code, KeyCode::Char(ch) if ch.eq_ignore_ascii_case(&c))
        && key.modifiers.contains(KeyModifiers::CONTROL)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState};

    fn ctrl_key(c: char) -> KeyEvent {
        KeyEvent {
            code: KeyCode::Char(c),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn plain_key(c: char) -> KeyEvent {
        KeyEvent {
            code: KeyCode::Char(c),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn step_clamps_at_both_ends() {
        assert_eq!(step(0, false, 3), 0);
        assert_eq!(step(2, true, 3), 2);
        assert_eq!(step(0, true, 3), 1);
        assert_eq!(step(1, false, 3), 0);
    }

    #[test]
    fn step_on_empty_list_is_always_zero() {
        assert_eq!(step(0, true, 0), 0);
        assert_eq!(step(0, false, 0), 0);
    }

    #[test]
    fn is_ctrl_matches_the_requested_char_case_insensitively() {
        assert!(is_ctrl(&ctrl_key('x'), 'x'));
        assert!(is_ctrl(&ctrl_key('X'), 'x'));
        assert!(is_ctrl(&ctrl_key('s'), 's'));
    }

    #[test]
    fn is_ctrl_rejects_other_chars_and_unmodified_keys() {
        assert!(!is_ctrl(&ctrl_key('x'), 's'));
        assert!(!is_ctrl(&plain_key('x'), 'x'));
    }
}
