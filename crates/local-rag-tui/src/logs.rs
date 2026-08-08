//! The Logs screen (spec 11 §7, T18-09): recent per-call telemetry (newest first) plus per-tool
//! aggregates, backed entirely by [`crate::admin_client::AdminPoller`]. No I/O of its own —
//! [`render_logs`] is pure (`TestBackend`-testable, mirroring every other screen's own
//! `render_*`), and `now_ms` is injected by the caller (`main.rs`) rather than read internally, so
//! the "{N}s ago" time column is deterministic under test.

use crate::admin_client::LogsSnapshot;

/// Pure render — no I/O, `TestBackend`-testable without a daemon. `now_ms`: epoch millis "now",
/// for the relative time column (`admin/tail_calls`'s own `at_ms` is an absolute epoch
/// timestamp — a live wall-clock reading, not worth adding a crate-local clock helper for the
/// three lines of call-site code this needs).
pub fn render_logs(frame: &mut ratatui::Frame, snapshot: &LogsSnapshot, now_ms: i64) {
    use ratatui::layout::{Constraint, Layout};
    use ratatui::widgets::{Block, Paragraph, Row, Table};

    let [calls_area, tools_area] =
        Layout::vertical([Constraint::Percentage(65), Constraint::Min(0)]).areas(frame.area());

    match snapshot {
        LogsSnapshot::Unreachable => {
            frame.render_widget(
                Paragraph::new("daemon: not running").block(Block::bordered().title("Logs")),
                calls_area,
            );
            frame.render_widget(Block::bordered().title("Tool stats"), tools_area);
        }
        LogsSnapshot::PollerStopped => {
            frame.render_widget(
                Paragraph::new("admin poller stopped unexpectedly (see logs)")
                    .block(Block::bordered().title("Logs")),
                calls_area,
            );
            frame.render_widget(Block::bordered().title("Tool stats"), tools_area);
        }
        LogsSnapshot::Connected { calls, tools } => {
            let mut call_rows = Vec::new();
            if calls.is_empty() {
                call_rows.push(Row::new([
                    String::new(),
                    String::new(),
                    "no calls yet".to_string(),
                ]));
            }
            for call in calls.iter().rev() {
                let elapsed_s = (now_ms - call.at_ms).max(0) / 1000;
                call_rows.push(Row::new([
                    format!("{elapsed_s}s ago"),
                    call.source.clone(),
                    call.tool.clone(),
                    format!("{}ms", call.duration_ms),
                    format!("{}/{}", call.bytes_in, call.bytes_out),
                    if call.is_error { "error" } else { "ok" }.to_string(),
                ]));
            }
            let calls_table = Table::new(
                call_rows,
                [
                    Constraint::Length(10),
                    Constraint::Length(20),
                    Constraint::Min(10),
                    Constraint::Length(10),
                    Constraint::Length(14),
                    Constraint::Length(7),
                ],
            )
            .header(Row::new([
                "time", "source", "tool", "duration", "bytes", "status",
            ]))
            .block(Block::bordered().title(format!("Logs ({} calls)", calls.len())));
            frame.render_widget(calls_table, calls_area);

            let mut tool_rows = Vec::new();
            if tools.is_empty() {
                tool_rows.push(Row::new(["no tool stats yet".to_string()]));
            }
            for tool in tools {
                tool_rows.push(Row::new([
                    tool.tool.clone(),
                    tool.calls.to_string(),
                    tool.errors.to_string(),
                    format!("{}/{}", tool.bytes_in, tool.bytes_out),
                    format!("{}ms", tool.total_ms),
                ]));
            }
            let tools_table = Table::new(
                tool_rows,
                [
                    Constraint::Min(10),
                    Constraint::Length(8),
                    Constraint::Length(8),
                    Constraint::Length(14),
                    Constraint::Length(10),
                ],
            )
            .header(Row::new(["tool", "calls", "errors", "bytes", "total_ms"]))
            .block(Block::bordered().title("Tool stats"));
            frame.render_widget(tools_table, tools_area);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin_client::{CallRow, ToolStatRow};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn rendered_text(snapshot: &LogsSnapshot, now_ms: i64) -> String {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("test backend terminal");
        terminal
            .draw(|frame| render_logs(frame, snapshot, now_ms))
            .expect("draw logs screen");
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn renders_not_running_stub() {
        let content = rendered_text(&LogsSnapshot::Unreachable, 0);
        assert!(content.contains("not running"), "{content}");
    }

    #[test]
    fn renders_poller_stopped_distinctly_from_not_running() {
        let content = rendered_text(&LogsSnapshot::PollerStopped, 0);
        assert!(content.contains("poller stopped"), "{content}");
        assert!(!content.contains("not running"), "{content}");
    }

    #[test]
    fn renders_empty_connected_state_without_panicking() {
        let content = rendered_text(
            &LogsSnapshot::Connected {
                calls: vec![],
                tools: vec![],
            },
            0,
        );
        assert!(content.contains("no calls yet"), "{content}");
        assert!(content.contains("no tool stats yet"), "{content}");
    }

    #[test]
    fn renders_calls_newest_first_with_all_columns() {
        let now_ms = 10_000;
        let data = LogsSnapshot::Connected {
            calls: vec![
                CallRow {
                    at_ms: 1_000, // oldest — wire order
                    source: "claude-code".to_string(),
                    tool: "search_code".to_string(),
                    duration_ms: 12,
                    bytes_in: 100,
                    bytes_out: 200,
                    is_error: false,
                },
                CallRow {
                    at_ms: 9_000, // newest
                    source: "claude-code-hook".to_string(),
                    tool: "recall".to_string(),
                    duration_ms: 5,
                    bytes_in: 10,
                    bytes_out: 20,
                    is_error: true,
                },
            ],
            tools: vec![ToolStatRow {
                tool: "recall".to_string(),
                calls: 1,
                errors: 1,
                bytes_in: 10,
                bytes_out: 20,
                total_ms: 5,
            }],
        };
        let content = rendered_text(&data, now_ms);
        assert!(content.contains("recall"), "{content}");
        assert!(content.contains("search_code"), "{content}");
        assert!(content.contains("claude-code-hook"), "{content}");
        assert!(content.contains("error"), "{content}");
        assert!(content.contains("ok"), "{content}");
        // Newest first: "recall" (at_ms 9_000) renders before "search_code" (at_ms 1_000).
        let recall_pos = content.find("recall").expect("recall present");
        let search_pos = content.find("search_code").expect("search_code present");
        assert!(
            recall_pos < search_pos,
            "expected newest call (recall) to render before the older one (search_code): {content}"
        );
    }
}
