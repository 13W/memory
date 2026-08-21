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
use crate::daemon::indexing::SupervisorClient;
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
    /// The daemon-managed indexing supervisor's client (T20-06/T20-07) —
    /// `None` exactly in `DaemonMode::MigrationOnly`. `admin/projects_list`/
    /// `admin/projects_reload`/`admin/reconcile_now` read it.
    pub indexing_supervisor: Option<&'a SupervisorClient>,
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
        // Daemon-managed indexing control surface (spec 11 §8, T20-07) — the
        // same TUI/CLI-only, non-catalog precedent as the two verbs just
        // above; `admin/*`'s self-exclusion from `ctx.telemetry` is already
        // handled by `handshake.rs` (prefix match), not here.
        "admin/projects_list" => encode_success(
            id,
            admin_projects_list_result(ctx.indexing_supervisor).await,
        ),
        "admin/projects_reload" => encode_success(
            id,
            admin_projects_reload_result(ctx.indexing_supervisor).await,
        ),
        "admin/reconcile_now" => {
            match admin_reconcile_now_result(ctx.indexing_supervisor, request.params).await {
                Ok(value) => encode_success(id, value),
                Err(msg) => encode_error(id, jsonrpc::INVALID_PARAMS, &msg),
            }
        }
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
        "search_code" => {
            // The translator comes from the memory context because that is
            // where the daemon's single `GeneratorPool` already lives (D-054);
            // the code pillar gets the translator, not the context.
            super::code::search_code(
                engine,
                &memory.translator(),
                root,
                &call.arguments,
                ctx.now_ms,
            )
            .await
        }
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
        "confirm_memory" => {
            super::memory_write::confirm_memory(memory, &call.arguments, ctx.now_ms).await
        }
        "reject_memory" => {
            super::memory_write::reject_memory(memory, &call.arguments, ctx.now_ms).await
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

/// `{"available": bool, "projects": [ProjectStatus, ...]}` — `admin/
/// projects_list`'s (spec 11 §8, T20-07) own result. `available: false`
/// (with an empty `projects`) exactly in `MigrationOnly` (no supervisor to
/// ask) — never conflated with "genuinely zero registered worktrees"
/// (`available: true`, empty `projects`): a caller must be able to tell
/// "the daemon cannot answer this right now" from "it answered: there is
/// nothing managed."
async fn admin_projects_list_result(supervisor: Option<&SupervisorClient>) -> Value {
    let Some(supervisor) = supervisor else {
        return serde_json::json!({ "available": false, "projects": [] });
    };
    let projects = supervisor.list_projects().await;
    serde_json::json!({ "available": true, "projects": projects })
}

/// `{"available": bool, "started": usize, "stopped": usize}` — `admin/
/// projects_reload`'s (T20-07) own result, `ReloadOutcome` flattened with
/// the same `available` convention as [`admin_projects_list_result`].
async fn admin_projects_reload_result(supervisor: Option<&SupervisorClient>) -> Value {
    let Some(supervisor) = supervisor else {
        return serde_json::json!({ "available": false, "started": 0, "stopped": 0 });
    };
    let outcome = supervisor.reload().await;
    let mut value = serde_json::to_value(outcome).expect("ReloadOutcome always serializes");
    value["available"] = Value::Bool(true);
    value
}

/// `admin/reconcile_now`'s (T20-07) own result: `{"available": bool}` on
/// success, `Err` (a human-readable message) for [`dispatch`] to turn into a
/// JSON-RPC `-32602` — a malformed `params`, or [`ReconcileNowError::
/// NotManaged`] (unknown worktree, or registered but no task currently
/// running for it; the two are not distinguished, spec 11 §8's own wording).
/// `MigrationOnly` (`supervisor` is `None`) answers `{"available": false}`
/// rather than an error — the same convention as the other two verbs, since
/// there is no supervisor to judge "managed" against at all.
async fn admin_reconcile_now_result(
    supervisor: Option<&SupervisorClient>,
    params: Option<Value>,
) -> Result<Value, String> {
    let worktree_id = parse_worktree_id(params)?;
    let Some(supervisor) = supervisor else {
        return Ok(serde_json::json!({ "available": false }));
    };
    supervisor
        .reconcile_now(&worktree_id)
        .await
        .map(|()| serde_json::json!({ "available": true }))
        .map_err(|e| e.to_string())
}

/// `{"worktree_id": "..."}` → the id, or a human-readable parse failure —
/// `admin/reconcile_now`'s own argument shape.
fn parse_worktree_id(params: Option<Value>) -> Result<String, String> {
    let Some(Value::Object(map)) = params else {
        return Err("params must be an object with a \"worktree_id\" string".to_string());
    };
    match map.get("worktree_id") {
        Some(Value::String(s)) => Ok(s.clone()),
        _ => Err("params.worktree_id must be a string".to_string()),
    }
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
            indexing_supervisor: None,
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

    /// T20-07: with no supervisor at all (`ctx.indexing_supervisor: None` —
    /// the same condition `MigrationOnly` produces in production), all three
    /// admin verbs answer `available: false` rather than an error or a
    /// fabricated healthy-looking result. `admin/reconcile_now` gets a
    /// syntactically valid `worktree_id` here specifically to prove the
    /// `available: false` short-circuit fires *before* any attempt to judge
    /// whether that id is managed.
    #[tokio::test]
    async fn all_three_admin_verbs_answer_unavailable_without_a_supervisor() {
        let mode = DaemonMode::Normal;
        let rc = request_context();
        let telemetry = TelemetryState::new();
        let tool_calls = ToolCallCounters::new();
        let dispatch_ctx = ctx(&mode, &rc, &telemetry, &tool_calls);

        let list = dispatch(
            r#"{"jsonrpc":"2.0","id":1,"method":"admin/projects_list"}"#,
            &dispatch_ctx,
        )
        .await
        .expect("a request always gets a response");
        let body: Value = serde_json::from_str(&list).unwrap();
        assert_eq!(
            body["result"],
            serde_json::json!({ "available": false, "projects": [] })
        );

        let reload = dispatch(
            r#"{"jsonrpc":"2.0","id":2,"method":"admin/projects_reload"}"#,
            &dispatch_ctx,
        )
        .await
        .expect("a request always gets a response");
        let body: Value = serde_json::from_str(&reload).unwrap();
        assert_eq!(
            body["result"],
            serde_json::json!({ "available": false, "started": 0, "stopped": 0 })
        );

        let reconcile = dispatch(
            r#"{"jsonrpc":"2.0","id":3,"method":"admin/reconcile_now","params":{"worktree_id":"any"}}"#,
            &dispatch_ctx,
        )
        .await
        .expect("a request always gets a response");
        let body: Value = serde_json::from_str(&reconcile).unwrap();
        assert_eq!(body["result"], serde_json::json!({ "available": false }));
    }

    /// `admin/reconcile_now`'s own argument validation is a JSON-RPC-level
    /// `-32602` (spec 11 §8), checked before the `available: false`
    /// short-circuit above — a malformed call is a protocol error regardless
    /// of whether a supervisor exists to ask.
    #[tokio::test]
    async fn admin_reconcile_now_rejects_a_missing_worktree_id() {
        let mode = DaemonMode::Normal;
        let rc = request_context();
        let telemetry = TelemetryState::new();
        let tool_calls = ToolCallCounters::new();
        let dispatch_ctx = ctx(&mode, &rc, &telemetry, &tool_calls);

        let text = dispatch(
            r#"{"jsonrpc":"2.0","id":1,"method":"admin/reconcile_now","params":{}}"#,
            &dispatch_ctx,
        )
        .await
        .expect("a request always gets a response");
        let body: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(body["error"]["code"], jsonrpc::INVALID_PARAMS);
    }
}
