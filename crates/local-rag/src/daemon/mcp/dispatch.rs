//! JSON-RPC method routing for the MCP surface (spec 11 §2) — T15-03.
//!
//! # Two error channels
//!
//! A JSON-RPC-level error (`-326xx`, [`jsonrpc::ErrorResponse`]) means "this
//! wire message itself is malformed or names something that doesn't exist"
//! — a bad envelope, an unknown method, an unknown tool, arguments that
//! violate the advertised schema. An MCP `isError: true` tool result
//! ([`content::err`]/[`content::infra_err`]) means "the message was valid
//! and the tool ran, but the *operation* failed" — `WORKTREE_NOT_INDEXED`,
//! `mode: "semantic"` (schema-valid, just unsupported), a broken store. This
//! split follows MCP's own "Error Handling" guidance verbatim: unknown
//! tools/invalid arguments/server errors are protocol errors; tool
//! execution errors are in-band content the model can see and react to.

use serde_json::Value;

use local_rag_protocol::{ErrorEnvelope, RequestContext};
use local_rag_search::SearchEngine;

use super::{content, instructions, jsonrpc, tools};
use crate::daemon::gitroot;
use crate::daemon::memory::MemoryContext;
use crate::daemon::mode::DaemonMode;
use crate::daemon::telemetry::TelemetryState;
use crate::daemon::tool_calls::ToolCallCounters;

/// Everything one `dispatch` call needs. Built fresh per request by
/// [`super::McpHandler::handle`] — `now_ms` in particular must be a live
/// clock read, not a startup-frozen value, since the FTS staleness decision
/// `SearchEngine` makes is clock-dependent. `tool_calls`/`telemetry` are the
/// exception to "fresh per request": both borrow daemon-lifetime shared
/// state (spec 11 §2/§7, T19-05/T18-08), the same instances every request
/// shares.
pub struct DispatchContext<'a> {
    pub engine: Option<&'a SearchEngine>,
    pub memory: Option<&'a MemoryContext>,
    pub mode: &'a DaemonMode,
    pub request_context: &'a RequestContext,
    pub now_ms: i64,
    pub tool_calls: &'a ToolCallCounters,
    pub telemetry: &'a TelemetryState,
}

/// Parse and answer one MCP JSON-RPC message. `None` means `text` was a
/// notification (no `"id"` member) — JSON-RPC 2.0 §4.1 forbids a response,
/// checked once, up front, so no branch below can accidentally answer one.
pub async fn dispatch(text: &str, ctx: &DispatchContext<'_>) -> Option<String> {
    let value: Value =
        serde_json::from_str(text).expect("mcp is already valid JSON by construction of Message");
    let Value::Object(map) = value else {
        return Some(encode_error(
            Value::Null,
            jsonrpc::INVALID_REQUEST,
            "request must be a JSON object",
        ));
    };
    let request = match jsonrpc::Request::parse(map) {
        Ok(r) => r,
        Err(msg) => return Some(encode_error(Value::Null, jsonrpc::INVALID_REQUEST, &msg)),
    };

    let Some(id) = request.id.clone() else {
        return None; // a notification: never answered, valid or not
    };

    if request.jsonrpc.as_deref() != Some("2.0") {
        return Some(encode_error(
            id,
            jsonrpc::INVALID_REQUEST,
            "jsonrpc must be \"2.0\"",
        ));
    }
    let Some(method) = request.method.as_deref() else {
        return Some(encode_error(
            id,
            jsonrpc::INVALID_REQUEST,
            "method is required",
        ));
    };

    let body = match method {
        "initialize" => {
            encode_success(id, instructions::initialize_result(request.params.as_ref()))
        }
        "ping" => encode_success(id, serde_json::json!({})),
        "tools/list" => match tools::list_params_ok(request.params.as_ref()) {
            Ok(()) => encode_success(id, tools::catalog()),
            Err(msg) => encode_error(id, jsonrpc::INVALID_PARAMS, &msg),
        },
        "tools/call" => match route_tools_call(ctx, &id, request.params).await {
            Ok(result) => encode_success(id, result),
            Err((code, msg)) => encode_error(id, code, &msg),
        },
        // TUI-only admin surface (spec 11 §7, T18-08) — not MCP tools, not
        // in `tools::catalog()`/`tools/list`; independent of `DaemonMode`
        // and `ctx.engine`/`ctx.memory` (in-memory telemetry, unrelated to
        // the store), so it stays answerable in `MigrationOnly` too, the
        // same way `local-rag-tui`'s Logs screen needs it to. Excluded
        // from the telemetry it exposes by `handshake.rs::handle_connection`
        // itself (self-exclusion), not here.
        "admin/tail_calls" => encode_success(id, admin_tail_calls_result(ctx.telemetry)),
        "admin/tool_stats" => encode_success(id, admin_tool_stats_result(ctx.telemetry)),
        other => encode_error(
            id,
            jsonrpc::METHOD_NOT_FOUND,
            &format!("unknown method: {other}"),
        ),
    };
    Some(body)
}

