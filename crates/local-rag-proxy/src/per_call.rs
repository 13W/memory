//! The per-call worktree fallback (D-137, ADR-0016), active only when this
//! proxy runs with `LOCAL_RAG_PER_CALL_WORKTREE=1`.
//!
//! A host that runs one proxy for many sessions from a directory that is not
//! a repository (Claude Cowork) has no launch worktree, so every call would
//! resolve to global scope. With the opt-in, each tool gains an optional
//! `worktree` argument: this module advertises it in `tools/list`, lifts it
//! out of `tools/call` arguments (the daemon's own schemas are
//! `additionalProperties: false` and would reject it), and hands it to the
//! relay as that one request's `RequestContext.worktree_fallback`. The daemon
//! alone decides whether to use it — only when the launch root resolves to
//! `GlobalOnly` — so a proxy launched inside a registered worktree keeps
//! routing there whatever the argument says.
//!
//! Without the opt-in this module is never constructed and the relay parses
//! nothing: spec 11 §1's byte-for-byte pass-through is the default. With it,
//! only two messages are ever rebuilt — a `tools/call` that actually carries
//! `worktree`, and the response to a `tools/list` — and a rebuilt message's
//! object keys come out sorted (`serde_json` without `preserve_order`, kept
//! off so the feature does not unify into the daemon's build). Every other
//! message, and anything that fails to parse, passes through untouched.

use std::path::Path;

use serde_json::value::RawValue;
use serde_json::{Map, Value};

/// The argument name the model passes, and the property added to every
/// tool's `inputSchema`.
pub const WORKTREE_ARGUMENT: &str = "worktree";

/// The advertised property's description (ADR-0016's own wording).
pub const WORKTREE_ARGUMENT_DESCRIPTION: &str = "Absolute path of the repository to use when this server was not started inside one. Ignored when the server already has a worktree.";

/// One connection's state: the ids of `tools/list` requests whose responses
/// have not come back yet. Per connection, not per process — a request the
/// daemon died holding is answered by the relay's own transport error, never
/// by a later connection's response.
#[derive(Debug, Default)]
pub struct PerCallWorktree {
    tools_list_ids: Vec<String>,
}

impl PerCallWorktree {
    /// Client → daemon. Returns the message to relay and this request's
    /// `worktree_fallback`: records a `tools/list` id, lifts `worktree` out of
    /// a `tools/call`'s arguments, and passes everything else through as the
    /// original bytes.
    pub fn outbound(&mut self, mcp: Box<RawValue>) -> (Box<RawValue>, Option<String>) {
        let Ok(Value::Object(mut message)) = serde_json::from_str::<Value>(mcp.get()) else {
            return (mcp, None);
        };
        match message.get("method").and_then(Value::as_str) {
            Some("tools/list") => {
                if let Some(id) = response_id(&message) {
                    self.tools_list_ids.push(id);
                }
                (mcp, None)
            }
            Some("tools/call") => {
                let Some(worktree) = message
                    .get_mut("params")
                    .and_then(|p| p.get_mut("arguments"))
                    .and_then(Value::as_object_mut)
                    .and_then(|args| args.remove(WORKTREE_ARGUMENT))
                else {
                    return (mcp, None);
                };
                let fallback = accept_fallback(worktree);
                (to_raw(&Value::Object(message)).unwrap_or(mcp), fallback)
            }
            _ => (mcp, None),
        }
    }

    /// Daemon → client. The response to a recorded `tools/list` gets the
    /// optional `worktree` property on every tool's `inputSchema`; every
    /// other message passes through as the original bytes.
    pub fn inbound(&mut self, mcp: Box<RawValue>) -> Box<RawValue> {
        if self.tools_list_ids.is_empty() {
            return mcp;
        }
        let Ok(Value::Object(mut message)) = serde_json::from_str::<Value>(mcp.get()) else {
            return mcp;
        };
        let Some(id) = response_id(&message) else {
            return mcp;
        };
        let Some(pos) = self.tools_list_ids.iter().position(|p| *p == id) else {
            return mcp;
        };
        self.tools_list_ids.remove(pos);
        let Some(tools) = message
            .get_mut("result")
            .and_then(|r| r.get_mut("tools"))
            .and_then(Value::as_array_mut)
        else {
            return mcp;
        };
        for tool in tools {
            add_worktree_property(tool);
        }
        to_raw(&Value::Object(message)).unwrap_or(mcp)
    }
}

/// The JSON-RPC id as its canonical JSON text, `None` for a notification or
/// a `null` id — the key a response is matched to its request by.
fn response_id(message: &Map<String, Value>) -> Option<String> {
    message
        .get("id")
        .filter(|id| !id.is_null())
        .map(Value::to_string)
}

/// Only an absolute path is a usable fallback: the daemon resolves it
/// outside any working directory of the caller's. Anything else is dropped,
/// said on stderr (stdout carries the JSON-RPC stream), and the call runs as
/// if no argument had been given.
fn accept_fallback(value: Value) -> Option<String> {
    match value {
        Value::String(path) if Path::new(&path).is_absolute() => Some(path),
        other => {
            eprintln!(
                "{}: ignoring the `{WORKTREE_ARGUMENT}` argument {other}: not an absolute path",
                crate::BIN
            );
            None
        }
    }
}

