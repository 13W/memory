//! The real MCP JSON-RPC dispatcher (spec 02 §4.2's passthrough content,
//! 11 §2's tool surface) — T15-03. Replaces T15-02's `EchoRequestHandler` as
//! the daemon's `RequestHandler`.
//!
//! [`jsonrpc`] is the inner JSON-RPC 2.0 envelope; [`dispatch`] routes
//! methods; [`content`] maps domain/infra outcomes into MCP `isError`
//! content; [`tools`] is the tool catalog and argument parsing; [`code`] is
//! the three code-query tool adapters over [`local_rag_search::
//! SearchEngine`]; [`memory`] is the six status/memory-read tool adapters
//! over [`crate::daemon::memory::MemoryContext`] (T15-04); [`memory_write`]
//! is the eight memory-write/candidate-review tool adapters over the same
//! context (T15-05); [`instructions`] is `initialize`'s server
//! identity/instructions/protocol negotiation.

mod code;
mod content;
mod dispatch;
mod instructions;
mod jsonrpc;
mod memory;
mod memory_write;
mod tools;

use std::sync::Arc;

use local_rag_protocol::RequestContext;
use local_rag_search::SearchEngine;
use serde_json::value::RawValue;
use tokio::sync::watch;

use super::handshake::RequestHandler;
use super::memory::MemoryContext;
use super::mode::DaemonMode;

pub use instructions::{
    PREFERRED_MCP_PROTOCOL, SERVER_INSTRUCTIONS, SERVER_NAME, SUPPORTED_MCP_PROTOCOL,
};
pub use tools::{
    DEFAULT_LIST_LIMIT, DEFAULT_RECALL_LIMIT, DEFAULT_SEARCH_LIMIT, MAX_LIST_LIMIT,
    MAX_RECALL_LIMIT, MAX_SEARCH_LIMIT, catalog,
};

/// The real `RequestHandler`: parses and dispatches MCP JSON-RPC, calling
/// [`SearchEngine`] for `search_code`/`get_file_context`/`project_overview`
/// and [`MemoryContext`] for `stats`/`health`/`recall`/`list_memory`/
/// `list_memory_candidates`/`inspect_memory_evidence`.
///
/// `engine`/`memory`: both `None` exactly when the daemon is in
/// [`DaemonMode::MigrationOnly`] (no usable `state.sqlite`/`cache.sqlite` to
/// build either from — they are constructed together, from the same
/// `(state_db, cache_db)` pair, so the two `Option`s can never disagree) —
/// `initialize`/`tools/list`/`ping`/notifications still work in that mode
/// (they touch no store); only `tools/call` short-circuits to `isError` +
/// `INCOMPATIBLE_STORE` (see `dispatch::route_tools_call`), uniformly for
/// every tool including `health`/`stats` — see `mcp::memory::health`'s own
/// doc comment for why that is not a gap.
///
/// Per-connection handling stays sequential — no per-call `tokio::spawn` —
/// the minimal delta on T15-02's transport: one proxy process serves one
/// session and issues tool calls serially within it (spec 02 §3.3), and
/// `L2.read`'s own bounded wait (`local_rag_search::
/// DEFAULT_L2_READ_WAIT_BUDGET`) already caps how long one call can hold up
/// the next.
#[derive(Clone)]
pub struct McpHandler {
    engine: Option<Arc<SearchEngine>>,
    memory: Option<Arc<MemoryContext>>,
    mode: watch::Receiver<DaemonMode>,
    /// A live clock read, not a value frozen at construction — the FTS
    /// staleness decision `SearchEngine` makes on every call is
    /// clock-dependent, so a stale `now_ms` would silently misjudge it on
    /// every request after the first.
    now: fn() -> i64,
    /// `tools/call` observability counters (spec 11 §2, T19-05) — the same
    /// shared instance every connection's `HandshakeContext::tool_calls`
    /// guard tracks; `dispatch::route_tools_call` records into it.
    tool_calls: super::tool_calls::ToolCallCounters,
    /// Recent-call ring buffer + per-tool aggregate (spec 11 §7, T18-08) —
    /// the same shared instance `HandshakeContext::telemetry` is recorded
    /// into; `dispatch`'s `admin/tail_calls`/`admin/tool_stats` read it.
    telemetry: super::telemetry::TelemetryState,
}

impl McpHandler {
    pub fn new(
        engine: Option<Arc<SearchEngine>>,
        memory: Option<Arc<MemoryContext>>,
        mode: watch::Receiver<DaemonMode>,
        now: fn() -> i64,
        tool_calls: super::tool_calls::ToolCallCounters,
        telemetry: super::telemetry::TelemetryState,
    ) -> Self {
        McpHandler {
            engine,
            memory,
            mode,
            now,
            tool_calls,
            telemetry,
        }
    }
}

impl RequestHandler for McpHandler {
    async fn handle(&self, ctx: RequestContext, mcp: Box<RawValue>) -> Option<Box<RawValue>> {
        let mode = self.mode.borrow().clone();
        let dispatch_ctx = dispatch::DispatchContext {
            engine: self.engine.as_deref(),
            memory: self.memory.as_deref(),
            mode: &mode,
            request_context: &ctx,
            now_ms: (self.now)(),
            tool_calls: &self.tool_calls,
            telemetry: &self.telemetry,
        };
        let response_text = dispatch::dispatch(mcp.get(), &dispatch_ctx).await?;
        Some(RawValue::from_string(response_text).expect("dispatch always produces valid JSON"))
    }
}
