//! The Status screen (spec 11 §7, T18-02): daemon identity/mode + durable counts, computed
//! independently of each other and independently of any render target — [`compute_status_data`]
//! does all the I/O, [`render_status`] does none.
//!
//! `DaemonStatus`/[`probe_daemon`] is an independent reimplementation of
//! `local_rag::cli::status::StatusReport`/`compute_status` (that type/function is private to the
//! `local-rag` binary target — `crates/local-rag/src/lib.rs` exports only `pub mod daemon`), over
//! the exact same public primitives, verified line-by-line against the original.
//!
//! # Why durable counts never silently apply a pending migration
//!
//! `StateDb::open` applies pending migrations as a side effect of opening (spec 02 §4.1's
//! open → migrate → serve ordering) — the same concern `cli::doctor`'s own module doc raises
//! about its own read-only report. Status is this dashboard's home screen and the one screen
//! whose entire purpose is "let me look without touching anything" — so, like `doctor`, it probes
//! `StateDb::diagnose_versions` (a raw read-only connection, never `StateDb::open`) first, and
//! opens `StateDb::open` for the real counts only once that confirms the store is `Applied` with
//! an empty `pending` list — at which point `open` is a genuine no-op with respect to migration.
//! `cli stats` (this module's closest CLI cousin) does not take this precaution, because its own
//! card is not framed as "offline-safe" the way this one explicitly is.

use std::path::Path;
use std::time::Duration;

use local_rag::daemon::{StoreLockFileState, gitroot, read_store_lock_file};
use local_rag_core::paths::StoreLayout;
use local_rag_core::process::pid_exists;
use local_rag_store::{
    CandidateCountRow, MemoryCountRow, ProjectionStateRow, RequestRoot, Resolution,
    memory_entry_counts, pending_candidate_counts, projection_state, resolve,
};

use crate::store_read::open_read_offline_safe;

/// Best-effort daemon identity/mode. See the module doc for why this is redefined rather than
/// imported from `local_rag::cli::status`.
#[derive(Debug, Clone, PartialEq)]
pub enum DaemonStatus {
    NotRunning,
    Starting {
        pid: u32,
    },
    Running {
        pid: u32,
        instance_uuid: String,
        daemon_version: String,
        daemon_mode: String,
        socket_path: String,
        started_at: i64,
        ready_at: Option<i64>,
    },
}

/// A resolved worktree's projection status, or `None` in `projection` if the row does not exist
/// yet (a worktree resolved but never indexed).
#[derive(Debug, Clone, PartialEq)]
pub struct WorktreeProjection {
    pub repo_id: String,
    pub worktree_id: String,
    pub projection: Option<ProjectionStateRow>,
}

