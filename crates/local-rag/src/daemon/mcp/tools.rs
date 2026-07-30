//! The MCP tool catalog (`tools/list`, spec 11 §2's table — param names are
//! spec-fixed: `search_code(query, mode?, limit?, name_pattern?)`,
//! `get_file_context(path)`, `project_overview()`) and `tools/call` argument
//! parsing — T15-03.
//!
//! Hand-written `serde_json::Value`, not a schema-derive crate: no
//! `schemars` (or similar) dependency exists anywhere in this workspace,
//! and this project's own precedent is hand-writing wire shapes (`Snippet`'s
//! manual `Serialize`, `format_additional_context`'s byte-exact writer) —
//! three schemas is well under where a dependency would pay for itself.

use serde_json::{Map, Value};

/// Not `[SPEC]`-fixed — spec 09 only discusses `limit` relative to
/// `candidate_depth`, never a caller-facing default/cap. Picked and
/// documented as chosen, not derived, the same precedent
/// `MAX_MESSAGE_BYTES`/`LIVENESS_PROBE_TIMEOUT_MS` set.
pub const DEFAULT_SEARCH_LIMIT: i64 = 10;
pub const MAX_SEARCH_LIMIT: i64 = 50;

/// The full `tools/list` result.
pub fn catalog() -> Value {
    serde_json::json!({
        "tools": [
            {
                "name": "search_code",
                "description": "Search this workspace's indexed code. Returns fused hits \
                    with path, unit kind, byte span, language, per-leg ranks and an excerpt \
                    cut from the exact indexed bytes. Never indexes on demand.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "minLength": 1,
                            "description": "An identifier, a phrase, or a description of the \
                                behavior to find."
                        },
                        "mode": {
                            "type": "string",
                            "enum": ["hybrid", "lexical", "code", "semantic"],
                            "default": "hybrid",
                            "description": "hybrid = BM25 fused with dense vectors (default); \
                                lexical = full-text only; code = dense only; semantic is not \
                                supported in this version."
                        },
                        "limit": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": MAX_SEARCH_LIMIT,
                            "default": DEFAULT_SEARCH_LIMIT,
                            "description": "Maximum number of fused hits to return."
                        },
                        "name_pattern": {
                            "type": "string",
                            "description": "Keep only units whose local or qualified name \
                                starts with this prefix."
                        }
                    },
                    "required": ["query"],
                    "additionalProperties": false
                }
            },
            {
                "name": "get_file_context",
                "description": "List everything the index knows about one file: its units \
                    (occurrence id, kind, name, qualified name, byte span) with excerpts from \
                    the exact indexed bytes, plus the generation they came from.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "minLength": 1,
                            "description": "Path relative to the worktree root, '/'-separated. \
                                An absolute path inside the worktree is also accepted."
                        }
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }
            },
            {
                "name": "project_overview",
                "description": "Orient in this workspace: a 3-level directory tree with \
                    recursive file and unit counts, likely entry-point files, and the most \
                    frequently imported module specifiers, all derived from the active index \
                    generation.",
                "inputSchema": {
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }
            }
        ]
    })
}

/// A parsed, still-unvalidated `tools/call` request.
pub struct ToolCall {
    pub name: String,
    pub arguments: Map<String, Value>,
}

/// `params` for `tools/call` must be `{"name": <string>, "arguments"?:
/// <object>}`. `arguments` defaults to an empty object when absent (a tool
/// with no required arguments, like `project_overview`, may be called
/// without it).
pub fn parse_tool_call(params: Option<Value>) -> Result<ToolCall, String> {
    let Some(Value::Object(mut map)) = params else {
        return Err("params must be an object".to_string());
    };
    let name = match map.remove("name") {
        Some(Value::String(s)) => s,
        Some(_) => return Err("params.name must be a string".to_string()),
        None => return Err("params.name is required".to_string()),
    };
    let arguments = match map.remove("arguments") {
        None => Map::new(),
        Some(Value::Object(args)) => args,
        Some(_) => return Err("params.arguments must be an object".to_string()),
    };
    Ok(ToolCall { name, arguments })
}

