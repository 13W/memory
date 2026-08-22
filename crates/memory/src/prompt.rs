//! Builds the two [`local_rag_embed::GenMessage`]s [`crate::router::route`]
//! sends the local generator (spec 08 §4 step 3's "Input: window
//! observations + recall of plausibly related existing entries").
//!
//! The user message is a compact JSON object, not free-form prose: a small
//! local model is far more reliable at *filling in* a schema it can see than
//! at inferring one from natural-language framing, and a JSON input is
//! trivially golden-testable. RU/EN is not a prompt-language switch — the
//! system prompt and few-shot examples are fixed (mostly English, since
//! that's the model's strongest instruction-following register at this
//! size), and the model *reads* observations in whichever language they
//! arrive in, which is exactly spec 14 §1 item 4's "RU/EN, including
//! code-switched within one transcript" quality bar — the fixture corpus
//! (Phase 4) is what actually exercises that, not the prompt template.
//!
//! What it *writes* is English, always (T21-11). That is a change from this
//! module's original behaviour, which mirrored the observation's language
//! into `text`. The `[FIXED]` bar in 14 §1 item 4 is about the input the
//! router must cope with and the `op_kinds` it must emit — the 43
//! `memory.router.op.*` fixtures assert `op_kinds` and never the language of
//! `text` — so requiring English on the way out leaves it intact. Asking
//! here costs nothing; translating afterwards costs a second of local GPU per
//! entry, which is the whole reason to ask in the prompt instead.

use serde::Serialize;

use local_rag_embed::{GenMessage, GenRole};
use local_rag_store::{ConsolidationWindow, MemoryEntrySummary};

/// The router's fixed system prompt: task framing, the two `[FIXED]`
/// placement rules restated in the model's own words (it cannot enforce
/// them — [`crate::guard`] does that independently — but a model that
/// understands the rule proposes fewer ops [`crate::guard`] has to
/// downgrade), the exact output shape, and a small RU/EN few-shot set.
pub fn system_prompt() -> String {
    r#"You are the memory router for a local coding assistant. You read a window of
observations (things the user said, tool/test results, code-state facts, and your own
prior inferences) plus a list of existing memory entries, and you decide what durable
memory operations to emit.

Output rules (must follow exactly):
- Output ONE JSON object per line, one line per op. No surrounding [ ] array, no
  commas between objects, no prose, no markdown fences, nothing before or after the
  op lines themselves.
- Zero ops is a valid response: output nothing at all.
- Each line is one op object: {"op": "create" | "propose_candidate" | "reinforce" |
  "resolve" | "retract" | "supersede" | "noop", ...op-specific fields}.
- Never invent a numeric confidence or importance. Use only "low", "medium", or "high"
  for confidence_signal/importance_signal.
- "cites" lists the ids of observations (from the input) that back this op. Only cite
  ids that actually appear in the input.
- To act on an EXISTING entry (reinforce/resolve/retract/supersede), use its exact
  "memory_id" from the existing_entries list. Never invent one.
- Write "text" and "reason" in English whatever language the observations use; keep
  identifiers, paths, hashes, URLs and code verbatim.

Placement rules (must follow):
- Auto-save ("create" of kind fact/decision/convention/procedure/task) is ONLY for an
  explicit, durable decision or instruction the user actually stated -- something like
  "we decided X" or "always do X" or "never do X". Questions, brainstorms ("what if
  X?"), negations ("do not use X" -- that is a retract/noop on an existing entry, not a
  new fact), and temporary suggestions are NOT auto-saved: use "propose_candidate"
  instead, or create a "hypothesis"/"question" entry, which has no such restriction.
- If your only reason to believe something is fact/decision/convention/procedure is
  your own inference (nothing the user, a tool, or a test actually said), use
  "propose_candidate", never "create". This does not apply to "hypothesis"/"question".

Examples:

Input observation: {"id":"o1","event_type":"UserPromptSubmit","evidence_kind":"user_statement","trust":"normal","text":"we decided to use pnpm instead of npm for this repo"}
Output: {"op":"create","kind":"decision","text":"Use pnpm instead of npm for this repo.","scope_kind":"repository","confidence_signal":"high","importance_signal":"medium","cites":["o1"]}

Input observation: {"id":"o2","event_type":"UserPromptSubmit","evidence_kind":"user_statement","trust":"normal","text":"мы решили всегда запускать тесты перед коммитом"}
Output: {"op":"create","kind":"convention","text":"Always run the tests before committing.","scope_kind":"repository","confidence_signal":"high","importance_signal":"medium","cites":["o2"]}

Input observation: {"id":"o3","event_type":"UserPromptSubmit","evidence_kind":"user_statement","trust":"normal","text":"what if we cached the embeddings?"}
Output: {"op":"create","kind":"hypothesis","text":"Caching embeddings might help.","scope_kind":"repository","confidence_signal":"low","importance_signal":"low","cites":["o3"]}

Input observation: {"id":"o4","event_type":"UserPromptSubmit","evidence_kind":"user_statement","trust":"normal","text":"не используй больше SQLite ATTACH для этой операции"}
Existing entry: {"memory_id":"m1","kind":"convention","state":"active","canonical_key":null,"text":"Use SQLite ATTACH for cross-database writes."}
Output: {"op":"retract","target_memory_id":"m1","cites":["o4"]}

Input observation: {"id":"o5","event_type":"PostToolUse","evidence_kind":"model_claim","trust":"low","text":"this function is probably the main entry point"}
Output: {"op":"propose_candidate","kind":"fact","text":"This function is probably the main entry point.","scope_kind":"repository","confidence_signal":"low","importance_signal":"low","cites":["o5"]}

Input observation: {"id":"o6","event_type":"Stop","evidence_kind":"tool_result","trust":"normal","text":"pytest: 42 passed, 0 failed"}
Output: {"op":"noop","reason":"routine test result, nothing durable to record"}

Input observation: {"id":"o7","event_type":"UserPromptSubmit","evidence_kind":"user_statement","trust":"normal","text":"we decided to use pytest for testing -- what if we added mutation testing later?"}
Output (two ops from one observation -- one line each, no separator between them):
{"op":"create","kind":"decision","text":"Use pytest for testing.","scope_kind":"repository","confidence_signal":"high","importance_signal":"medium","cites":["o7"]}
{"op":"create","kind":"hypothesis","text":"Mutation testing might be added later.","scope_kind":"repository","confidence_signal":"low","importance_signal":"low","cites":["o7"]}
"#
    .to_string()
}

