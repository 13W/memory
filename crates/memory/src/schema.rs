//! The router's wire JSON schema (T14-07, spec 08 §4 `[FIXED]`/`[SPEC]`):
//! [`RawRouterOp`] is exactly what the local generator is asked to emit — one
//! JSON object per line (JSONL, D-051 — not a single top-level `[...]` array;
//! see [`crate::parse`]'s own module doc for why one bad/truncated element
//! must not be able to invalidate every other, syntactically valid, element
//! in the same response), nothing else on a line. [`crate::parse`]
//! deserializes each line into this type; [`crate::guard`] turns each value
//! into a
//! [`local_rag_store::GeneratedOp`] (materializing it, downgrading it to a
//! candidate, or discarding it as a no-op) — this module owns *shape* only,
//! never placement policy.
//!
//! # Confidence is a policy score, not an LLM probability (spec 08 §2 `[FIXED]`)
//!
//! The model never emits a raw `confidence`/`importance` float. It emits a
//! coarse [`Signal`] (`low`/`medium`/`high`); [`Signal::confidence`]/
//! [`Signal::importance`] map that to fixed constants. These constants are an
//! explicit `[SPEC values TBD]` placeholder (08 §2's own wording) — the seven
//! weights that formula describes need signals (`w_repeat` cross-session
//! counting, `w_code` code-state agreement, `w_contra` conflict detection,
//! ...) that don't exist yet; inventing numbers to fit a formula with no
//! measured inputs is exactly what O2's "collect metrics, do not invent
//! thresholds" rule forbids. A future task with real signals replaces this
//! mapping with the actual weighted formula.
//!
//! # Targeting an existing entry is always by `memory_id`, never `canonical_key`
//!
//! [`crate::prompt`]'s user message always lists every entry in the window's
//! candidate conflict set with its `memory_id` inline (`crate::recall`), so
//! the model never needs to reconstruct one. `canonical_key` uniqueness is
//! only scoped to one `(scope_kind, scope_owner_id)` pair — the same key text
//! can legitimately exist in two different scopes — so a `canonical_key`
//! alone is ambiguous for *addressing* an existing row without also carrying
//! a `scope_kind`/`scope_owner_id` the model would have to echo correctly. A
//! `canonical_key` field only ever appears here as a *newly assigned* key on
//! a freshly minted entry ([`RawRouterOp::Create`]/[`RawRouterOp::
//! ProposeCandidate`]'s `canonical_key`, [`RawRouterOp::Supersede`]'s
//! `new_canonical_key`), where no addressing ambiguity exists.
//!
//! # `propose_candidate` is both a model choice and a guard outcome
//!
//! Spec 08 §4's op vocabulary includes `propose_candidate` directly (the
//! model may signal low confidence itself); [`crate::guard`] *also* forces
//! this outcome for a `create`/`supersede` that fails one of the two
//! placement rules (08 §4's "auto-save only for explicit durable
//! decisions/instructions" and 12 §4's "model-claims are never auto-promoted
//! to facts"). [`RawRouterOp::ProposeCandidate`] intentionally mirrors
//! [`RawRouterOp::Create`]'s shape exactly (not `reinforce`/`resolve`/
//! `retract`/`supersede`-shaped) — a low-confidence claim about a *new* fact
//! is overwhelmingly the common real case, and doubling the wire schema to
//! cover every op shape as a proposal is not justified for v0.

use serde::{Deserialize, Serialize};

/// A qualitative confidence/importance signal (see the module doc's
/// "Confidence is a policy score" section) — the only form either value is
/// ever allowed to take on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Signal {
    Low,
    Medium,
    High,
}

/// `[SPEC values TBD]` placeholder confidence constants (see the module doc).
pub const CONFIDENCE_LOW: f64 = 0.3;
pub const CONFIDENCE_MEDIUM: f64 = 0.6;
pub const CONFIDENCE_HIGH: f64 = 0.85;

/// `[SPEC values TBD]` placeholder importance constants (see the module doc).
pub const IMPORTANCE_LOW: f64 = 0.3;
pub const IMPORTANCE_MEDIUM: f64 = 0.6;
pub const IMPORTANCE_HIGH: f64 = 0.85;

impl Signal {
    pub fn confidence(self) -> f64 {
        match self {
            Signal::Low => CONFIDENCE_LOW,
            Signal::Medium => CONFIDENCE_MEDIUM,
            Signal::High => CONFIDENCE_HIGH,
        }
    }

