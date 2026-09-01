//! The deterministic identity of a proposed operation (`T23-07`, ADR-0014
//! Decision 2, `D-118`/`D-127`).
//!
//! Two independent defects share one cause and one fix. `D-118`: the same
//! router output proposed as a `pending_memory_candidate` twice (across
//! different consolidation windows) produces two rows — measured live at
//! 11 204 pending candidates over only 4 368 distinct proposals, one text
//! duplicated 476 times. `D-127`: the same `create` proposed twice *inside
//! one window's plan* mints two `memory_entry` rows, because `D-078`'s
//! create→reinforce rewrite (`super::entry::active_entry_with_text`) only
//! ever consults the store, never a sibling op still being planned. Both are
//! "is this the same proposal as one already seen" — this module answers
//! that question once, so [`super::review::propose_candidate`] (the
//! cross-window and within-transaction half) and [`crate::memory::plan`]-side
//! callers in `local_rag_memory` (the within-plan `create` half) cannot
//! silently disagree about what "the same" means.
//!
//! # Deliberately not persisted
//!
//! There is no stored `dedup_key` column. `pending_memory_candidate` stays
//! `SCHEMA_V9` (`super::SCHEMA_V9`) — this key is computed on demand from the
//! live `proposed_operation` JSON every time it is needed. A stored column
//! would need `edit_candidate` to keep it in sync on every edit (a second
//! source of truth this crate's own history keeps finding drift in — see
//! `super::entry::MemoryEntrySummary`'s and `canonical_key`'s own histories)
//! and a migration backfill over the existing backlog for no measured
//! benefit: a full-table scan computing this key over the entire live
//! `pending_memory_candidate` table (11 204 rows, no index) measured 73 ms;
//! one exact lookup measured 41 ms. Both are noise next to a router
//! generation call, which costs whole seconds to minutes of local decode
//! (`T23-06`). If growth ever makes this slow, the fix is an index on the
//! same `json_extract` expressions the query already uses — an index of an
//! unchanged table, not a migration of one.
//!
//! # What the key excludes, and why each exclusion is deliberate
//!
//! ADR-0014 Decision 2 names the identity in as many words: "the exact
//! proposal — its op, kind, scope and text." Everything else a proposal
//! carries is excluded on purpose:
//!
//! - **`memory_id`** (of a `Create`): minted fresh per proposal
//!   (`local_rag_memory::guard::handle_create`). Including it would make
//!   every key unique and this whole module a no-op.
//! - **`confidence`/`importance`**: one window's *opinion* of a claim, not
//!   the claim's identity — the same reasoning `D-078`'s own comment gives
//!   for leaving a reinforced entry's stored confidence alone.
//! - **`canonical_key`**: measured `null` on 100 % of proposals (ADR-0014's
//!   own context section), and it already has its own, stronger mechanism
//!   (`memory_canonical`, a real unique index, enforced at `apply_create`).
//!   Including an arbitrary-text field here would also break the delimiter
//!   safety argument below.
//! - **`expected_version`** (of `Reinforce`/`Resolve`/`Retract`): a
//!   snapshot read at proposal time, not an identity — two candidates
//!   asking to reinforce entry E are the same request whichever version
//!   they happened to read.
//! - **`valid_from_tree`/`last_verified_tree`**: provenance, not identity;
//!   null on every proposal measured.

use local_rag_core::hash::sha256_hex;

use super::review::ProposedOperation;

/// The key format version. A fuzzier or differently-scoped key is a new
/// version and a new format, never a silent reinterpretation of what an
/// already-computed key meant.
pub const CANDIDATE_DEDUP_KEY_VERSION: &str = "v1";

/// The deterministic identity of a [`ProposedOperation`] (see the module
/// doc). Two proposals compare equal under this key exactly when ADR-0014
/// Decision 2 says the store must treat them as the same proposal.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CandidateDedupKey(String);

