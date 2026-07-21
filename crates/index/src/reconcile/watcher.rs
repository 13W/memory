//! The filesystem-watcher adapter (spec 06 §1) — T05-04, the "watcher = hint" edge.
//!
//! The watcher only *schedules* work; the authoritative scan+build is the truth
//! (spec 06 principle, `[FIXED]`). To keep the scheduler deterministically testable
//! despite a live, timing-dependent OS watcher, this module splits the concern:
//!
//! - [`WatchEvent`] is a small, `notify`-agnostic event, and
//!   [`watch_event_to_trigger`] is a **pure** mapping from it to a
//!   [`TriggerKind`] — fully unit-tested, no filesystem, no clock.
//! - [`spawn_watcher`] is the thin live wrapper that lowers a `notify::Event` to a
//!   [`WatchEvent`] and feeds the driver's trigger channel. It is exercised by the
//!   daemon (group 15), not by the deterministic test suite (a real watcher's event
//!   timing is not reproducible), so it carries no automated CI test.

use std::path::{Path, PathBuf};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;

use super::schedule::TriggerKind;

/// A watcher signal, abstracted away from `notify`'s event types so the mapping to
/// a [`TriggerKind`] can be tested without constructing backend events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchEvent {
    /// A path was created, modified, or removed.
    Path(PathBuf),
    /// The backend reported a possible loss of events (queue overflow / forced
    /// rescan). Must escalate to a mandatory strict reconcile (spec 06 §1 `[FIXED]`).
    Rescan,
}

/// Map a [`WatchEvent`] to the [`TriggerKind`] it schedules (spec 06 §1). Pure.
///
/// - `Rescan` → [`WatcherOverflow`](TriggerKind::WatcherOverflow) (strict, always).
/// - A path under a `.git` directory on a **git** worktree →
///   [`GitHead`](TriggerKind::GitHead); on a non-git worktree there are no git
///   semantics (spec 06 §6), so it is an ordinary [`FsChange`](TriggerKind::FsChange).
/// - Any other path → `FsChange`.
///
/// Never returns `None` today (every event schedules *something*); the `Option`
/// leaves room for a future "ignore" verdict without a signature change.
pub fn watch_event_to_trigger(event: &WatchEvent, is_git: bool) -> Option<TriggerKind> {
    match event {
        WatchEvent::Rescan => Some(TriggerKind::WatcherOverflow),
        WatchEvent::Path(path) => {
            if is_git && path_touches_git_meta(path) {
                Some(TriggerKind::GitHead)
            } else {
                Some(TriggerKind::FsChange)
            }
        }
    }
}

/// Whether any path component is `.git` (the git metadata directory / file).
fn path_touches_git_meta(path: &Path) -> bool {
    path.components().any(|c| c.as_os_str() == ".git")
}

/// Start a recursive live watcher on `root`, translating `notify` events into
/// [`TriggerKind`]s on `tx` (spec 06 §1). Best-effort: a dropped `try_send` (full
/// queue) or a watcher error is tolerated because reconcile — not the event stream —
/// is the source of truth, and the periodic/strict backstops recover any loss.
///
/// The returned watcher must be kept alive for events to flow; dropping it stops
/// watching. Not covered by the deterministic suite (see the module doc).
pub fn spawn_watcher(
    root: &Path,
    is_git: bool,
    tx: mpsc::Sender<TriggerKind>,
) -> notify::Result<RecommendedWatcher> {
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let event = match res {
            Ok(event) => event,
            // A watcher-level error means events may have been missed → strict rescan.
            Err(_) => {
                let _ = tx.try_send(TriggerKind::WatcherOverflow);
                return;
            }
        };
        let watch_event = if event.need_rescan() {
            WatchEvent::Rescan
        } else if matches!(event.kind, EventKind::Access(_)) {
            // Reads never change the tree.
            return;
        } else if let Some(path) = event.paths.into_iter().next() {
            WatchEvent::Path(path)
        } else {
            return;
        };
        if let Some(kind) = watch_event_to_trigger(&watch_event, is_git) {
            let _ = tx.try_send(kind);
        }
    })?;
    watcher.watch(root, RecursiveMode::Recursive)?;
    Ok(watcher)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rescan_maps_to_overflow_regardless_of_git() {
        assert_eq!(
            watch_event_to_trigger(&WatchEvent::Rescan, true),
            Some(TriggerKind::WatcherOverflow),
        );
        assert_eq!(
            watch_event_to_trigger(&WatchEvent::Rescan, false),
            Some(TriggerKind::WatcherOverflow),
        );
    }

    #[test]
    fn ordinary_path_maps_to_fs_change() {
        let ev = WatchEvent::Path(PathBuf::from("src/lib.rs"));
        assert_eq!(
            watch_event_to_trigger(&ev, true),
            Some(TriggerKind::FsChange),
        );
        assert_eq!(
            watch_event_to_trigger(&ev, false),
            Some(TriggerKind::FsChange),
        );
    }

    #[test]
    fn git_meta_path_is_a_git_trigger_only_on_git_worktrees() {
        let ev = WatchEvent::Path(PathBuf::from(".git/HEAD"));
        assert_eq!(
            watch_event_to_trigger(&ev, true),
            Some(TriggerKind::GitHead),
            "a git worktree's .git change is a git trigger",
        );
        assert_eq!(
            watch_event_to_trigger(&ev, false),
            Some(TriggerKind::FsChange),
            "a non-git worktree has no git semantics (spec 06 §6)",
        );
    }

    #[test]
    fn nested_git_meta_path_is_detected() {
        let ev = WatchEvent::Path(PathBuf::from("sub/.git/refs/heads/main"));
        assert_eq!(
            watch_event_to_trigger(&ev, true),
            Some(TriggerKind::GitHead),
        );
    }
}
