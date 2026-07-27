//! Typed model of Claude Code's hook JSON (stdin), one variant per capture-set
//! event (spec 07 §1: `SessionStart`, `UserPromptSubmit`, `PostToolUse`,
//! `PostToolUseFailure`, `Stop`, `SubagentStop`, `SessionEnd`).
//!
//! This is an **external, evolving** contract this project does not control —
//! not something spec 07 itself defines (spec 07 only fixes what identity/
//! redaction to *derive* from an event, §4). Parsing is deliberately tolerant:
//! only the fields the spec 07 §4 identity table actually needs
//! (`session_id`; `tool_use_id` for PostToolUse/Failure; `agent_id` for
//! SubagentStop; `prompt` for UserPromptSubmit) are hard requirements. Every
//! other field is optional — its absence degrades payload richness but never
//! blocks writing an observation. An unrecognized `hook_event_name` is a
//! distinct, forward-compatible failure: a future Claude Code hook type
//! should not crash an old binary.

use serde::Deserialize;
use serde_json::Value;

/// A parsed hook event: common fields plus its event-specific payload.
#[derive(Debug, Clone)]
pub struct ParsedEvent {
    pub session_id: String,
    pub cwd: Option<String>,
    pub kind: EventPayload,
}

/// The event-specific half of a [`ParsedEvent`], one variant per spec 07 §1
/// capture-set member. `PostToolUseFailure` is Claude Code's own **distinct**
/// hook event (not a status flag inside `PostToolUse`).
#[derive(Debug, Clone)]
pub enum EventPayload {
    SessionStart(SessionStartPayload),
    UserPromptSubmit(UserPromptSubmitPayload),
    PostToolUse(PostToolUsePayload),
    PostToolUseFailure(PostToolUseFailurePayload),
    Stop(StopPayload),
    SubagentStop(SubagentStopPayload),
    SessionEnd(SessionEndPayload),
}