impl CandidateDedupKey {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CandidateDedupKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Compute `op`'s deterministic identity. Pure and total: no clock, no UUID
/// source, so the same proposal always yields the same key — in this
/// process, and against a row a previous release wrote.
///
/// Format, `|`-delimited (every component is drawn from a fixed
/// `CHECK`-constrained tag, a UUID, or a hex digest — never arbitrary text —
/// so `|` cannot appear inside a component):
/// - `Create`: `v1|create|<scope_kind>|<scope_owner_id>|<kind>|<sha256(text)>`
/// - `Reinforce`/`Resolve`/`Retract`: `v1|<op>|<memory_id>` — nothing else
///   about "reinforce/resolve/retract E" can differ; it is a pure statement
///   about one entry.
/// - `Supersede`: `v1|supersede|<old_memory_id>|<new_scope_kind>|
///   <new_scope_owner_id>|<new_kind>|<sha256(new_text)>` — like `Create`,
///   because a supersede mints a new entry with its own content; two
///   supersedes of the same `old_memory_id` proposing different
///   replacement text are different proposals, not duplicates.
///
/// The text component is a digest, not the raw text: this project's own
/// `D-118`/`D-127` measurements are read by hand at the `sqlite3` CLI, so
/// the op/kind/scope prefix stays human-legible while the (potentially
/// several-KiB) text stays a fixed-width 64 hex characters. `sha256_hex` is
/// [`local_rag_core::hash`]'s "stable namespacing / drift-detection digest"
/// family — not a spec 03 §1.2 identity hash, and not claimed to be one:
/// this key is never used to look an entry up by content, only to compare
/// two already-known proposals for exact equality.
pub fn candidate_dedup_key(op: &ProposedOperation) -> CandidateDedupKey {
    let key = match op {
        ProposedOperation::Create {
            kind,
            text,
            scope_kind,
            scope_owner_id,
            ..
        } => format!(
            "{CANDIDATE_DEDUP_KEY_VERSION}|create|{scope_kind}|{scope_owner_id}|{kind}|{}",
            sha256_hex(text.as_bytes())
        ),
        ProposedOperation::Reinforce { memory_id, .. } => {
            format!("{CANDIDATE_DEDUP_KEY_VERSION}|reinforce|{memory_id}")
        }
        ProposedOperation::Resolve { memory_id, .. } => {
            format!("{CANDIDATE_DEDUP_KEY_VERSION}|resolve|{memory_id}")
        }
        ProposedOperation::Retract { memory_id, .. } => {
            format!("{CANDIDATE_DEDUP_KEY_VERSION}|retract|{memory_id}")
        }
        ProposedOperation::Supersede {
            old_memory_id,
            new_kind,
            new_text,
            new_scope_kind,
            new_scope_owner_id,
            ..
        } => format!(
            "{CANDIDATE_DEDUP_KEY_VERSION}|supersede|{old_memory_id}|{new_scope_kind}|\
             {new_scope_owner_id}|{new_kind}|{}",
            sha256_hex(new_text.as_bytes())
        ),
    };
    CandidateDedupKey(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_op(
        kind: &str,
        scope_kind: &str,
        scope_owner_id: &str,
        text: &str,
    ) -> ProposedOperation {
        create_op_full(
            "minted-fresh-each-time",
            kind,
            scope_kind,
            scope_owner_id,
            text,
            0.5,
            0.5,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn create_op_full(
        memory_id: &str,
        kind: &str,
        scope_kind: &str,
        scope_owner_id: &str,
        text: &str,
        confidence: f64,
        importance: f64,
        canonical_key: Option<&str>,
    ) -> ProposedOperation {
        ProposedOperation::Create {
            memory_id: memory_id.to_string(),
            kind: kind.to_string(),
            text: text.to_string(),
            canonical_key: canonical_key.map(str::to_string),
            scope_kind: scope_kind.to_string(),
            scope_owner_id: scope_owner_id.to_string(),
            confidence,
            importance,
            valid_from_tree: None,
            last_verified_tree: None,
        }
    }

    fn supersede_op(
        old_memory_id: &str,
        old_expected_version: i64,
        new_memory_id: &str,
        new_text: &str,
    ) -> ProposedOperation {
        ProposedOperation::Supersede {
            old_memory_id: old_memory_id.to_string(),
            old_expected_version,
            new_memory_id: new_memory_id.to_string(),
            new_kind: "fact".to_string(),
            new_text: new_text.to_string(),
            new_canonical_key: None,
            new_scope_kind: "global".to_string(),
            new_scope_owner_id: "owner".to_string(),
            new_confidence: 0.5,
            new_importance: 0.5,
            new_valid_from_tree: None,
            new_last_verified_tree: None,
        }
    }

    #[test]
    fn identical_creates_yield_the_same_key_regardless_of_memory_id_confidence_or_importance() {
        let a = create_op_full("m1", "fact", "global", "owner", "same text", 0.1, 0.9, None);
        let b = create_op_full("m2", "fact", "global", "owner", "same text", 0.9, 0.1, None);
        assert_eq!(candidate_dedup_key(&a), candidate_dedup_key(&b));
    }

    #[test]
    fn a_canonical_key_does_not_change_the_dedup_key() {
        let a = create_op("fact", "global", "owner", "same text");
        let b = create_op_full(
            "minted-fresh-each-time",
            "fact",
            "global",
            "owner",
            "same text",
            0.5,
            0.5,
            Some("k"),
        );
        assert_eq!(candidate_dedup_key(&a), candidate_dedup_key(&b));
    }

    #[test]
    fn dedup_key_separates_kind_scope_kind_scope_owner_and_text() {
        let base = create_op("fact", "global", "owner-a", "text");
        let different_kind = create_op("decision", "global", "owner-a", "text");
        let different_scope_kind = create_op("fact", "repository", "owner-a", "text");
        let different_owner = create_op("fact", "global", "owner-b", "text");
        let different_text = create_op("fact", "global", "owner-a", "other text");

        let base_key = candidate_dedup_key(&base);
        for other in [
            &different_kind,
            &different_scope_kind,
            &different_owner,
            &different_text,
        ] {
            assert_ne!(base_key, candidate_dedup_key(other));
        }
    }

    #[test]
    fn each_op_variant_carries_its_own_tag_even_on_the_same_memory_id() {
        let reinforce = ProposedOperation::Reinforce {
            memory_id: "E".to_string(),
            expected_version: 1,
            confidence: None,
        };
        let resolve = ProposedOperation::Resolve {
            memory_id: "E".to_string(),
            expected_version: 1,
        };
        let retract = ProposedOperation::Retract {
            memory_id: "E".to_string(),
            expected_version: 1,
        };
        let keys = [
            candidate_dedup_key(&reinforce),
            candidate_dedup_key(&resolve),
            candidate_dedup_key(&retract),
        ];
        assert_ne!(keys[0], keys[1]);
        assert_ne!(keys[0], keys[2]);
        assert_ne!(keys[1], keys[2]);
    }

    #[test]
    fn reinforce_ignores_expected_version_and_confidence() {
        let a = ProposedOperation::Reinforce {
            memory_id: "E".to_string(),
            expected_version: 3,
            confidence: Some(0.1),
        };
        let b = ProposedOperation::Reinforce {
            memory_id: "E".to_string(),
            expected_version: 9,
            confidence: Some(0.9),
        };
        assert_eq!(candidate_dedup_key(&a), candidate_dedup_key(&b));
    }

    #[test]
    fn supersede_of_the_same_old_entry_with_different_new_text_is_not_a_duplicate() {
        let a = supersede_op("E", 1, "N1", "replacement one");
        let b = supersede_op("E", 1, "N2", "replacement two");
        assert_ne!(candidate_dedup_key(&a), candidate_dedup_key(&b));
    }

    #[test]
    fn supersede_of_the_same_old_entry_with_the_same_new_claim_is_a_duplicate_regardless_of_new_memory_id()
     {
        let a = supersede_op("E", 1, "N1", "replacement");
        let b = supersede_op("E", 9, "N2", "replacement");
        assert_eq!(candidate_dedup_key(&a), candidate_dedup_key(&b));
    }

    #[test]
    fn the_key_carries_its_format_version() {
        let op = create_op("fact", "global", "owner", "text");
        assert!(
            candidate_dedup_key(&op)
                .as_str()
                .starts_with(&format!("{CANDIDATE_DEDUP_KEY_VERSION}|"))
        );
    }

    /// Every component is drawn from a fixed enum tag, a UUID, or a hex
    /// digest, never arbitrary text — so a `|` inside a proposal's own text
    /// can never be mistaken for the delimiter. This pins that structural
    /// argument rather than leaving it as an unchecked comment.
    #[test]
    fn a_pipe_inside_the_text_cannot_be_mistaken_for_the_delimiter() {
        let with_pipe = create_op("fact", "global", "owner", "a|b|c");
        let without = create_op("fact", "global", "owner", "abc");
        assert_ne!(
            candidate_dedup_key(&with_pipe),
            candidate_dedup_key(&without)
        );
        // And the pipe-carrying text does not collide with some other
        // legitimately-different proposal that happens to share a prefix.
        let different_kind_with_pipe = create_op("decision", "global", "owner", "a|b|c");
        assert_ne!(
            candidate_dedup_key(&with_pipe),
            candidate_dedup_key(&different_kind_with_pipe)
        );
    }
}