async fn route_tools_call(
    ctx: &DispatchContext<'_>,
    id: &Value,
    params: Option<Value>,
) -> Result<Value, (i64, String)> {
    let call = tools::parse_tool_call(params).map_err(|msg| (jsonrpc::INVALID_PARAMS, msg))?;

    // T19-05, spec 11 §2: count the attempt, not the outcome — before the
    // MigrationOnly short-circuit below and before per-tool argument
    // validation, so a degraded-mode or malformed call still shows up in
    // `stats`. A client only ever sends names from the catalog it fetched;
    // an unrecognized name here (the `other` arm below) still gets counted
    // rather than specially filtered — a harmless one-off entry for a
    // malformed/adversarial client, or a signal worth seeing for a genuine
    // protocol bug, not silently dropped.
    ctx.tool_calls
        .record(&ctx.request_context.session_id, &call.name);

    // `engine`/`memory` are always `Some` together or `None` together (built
    // from the same `(state_db, cache_db)` pair in `lifecycle.rs`) — one
    // combined gate, not two, so this stays the identical single
    // MigrationOnly short-circuit every tool already shared before T15-04
    // added a second resource.
    let (Some(engine), Some(memory)) = (ctx.engine, ctx.memory) else {
        // MigrationOnly (or, defensively, any other reason neither is
        // built): there is no store to query at all. Still a normal tool
        // result, not a JSON-RPC error — the client asked a well-formed
        // question, the daemon just cannot answer it right now.
        let envelope = match ctx.mode {
            DaemonMode::MigrationOnly { reason } => crate::daemon::error_envelope(reason),
            DaemonMode::Normal => ErrorEnvelope::incompatible_store("search engine unavailable"),
        };
        return Ok(to_value(content::err(&envelope)));
    };

    let root = gitroot::request_root(ctx.request_context);
    let result = match call.name.as_str() {
        "search_code" => super::code::search_code(engine, root, &call.arguments, ctx.now_ms).await,
        "get_file_context" => super::code::get_file_context(engine, root, &call.arguments).await,
        "project_overview" => super::code::project_overview(engine, root, &call.arguments).await,
        "recall" => super::memory::recall(memory, root, &call.arguments).await,
        "list_memory" => super::memory::list_memory(memory, root, &call.arguments).await,
        "list_memory_candidates" => {
            super::memory::list_memory_candidates(memory, &call.arguments).await
        }
        "inspect_memory_evidence" => {
            super::memory::inspect_memory_evidence(memory, &call.arguments).await
        }
        "stats" => {
            super::memory::stats(
                memory,
                root,
                &call.arguments,
                &ctx.request_context.session_id,
                ctx.tool_calls,
            )
            .await
        }
        "health" => super::memory::health(memory, ctx.mode, &call.arguments).await,
        "remember" => {
            super::memory_write::remember(
                memory,
                root,
                &call.arguments,
                &ctx.request_context.session_id,
                id,
                ctx.now_ms,
            )
            .await
        }
        "approve_memory_candidate" => {
            super::memory_write::approve_memory_candidate(memory, &call.arguments, ctx.now_ms).await
        }
        "reject_memory_candidate" => {
            super::memory_write::reject_memory_candidate(memory, &call.arguments).await
        }
        "edit_memory_candidate" => {
            super::memory_write::edit_memory_candidate(memory, &call.arguments).await
        }
        "edit_memory" => {
            super::memory_write::edit_memory(memory, &call.arguments, ctx.now_ms).await
        }
        "retract_memory" => {
            super::memory_write::retract_memory(memory, &call.arguments, ctx.now_ms).await
        }
        "merge_memories" => {
            super::memory_write::merge_memories(memory, &call.arguments, ctx.now_ms).await
        }
        "give_feedback" => {
            super::memory_write::give_feedback(
                memory,
                root,
                &call.arguments,
                &ctx.request_context.session_id,
                id,
                ctx.now_ms,
            )
            .await
        }
        other => return Err((jsonrpc::INVALID_PARAMS, format!("unknown tool: {other}"))),
    };
    result
        .map(to_value)
        .map_err(|msg| (jsonrpc::INVALID_PARAMS, msg))
}

fn to_value(result: content::CallToolResult) -> Value {
    serde_json::to_value(result).expect("CallToolResult always serializes")
}

/// `{"calls": [CallRecord, ...]}`, oldest first — `admin/tail_calls`'s
/// result (spec 11 §7, T18-08).
fn admin_tail_calls_result(telemetry: &TelemetryState) -> Value {
    serde_json::json!({ "calls": telemetry.tail_calls() })
}

