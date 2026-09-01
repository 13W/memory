//! Parses a [`local_rag_embed::GenResponse`]'s raw text into
//! `Vec<`[`crate::schema::RawRouterOp`]`>` (T14-07, spec 08 §4 step 3's
//! "ordered ops list") — one [`RawRouterOp`] JSON object per line (JSONL,
//! D-051), not a single top-level `[...]` array.
//!
//! # Three-tier malformed-output handling
//!
//! 1. **Hard failure** — the *first* non-empty line does not deserialize as
//!    a [`RawRouterOp`] at all (extra prose, a missing required field, an
//!    unknown `op`/enum value, ...). [`parse_ops`] returns [`ParseError`];
//!    [`crate::router::route`] sends exactly one corrective re-prompt (spec
//!    04 §4's "router/LLM error ⇒ failed (retryable)" edge, generalized the
//!    same way `local_rag_store::memory::runner`'s own module doc already
//!    generalizes "any apply-time rejection") before giving up on the
//!    *whole* window — a bounded retry, not an unbounded loop, so a
//!    deterministically-broken response cannot livelock.
//! 2. **Partial recovery** — one or more leading lines parse cleanly, then a
//!    later line does not (trailing prose/garbage after an otherwise
//!    complete response, or a truncated final line at the router's answer
//!    reserve boundary (`local_rag_memory::budget::PromptBudget::
//!    answer_reserve_tokens`, `T23-06`) — both observed live incidents,
//!    D-050's own evidence). [`parse_ops`] returns `Ok(`[`ParseOutcome`]`)` with the
//!    successfully parsed *prefix* and `dropped_tail` naming why recovery
//!    stopped there — this is the whole reason JSONL replaced a single JSON
//!    array (D-051): under the old array framing, one bad trailing element
//!    invalidated deserialization of the *entire* response, including every
//!    syntactically valid element before it. [`crate::router::route`] does
//!    **not** spend its one corrective re-prompt trying to recover the
//!    dropped tail — a live incident's own corrective retry reproduced the
//!    identical truncation, byte-for-byte, because nothing about a second,
//!    otherwise-identical generation call changes a deterministic (greedy)
//!    outcome. The recovered prefix is accepted as this window's ops.
//! 3. **Referential** — the JSON is well-formed and every field is the right
//!    *type*, but a value doesn't resolve to anything real (a
//!    `target_memory_id` recall never found, a `kind`/`scope_kind` string
//!    outside its domain, a missing `confidence_signal`/`importance_signal`,
//!    ...). This never reaches `parse.rs` as an error — it degrades the
//!    *one* affected op via [`crate::guard`], never the whole batch, and
//!    never drops anything after it. See that module's doc for why
//!    forwarding an unresolvable reference downstream is unsafe (a
//!    batch-wide rollback that reproduces on every deterministic retry).
//!
//! [`strip_markdown_fence`]'s best-effort unwrap exists because a small
//! local model occasionally wraps an otherwise-valid response in a markdown
//! code fence despite the system prompt's explicit instruction not to —
//! recovering that case costs nothing and avoids burning the one corrective
//! re-prompt (tier 1) on a trivially fixable response. It is deliberately
//! **not** a search for JSON content anywhere in the text: only a single
//! fence wrapping the *whole* response is unwrapped, never prose mixed in
//! around valid lines — see the module's own tests for the boundary this
//! draws (a genuinely prose-prefixed response is a tier-1 hard failure, not
//! recovered, unlike the pre-D-051 `extract_json_array`'s wider search).

use crate::schema::RawRouterOp;

/// Why [`parse_ops`] could not extract any ops at all (tier 1 — see the
/// module doc): the first non-empty line was not a valid [`RawRouterOp`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError(String);

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "router output did not parse as a JSONL ops stream: {}",
            self.0
        )
    }
}

impl std::error::Error for ParseError {}

