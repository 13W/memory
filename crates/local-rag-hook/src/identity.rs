//! Table-driven `source_event_id`/`dedup_key` computation (spec 07 §4) and the
//! `evidence_kind`/`trust` classification each event type gets in the frame.
//!
//! ```text
//! PostToolUse         pt:<session>:<tool_use_id>:ok    stable (dedup_key = same)
//! PostToolUseFailure  pt:<session>:<tool_use_id>:fail  stable (dedup_key = same)
//! SubagentStop        ss:<session>:<agent_id>:<n>      stable (dedup_key = same)
//! UserPromptSubmit    up:<session>:<H(prompt)>:<ts>    best-effort (dedup_key = null)
//! Stop                st:<session>:<H(context)>:<ts>   best-effort (dedup_key = null)
//! SessionStart/End    se:<session>:start/end:<ts>      best-effort (dedup_key = null)
//! ```
//!
//! # `H(prompt)`/`H(context)`: plain `sha256_hex`, not a new `Domain::` variant
//!
//! `local_rag_core::identity::domain`'s domain-separated BLAKE3 family is
//! reserved for values backing a durable, retry-stable, schema-level identity
//! — an FK target or a UNIQUE lookup key (every existing `Domain` variant does
//! exactly that). These fingerprints are one segment of a compound string
//! that is **explicitly never** under a UNIQUE constraint (`[FIXED]`, spec 07
//! §4) and never itself a stored identity column. This is the same shape as
//! `subject_memory_entry`'s own inner `H(text)`, which is documented as
//! deliberately using plain `sha256_hex` — "no domain exists for raw memory
//! text... not a spec 03 §1.2 content-identity domain" — for the same reason.
//!
//! # `coarse_ts`: 1-second buckets
//!
//! `coarse_ts = captured_at_ms / 1000`. Coarse enough to absorb a duplicate
//! hook invocation for the same real event landing within the same second,
//! without widening the false-collision window meaningfully beyond what the
//! *separate*, later import-side bounded dedup window (10 min / 512
//! envelopes, spec 07 §5) already tolerates by design (spec 07 §4 `[FIXED]`:
//! "two legitimate identical prompts... are legal"). No principled derivation
//! fixes this number exactly; 1 second is this task's concrete `[SPEC]` pick.

use local_rag_core::hash::sha256_hex;

use crate::event::{EventPayload, ParsedEvent};

/// The identity a frame embeds: `source_event_id` always, `dedup_key` only for
/// stable-identity events (spec 07 §4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub source_event_id: String,
    pub dedup_key: Option<String>,
}

/// `compute_identity` could not proceed.
#[derive(Debug)]
pub enum IdentityError {
    /// A `SubagentStop` event with no `stop_occurrence` supplied — the caller
    /// must obtain one from [`crate::subagent_counter::next_stop_occurrence`]
    /// before calling this function for that event.
    MissingStopOccurrence,
}

impl std::fmt::Display for IdentityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IdentityError::MissingStopOccurrence => {
                write!(f, "SubagentStop identity requires a stop_occurrence")
            }
        }
    }
}

impl std::error::Error for IdentityError {}

/// `coarse_ts` — see module docs for the 1-second bucket rationale.
pub fn coarse_ts(captured_at_ms: i64) -> i64 {
    captured_at_ms.div_euclid(1000)
}

/// Compute the `source_event_id`/`dedup_key` pair for `event` (spec 07 §4).
///
/// `stop_occurrence` is required (and used) only for `SubagentStop`; every
/// other event type ignores it.
pub fn compute_identity(
    event: &ParsedEvent,
    coarse_ts: i64,
    stop_occurrence: Option<u64>,
) -> Result<Identity, IdentityError> {
    let s = &event.session_id;
    let (source_event_id, stable) = match &event.kind {
        EventPayload::PostToolUse(p) => (format!("pt:{s}:{}:ok", p.tool_use_id), true),
        EventPayload::PostToolUseFailure(p) => (format!("pt:{s}:{}:fail", p.tool_use_id), true),
        EventPayload::SubagentStop(p) => {
            let occ = stop_occurrence.ok_or(IdentityError::MissingStopOccurrence)?;
            (format!("ss:{s}:{}:{occ}", p.agent_id), true)
        }
        EventPayload::UserPromptSubmit(p) => {
            let h = sha256_hex(p.prompt.as_bytes());
            (format!("up:{s}:{h}:{coarse_ts}"), false)
        }
        EventPayload::Stop(p) => {
            let context = p.last_assistant_message.as_deref().unwrap_or("");
            let h = sha256_hex(context.as_bytes());
            (format!("st:{s}:{h}:{coarse_ts}"), false)
        }
        EventPayload::SessionStart(_) => (format!("se:{s}:start:{coarse_ts}"), false),
        EventPayload::SessionEnd(_) => (format!("se:{s}:end:{coarse_ts}"), false),
    };
    Ok(Identity {
        dedup_key: stable.then(|| source_event_id.clone()),
        source_event_id,
    })
}