/// `params` for `tools/list` — this daemon never issues a cursor, so a
/// client-supplied one cannot be honored.
pub fn list_params_ok(params: Option<&Value>) -> Result<(), String> {
    match params {
        None => Ok(()),
        Some(Value::Object(map)) if !map.contains_key("cursor") => Ok(()),
        Some(Value::Object(_)) => Err("cursor is not supported".to_string()),
        Some(_) => Err("params must be an object".to_string()),
    }
}

/// A required, non-empty string argument.
pub fn require_string(args: &Map<String, Value>, key: &str) -> Result<String, String> {
    match args.get(key) {
        Some(Value::String(s)) if !s.is_empty() => Ok(s.clone()),
        Some(Value::String(_)) => Err(format!("{key} must not be empty")),
        Some(_) => Err(format!("{key} must be a string")),
        None => Err(format!("{key} is required")),
    }
}

/// Reject any argument key not in `known` — the structural realization of
/// each schema's own `additionalProperties: false`.
pub fn reject_unknown_keys(args: &Map<String, Value>, known: &[&str]) -> Result<(), String> {
    for key in args.keys() {
        if !known.contains(&key.as_str()) {
            return Err(format!("unknown argument: {key}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_advertises_all_three_tools_with_the_spec_fixed_names() {
        let catalog = catalog();
        let names: Vec<&str> = catalog["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            ["search_code", "get_file_context", "project_overview"]
        );
    }

    #[test]
    fn catalog_every_schema_forbids_additional_properties() {
        for tool in catalog()["tools"].as_array().unwrap() {
            assert_eq!(
                tool["inputSchema"]["additionalProperties"],
                Value::Bool(false)
            );
        }
    }

    #[test]
    fn search_code_schema_requires_only_query() {
        let catalog = catalog();
        let required = catalog["tools"][0]["inputSchema"]["required"]
            .as_array()
            .unwrap();
        assert_eq!(required, &[Value::String("query".to_string())]);
    }

    #[test]
    fn catalog_serializes_identically_across_two_calls() {
        assert_eq!(
            serde_json::to_string(&catalog()).unwrap(),
            serde_json::to_string(&catalog()).unwrap()
        );
    }

    #[test]
    fn parse_tool_call_defaults_arguments_to_empty() {
        let call = parse_tool_call(Some(serde_json::json!({"name": "project_overview"}))).unwrap();
        assert_eq!(call.name, "project_overview");
        assert!(call.arguments.is_empty());
    }

    #[test]
    fn parse_tool_call_rejects_a_missing_name() {
        assert!(parse_tool_call(Some(serde_json::json!({}))).is_err());
    }

    #[test]
    fn parse_tool_call_rejects_non_object_params() {
        assert!(parse_tool_call(Some(Value::String("x".to_string()))).is_err());
        assert!(parse_tool_call(None).is_err());
    }

    #[test]
    fn parse_tool_call_rejects_non_object_arguments() {
        let params = serde_json::json!({"name": "search_code", "arguments": "x"});
        assert!(parse_tool_call(Some(params)).is_err());
    }

    #[test]
    fn list_params_ok_rejects_a_cursor() {
        assert!(list_params_ok(Some(&serde_json::json!({"cursor": "x"}))).is_err());
        assert!(list_params_ok(None).is_ok());
        assert!(list_params_ok(Some(&serde_json::json!({}))).is_ok());
    }

    #[test]
    fn require_string_distinguishes_missing_empty_and_wrong_type() {
        let mut args = Map::new();
        assert!(require_string(&args, "q").is_err());
        args.insert("q".to_string(), Value::String(String::new()));
        assert!(require_string(&args, "q").is_err());
        args.insert("q".to_string(), Value::Number(1.into()));
        assert!(require_string(&args, "q").is_err());
        args.insert("q".to_string(), Value::String("hi".to_string()));
        assert_eq!(require_string(&args, "q").unwrap(), "hi");
    }

    #[test]
    fn reject_unknown_keys_flags_the_first_unrecognized_one() {
        let mut args = Map::new();
        args.insert("query".to_string(), Value::String("x".to_string()));
        assert!(reject_unknown_keys(&args, &["query", "mode"]).is_ok());
        args.insert("bogus".to_string(), Value::Bool(true));
        assert!(reject_unknown_keys(&args, &["query", "mode"]).is_err());
    }
}
