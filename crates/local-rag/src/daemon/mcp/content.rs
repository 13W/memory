//! MCP `tools/call` result content (`CallToolResult`) and the `isError`
//! mapping from this daemon's own canonical error vocabulary (spec 02 §6:
//! "MCP tools map `code` into `isError` content with the same code
//! string") — T15-03.

use serde::Serialize;

use local_rag_protocol::ErrorEnvelope;
use local_rag_search::SearchInfraError;

/// The result of one `tools/call` — a JSON-RPC **success** response whose
/// `result` is this value; `isError` is MCP's own in-band failure signal,
/// not a JSON-RPC-level error (see `dispatch`'s module doc for the split
/// between the two error channels).
#[derive(Debug, Serialize)]
pub struct CallToolResult {
    pub content: Vec<Content>,
    #[serde(rename = "isError")]
    pub is_error: bool,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Content {
    Text { text: String },
}

/// A successful tool result: `value`'s compact JSON as the sole content
/// item, `isError: false`.
pub fn ok(value: &impl Serialize) -> CallToolResult {
    let text = serde_json::to_string(value).expect("domain values always serialize");
    CallToolResult {
        content: vec![Content::Text { text }],
        is_error: false,
    }
}

/// A domain failure (spec 02 §6's canonical taxonomy): `envelope`'s compact
/// JSON, `isError: true`.
pub fn err(envelope: &ErrorEnvelope) -> CallToolResult {
    let text = serde_json::to_string(envelope).expect("ErrorEnvelope always serializes");
    CallToolResult {
        content: vec![Content::Text { text }],
        is_error: true,
    }
}

/// An infrastructure failure — `state.sqlite`/`cache.sqlite` would not open,
/// a corrupt `worktree_id`, a missing generation row. Never a JSON-RPC
/// `-32603 Internal error` (indistinguishable from a server bug to the
/// model reading it) and never a panic or a dropped connection: folded into
/// the same `isError` shape as a domain failure, under
/// [`ErrorEnvelope::index_unavailable`] — spec 02 §6's own name for "the
/// index cannot serve this request", which is exactly what every
/// `SearchInfraError` variant means. `retryable: false` is correct here: an
/// immediate retry against a broken store observes the same failure.
pub fn infra_err(e: SearchInfraError) -> CallToolResult {
    err(&ErrorEnvelope::index_unavailable(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize)]
    struct Dummy {
        n: u32,
    }

    #[test]
    fn ok_wraps_compact_json_with_is_error_false() {
        let result = ok(&Dummy { n: 1 });
        assert!(!result.is_error);
        assert_eq!(result.content.len(), 1);
        let Content::Text { text } = &result.content[0];
        assert_eq!(text, "{\"n\":1}");
    }

    #[test]
    fn err_wraps_the_envelope_with_is_error_true() {
        let envelope = ErrorEnvelope::worktree_not_indexed();
        let result = err(&envelope);
        assert!(result.is_error);
        let Content::Text { text } = &result.content[0];
        assert!(text.contains("\"code\":\"WORKTREE_NOT_INDEXED\""), "{text}");
    }

    #[test]
    fn call_tool_result_serializes_with_is_error_camel_case() {
        let body = serde_json::to_string(&ok(&Dummy { n: 7 })).unwrap();
        assert_eq!(
            body,
            "{\"content\":[{\"type\":\"text\",\"text\":\"{\\\"n\\\":7}\"}],\"isError\":false}"
        );
    }

    #[test]
    fn every_error_code_maps_through_is_error_with_the_same_code_string() {
        let envelopes = [
            ErrorEnvelope::index_unavailable("x"),
            ErrorEnvelope::worktree_not_indexed(),
            ErrorEnvelope::unsupported_mode("semantic"),
            ErrorEnvelope::path_not_indexed("a.rs", "never seen"),
            ErrorEnvelope::busy_retry(),
            ErrorEnvelope::migration_in_progress(),
            ErrorEnvelope::store_locked(1, "u"),
            ErrorEnvelope::incompatible_store("x"),
        ];
        for envelope in envelopes {
            let result = err(&envelope);
            assert!(result.is_error);
            let Content::Text { text } = &result.content[0];
            let expected_code = format!("\"code\":\"{}\"", envelope.code.as_str());
            assert!(text.contains(&expected_code), "{text}");
        }
    }

    #[test]
    fn infra_err_maps_to_index_unavailable_is_error_true() {
        let result = infra_err(SearchInfraError::CorruptWorktreeId(
            "not-a-uuid".to_string(),
        ));
        assert!(result.is_error);
        let Content::Text { text } = &result.content[0];
        assert!(text.contains("\"code\":\"INDEX_UNAVAILABLE\""), "{text}");
        assert!(text.contains("\"retryable\":false"), "{text}");
        assert!(text.contains("not-a-uuid"), "{text}");
    }
}