#[derive(Serialize)]
struct PromptObservation<'a> {
    id: &'a str,
    event_type: &'a str,
    evidence_kind: &'a str,
    trust: &'a str,
    text: &'a str,
}

#[derive(Serialize)]
struct PromptExistingEntry<'a> {
    memory_id: &'a str,
    kind: &'a str,
    state: &'a str,
    canonical_key: Option<&'a str>,
    text: &'a str,
}

#[derive(Serialize)]
struct PromptInput<'a> {
    observations: Vec<PromptObservation<'a>>,
    existing_entries: Vec<PromptExistingEntry<'a>>,
}

/// The user message: the window's observations plus
/// [`crate::recall::candidate_conflict_set`]'s existing entries, as one
/// compact JSON object. An observation with no `short_evidence_excerpt`
/// (normal for an envelope-only/TTL-swept event) surfaces as `"(no
/// excerpt)"` rather than being dropped -- the model still needs to see
/// *that* something happened, even without its content.
pub fn user_prompt(window: &ConsolidationWindow, existing: &[MemoryEntrySummary]) -> String {
    let observations = window
        .observations
        .iter()
        .map(|o| PromptObservation {
            id: &o.observation_id,
            event_type: &o.event_type,
            evidence_kind: o.evidence_kind.as_str(),
            trust: o.trust.as_str(),
            text: o
                .short_evidence_excerpt
                .as_deref()
                .unwrap_or("(no excerpt)"),
        })
        .collect();
    let existing_entries = existing
        .iter()
        .map(|e| PromptExistingEntry {
            memory_id: &e.memory_id,
            kind: e.kind.as_str(),
            state: e.state.as_str(),
            canonical_key: e.canonical_key.as_deref(),
            text: &e.text,
        })
        .collect();
    let input = PromptInput {
        observations,
        existing_entries,
    };
    serde_json::to_string(&input).expect("PromptInput serializes infallibly")
}

/// The one bounded corrective re-prompt [`crate::router::route`] sends when
/// the first response fails to parse as JSON at all (see [`crate::parse`]'s
/// module doc for the tier-1/tier-2 split this is part of).
pub fn correction_prompt(parse_error: &str) -> String {
    format!(
        "Your previous response was not valid ({parse_error}). Respond again with ONLY one \
         JSON object per line, one line per op -- no [ ] array, no commas between objects, \
         no prose, no markdown fences."
    )
}

/// Assembles the full chat turn [`crate::router::route`] sends on the first
/// attempt: system + the window/recall user message.
pub fn initial_messages(
    window: &ConsolidationWindow,
    existing: &[MemoryEntrySummary],
) -> Vec<GenMessage> {
    vec![
        GenMessage {
            role: GenRole::System,
            content: system_prompt(),
        },
        GenMessage {
            role: GenRole::User,
            content: user_prompt(window, existing),
        },
    ]
}