#[derive(serde::Serialize)]
struct ToolStatsEntry<'a> {
    tool: &'a str,
    #[serde(flatten)]
    stats: &'a crate::daemon::telemetry::ToolStats,
}

/// `{"tools": [{"tool": ..., "calls": ..., ...}, ...]}`, sorted by tool
/// name — `admin/tool_stats`'s result (spec 11 §7, T18-08).
fn admin_tool_stats_result(telemetry: &TelemetryState) -> Value {
    let snapshot = telemetry.tool_stats();
    let tools: Vec<ToolStatsEntry> = snapshot
        .iter()
        .map(|(tool, stats)| ToolStatsEntry { tool, stats })
        .collect();
    serde_json::json!({ "tools": tools })
}

fn encode_success(id: Value, result: Value) -> String {
    serde_json::to_string(&jsonrpc::Response::new(id, result)).expect("Response always serializes")
}

fn encode_error(id: Value, code: i64, message: &str) -> String {
    serde_json::to_string(&jsonrpc::ErrorResponse::new(id, code, message))
        .expect("ErrorResponse always serializes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::telemetry::CallRecord;

    fn ctx<'a>(
        mode: &'a DaemonMode,
        request_context: &'a RequestContext,
        telemetry: &'a TelemetryState,
        tool_calls: &'a ToolCallCounters,
    ) -> DispatchContext<'a> {
        DispatchContext {
            engine: None,
            memory: None,
            mode,
            request_context,
            now_ms: 0,
            tool_calls,
            telemetry,
        }
    }

    fn request_context() -> RequestContext {
        RequestContext {
            session_id: "sess-1".to_string(),
            worktree_root: None,
            repo_hint: None,
        }
    }

    /// `admin/*` never touches `engine`/`memory` — this must answer
    /// identically in `MigrationOnly` (no store built at all) as in
    /// `Normal`, unlike `tools/call`.
    #[tokio::test]
    async fn admin_tail_calls_answers_even_without_a_store() {
        let mode = DaemonMode::Normal;
        let rc = request_context();
        let telemetry = TelemetryState::new();
        let tool_calls = ToolCallCounters::new();
        telemetry.record(CallRecord {
            at_ms: 1,
            source: "claude-code".to_string(),
            tool: "recall".to_string(),
            duration_ms: 5,
            bytes_in: 10,
            bytes_out: 20,
            is_error: false,
        });
        let dispatch_ctx = ctx(&mode, &rc, &telemetry, &tool_calls);

        let text = dispatch(
            r#"{"jsonrpc":"2.0","id":1,"method":"admin/tail_calls"}"#,
            &dispatch_ctx,
        )
        .await
        .expect("a request always gets a response");
        let body: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(body["result"]["calls"][0]["tool"], "recall");
    }

    #[tokio::test]
    async fn admin_tool_stats_reflects_recorded_calls_sorted_by_tool() {
        let mode = DaemonMode::Normal;
        let rc = request_context();
        let telemetry = TelemetryState::new();
        let tool_calls = ToolCallCounters::new();
        for tool in ["search_code", "recall"] {
            telemetry.record(CallRecord {
                at_ms: 1,
                source: "claude-code".to_string(),
                tool: tool.to_string(),
                duration_ms: 5,
                bytes_in: 10,
                bytes_out: 20,
                is_error: false,
            });
        }
        let dispatch_ctx = ctx(&mode, &rc, &telemetry, &tool_calls);

        let text = dispatch(
            r#"{"jsonrpc":"2.0","id":1,"method":"admin/tool_stats"}"#,
            &dispatch_ctx,
        )
        .await
        .expect("a request always gets a response");
        let body: Value = serde_json::from_str(&text).unwrap();
        let tools = body["result"]["tools"].as_array().unwrap();
        assert_eq!(tools[0]["tool"], "recall", "sorted by tool name");
        assert_eq!(tools[1]["tool"], "search_code");
        assert_eq!(tools[0]["calls"], 1);
    }

    #[tokio::test]
    async fn an_empty_telemetry_state_answers_with_empty_arrays() {
        let mode = DaemonMode::Normal;
        let rc = request_context();
        let telemetry = TelemetryState::new();
        let tool_calls = ToolCallCounters::new();
        let dispatch_ctx = ctx(&mode, &rc, &telemetry, &tool_calls);

        let text = dispatch(
            r#"{"jsonrpc":"2.0","id":1,"method":"admin/tail_calls"}"#,
            &dispatch_ctx,
        )
        .await
        .expect("a request always gets a response");
        let body: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(body["result"]["calls"], serde_json::json!([]));
    }
}
