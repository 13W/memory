//! In-memory per-call telemetry (spec 11 §7, ADR-0008, T18-08) — a bounded
//! ring buffer of recent JSON-RPC calls plus a running per-tool aggregate,
//! written at the single point every request already passes through
//! (`daemon/handshake.rs::handle_connection`, around `handler.handle(...)`)
//! and read back through two new JSON-RPC methods that are siblings of
//! `initialize`/`ping`/`tools/list` (`daemon/mcp/dispatch.rs`'s own match):
//! `admin/tail_calls`, `admin/tool_stats`. Deliberately no persistence, like
//! `super::tool_calls::ToolCallCounters` — a daemon restart resets both the
//! buffer and the aggregate. `admin/*` calls themselves are never recorded
//! (self-exclusion — an `admin/tail_calls` poll must not show up inside its
//! own tail); the caller (`handle_connection`) is responsible for that
//! filter, this module only provides the primitives.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

/// Ring buffer capacity (spec 11 §7 as-built, T18-08) — an approximate,
/// non-`[FIXED]` bound, not a normative wire limit.
pub const CAPACITY: usize = 500;

/// One completed (or notification-only) request that passed through
/// `handle_connection`, minus every `admin/*` call.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct CallRecord {
    pub at_ms: i64,
    /// The raw `Hello.harness` string of the connection that made this
    /// call — not normalized into a closed `mcp`/`hook` enum, mirroring
    /// `local_rag_protocol::handshake::Hello::harness`'s own "free string,
    /// not an enum" design (forward-compatible with harnesses this build
    /// does not know about).
    pub source: String,
    /// The MCP tool name for a `tools/call` request, otherwise the raw
    /// JSON-RPC method (`"initialize"`, `"ping"`, `"tools/list"`, ...).
    pub tool: String,
    pub duration_ms: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
    /// JSON-RPC-level error only (`dispatch.rs`'s own "two error
    /// channels" doctrine) — an in-band `isError: true` inside a
    /// successful `CallToolResult` is a valid, successful JSON-RPC
    /// response and does not set this.
    pub is_error: bool,
}

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct ToolStats {
    pub calls: u64,
    pub errors: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub total_ms: u64,
}

#[derive(Debug, Default)]
struct Inner {
    calls: Mutex<VecDeque<CallRecord>>,
    stats: Mutex<HashMap<String, ToolStats>>,
}

/// Cheaply cloneable, like `super::session::SessionRegistry`/
/// `super::tool_calls::ToolCallCounters` — every clone shares the same
/// underlying buffer and aggregate.
#[derive(Debug, Clone, Default)]
pub struct TelemetryState {
    inner: Arc<Inner>,
}

impl TelemetryState {
    /// A fresh state, no calls recorded.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one call. Pushes `record` onto the ring buffer, evicting the
    /// oldest entry once [`CAPACITY`] is reached, and folds it into
    /// `record.tool`'s running aggregate (never evicted — a running total
    /// since the daemon started, like `ToolCallCounters::aggregate`). Two
    /// separate locks, acquired one after another, not one combined
    /// critical section — the same relaxed-atomicity precedent
    /// `ToolCallCounters::record` already establishes for this class of
    /// observability data.
    pub fn record(&self, record: CallRecord) {
        {
            let mut calls = self.inner.calls.lock().expect("telemetry mutex poisoned");
            if calls.len() >= CAPACITY {
                calls.pop_front();
            }
            calls.push_back(record.clone());
        }
        let mut stats = self.inner.stats.lock().expect("telemetry mutex poisoned");
        let entry = stats.entry(record.tool).or_default();
        entry.calls += 1;
        if record.is_error {
            entry.errors += 1;
        }
        entry.bytes_in += record.bytes_in;
        entry.bytes_out += record.bytes_out;
        entry.total_ms += record.duration_ms;
    }

    /// The buffer's current contents, oldest first (insertion order).
    pub fn tail_calls(&self) -> Vec<CallRecord> {
        self.inner
            .calls
            .lock()
            .expect("telemetry mutex poisoned")
            .iter()
            .cloned()
            .collect()
    }

    /// Every tool's aggregate since the daemon started, sorted by tool
    /// name — deterministic output, mirroring
    /// `ToolCallCounters::aggregate_snapshot`.
    pub fn tool_stats(&self) -> Vec<(String, ToolStats)> {
        let stats = self.inner.stats.lock().expect("telemetry mutex poisoned");
        let mut rows: Vec<(String, ToolStats)> =
            stats.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        rows
    }
}

#[derive(serde::Deserialize)]
struct MethodProbe<'a> {
    #[serde(borrow, default)]
    method: Option<&'a str>,
    #[serde(borrow, default)]
    params: Option<&'a serde_json::value::RawValue>,
}

#[derive(serde::Deserialize)]
struct ToolNameProbe<'a> {
    #[serde(borrow, default)]
    name: Option<&'a str>,
}

