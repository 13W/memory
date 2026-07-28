//! Parses a [`local_rag_embed::GenResponse`]'s raw text into
//! `Vec<`[`crate::schema::RawRouterOp`]`>` (T14-07, spec 08 §4 step 3's
//! "ordered ops list").
//!
//! # Two-tier malformed-output handling
//!
//! 1. **Structural** — the text does not deserialize as a JSON array of
//!    [`RawRouterOp`] at all (extra prose, a missing required field, an
//!    unknown `op`/enum value, ...). [`parse_ops`] returns
//!    [`ParseError`]; [`crate::router::route`] sends exactly one corrective
//!    re-prompt (spec 04 §4's "router/LLM error ⇒ failed (retryable)" edge,
//!    generalized the same way `local_rag_store::memory::runner`'s own
//!    module doc already generalizes "any apply-time rejection") before
//!    giving up on the *whole* window — a bounded retry, not an unbounded
//!    loop, so a deterministically-broken response cannot livelock.
//! 2. **Referential** — the JSON is well-formed and every field is the right
//!    *type*, but a value doesn't resolve to anything real (a
//!    `target_memory_id` recall never found, a `kind`/`scope_kind` string
//!    outside its domain, ...). This never reaches [`parse.rs`] as an
//!    error — it degrades the *one* affected op via [`crate::guard`], never
//!    the whole batch. See that module's doc for why forwarding an
//!    unresolvable reference downstream is unsafe (a batch-wide rollback
//!    that reproduces on every deterministic retry).
//!
//! [`extract_json_array`]'s best-effort bracket search exists because a
//! small local model occasionally wraps otherwise-valid JSON in prose or a
//! markdown code fence despite the system prompt's explicit instruction not
//! to — recovering that case costs nothing and avoids burning the one
//! corrective re-prompt on a trivially fixable response.

use crate::schema::RawRouterOp;

/// Why [`parse_ops`] could not extract any ops at all (tier 1 — see the
/// module doc).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError(String);

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "router output did not parse as a JSON ops array: {}",
            self.0
        )
    }
}

impl std::error::Error for ParseError {}

/// Parse `text` as a JSON array of [`RawRouterOp`]. Tries a direct parse
/// first; on failure, falls back to the first `[...]`-bracketed substring
/// (see the module doc) before giving up.
pub fn parse_ops(text: &str) -> Result<Vec<RawRouterOp>, ParseError> {
    match serde_json::from_str::<Vec<RawRouterOp>>(text) {
        Ok(ops) => Ok(ops),
        Err(direct_err) => {
            if let Some(slice) = extract_json_array(text)
                && let Ok(ops) = serde_json::from_str::<Vec<RawRouterOp>>(slice)
            {
                return Ok(ops);
            }
            Err(ParseError(direct_err.to_string()))
        }
    }
}

/// The first `[` to the last `]` in `text`, if both exist and are ordered —
/// a best-effort recovery for a response wrapped in prose/markdown fences
/// (see the module doc). Not a real parser: a `]` inside a string value
/// before the array's own close would still be handled correctly, because
/// the *substring* is then handed to `serde_json` for real parsing, not
/// trusted as-is.
fn extract_json_array(text: &str) -> Option<&str> {
    let start = text.find('[')?;
    let end = text.rfind(']')?;
    if end < start {
        return None;
    }
    Some(&text[start..=end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_array_parses_directly() {
        let ops = parse_ops(r#"[{"op":"noop"}]"#).expect("valid array");
        assert_eq!(ops.len(), 1);
    }

    #[test]
    fn an_array_wrapped_in_a_markdown_fence_is_recovered() {
        let text = "```json\n[{\"op\":\"noop\"}]\n```";
        let ops = parse_ops(text).expect("recovered array");
        assert_eq!(ops.len(), 1);
    }

    #[test]
    fn an_array_wrapped_in_prose_is_recovered() {
        let text = "Sure, here is the array: [{\"op\":\"noop\"}] Hope that helps!";
        let ops = parse_ops(text).expect("recovered array");
        assert_eq!(ops.len(), 1);
    }

    #[test]
    fn pure_prose_with_no_brackets_is_a_parse_error() {
        let err = parse_ops("I cannot help with that.").expect_err("no array present");
        assert!(err.to_string().contains("did not parse"));
    }

    #[test]
    fn an_empty_array_is_valid_and_empty() {
        let ops = parse_ops("[]").expect("valid empty array");
        assert!(ops.is_empty());
    }

    #[test]
    fn a_single_malformed_element_fails_the_whole_response() {
        // "reinforce" requires target_memory_id -- see schema.rs's own
        // `reinforce_requires_a_target_memory_id` unit test. A missing
        // required field is tier 1 (see the module doc), not something a
        // per-op degradation should silently paper over.
        let err = parse_ops(r#"[{"op":"reinforce"}]"#).expect_err("missing target_memory_id");
        assert!(!err.to_string().is_empty());
    }

    #[test]
    fn mismatched_brackets_do_not_panic_and_fall_through_to_an_error() {
        let err = parse_ops("] not an array [").expect_err("reversed brackets");
        assert!(!err.to_string().is_empty());
    }
}