/// A successful [`parse_ops`] call (tier 1 passed) — possibly a **partial**
/// recovery (tier 2, D-051): see the module doc.
///
/// No `PartialEq`/`Eq`: [`RawRouterOp`] itself derives neither (it has no
/// need to — nothing compares two of them for equality outside tests, which
/// compare individual fields instead).
#[derive(Debug, Clone)]
pub struct ParseOutcome {
    /// Every op successfully parsed, in order, up to (not including)
    /// [`dropped_tail`](Self::dropped_tail)'s line if any.
    pub ops: Vec<RawRouterOp>,
    /// `Some(reason)` when a non-empty line existed after the last
    /// successfully parsed op but could not itself be parsed — trailing
    /// garbage, or a truncated final line. `None` when every line parsed
    /// cleanly (including the legitimate zero-ops case: an empty or
    /// whitespace-only response).
    pub dropped_tail: Option<String>,
}

/// Parse `text` as JSONL: one [`RawRouterOp`] per non-empty line (D-051),
/// stopping at the first line that fails to parse rather than either
/// rejecting the whole response (tier 1, only when *no* line parsed yet) or
/// silently skipping around a bad line to keep scanning (never done — an
/// unobserved failure shape this project has no evidence would still mean
/// "the rest is trustworthy"; see the module doc's tier 2).
pub fn parse_ops(text: &str) -> Result<ParseOutcome, ParseError> {
    let text = strip_markdown_fence(text);
    let mut ops = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<RawRouterOp>(line) {
            Ok(op) => ops.push(op),
            Err(e) => {
                if ops.is_empty() {
                    return Err(ParseError(e.to_string()));
                }
                return Ok(ParseOutcome {
                    ops,
                    dropped_tail: Some(e.to_string()),
                });
            }
        }
    }
    Ok(ParseOutcome {
        ops,
        dropped_tail: None,
    })
}