    pub fn importance(self) -> f64 {
        match self {
            Signal::Low => IMPORTANCE_LOW,
            Signal::Medium => IMPORTANCE_MEDIUM,
            Signal::High => IMPORTANCE_HIGH,
        }
    }
}

/// One router-emitted op (spec 08 §4's `{create, reinforce, supersede,
/// resolve, retract, noop, propose_candidate}` envelope). Wire format:
/// `{"op": "create", ...fields}` (`serde` internally tagged, `snake_case`) —
/// one such object per line ([`crate::parse`] parses each line
/// independently), matching spec 08 §4 step 3's "ordered ops list" literally.
///
/// `kind`/`scope_kind` stay plain `String` here, exactly like
/// [`local_rag_store::ProposedOperation`] — parsed against
/// [`local_rag_store::MemoryKind`]/[`local_rag_store::ScopeKind`]'s CHECK
/// domains in [`crate::guard`], not here. An out-of-domain **or missing**
/// value is a per-op semantic failure (`crate::guard` degrades that one op
/// to `noop`), never a whole-response parse failure — see [`crate::parse`]'s
/// module doc for the two-tier malformed-output split this preserves. D-048:
/// `scope_kind` carries `#[serde(default)]` (empty string on omission) for
/// exactly this reason — a small local model occasionally emits a `create`/
/// `propose_candidate` op with every other field but this one; without the
/// default, that single op's missing field failed the *entire* batch's
/// deserialization (tier 1) instead of just this one op's `guard::
/// handle_create` (which already turns an unrecognized `scope_kind` —
/// `ScopeKind::from_db` returns `None` for anything outside its three known
/// values, `""` included — into `Noop`, no crash, no per-op regression).
/// D-051: `confidence_signal`/`importance_signal` on `Create`/
/// `ProposeCandidate`/`Supersede` carry the same `#[serde(default)]`
/// treatment, but as `Option<Signal>` rather than a fabricated fallback
/// value — the same shape [`RawRouterOp::Reinforce`]'s own
/// `confidence_signal` already has (though there it means "leave the
/// existing value alone," a different semantic; here a missing value is a
/// per-op degrade to `noop` in [`crate::guard`], never an invented
/// `Signal::Low`/`Medium`/`High` the model never actually said).
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum RawRouterOp {
    Create {
        kind: String,
        text: String,
        #[serde(default)]
        canonical_key: Option<String>,
        #[serde(default)]
        scope_kind: String,
        #[serde(default)]
        confidence_signal: Option<Signal>,
        #[serde(default)]
        importance_signal: Option<Signal>,
        #[serde(default)]
        cites: Vec<String>,
    },
    /// Same shape as [`RawRouterOp::Create`] — see the module doc's
    /// "`propose_candidate` is both a model choice and a guard outcome".
    ProposeCandidate {
        kind: String,
        text: String,
        #[serde(default)]
        canonical_key: Option<String>,
        #[serde(default)]
        scope_kind: String,
        #[serde(default)]
        confidence_signal: Option<Signal>,
        #[serde(default)]
        importance_signal: Option<Signal>,
        #[serde(default)]
        cites: Vec<String>,
    },
    Reinforce {
        target_memory_id: String,
        #[serde(default)]
        confidence_signal: Option<Signal>,
        #[serde(default)]
        cites: Vec<String>,
    },
    Resolve {
        target_memory_id: String,
        #[serde(default)]
        cites: Vec<String>,
    },
    Retract {
        target_memory_id: String,
        #[serde(default)]
        cites: Vec<String>,
    },
    Supersede {
        target_memory_id: String,
        new_kind: String,
        new_text: String,
        #[serde(default)]
        new_canonical_key: Option<String>,
        #[serde(default)]
        confidence_signal: Option<Signal>,
        #[serde(default)]
        importance_signal: Option<Signal>,
        #[serde(default)]
        cites: Vec<String>,
    },
    Noop {
        /// Diagnostic only (prompt legibility) — never persisted.
        #[serde(default)]
        #[allow(dead_code)]
        reason: Option<String>,
    },
}