#[derive(Debug, Clone, Deserialize)]
pub struct PostToolUsePayload {
    pub tool_use_id: String,
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub tool_input: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PostToolUseFailurePayload {
    pub tool_use_id: String,
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub tool_input: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SubagentStopPayload {
    pub agent_id: String,
    #[serde(default)]
    pub last_assistant_message: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UserPromptSubmitPayload {
    pub prompt: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StopPayload {
    #[serde(default)]
    pub last_assistant_message: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SessionStartPayload {
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SessionEndPayload {
    #[serde(default)]
    pub reason: Option<String>,
}

/// Why a raw hook JSON blob could not be turned into a [`ParsedEvent`].
#[derive(Debug)]
pub enum ParseError {
    /// Not valid JSON at all.
    Json(serde_json::Error),
    /// The common `session_id` field is absent (every event carries it).
    MissingSessionId,
    /// The common `hook_event_name` field is absent.
    MissingEventName,
    /// `hook_event_name` is not one of the 7 known capture-set events —
    /// forward-compatible: a future Claude Code hook type fails open rather
    /// than crashing.
    UnknownEventType(String),
    /// The event's own identity-critical field(s) are missing/malformed.
    InvalidPayload {
        event_type: String,
        source: serde_json::Error,
    },
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::Json(e) => write!(f, "hook event is not valid JSON: {e}"),
            ParseError::MissingSessionId => write!(f, "hook event has no session_id"),
            ParseError::MissingEventName => write!(f, "hook event has no hook_event_name"),
            ParseError::UnknownEventType(name) => {
                write!(f, "unknown hook_event_name {name:?}")
            }
            ParseError::InvalidPayload { event_type, source } => {
                write!(
                    f,
                    "{event_type} payload is missing required fields: {source}"
                )
            }
        }
    }
}

impl std::error::Error for ParseError {}

/// Parse a raw hook JSON blob (stdin) into a [`ParsedEvent`] (spec 07 §2 "parse
/// hook JSON" step).
pub fn parse_hook_event(raw: &[u8]) -> Result<ParsedEvent, ParseError> {
    let value: Value = serde_json::from_slice(raw).map_err(ParseError::Json)?;

    let session_id = value
        .get("session_id")
        .and_then(Value::as_str)
        .ok_or(ParseError::MissingSessionId)?
        .to_string();
    let cwd = value.get("cwd").and_then(Value::as_str).map(str::to_string);
    let event_name = value
        .get("hook_event_name")
        .and_then(Value::as_str)
        .ok_or(ParseError::MissingEventName)?
        .to_string();

    let from_value = |event_type: &'static str| {
        move |source: serde_json::Error| ParseError::InvalidPayload {
            event_type: event_type.to_string(),
            source,
        }
    };

    let kind = match event_name.as_str() {
        "SessionStart" => EventPayload::SessionStart(
            serde_json::from_value(value.clone()).map_err(from_value("SessionStart"))?,
        ),
        "UserPromptSubmit" => EventPayload::UserPromptSubmit(
            serde_json::from_value(value.clone()).map_err(from_value("UserPromptSubmit"))?,
        ),
        "PostToolUse" => EventPayload::PostToolUse(
            serde_json::from_value(value.clone()).map_err(from_value("PostToolUse"))?,
        ),
        "PostToolUseFailure" => EventPayload::PostToolUseFailure(
            serde_json::from_value(value.clone()).map_err(from_value("PostToolUseFailure"))?,
        ),
        "Stop" => {
            EventPayload::Stop(serde_json::from_value(value.clone()).map_err(from_value("Stop"))?)
        }
        "SubagentStop" => EventPayload::SubagentStop(
            serde_json::from_value(value.clone()).map_err(from_value("SubagentStop"))?,
        ),
        "SessionEnd" => EventPayload::SessionEnd(
            serde_json::from_value(value.clone()).map_err(from_value("SessionEnd"))?,
        ),
        other => return Err(ParseError::UnknownEventType(other.to_string())),
    };

    Ok(ParsedEvent {
        session_id,
        cwd,
        kind,
    })
}

/// The `hook_event_name` string this event carries — reused verbatim as the
/// frame's `event_type` field (spec 07 §3).
pub fn event_type_name(kind: &EventPayload) -> &'static str {
    match kind {
        EventPayload::SessionStart(_) => "SessionStart",
        EventPayload::UserPromptSubmit(_) => "UserPromptSubmit",
        EventPayload::PostToolUse(_) => "PostToolUse",
        EventPayload::PostToolUseFailure(_) => "PostToolUseFailure",
        EventPayload::Stop(_) => "Stop",
        EventPayload::SubagentStop(_) => "SubagentStop",
        EventPayload::SessionEnd(_) => "SessionEnd",
    }
}

/// Well-known `tool_input` key names that carry a touched path, scanned
/// generically rather than per-tool: Read/Write/Edit/NotebookEdit use
/// `file_path`, Glob/Grep use `path`, NotebookEdit-specific tooling might use
/// `notebook_path`. Bash/Task/WebFetch/WebSearch carry none of these keys and
/// correctly yield no paths.
///
/// Deliberately **not** parsing path-like substrings out of Bash's `command`
/// string — that's unstructured and error-prone; `SpoolConfig::deny_tools`
/// (T13-01) is the right lever for "never record Bash payloads at all". Since
/// this feeds the deny-list *gate* (12 §2), erring toward more candidates is
/// the safe direction: a false positive here is harmless, a false negative is
/// the security-relevant failure mode.
const PATH_LIKE_KEYS: &[&str] = &["file_path", "path", "notebook_path"];

/// Extract candidate touched-path strings from a `tool_input` value.
pub fn extract_paths(tool_input: Option<&Value>) -> Vec<String> {
    let Some(Value::Object(map)) = tool_input else {
        return Vec::new();
    };
    PATH_LIKE_KEYS
        .iter()
        .filter_map(|k| map.get(*k).and_then(Value::as_str))
        .map(str::to_string)
        .collect()
}

/// The `tool_name`/`tool_input` a [`ParsedEvent`] carries, if any — the
/// event-shape-agnostic input `local_rag_hook::payload::prepare_payload`
/// needs for its deny-list check.
pub fn tool_context(kind: &EventPayload) -> (Option<&str>, Option<&Value>) {
    match kind {
        EventPayload::PostToolUse(p) => (p.tool_name.as_deref(), p.tool_input.as_ref()),
        EventPayload::PostToolUseFailure(p) => (p.tool_name.as_deref(), p.tool_input.as_ref()),
        _ => (None, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_json_is_a_parse_error() {
        assert!(matches!(
            parse_hook_event(b"not json"),
            Err(ParseError::Json(_))
        ));
    }

    #[test]
    fn missing_session_id_is_rejected() {
        let raw = br#"{"hook_event_name":"Stop"}"#;
        assert!(matches!(
            parse_hook_event(raw),
            Err(ParseError::MissingSessionId)
        ));
    }

    #[test]
    fn missing_event_name_is_rejected() {
        let raw = br#"{"session_id":"s"}"#;
        assert!(matches!(
            parse_hook_event(raw),
            Err(ParseError::MissingEventName)
        ));
    }

    #[test]
    fn unknown_event_type_fails_open_forward_compatibly() {
        let raw = br#"{"session_id":"s","hook_event_name":"PreCompact"}"#;
        match parse_hook_event(raw) {
            Err(ParseError::UnknownEventType(name)) => assert_eq!(name, "PreCompact"),
            other => panic!("expected UnknownEventType, got {other:?}"),
        }
    }

    #[test]
    fn post_tool_use_requires_tool_use_id() {
        let raw = br#"{"session_id":"s","hook_event_name":"PostToolUse","tool_name":"Read"}"#;
        assert!(matches!(
            parse_hook_event(raw),
            Err(ParseError::InvalidPayload { .. })
        ));
    }

    #[test]
    fn post_tool_use_parses_with_tool_input() {
        let raw = br#"{"session_id":"s","cwd":"/repo","hook_event_name":"PostToolUse",
            "tool_name":"Read","tool_use_id":"abc123","tool_input":{"file_path":"src/a.ts"}}"#;
        let event = parse_hook_event(raw).expect("valid");
        assert_eq!(event.session_id, "s");
        assert_eq!(event.cwd.as_deref(), Some("/repo"));
        match &event.kind {
            EventPayload::PostToolUse(p) => {
                assert_eq!(p.tool_use_id, "abc123");
                assert_eq!(p.tool_name.as_deref(), Some("Read"));
                assert_eq!(extract_paths(p.tool_input.as_ref()), vec!["src/a.ts"]);
            }
            other => panic!("expected PostToolUse, got {other:?}"),
        }
    }

    #[test]
    fn post_tool_use_failure_is_a_distinct_event_with_tool_use_id() {
        let raw = br#"{"session_id":"s","hook_event_name":"PostToolUseFailure",
            "tool_name":"Bash","tool_use_id":"xyz","tool_error":"exit 1"}"#;
        let event = parse_hook_event(raw).expect("valid");
        assert!(matches!(event.kind, EventPayload::PostToolUseFailure(_)));
        assert_eq!(event_type_name(&event.kind), "PostToolUseFailure");
    }

    #[test]
    fn subagent_stop_requires_agent_id() {
        let raw = br#"{"session_id":"s","hook_event_name":"SubagentStop"}"#;
        assert!(matches!(
            parse_hook_event(raw),
            Err(ParseError::InvalidPayload { .. })
        ));
    }

    #[test]
    fn user_prompt_submit_requires_prompt() {
        let raw = br#"{"session_id":"s","hook_event_name":"UserPromptSubmit"}"#;
        assert!(matches!(
            parse_hook_event(raw),
            Err(ParseError::InvalidPayload { .. })
        ));
    }

    #[test]
    fn stop_and_session_events_have_no_required_fields_beyond_common() {
        for name in ["Stop", "SessionStart", "SessionEnd"] {
            let raw = format!(r#"{{"session_id":"s","hook_event_name":"{name}"}}"#);
            let event = parse_hook_event(raw.as_bytes())
                .unwrap_or_else(|e| panic!("{name} should parse with only common fields: {e}"));
            assert_eq!(event_type_name(&event.kind), name);
        }
    }

    #[test]
    fn extract_paths_covers_known_keys_and_ignores_others() {
        let input = serde_json::json!({"file_path": "a.ts", "unrelated": "x"});
        assert_eq!(extract_paths(Some(&input)), vec!["a.ts"]);

        let input = serde_json::json!({"path": "b.rs"});
        assert_eq!(extract_paths(Some(&input)), vec!["b.rs"]);

        let input = serde_json::json!({"notebook_path": "c.ipynb"});
        assert_eq!(extract_paths(Some(&input)), vec!["c.ipynb"]);

        // Bash-shaped input carries none of the known keys.
        let input = serde_json::json!({"command": "rm -rf /tmp/x"});
        assert_eq!(extract_paths(Some(&input)), Vec::<String>::new());

        assert_eq!(extract_paths(None), Vec::<String>::new());
    }

    #[test]
    fn tool_context_is_none_for_non_tool_events() {
        let raw = br#"{"session_id":"s","hook_event_name":"Stop"}"#;
        let event = parse_hook_event(raw).expect("valid");
        assert_eq!(tool_context(&event.kind), (None, None));
    }
}