/// Unwrap a single markdown code fence wrapping the *whole* response (see
/// the module doc) — `` ````` or `` ```json `` on its own opening line, a
/// closing `` ``` `` somewhere after it. Not found (no fence, or a fence
/// with no matching close — e.g. the close itself got truncated) → `text`
/// unchanged, left to the normal per-line parse below.
pub(crate) fn strip_markdown_fence(text: &str) -> &str {
    let trimmed = text.trim();
    let Some(after_open) = trimmed.strip_prefix("```") else {
        return text;
    };
    let after_open = match after_open.find('\n') {
        Some(newline) => &after_open[newline + 1..],
        None => return text,
    };
    match after_open.rfind("```") {
        Some(close) => &after_open[..close],
        None => after_open,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_single_line_parses_directly() {
        let outcome = parse_ops(r#"{"op":"noop"}"#).expect("valid line");
        assert_eq!(outcome.ops.len(), 1);
        assert_eq!(outcome.dropped_tail, None);
    }

    #[test]
    fn multiple_lines_all_parse_in_order() {
        let text = "{\"op\":\"noop\"}\n{\"op\":\"retract\",\"target_memory_id\":\"m-1\"}";
        let outcome = parse_ops(text).expect("both lines valid");
        assert_eq!(outcome.ops.len(), 2);
        assert!(matches!(outcome.ops[0], RawRouterOp::Noop { .. }));
        assert!(matches!(outcome.ops[1], RawRouterOp::Retract { .. }));
        assert_eq!(outcome.dropped_tail, None);
    }

    #[test]
    fn blank_lines_between_ops_are_skipped() {
        let text = "{\"op\":\"noop\"}\n\n\n{\"op\":\"retract\",\"target_memory_id\":\"m-1\"}\n";
        let outcome = parse_ops(text).expect("blank lines ignored");
        assert_eq!(outcome.ops.len(), 2);
    }

    #[test]
    fn an_empty_response_is_valid_and_empty() {
        let outcome = parse_ops("").expect("empty is a legitimate zero-ops response");
        assert!(outcome.ops.is_empty());
        assert_eq!(outcome.dropped_tail, None);
    }

    #[test]
    fn a_whitespace_only_response_is_valid_and_empty() {
        let outcome = parse_ops("   \n  \n").expect("whitespace-only is empty, not an error");
        assert!(outcome.ops.is_empty());
        assert_eq!(outcome.dropped_tail, None);
    }

    /// Tier 1: the first (only) line is not valid JSON at all.
    #[test]
    fn pure_prose_with_no_valid_first_line_is_a_parse_error() {
        let err = parse_ops("I cannot help with that.").expect_err("no valid line at all");
        assert!(err.to_string().contains("did not parse"));
    }

    /// Tier 1: a single malformed op (missing a required field) still fails
    /// outright — there is no earlier successfully parsed line to fall back
    /// to a partial recovery from.
    #[test]
    fn a_single_malformed_line_is_a_parse_error() {
        // "reinforce" requires target_memory_id -- see schema.rs's own
        // `reinforce_requires_a_target_memory_id` unit test.
        let err = parse_ops(r#"{"op":"reinforce"}"#).expect_err("missing target_memory_id");
        assert!(!err.to_string().is_empty());
    }

    /// Tier 2 (D-051's own reason for existing): trailing garbage after a
    /// complete, valid line — the live `019feca9…` incident's exact shape
    /// (`trailing characters at line 2 column 1`). The valid prefix is kept.
    #[test]
    fn trailing_garbage_after_a_valid_line_recovers_the_prefix() {
        let text = "{\"op\":\"noop\"}\nnot valid json at all";
        let outcome = parse_ops(text).expect("prefix recovered despite the trailing garbage");
        assert_eq!(outcome.ops.len(), 1);
        assert!(outcome.dropped_tail.is_some());
    }

    /// Tier 2: a truncated final line (the `019fee04…` incident's exact
    /// shape — `EOF while parsing a string` mid-way through a `text` field).
    /// The valid prefix is kept, the incomplete line is dropped, not
    /// force-completed or guessed at.
    #[test]
    fn a_truncated_trailing_line_recovers_the_prefix() {
        let text = "{\"op\":\"noop\"}\n{\"op\":\"create\",\"kind\":\"fact\",\"text\":\"uses p";
        let outcome = parse_ops(text).expect("prefix recovered despite the truncated tail");
        assert_eq!(outcome.ops.len(), 1);
        assert!(matches!(outcome.ops[0], RawRouterOp::Noop { .. }));
        assert!(outcome.dropped_tail.is_some());
    }

    #[test]
    fn a_response_wrapped_in_a_plain_markdown_fence_is_recovered() {
        let text = "```\n{\"op\":\"noop\"}\n```";
        let outcome = parse_ops(text).expect("fence unwrapped");
        assert_eq!(outcome.ops.len(), 1);
        assert_eq!(outcome.dropped_tail, None);
    }

    #[test]
    fn a_response_wrapped_in_a_json_tagged_markdown_fence_is_recovered() {
        let text =
            "```json\n{\"op\":\"noop\"}\n{\"op\":\"retract\",\"target_memory_id\":\"m-1\"}\n```";
        let outcome = parse_ops(text).expect("fence unwrapped");
        assert_eq!(outcome.ops.len(), 2);
    }

    /// An opening fence with no matching close (e.g. the close itself got
    /// truncated at the router's answer reserve) still has its opening line
    /// stripped -- only the *closing* fence is optional, since a missing
    /// close is exactly the truncation shape D-051 exists to tolerate. The
    /// content on the lines that follow is then parsed on its own merits.
    #[test]
    fn an_unterminated_fence_still_strips_its_own_opening_line() {
        let text = "```json\n{\"op\":\"noop\"}";
        let outcome = parse_ops(text).expect("opening fence line stripped, content parses");
        assert_eq!(outcome.ops.len(), 1);
    }

    /// D-051's own documented tradeoff (see the module doc): unlike the
    /// pre-D-051 `extract_json_array`, prefix-stop recovery does not search
    /// for valid content *after* leading prose -- a response that opens
    /// with commentary before any valid line is a tier-1 hard failure, not
    /// recovered. This is intentional, not a regression nobody noticed.
    #[test]
    fn leading_prose_before_a_valid_line_is_not_recovered() {
        let text = "Sure, here you go:\n{\"op\":\"noop\"}";
        let err = parse_ops(text).expect_err("leading prose is out of scope for recovery");
        assert!(!err.to_string().is_empty());
    }
}
