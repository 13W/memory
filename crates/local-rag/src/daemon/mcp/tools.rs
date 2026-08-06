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

/// `recall`'s own limit (T15-04, `[SPEC]`) — spec 08 §6 fixes the token
/// budget, not a caller-facing entry-count cap. Same order of magnitude as
/// `search_code`'s: `recall` is a top-K relevance result too, not an
/// exhaustive listing.
pub const DEFAULT_RECALL_LIMIT: i64 = 10;
pub const MAX_RECALL_LIMIT: i64 = 50;

/// `list_memory`/`list_memory_candidates`'s pagination window (T15-04,
/// `[SPEC]`) — deliberately a larger cap than `MAX_SEARCH_LIMIT`/
/// `MAX_RECALL_LIMIT`: these are exhaustive-pagination review tools (spec
/// 11 §2's "review reads"/"candidate review"), not top-K relevance results,
/// so a caller paging through everything in a scope needs a wider window
/// per call.
pub const DEFAULT_LIST_LIMIT: i64 = 20;
pub const MAX_LIST_LIMIT: i64 = 100;

/// `Tool.annotations` (X-003, `[SPEC]`) — `openWorldHint` is always `false`:
/// this system is fully local (`data_policy` default `local_only`, CLAUDE.md's
/// own architecture guardrail), no tool ever reaches out to the world.
/// `destructive`/`idempotent` are chosen per tool, not derived from a
/// mechanical rule -- see the X-003 as-built note (spec 11 §2) for the full
/// per-tool table and the reasoning behind each one.
fn annotations(title: &str, read_only: bool, destructive: bool, idempotent: bool) -> Value {
    serde_json::json!({
        "title": title,
        "readOnlyHint": read_only,
        "destructiveHint": destructive,
        "idempotentHint": idempotent,
        "openWorldHint": false
    })
}