#[cfg(test)]
mod tests {
    use local_rag_store::{
        EvidenceKind, MemoryKind, MemoryState, ScopeKind, TrustLevel, WindowObservation,
    };

    use super::*;

    #[test]
    fn system_prompt_is_non_empty_and_mentions_both_placement_rules() {
        let prompt = system_prompt();
        assert!(prompt.contains("Auto-save"));
        assert!(prompt.contains("propose_candidate"));
    }

    /// T21-11: the router writes English whatever the observations are in.
    #[test]
    fn system_prompt_requires_english_output_and_verbatim_identifiers() {
        // The prompt is hard-wrapped, so match on whitespace-normalized text:
        // a test that fails when a line is rewrapped guards formatting, not
        // meaning.
        let prompt = system_prompt();
        let flat = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            flat.contains(r#"Write "text" and "reason" in English"#),
            "the language rule must be stated as an output rule, not implied: {flat}",
        );
        assert!(
            flat.contains("keep identifiers, paths, hashes, URLs and code verbatim"),
            "asking for English without exempting identifiers would corrupt them: {flat}",
        );
    }

    /// The few-shot set is the strongest instruction a small model reads, so a
    /// non-Latin `"text"` in any example would teach the opposite of the rule
    /// above — which is exactly what the pre-T21-11 prompt did (a Russian
    /// observation with a Russian `text` in its output).
    ///
    /// Inputs stay multilingual on purpose: spec 14 §1 item 4 `[FIXED]` is a
    /// bar on the transcripts the router must cope with, and this test must not
    /// be read as licence to make them English too.
    #[test]
    fn no_few_shot_output_text_is_written_in_a_non_latin_script() {
        let prompt = system_prompt();
        let examples = prompt
            .split_once("Examples:")
            .expect("the prompt carries a few-shot section")
            .1;

        for line in examples
            .lines()
            .filter(|l| l.starts_with("Output:") || l.starts_with('{'))
        {
            for field in ["\"text\":\"", "\"reason\":\""] {
                let mut rest = line;
                while let Some((_, after)) = rest.split_once(field) {
                    let value = after.split('"').next().unwrap_or_default();
                    assert!(
                        !value.chars().any(|c| c.is_alphabetic() && !c.is_ascii()),
                        "few-shot output {field}{value}\" is not English, which teaches the \
                         model to mirror the observation's language",
                    );
                    rest = after;
                }
            }
        }
    }

    #[test]
    fn user_prompt_is_valid_json_carrying_both_sections() {
        let window = ConsolidationWindow {
            session_id: "sess-1".to_string(),
            from_received_seq: 1,
            to_received_seq: 1,
            observations: vec![WindowObservation {
                observation_id: "o1".to_string(),
                received_seq: 1,
                event_type: "Stop".to_string(),
                evidence_kind: EvidenceKind::UserStatement,
                trust: TrustLevel::Normal,
                session_id: "sess-1".to_string(),
                repo_id: None,
                worktree_id: None,
                agent_id: None,
                commit_hash: None,
                short_evidence_excerpt: Some("we decided to use pnpm".to_string()),
                payload: None,
            }],
        };
        let existing = vec![MemoryEntrySummary {
            memory_id: "m1".to_string(),
            kind: MemoryKind::Fact,
            state: MemoryState::Active,
            text: "existing fact".to_string(),
            scope_kind: ScopeKind::Global,
            scope_owner_id: local_rag_store::GLOBAL_SCOPE_OWNER_ID.to_string(),
            canonical_key: None,
            entry_version: 1,
        }];

        let json = user_prompt(&window, &existing);
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(value["observations"][0]["id"], "o1");
        assert_eq!(value["observations"][0]["text"], "we decided to use pnpm");
        assert_eq!(value["existing_entries"][0]["memory_id"], "m1");
    }

    #[test]
    fn a_missing_excerpt_falls_back_to_a_placeholder_not_an_absent_field() {
        let window = ConsolidationWindow {
            session_id: "sess-1".to_string(),
            from_received_seq: 1,
            to_received_seq: 1,
            observations: vec![WindowObservation {
                observation_id: "o1".to_string(),
                received_seq: 1,
                event_type: "Stop".to_string(),
                evidence_kind: EvidenceKind::ToolResult,
                trust: TrustLevel::Normal,
                session_id: "sess-1".to_string(),
                repo_id: None,
                worktree_id: None,
                agent_id: None,
                commit_hash: None,
                short_evidence_excerpt: None,
                payload: None,
            }],
        };
        let json = user_prompt(&window, &[]);
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(value["observations"][0]["text"], "(no excerpt)");
    }

    #[test]
    fn correction_prompt_names_the_parse_error() {
        let msg = correction_prompt("expected value at line 1");
        assert!(msg.contains("expected value at line 1"));
    }
}