/// An advisory JSON Schema for **one line** of [`RawRouterOp`]'s JSONL wire
/// form (D-051 — the response as a whole is not itself valid JSON, so this
/// describes a single object, not a `{"type": "array", "items": ...}`
/// wrapper around one), passed via
/// [`local_rag_embed::GenRequest::with_json_schema`]. `[SPEC]`, not
/// `[FIXED]`: a runtime that supports grammar-constrained decoding compiles
/// this into its own grammar; `local_rag_generate::LlamaGenerator` (v0) does
/// not — see that crate's module doc — so today this is documentation only,
/// forward-compatible with a future runtime that honors it.
pub const ROUTER_OPS_JSON_SCHEMA: &str = r#"{
  "type": "object",
  "required": ["op"],
  "properties": {
    "op": {
      "enum": ["create", "propose_candidate", "reinforce", "resolve", "retract", "supersede", "noop"]
    },
    "kind": {
      "enum": ["fact", "decision", "convention", "procedure", "task", "question", "hypothesis"]
    },
    "text": { "type": "string" },
    "canonical_key": { "type": ["string", "null"] },
    "scope_kind": { "enum": ["global", "repository", "worktree"] },
    "confidence_signal": { "enum": ["low", "medium", "high"] },
    "importance_signal": { "enum": ["low", "medium", "high"] },
    "cites": { "type": "array", "items": { "type": "string" } },
    "target_memory_id": { "type": "string" },
    "new_kind": {
      "enum": ["fact", "decision", "convention", "procedure", "task", "question", "hypothesis"]
    },
    "new_text": { "type": "string" },
    "new_canonical_key": { "type": ["string", "null"] },
    "reason": { "type": ["string", "null"] }
  }
}"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_maps_to_the_documented_placeholder_constants() {
        assert_eq!(Signal::Low.confidence(), CONFIDENCE_LOW);
        assert_eq!(Signal::Medium.confidence(), CONFIDENCE_MEDIUM);
        assert_eq!(Signal::High.confidence(), CONFIDENCE_HIGH);
        assert_eq!(Signal::Low.importance(), IMPORTANCE_LOW);
        assert_eq!(Signal::Medium.importance(), IMPORTANCE_MEDIUM);
        assert_eq!(Signal::High.importance(), IMPORTANCE_HIGH);
    }

    #[test]
    fn confidence_ordering_is_monotonic_low_to_high() {
        assert!(Signal::Low.confidence() < Signal::Medium.confidence());
        assert!(Signal::Medium.confidence() < Signal::High.confidence());
    }

    #[test]
    fn every_constant_is_a_valid_confidence_domain_value() {
        for c in [CONFIDENCE_LOW, CONFIDENCE_MEDIUM, CONFIDENCE_HIGH] {
            assert!((0.0..=1.0).contains(&c));
        }
        for i in [IMPORTANCE_LOW, IMPORTANCE_MEDIUM, IMPORTANCE_HIGH] {
            assert!((0.0..=1.0).contains(&i));
        }
    }

    #[test]
    fn create_round_trips_from_its_wire_shape() {
        let json = r#"{"op":"create","kind":"fact","text":"uses pnpm","scope_kind":"repository","confidence_signal":"high","importance_signal":"medium","cites":["obs-1"]}"#;
        let op: RawRouterOp = serde_json::from_str(json).expect("valid create op");
        match op {
            RawRouterOp::Create {
                kind,
                text,
                canonical_key,
                scope_kind,
                confidence_signal,
                importance_signal,
                cites,
            } => {
                assert_eq!(kind, "fact");
                assert_eq!(text, "uses pnpm");
                assert_eq!(canonical_key, None);
                assert_eq!(scope_kind, "repository");
                assert_eq!(confidence_signal, Some(Signal::High));
                assert_eq!(importance_signal, Some(Signal::Medium));
                assert_eq!(cites, vec!["obs-1".to_string()]);
            }
            other => panic!("expected Create, got {other:?}"),
        }
    }

    #[test]
    fn cites_defaults_to_empty_when_omitted() {
        let json = r#"{"op":"noop"}"#;
        let op: RawRouterOp = serde_json::from_str(json).expect("valid noop");
        assert!(matches!(op, RawRouterOp::Noop { reason: None }));
    }

    /// D-048 regression: a `create` op missing `scope_kind` entirely must
    /// not fail the whole batch's deserialization (tier 1) — the field
    /// defaults to `""`, leaving `guard::handle_create`'s existing
    /// out-of-domain check (tier 2) to degrade just this one op to `Noop`.
    #[test]
    fn create_defaults_scope_kind_to_empty_when_omitted() {
        let json = r#"{"op":"create","kind":"fact","text":"uses pnpm","confidence_signal":"high","importance_signal":"medium"}"#;
        let op: RawRouterOp = serde_json::from_str(json).expect("valid create despite the gap");
        match op {
            RawRouterOp::Create { scope_kind, .. } => assert_eq!(scope_kind, ""),
            other => panic!("expected Create, got {other:?}"),
        }
    }

    /// Same gap, same fix, `propose_candidate` variant — an independently
    /// declared enum variant with its own `scope_kind` field.
    #[test]
    fn propose_candidate_defaults_scope_kind_to_empty_when_omitted() {
        let json = r#"{"op":"propose_candidate","kind":"fact","text":"uses pnpm","confidence_signal":"high","importance_signal":"medium"}"#;
        let op: RawRouterOp =
            serde_json::from_str(json).expect("valid propose_candidate despite the gap");
        match op {
            RawRouterOp::ProposeCandidate { scope_kind, .. } => assert_eq!(scope_kind, ""),
            other => panic!("expected ProposeCandidate, got {other:?}"),
        }
    }

    #[test]
    fn reinforce_requires_a_target_memory_id() {
        let json = r#"{"op":"reinforce","cites":[]}"#;
        assert!(serde_json::from_str::<RawRouterOp>(json).is_err());
    }

    /// D-051: a `create` op missing `confidence_signal` entirely must not
    /// fail the whole batch's deserialization (tier 1) — the field defaults
    /// to `None`, leaving `guard::handle_create`'s degrade (tier 2) to turn
    /// just this one op into `Noop`, exactly like `create_defaults_scope_kind_
    /// to_empty_when_omitted` above does for the sibling `scope_kind` gap.
    #[test]
    fn create_defaults_confidence_signal_to_none_when_omitted() {
        let json = r#"{"op":"create","kind":"fact","text":"uses pnpm","scope_kind":"global","importance_signal":"medium"}"#;
        let op: RawRouterOp = serde_json::from_str(json).expect("valid create despite the gap");
        match op {
            RawRouterOp::Create {
                confidence_signal, ..
            } => assert_eq!(confidence_signal, None),
            other => panic!("expected Create, got {other:?}"),
        }
    }

    /// Same gap, `importance_signal` instead.
    #[test]
    fn create_defaults_importance_signal_to_none_when_omitted() {
        let json = r#"{"op":"create","kind":"fact","text":"uses pnpm","scope_kind":"global","confidence_signal":"high"}"#;
        let op: RawRouterOp = serde_json::from_str(json).expect("valid create despite the gap");
        match op {
            RawRouterOp::Create {
                importance_signal, ..
            } => assert_eq!(importance_signal, None),
            other => panic!("expected Create, got {other:?}"),
        }
    }

    /// Same gap, `propose_candidate` variant.
    #[test]
    fn propose_candidate_defaults_confidence_signal_to_none_when_omitted() {
        let json = r#"{"op":"propose_candidate","kind":"fact","text":"uses pnpm","scope_kind":"global","importance_signal":"medium"}"#;
        let op: RawRouterOp =
            serde_json::from_str(json).expect("valid propose_candidate despite the gap");
        match op {
            RawRouterOp::ProposeCandidate {
                confidence_signal, ..
            } => assert_eq!(confidence_signal, None),
            other => panic!("expected ProposeCandidate, got {other:?}"),
        }
    }

    /// Same gap, `supersede` variant.
    #[test]
    fn supersede_defaults_confidence_signal_to_none_when_omitted() {
        let json = r#"{"op":"supersede","target_memory_id":"m-1","new_kind":"fact","new_text":"confirmed","importance_signal":"medium"}"#;
        let op: RawRouterOp = serde_json::from_str(json).expect("valid supersede despite the gap");
        match op {
            RawRouterOp::Supersede {
                confidence_signal, ..
            } => assert_eq!(confidence_signal, None),
            other => panic!("expected Supersede, got {other:?}"),
        }
    }

    #[test]
    fn the_advisory_json_schema_is_itself_valid_json() {
        let value: serde_json::Value =
            serde_json::from_str(ROUTER_OPS_JSON_SCHEMA).expect("schema is valid JSON");
        assert_eq!(value["type"], "object");
    }
}