/// The full `tools/list` result.
pub fn catalog() -> Value {
    serde_json::json!({
        "tools": [
            {
                "name": "search_code",
                "description": "Search this workspace's indexed code. Returns fused hits \
                    with path, unit kind, byte span, language, per-leg ranks and an excerpt \
                    cut from the exact indexed bytes. Never indexes on demand.",
                "annotations": annotations("Search code", true, false, true),
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
                "annotations": annotations("Get file context", true, false, true),
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
                "annotations": annotations("Project overview", true, false, true),
                "inputSchema": {
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }
            },
            {
                "name": "recall",
                "description": "Explicit durable-memory recall: the same scored pipeline the \
                    session-start hook uses. An empty/absent query is legal and returns the \
                    scope's most recent eligible memories. Returns both the rendered \
                    additionalContext text block and structured entries with ids for follow-up \
                    tool calls (inspect_memory_evidence, edit_memory).",
                "annotations": annotations("Recall memory", true, false, true),
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Free-text relevance query. Omit or leave empty for \
                                a termless recall (most recent eligible memories)."
                        },
                        "limit": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": MAX_RECALL_LIMIT,
                            "default": DEFAULT_RECALL_LIMIT,
                            "description": "Maximum number of structured entries to return \
                                (does not truncate the additionalContext text block)."
                        }
                    },
                    "additionalProperties": false
                }
            },
            {
                "name": "list_memory",
                "description": "Review durable memory entries in scope (global, repository, \
                    and — when a worktree resolves — worktree), including terminal states \
                    (superseded/retracted/resolved/rejected), unlike recall which excludes them.",
                "annotations": annotations("List memory entries", true, false, true),
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "kind": {
                            "type": "string",
                            "enum": [
                                "fact", "decision", "convention", "procedure", "task",
                                "question", "hypothesis"
                            ],
                            "description": "Keep only entries of this kind."
                        },
                        "state": {
                            "type": "string",
                            "enum": [
                                "active", "resolved", "retracted", "confirmed", "rejected",
                                "superseded"
                            ],
                            "description": "Keep only entries in this state. Omit to see every \
                                state, including terminal ones."
                        },
                        "scope": {
                            "type": "string",
                            "enum": ["global", "repository", "worktree"],
                            "description": "Restrict to one resolved scope instead of the \
                                default global ∪ repository ∪ worktree union."
                        },
                        "limit": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": MAX_LIST_LIMIT,
                            "default": DEFAULT_LIST_LIMIT
                        },
                        "offset": {
                            "type": "integer",
                            "minimum": 0,
                            "default": 0
                        }
                    },
                    "additionalProperties": false
                }
            },
            {
                "name": "list_memory_candidates",
                "description": "Review pending/approved/rejected/expired memory candidates \
                    proposed by consolidation. Candidates have no scope (global to the store) — \
                    the request's worktree context is not consulted.",
                "annotations": annotations("List memory candidates", true, false, true),
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "state": {
                            "type": "string",
                            "enum": ["pending", "approved", "rejected", "expired"],
                            "description": "Keep only candidates in this review state. Omit to \
                                see every state."
                        },
                        "limit": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": MAX_LIST_LIMIT,
                            "default": DEFAULT_LIST_LIMIT
                        },
                        "offset": {
                            "type": "integer",
                            "minimum": 0,
                            "default": 0
                        }
                    },
                    "additionalProperties": false
                }
            },
            {
                "name": "inspect_memory_evidence",
                "description": "The observation ids cited as evidence for one memory entry. An \
                    unknown memory_id returns an empty list, not an error.",
                "annotations": annotations("Inspect memory evidence", true, false, true),
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "memory_id": {
                            "type": "string",
                            "minLength": 1
                        }
                    },
                    "required": ["memory_id"],
                    "additionalProperties": false
                }
            },
            {
                "name": "stats",
                "description": "Store-wide counts of memory entries (by kind/state) and pending \
                    candidates (by review state), plus write-queue backpressure and, when the \
                    request's worktree resolves, its projection status.",
                "annotations": annotations("Store statistics", true, false, true),
                "inputSchema": {
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }
            },
            {
                "name": "health",
                "description": "Daemon mode, version, and store instance identity.",
                "annotations": annotations("Daemon health", true, false, true),
                "inputSchema": {
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }
            },
            {
                "name": "remember",
                "description": "Create a new durable memory entry directly (not via candidate \
                    review). Defaults to repository scope when the request's worktree resolves, \
                    else global.",
                "annotations": annotations("Create memory entry", false, false, false),
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "text": {
                            "type": "string",
                            "minLength": 1
                        },
                        "kind": {
                            "type": "string",
                            "enum": [
                                "fact", "decision", "convention", "procedure", "task",
                                "question", "hypothesis"
                            ]
                        },
                        "scope": {
                            "type": "string",
                            "enum": ["global", "repository", "worktree"],
                            "description": "Restrict to one scope instead of the default \
                                (repository when the worktree resolves, else global)."
                        },
                        "canonical_key": {
                            "type": "string",
                            "description": "Unique within (scope_kind, scope_owner_id); a \
                                conflict is CANONICAL_KEY_CONFLICT."
                        },
                        "importance": {
                            "type": "string",
                            "enum": ["low", "medium", "high"],
                            "default": "medium"
                        },
                        "confirmed_by_user": {
                            "type": "boolean",
                            "default": false,
                            "description": "Whether a human explicitly confirmed this text. \
                                Raises the entry's confidence; does not change who is recorded \
                                as the acting actor (always the caller)."
                        }
                    },
                    "required": ["text", "kind"],
                    "additionalProperties": false
                }
            },
            {
                "name": "approve_memory_candidate",
                "description": "Approve a pending memory candidate, materializing its proposed \
                    operation through the same transactional path a direct write would use.",
                "annotations": annotations("Approve memory candidate", false, false, true),
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id": {
                            "type": "string",
                            "minLength": 1
                        }
                    },
                    "required": ["id"],
                    "additionalProperties": false
                }
            },
            {
                "name": "reject_memory_candidate",
                "description": "Reject a pending memory candidate. Never materializes.",
                "annotations": annotations("Reject memory candidate", false, false, false),
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id": {
                            "type": "string",
                            "minLength": 1
                        }
                    },
                    "required": ["id"],
                    "additionalProperties": false
                }
            },
            {
                "name": "edit_memory_candidate",
                "description": "Edit a pending memory candidate's proposed operation and/or \
                    conflict list. Legal only while the candidate is still pending (candidates \
                    have no version to check instead).",
                "annotations": annotations("Edit memory candidate", false, false, true),
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id": {
                            "type": "string",
                            "minLength": 1
                        },
                        "patch": {
                            "type": "object",
                            "properties": {
                                "proposed_operation": {
                                    "type": "object",
                                    "description": "Full replacement for the candidate's \
                                        proposed_operation: a tagged object {\"op\": \
                                        \"create\"|\"reinforce\"|\"resolve\"|\"retract\"|\
                                        \"supersede\", ...op-specific fields}."
                                },
                                "conflicts": {
                                    "type": "array",
                                    "items": {"type": "string"}
                                }
                            },
                            "additionalProperties": false
                        }
                    },
                    "required": ["id", "patch"],
                    "additionalProperties": false
                }
            },
            {
                "name": "edit_memory",
                "description": "Edit an existing memory entry's text and/or importance. \
                    Rejects editing a terminal-state entry.",
                "annotations": annotations("Edit memory entry", false, false, false),
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id": {
                            "type": "string",
                            "minLength": 1
                        },
                        "expected_version": {
                            "type": "integer",
                            "description": "Optimistic-concurrency precondition; a mismatch is \
                                OPTIMISTIC_CONFLICT."
                        },
                        "patch": {
                            "type": "object",
                            "properties": {
                                "text": {"type": "string"},
                                "importance": {
                                    "type": "number",
                                    "minimum": 0,
                                    "maximum": 1
                                }
                            },
                            "additionalProperties": false
                        }
                    },
                    "required": ["id", "expected_version", "patch"],
                    "additionalProperties": false
                }
            },
            {
                "name": "retract_memory",
                "description": "Retract a memory entry (v1 'forget'): audit-preserving \
                    withdrawal, not a delete. Illegal for kinds without a retracted state (e.g. \
                    hypothesis).",
                "annotations": annotations("Retract memory entry", false, true, false),
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id": {
                            "type": "string",
                            "minLength": 1
                        },
                        "expected_version": {
                            "type": "integer",
                            "description": "Optimistic-concurrency precondition; a mismatch is \
                                OPTIMISTIC_CONFLICT."
                        }
                    },
                    "required": ["id", "expected_version"],
                    "additionalProperties": false
                }
            },
            {
                "name": "merge_memories",
                "description": "Merge two or more memory entries (v1 'consolidate'): the \
                    survivor absorbs the losers' evidence; losers become superseded, pointing at \
                    the survivor.",
                "annotations": annotations("Merge memory entries", false, false, false),
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "ids": {
                            "type": "array",
                            "minItems": 2,
                            "items": {
                                "type": "object",
                                "properties": {
                                    "memory_id": {"type": "string", "minLength": 1},
                                    "expected_version": {"type": "integer"}
                                },
                                "required": ["memory_id", "expected_version"],
                                "additionalProperties": false
                            }
                        },
                        "survivor_id": {
                            "type": "string",
                            "minLength": 1,
                            "description": "Must be one of ids[].memory_id; that entry survives, \
                                the rest become superseded losers."
                        }
                    },
                    "required": ["ids", "survivor_id"],
                    "additionalProperties": false
                }
            },
            {
                "name": "give_feedback",
                "description": "Record free-text feedback as a durable observation, directly \
                    (not through the spool) — the daemon-internal equivalent of a hook's ingest \
                    append. Feeds the next consolidation pass; does not itself mutate any memory \
                    entry.",
                "annotations": annotations("Give feedback", false, false, true),
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "text": {
                            "type": "string",
                            "minLength": 1
                        }
                    },
                    "required": ["text"],
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
    fn catalog_advertises_all_seventeen_v0_tools_with_the_spec_fixed_names() {
        let catalog = catalog();
        let names: Vec<&str> = catalog["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "search_code",
                "get_file_context",
                "project_overview",
                "recall",
                "list_memory",
                "list_memory_candidates",
                "inspect_memory_evidence",
                "stats",
                "health",
                "remember",
                "approve_memory_candidate",
                "reject_memory_candidate",
                "edit_memory_candidate",
                "edit_memory",
                "retract_memory",
                "merge_memories",
                "give_feedback",
            ]
        );
    }

    #[test]
    fn catalog_never_exposes_the_v1_forget_or_consolidate_names() {
        // v1 name mapping (spec 11 §2): forget -> retract_memory,
        // consolidate(src,tgt) -> merge_memories. Neither v1 name is ever a
        // tool name -- this task card's own "v1 forget/consolidate names
        // not exposed as destructive behavior" bullet.
        let catalog = catalog();
        let names: Vec<&str> = catalog["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(!names.contains(&"forget"), "{names:?}");
        assert!(!names.contains(&"consolidate"), "{names:?}");
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
    fn catalog_every_tool_has_annotations() {
        for tool in catalog()["tools"].as_array().unwrap() {
            let annotations = tool["annotations"]
                .as_object()
                .unwrap_or_else(|| panic!("{} has no annotations object", tool["name"]));
            for key in [
                "title",
                "readOnlyHint",
                "destructiveHint",
                "idempotentHint",
                "openWorldHint",
            ] {
                assert!(
                    annotations.contains_key(key),
                    "{} annotations missing {key}",
                    tool["name"]
                );
            }
            assert_eq!(annotations["openWorldHint"], Value::Bool(false));
        }
    }

    #[test]
    fn catalog_destructive_hint_is_true_only_for_retract_memory() {
        let catalog = catalog();
        let destructive: Vec<&str> = catalog["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|t| t["annotations"]["destructiveHint"] == Value::Bool(true))
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(destructive, ["retract_memory"]);
    }

    #[test]
    fn catalog_read_only_hint_matches_the_read_only_tool_list() {
        let catalog = catalog();
        let read_only: Vec<&str> = catalog["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|t| t["annotations"]["readOnlyHint"] == Value::Bool(true))
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            read_only,
            [
                "search_code",
                "get_file_context",
                "project_overview",
                "recall",
                "list_memory",
                "list_memory_candidates",
                "inspect_memory_evidence",
                "stats",
                "health",
            ]
        );
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