/// The JSON-RPC `"method"` field of `mcp_text`, or `None` if it is missing
/// or the body does not parse as an object — best-effort, never panics.
/// `dispatch()` is the authority on rejecting a malformed request; this is
/// only an observability label.
pub fn method_of(mcp_text: &str) -> Option<String> {
    serde_json::from_str::<MethodProbe>(mcp_text)
        .ok()
        .and_then(|probe| probe.method)
        .map(str::to_string)
}

/// The telemetry label for one call: for `"tools/call"`, the tool name
/// inside `params.name`; for every other method, `method` itself.
pub fn call_label(mcp_text: &str, method: &str) -> String {
    if method != "tools/call" {
        return method.to_string();
    }
    serde_json::from_str::<MethodProbe>(mcp_text)
        .ok()
        .and_then(|probe| probe.params)
        .and_then(|params| serde_json::from_str::<ToolNameProbe>(params.get()).ok())
        .and_then(|tool| tool.name)
        .map(str::to_string)
        .unwrap_or_else(|| method.to_string())
}

/// Whether `mcp_text` (a JSON-RPC response body) is a top-level `"error"`
/// response (`dispatch.rs`'s "protocol error" channel) — an in-band
/// `isError: true` inside a successful `"result"` is deliberately NOT an
/// error here, see this module's own doc.
pub fn response_is_error(mcp_text: &str) -> bool {
    #[derive(serde::Deserialize)]
    struct ErrorProbe {
        #[serde(default)]
        error: Option<serde_json::Value>,
    }
    serde_json::from_str::<ErrorProbe>(mcp_text)
        .map(|probe| probe.error.is_some())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(tool: &str, is_error: bool) -> CallRecord {
        CallRecord {
            at_ms: 1_700_000_000_000,
            source: "claude-code".to_string(),
            tool: tool.to_string(),
            duration_ms: 5,
            bytes_in: 10,
            bytes_out: 20,
            is_error,
        }
    }

    #[test]
    fn a_fresh_state_has_no_calls_or_stats() {
        let telemetry = TelemetryState::new();
        assert_eq!(telemetry.tail_calls(), vec![]);
        assert_eq!(telemetry.tool_stats(), vec![]);
    }

    #[test]
    fn recording_appends_to_the_buffer_and_aggregates_by_tool() {
        let telemetry = TelemetryState::new();
        telemetry.record(rec("recall", false));
        telemetry.record(rec("recall", true));
        telemetry.record(rec("search_code", false));
        assert_eq!(telemetry.tail_calls().len(), 3);
        assert_eq!(
            telemetry.tool_stats(),
            vec![
                (
                    "recall".to_string(),
                    ToolStats {
                        calls: 2,
                        errors: 1,
                        bytes_in: 20,
                        bytes_out: 40,
                        total_ms: 10
                    }
                ),
                (
                    "search_code".to_string(),
                    ToolStats {
                        calls: 1,
                        errors: 0,
                        bytes_in: 10,
                        bytes_out: 20,
                        total_ms: 5
                    }
                ),
            ]
        );
    }

    #[test]
    fn the_buffer_evicts_the_oldest_entry_once_full_but_the_aggregate_keeps_everything() {
        let telemetry = TelemetryState::new();
        for i in 0..CAPACITY {
            telemetry.record(rec(&format!("tool-{i}"), false));
        }
        telemetry.record(rec("tool-overflow", false));

        let tail = telemetry.tail_calls();
        assert_eq!(tail.len(), CAPACITY);
        assert_eq!(
            tail.first().unwrap().tool,
            "tool-1",
            "the oldest entry (tool-0) must be evicted"
        );
        assert_eq!(tail.last().unwrap().tool, "tool-overflow");
        assert_eq!(
            telemetry.tool_stats().len(),
            CAPACITY + 1,
            "the aggregate is never evicted alongside the buffer"
        );
    }

    #[test]
    fn clones_observe_the_same_state() {
        let telemetry = TelemetryState::new();
        let clone = telemetry.clone();
        telemetry.record(rec("recall", false));
        assert_eq!(clone.tail_calls().len(), 1);
    }

    #[test]
    fn method_of_reads_the_method_field_and_tolerates_garbage() {
        assert_eq!(
            method_of(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#),
            Some("ping".to_string())
        );
        assert_eq!(method_of("not json"), None);
        assert_eq!(method_of(r#"{"id":1}"#), None);
    }

    #[test]
    fn call_label_resolves_tools_call_to_the_inner_tool_name() {
        let text = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"recall","arguments":{}}}"#;
        assert_eq!(call_label(text, "tools/call"), "recall");
    }

    #[test]
    fn call_label_falls_back_to_the_method_when_params_are_malformed() {
        let text = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call"}"#;
        assert_eq!(call_label(text, "tools/call"), "tools/call");
    }

    #[test]
    fn call_label_is_the_method_itself_for_non_tools_call() {
        assert_eq!(call_label(r#"{"method":"ping"}"#, "ping"), "ping");
    }

    #[test]
    fn response_is_error_detects_the_top_level_error_key_only() {
        assert!(response_is_error(
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"nope"}}"#
        ));
        assert!(!response_is_error(
            r#"{"jsonrpc":"2.0","id":1,"result":{"isError":true,"content":[]}}"#
        ));
    }
}