/// `(evidence_kind, trust)` for `kind` (spec 03 §2.5's `observation_envelope`
/// `NOT NULL CHECK` columns — not provided by Claude Code's own hook JSON,
/// this project's own classification).
///
/// - `PostToolUse`/`PostToolUseFailure` → `tool_result`/`normal`: an objective
///   record of what a tool did or failed to do — not a subjective claim, but
///   not independently verified truth either (a tool's own output can be
///   wrong), so `normal` rather than `high`.
/// - `UserPromptSubmit` → `user_statement`/`high`: the user is the most
///   authoritative, unmediated source available — no model inference sits
///   between "what was said" and its capture.
/// - `Stop`/`SubagentStop` → `model_claim`/`low`: both carry the model's own
///   generated text (`last_assistant_message`); directly justified by spec 12
///   §4 `[FIXED]` "model-claims are never auto-promoted to facts".
/// - `SessionStart`/`SessionEnd` → `code_state`/`normal`: by elimination among
///   the five fixed values — no tool ran, no party "stated" or "claimed"
///   anything; `source`/`reason` are lifecycle metadata about the runtime
///   environment.
pub fn evidence_kind_and_trust(kind: &EventPayload) -> (&'static str, &'static str) {
    match kind {
        EventPayload::PostToolUse(_) | EventPayload::PostToolUseFailure(_) => {
            ("tool_result", "normal")
        }
        EventPayload::UserPromptSubmit(_) => ("user_statement", "high"),
        EventPayload::Stop(_) | EventPayload::SubagentStop(_) => ("model_claim", "low"),
        EventPayload::SessionStart(_) | EventPayload::SessionEnd(_) => ("code_state", "normal"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::parse_hook_event;

    fn event(json: &str) -> ParsedEvent {
        parse_hook_event(json.as_bytes()).expect("valid event")
    }

    #[test]
    fn post_tool_use_is_stable_ok() {
        let e = event(r#"{"session_id":"s1","hook_event_name":"PostToolUse","tool_use_id":"t1"}"#);
        let id = compute_identity(&e, 0, None).unwrap();
        assert_eq!(id.source_event_id, "pt:s1:t1:ok");
        assert_eq!(id.dedup_key.as_deref(), Some("pt:s1:t1:ok"));
    }

    #[test]
    fn post_tool_use_failure_is_stable_fail() {
        let e = event(
            r#"{"session_id":"s1","hook_event_name":"PostToolUseFailure","tool_use_id":"t1"}"#,
        );
        let id = compute_identity(&e, 0, None).unwrap();
        assert_eq!(id.source_event_id, "pt:s1:t1:fail");
        assert_eq!(id.dedup_key.as_deref(), Some("pt:s1:t1:fail"));
    }

    #[test]
    fn subagent_stop_is_stable_and_needs_an_occurrence() {
        let e = event(r#"{"session_id":"s1","hook_event_name":"SubagentStop","agent_id":"a1"}"#);
        assert!(matches!(
            compute_identity(&e, 0, None),
            Err(IdentityError::MissingStopOccurrence)
        ));
        let id = compute_identity(&e, 0, Some(3)).unwrap();
        assert_eq!(id.source_event_id, "ss:s1:a1:3");
        assert_eq!(id.dedup_key.as_deref(), Some("ss:s1:a1:3"));
    }

    #[test]
    fn user_prompt_submit_is_best_effort_and_hashes_the_prompt() {
        let e =
            event(r#"{"session_id":"s1","hook_event_name":"UserPromptSubmit","prompt":"hello"}"#);
        let id = compute_identity(&e, 42, None).unwrap();
        let expected_hash = sha256_hex(b"hello");
        assert_eq!(id.source_event_id, format!("up:s1:{expected_hash}:42"));
        assert_eq!(id.dedup_key, None);
    }

    #[test]
    fn identical_prompts_share_a_source_event_id_but_stay_best_effort() {
        let e1 = event(
            r#"{"session_id":"s1","hook_event_name":"UserPromptSubmit","prompt":"same text"}"#,
        );
        let e2 = event(
            r#"{"session_id":"s1","hook_event_name":"UserPromptSubmit","prompt":"same text"}"#,
        );
        let id1 = compute_identity(&e1, 100, None).unwrap();
        let id2 = compute_identity(&e2, 100, None).unwrap();
        assert_eq!(id1.source_event_id, id2.source_event_id);
        assert_eq!(id1.dedup_key, None);
        assert_eq!(id2.dedup_key, None);
    }

    #[test]
    fn stop_hashes_last_assistant_message_as_context() {
        let e = event(
            r#"{"session_id":"s1","hook_event_name":"Stop","last_assistant_message":"done"}"#,
        );
        let id = compute_identity(&e, 7, None).unwrap();
        let expected_hash = sha256_hex(b"done");
        assert_eq!(id.source_event_id, format!("st:s1:{expected_hash}:7"));
        assert_eq!(id.dedup_key, None);
    }

    #[test]
    fn stop_with_no_message_hashes_empty_context() {
        let e = event(r#"{"session_id":"s1","hook_event_name":"Stop"}"#);
        let id = compute_identity(&e, 7, None).unwrap();
        assert_eq!(id.source_event_id, format!("st:s1:{}:7", sha256_hex(b"")));
    }

    #[test]
    fn session_start_and_end_are_best_effort() {
        let start = event(r#"{"session_id":"s1","hook_event_name":"SessionStart"}"#);
        let end = event(r#"{"session_id":"s1","hook_event_name":"SessionEnd"}"#);
        assert_eq!(
            compute_identity(&start, 5, None).unwrap().source_event_id,
            "se:s1:start:5"
        );
        assert_eq!(
            compute_identity(&end, 5, None).unwrap().source_event_id,
            "se:s1:end:5"
        );
        assert_eq!(compute_identity(&start, 5, None).unwrap().dedup_key, None);
    }

    #[test]
    fn coarse_ts_buckets_by_the_second() {
        assert_eq!(coarse_ts(0), 0);
        assert_eq!(coarse_ts(999), 0);
        assert_eq!(coarse_ts(1000), 1);
        assert_eq!(coarse_ts(1999), 1);
        assert_eq!(coarse_ts(2000), 2);
    }

    #[test]
    fn evidence_kind_and_trust_mapping() {
        let cases = [
            (
                r#"{"session_id":"s","hook_event_name":"PostToolUse","tool_use_id":"t"}"#,
                "tool_result",
                "normal",
            ),
            (
                r#"{"session_id":"s","hook_event_name":"PostToolUseFailure","tool_use_id":"t"}"#,
                "tool_result",
                "normal",
            ),
            (
                r#"{"session_id":"s","hook_event_name":"UserPromptSubmit","prompt":"p"}"#,
                "user_statement",
                "high",
            ),
            (
                r#"{"session_id":"s","hook_event_name":"Stop"}"#,
                "model_claim",
                "low",
            ),
            (
                r#"{"session_id":"s","hook_event_name":"SubagentStop","agent_id":"a"}"#,
                "model_claim",
                "low",
            ),
            (
                r#"{"session_id":"s","hook_event_name":"SessionStart"}"#,
                "code_state",
                "normal",
            ),
            (
                r#"{"session_id":"s","hook_event_name":"SessionEnd"}"#,
                "code_state",
                "normal",
            ),
        ];
        for (json, expected_kind, expected_trust) in cases {
            let e = event(json);
            assert_eq!(
                evidence_kind_and_trust(&e.kind),
                (expected_kind, expected_trust)
            );
        }
    }
}