/// Add the optional `worktree` string to one tool's `inputSchema.properties`
/// — never to `required`. A tool without an object schema is left alone.
fn add_worktree_property(tool: &mut Value) {
    let Some(schema) = tool.get_mut("inputSchema").and_then(Value::as_object_mut) else {
        return;
    };
    let properties = schema
        .entry("properties")
        .or_insert_with(|| Value::Object(Map::new()));
    if let Some(properties) = properties.as_object_mut() {
        properties.insert(
            WORKTREE_ARGUMENT.to_string(),
            serde_json::json!({
                "type": "string",
                "description": WORKTREE_ARGUMENT_DESCRIPTION,
            }),
        );
    }
}

fn to_raw(value: &Value) -> Option<Box<RawValue>> {
    RawValue::from_string(serde_json::to_string(value).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(text: &str) -> Box<RawValue> {
        RawValue::from_string(text.to_string()).unwrap()
    }

    fn parse(raw: &RawValue) -> Value {
        serde_json::from_str(raw.get()).unwrap()
    }

    const TOOLS_LIST: &str = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
    const CATALOG: &str = r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"recall","inputSchema":{"type":"object","properties":{"query":{"type":"string"}},"additionalProperties":false}},{"name":"health","inputSchema":{"type":"object","additionalProperties":false}}]}}"#;

    #[test]
    fn a_tools_call_loses_its_worktree_argument_and_carries_it_as_the_fallback() {
        let mut per_call = PerCallWorktree::default();
        let (relayed, fallback) = per_call.outbound(raw(
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"recall","arguments":{"query":"q","worktree":"/repo"}}}"#,
        ));
        assert_eq!(fallback.as_deref(), Some("/repo"));
        assert_eq!(
            parse(&relayed),
            serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"recall","arguments":{"query":"q"}}})
        );
    }

    #[test]
    fn a_non_absolute_worktree_is_stripped_but_not_used() {
        let mut per_call = PerCallWorktree::default();
        for bad in [r#""relative/repo""#, r#""""#, "42", "null"] {
            let text = format!(
                r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{"name":"recall","arguments":{{"worktree":{bad}}}}}}}"#
            );
            let (relayed, fallback) = per_call.outbound(raw(&text));
            assert_eq!(fallback, None, "{bad}");
            assert_eq!(
                parse(&relayed)["params"]["arguments"],
                serde_json::json!({}),
                "{bad}"
            );
        }
    }

    #[test]
    fn messages_without_a_worktree_argument_pass_through_byte_identical() {
        let mut per_call = PerCallWorktree::default();
        for text in [
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"recall","arguments":{"query":"q"}}}"#,
            r#"{"jsonrpc":"2.0","id":5,"method":"initialize","params":{"z":1,"a":2}}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            r#"not json at all"#,
        ] {
            let Ok(mcp) = RawValue::from_string(text.to_string()) else {
                continue; // not a RawValue either: the relay itself rejects it earlier
            };
            let (relayed, fallback) = per_call.outbound(mcp);
            assert_eq!(relayed.get(), text);
            assert_eq!(fallback, None);
        }
    }

    #[test]
    fn the_tools_list_response_gains_an_optional_worktree_property_on_every_tool() {
        let mut per_call = PerCallWorktree::default();
        let (relayed, fallback) = per_call.outbound(raw(TOOLS_LIST));
        assert_eq!(relayed.get(), TOOLS_LIST);
        assert_eq!(fallback, None);

        let answered = parse(&per_call.inbound(raw(CATALOG)));
        let tools = answered["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        for tool in tools {
            let schema = &tool["inputSchema"];
            assert_eq!(schema["properties"]["worktree"]["type"], "string", "{tool}");
            assert_eq!(
                schema["properties"]["worktree"]["description"],
                WORKTREE_ARGUMENT_DESCRIPTION
            );
            assert!(
                schema
                    .get("required")
                    .is_none_or(|r| !r.as_array().unwrap().contains(&Value::from("worktree")))
            );
            assert_eq!(schema["additionalProperties"], false);
        }
        assert_eq!(
            tools[0]["inputSchema"]["properties"]["query"]["type"],
            "string"
        );
    }

    #[test]
    fn only_the_response_to_a_recorded_tools_list_is_rewritten() {
        let mut per_call = PerCallWorktree::default();
        // No tools/list in flight: an identically shaped response is untouched.
        assert_eq!(per_call.inbound(raw(CATALOG)).get(), CATALOG);

        per_call.outbound(raw(TOOLS_LIST));
        let other_id = CATALOG.replace(r#""id":1"#, r#""id":9"#);
        assert_eq!(per_call.inbound(raw(&other_id)).get(), other_id);

        // The recorded id is answered once, then forgotten.
        assert_ne!(per_call.inbound(raw(CATALOG)).get(), CATALOG);
        assert_eq!(per_call.inbound(raw(CATALOG)).get(), CATALOG);
    }
}
