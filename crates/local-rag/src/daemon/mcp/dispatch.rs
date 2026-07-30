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

/// Everything one `dispatch` call needs. Built fresh per request by
/// [`super::McpHandler::handle`] — `now_ms` in particular must be a live
/// clock read, not a startup-frozen value, since the FTS staleness decision
/// `SearchEngine` makes is clock-dependent.
pub struct DispatchContext<'a> {
    pub engine: Option<&'a SearchEngine>,
    pub memory: Option<&'a MemoryContext>,
    pub mode: &'a DaemonMode,
    pub request_context: &'a RequestContext,
    pub now_ms: i64,
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
        "stats" => super::memory::stats(memory, root, &call.arguments).await,
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

fn encode_success(id: Value, result: Value) -> String {
    serde_json::to_string(&jsonrpc::Response::new(id, result)).expect("Response always serializes")
}

fn encode_error(id: Value, code: i64, message: &str) -> String {
    serde_json::to_string(&jsonrpc::ErrorResponse::new(id, code, message))
        .expect("ErrorResponse always serializes")
}