/// Durable counts read directly from `state.sqlite` — `Unavailable` only when the store cannot
/// honestly be read without either applying a migration or opening a file this build does not
/// recognize (see module doc); an *empty* store (no rows) is `Available` with empty vectors, not
/// `Unavailable`.
#[derive(Debug, Clone, PartialEq)]
pub enum DurableCounts {
    Unavailable {
        reason: String,
    },
    Available {
        entries_by_kind_state: Vec<MemoryCountRow>,
        pending_candidates_by_state: Vec<CandidateCountRow>,
        /// `None`: the current directory is outside any registered repository/worktree — a
        /// realistic case for a dashboard launched from anywhere, not an error. Boxed: `Unavailable`
        /// is otherwise far smaller than `Available` (`clippy::large_enum_variant`) — `WorktreeProjection`
        /// embeds a full `ProjectionStateRow`.
        worktree: Option<Box<WorktreeProjection>>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct StatusScreenData {
    pub daemon: DaemonStatus,
    pub durable: DurableCounts,
}

/// Probe `store.lock` and, if `ready`, the live socket — mirrors
/// `local_rag::cli::status::compute_status` exactly.
pub fn probe_daemon(layout: &StoreLayout, probe_timeout: Duration) -> DaemonStatus {
    let info = match read_store_lock_file(layout) {
        StoreLockFileState::Absent | StoreLockFileState::Corrupt => {
            return DaemonStatus::NotRunning;
        }
        StoreLockFileState::Parsed(info) => info,
    };

    if !info.ready {
        return if pid_exists(info.pid) {
            DaemonStatus::Starting { pid: info.pid }
        } else {
            DaemonStatus::NotRunning
        };
    }

    if !pid_exists(info.pid) {
        return DaemonStatus::NotRunning;
    }

    #[cfg(unix)]
    {
        let welcome = local_rag::daemon::fetch_welcome(&layout.socket_path(), probe_timeout);
        match welcome {
            Some(w) if w.store_instance_uuid == info.instance_uuid => DaemonStatus::Running {
                pid: info.pid,
                instance_uuid: info.instance_uuid,
                daemon_version: w.daemon_version,
                daemon_mode: w.mode,
                socket_path: info
                    .socket_path
                    .unwrap_or_else(|| layout.socket_path().display().to_string()),
                started_at: info.started_at,
                ready_at: info.ready_at,
            },
            _ => DaemonStatus::NotRunning,
        }
    }
    #[cfg(not(unix))]
    {
        DaemonStatus::NotRunning
    }
}

/// Read durable counts, never applying a pending migration (module doc). `cwd` is git-probed to
/// resolve worktree identity, mirroring `cli stats` — pass the real `std::env::current_dir()` in
/// production, an arbitrary path in a fixture test.
pub fn read_durable_counts(layout: &StoreLayout, cwd: &Path) -> DurableCounts {
    let conn = match open_read_offline_safe(layout) {
        Ok(c) => c,
        Err(reason) => return DurableCounts::Unavailable { reason },
    };

    let entries_by_kind_state = match memory_entry_counts(&conn) {
        Ok(v) => v,
        Err(e) => {
            return DurableCounts::Unavailable {
                reason: format!("could not read memory counts: {e}"),
            };
        }
    };
    let pending_candidates_by_state = match pending_candidate_counts(&conn) {
        Ok(v) => v,
        Err(e) => {
            return DurableCounts::Unavailable {
                reason: format!("could not read candidate counts: {e}"),
            };
        }
    };

    let facts = gitroot::probe(cwd);
    let worktree = match resolve(
        &conn,
        &RequestRoot {
            worktree_root: facts,
            repo_hint: None,
        },
    ) {
        Ok(Resolution::Resolved {
            repo_id,
            worktree_id,
        }) => {
            let projection = projection_state(&conn, &worktree_id).unwrap_or(None);
            Some(Box::new(WorktreeProjection {
                repo_id,
                worktree_id,
                projection,
            }))
        }
        Ok(Resolution::GlobalOnly | Resolution::Ambiguous { .. }) | Err(_) => None,
    };

    DurableCounts::Available {
        entries_by_kind_state,
        pending_candidates_by_state,
        worktree,
    }
}

/// Compose both halves — what `run_app` (and every test) actually calls.
pub fn compute_status_data(
    layout: &StoreLayout,
    cwd: &Path,
    probe_timeout: Duration,
) -> StatusScreenData {
    StatusScreenData {
        daemon: probe_daemon(layout, probe_timeout),
        durable: read_durable_counts(layout, cwd),
    }
}

/// Pure render — no I/O, `TestBackend`-testable without a daemon or a store.
pub fn render_status(frame: &mut ratatui::Frame, data: &StatusScreenData) {
    use ratatui::layout::{Constraint, Layout};
    use ratatui::text::Line;
    use ratatui::widgets::{Block, Paragraph, Row, Table};

    let [daemon_area, counts_area] =
        Layout::vertical([Constraint::Length(7), Constraint::Min(0)]).areas(frame.area());

    let daemon_lines: Vec<Line> = match &data.daemon {
        DaemonStatus::NotRunning => vec![Line::from("daemon: not running")],
        DaemonStatus::Starting { pid } => {
            vec![Line::from(format!("daemon: starting (pid {pid})"))]
        }
        DaemonStatus::Running {
            pid,
            instance_uuid,
            daemon_version,
            daemon_mode,
            socket_path,
            ..
        } => vec![
            Line::from(format!("daemon: running (mode {daemon_mode})")),
            Line::from(format!("pid: {pid}")),
            Line::from(format!("instance_uuid: {instance_uuid}")),
            Line::from(format!("daemon_version: {daemon_version}")),
            Line::from(format!("socket_path: {socket_path}")),
        ],
    };
    frame.render_widget(
        Paragraph::new(daemon_lines).block(Block::bordered().title("Status")),
        daemon_area,
    );

    match &data.durable {
        DurableCounts::Unavailable { reason } => {
            frame.render_widget(
                Paragraph::new(reason.as_str()).block(Block::bordered().title("Durable counts")),
                counts_area,
            );
        }
        DurableCounts::Available {
            entries_by_kind_state,
            pending_candidates_by_state,
            worktree,
        } => {
            let mut rows = Vec::new();
            if entries_by_kind_state.is_empty() {
                rows.push(Row::new(["memory entries".to_string(), "none".to_string()]));
            }
            for r in entries_by_kind_state {
                rows.push(Row::new([
                    format!("memory entries {}/{}", r.kind.as_str(), r.state.as_str()),
                    r.count.to_string(),
                ]));
            }
            if pending_candidates_by_state.is_empty() {
                rows.push(Row::new([
                    "pending candidates".to_string(),
                    "none".to_string(),
                ]));
            }
            for r in pending_candidates_by_state {
                rows.push(Row::new([
                    format!("pending candidates {}", r.state.as_str()),
                    r.count.to_string(),
                ]));
            }
            match worktree {
                Some(w) => {
                    rows.push(Row::new([
                        "worktree".to_string(),
                        format!("repo {} / worktree {}", w.repo_id, w.worktree_id),
                    ]));
                    match &w.projection {
                        Some(p) => rows.push(Row::new([
                            "projection status".to_string(),
                            p.status.as_str().to_string(),
                        ])),
                        None => rows.push(Row::new([
                            "projection status".to_string(),
                            "no projection state yet".to_string(),
                        ])),
                    }
                }
                None => rows.push(Row::new([
                    "worktree".to_string(),
                    "(unresolved)".to_string(),
                ])),
            }

            let table = Table::new(rows, [Constraint::Length(30), Constraint::Min(0)])
                .block(Block::bordered().title("Durable counts"));
            frame.render_widget(table, counts_area);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn rendered_text(data: &StatusScreenData) -> String {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test backend terminal");
        terminal
            .draw(|frame| render_status(frame, data))
            .expect("draw status screen");
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn renders_not_running_state() {
        let data = StatusScreenData {
            daemon: DaemonStatus::NotRunning,
            durable: DurableCounts::Available {
                entries_by_kind_state: vec![],
                pending_candidates_by_state: vec![],
                worktree: None,
            },
        };
        let content = rendered_text(&data);
        assert!(content.contains("not running"), "{content}");
        assert!(content.contains("(unresolved)"), "{content}");
    }

    #[test]
    fn renders_running_state_with_durable_counts() {
        let data = StatusScreenData {
            daemon: DaemonStatus::Running {
                pid: 4242,
                instance_uuid: "inst-abc".to_string(),
                daemon_version: "0.0.0".to_string(),
                daemon_mode: "normal".to_string(),
                socket_path: "/tmp/daemon.sock".to_string(),
                started_at: 1_000,
                ready_at: Some(1_100),
            },
            durable: DurableCounts::Available {
                entries_by_kind_state: vec![MemoryCountRow {
                    kind: local_rag_store::MemoryKind::Fact,
                    state: local_rag_store::MemoryState::Active,
                    count: 3,
                }],
                pending_candidates_by_state: vec![],
                worktree: None,
            },
        };
        let content = rendered_text(&data);
        assert!(content.contains("4242"), "{content}");
        assert!(content.contains("inst-abc"), "{content}");
        assert!(content.contains("normal"), "{content}");
        assert!(content.contains("fact/active"), "{content}");
    }

    #[test]
    fn renders_unavailable_reason_instead_of_crashing_on_missing_counts() {
        let data = StatusScreenData {
            daemon: DaemonStatus::NotRunning,
            durable: DurableCounts::Unavailable {
                reason: "1 migration(s) pending; run `local-rag serve`/`index` first".to_string(),
            },
        };
        let content = rendered_text(&data);
        assert!(content.contains("migration"), "{content}");
    }
}
