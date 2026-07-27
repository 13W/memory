//! Redaction, deny-list exclusion, and size-capping for an observation's
//! payload — the REDACTION step of the hook write path (spec 07 §2), before
//! anything is handed to the (T13-02) segment writer.
//!
//! ```text
//! parse hook JSON (T13-02) → REDACTION (this module) → identity (T13-02) →
//!   build frame → flock → write → fdatasync → funlock → exit 0
//! ```
//!
//! This module is deliberately event-shape-agnostic: it takes an already-
//! extracted payload string, path list, and tool name (whatever Claude Code's
//! hook JSON parsing — T13-02 — decides those are for a given event type) and
//! returns either an envelope-only exclusion or a redacted, capped payload.
//! Parsing the actual hook JSON and computing `source_event_id` are out of
//! scope here.
//!
//! # Order: redact, then cap
//!
//! The [`local_rag_core::redaction::Scanner`] runs first, over the full raw
//! payload; the [`PAYLOAD_CAP_BYTES`] cap applies to the *redacted* bytes.
//! Capping first could truncate a secret in half at the boundary and leave the
//! surviving half on disk; redacting first means a secret cannot survive by
//! virtue of falling near the cut.
//!
//! # A known, accepted limitation
//!
//! The scanner runs over the payload as flat text, the same way file
//! classification (T03-02) scans a whole source file — reused as-is, not
//! reshaped into a JSON-aware transform. Consequently the `AssignedSecret`
//! rule (which looks for a bare `"`/`'` immediately after `key =`) is weaker
//! inside a JSON-escaped value (`\"…\"` rather than `"…"`), since the escaping
//! backslash sits where the rule expects a quote. The `CredentialToken` and
//! `HighEntropy` rules — token-boundary rules, not quote-adjacency rules — are
//! unaffected by escaping and remain fully effective; these are the two rules
//! the group-13 card names explicitly ("credential/high-entropy patterns").
//! Reshaping the scanner into a `serde_json::Value` walker would expand
//! T03-02's already-gated scope rather than reuse it, so this is accepted and
//! documented rather than fixed here.

use local_rag_core::config::SpoolConfig;
use local_rag_core::identity::domain::truncated_excerpt;
use local_rag_core::redaction::Scanner;

/// The spool-payload size cap (spec 12 §2 `[SPEC]`: "spool payload 256 KiB").
/// The 4 KiB evidence-excerpt cap and 8 KiB snippet cap are different sites —
/// the former is group 14's (memory evidence), the latter is already built
/// (T12-04, `local_rag_search::SNIPPET_CAP_BYTES`).
pub const PAYLOAD_CAP_BYTES: usize = 256 * 1024;

/// The `{hash, original_size}` metadata a truncated payload leaves behind
/// (spec 12 §2 `[FIXED]`), over the **full** pre-truncation (but
/// post-redaction) bytes — mirrors `local_rag_search::snippet`'s `Truncation`
/// shape exactly, but is its own type: a spool payload is a different
/// consumer/domain than a search snippet, and the two need not share a wire
/// type to share an idiom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayloadTruncation {
    /// `H(truncated_excerpt, full_bytes)` (spec 03 §1.2's `Domain::TruncatedExcerpt`).
    pub hash: String,
    /// The full (redacted, pre-cap) byte length.
    pub original_size: u64,
}

/// The outcome of preparing an observation's payload for the spool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreparedPayload {
    /// The event's path(s) or tool matched the deny-list (spec 12 §2): captured
    /// envelope-only (identity + metadata only). The raw payload was never
    /// even scanned — an excluded event's content never reaches this module's
    /// output in any form.
    EnvelopeOnly,
    /// The event was not denied; its (possibly truncated) redacted payload.
    Included {
        /// Redacted, capped payload bytes. Not guaranteed to still be valid
        /// JSON when truncated — the same way a truncated search snippet is
        /// not guaranteed to be syntactically complete code: the cap describes
        /// what survived, not a promise about its structure.
        bytes: Vec<u8>,
        /// The [`Scanner`] rule-set version that produced this redaction,
        /// recorded so a verdict is auditable against the exact rules that
        /// produced it (spec 12 §2 `[SPEC]`).
        redaction_version: u32,
        /// How many secret spans were found and replaced (after merging
        /// overlaps — see `Scanner::redact`).
        secrets_found: usize,
        /// Present only if `bytes` were capped.
        truncation: Option<PayloadTruncation>,
    },
}

/// Prepare an observation's payload for the spool: deny-list check, then
/// redact, then cap (spec 07 §2 REDACTION step; 12 §2).
///
/// `paths`/`tool_name` are whatever the caller (T13-02, parsing the actual
/// hook JSON) has already extracted for this event; this function does not
/// know or care which hook event type produced them.
pub fn prepare_payload(
    raw_payload: &str,
    paths: &[String],
    tool_name: Option<&str>,
    deny: &SpoolConfig,
    scanner: &Scanner,
) -> PreparedPayload {
    if is_denied(paths, tool_name, deny) {
        return PreparedPayload::EnvelopeOnly;
    }

    let redacted = scanner.redact(raw_payload);
    let full = redacted.text.into_bytes();

    if full.len() <= PAYLOAD_CAP_BYTES {
        return PreparedPayload::Included {
            bytes: full,
            redaction_version: scanner.version(),
            secrets_found: redacted.findings,
            truncation: None,
        };
    }

    // Over the cap: move the cut back to a UTF-8 boundary (identical idiom to
    // `local_rag_search::snippet::cut` — at most three bytes, since no UTF-8
    // sequence is longer than four).
    let mut cut_at = PAYLOAD_CAP_BYTES;
    while cut_at > 0 && (full[cut_at] & 0b1100_0000) == 0b1000_0000 {
        cut_at -= 1;
    }
    let truncation = PayloadTruncation {
        hash: truncated_excerpt(&full),
        original_size: full.len() as u64,
    };
    let mut bytes = full;
    bytes.truncate(cut_at);
    PreparedPayload::Included {
        bytes,
        redaction_version: scanner.version(),
        secrets_found: redacted.findings,
        truncation: Some(truncation),
    }
}

/// Fold a [`PreparedPayload`] into the frame's `payload` field (`None` for
/// [`PreparedPayload::EnvelopeOnly`], a JSON string of the redacted bytes for
/// [`PreparedPayload::Included`]) — see
/// `local_rag_core::spool`'s module docs for why the field is a JSON string
/// rather than a raw nested object.
///
/// `String::from_utf8_lossy` rather than a fallible `from_utf8` even though
/// `prepare_payload` already guarantees valid UTF-8: this binary's mandate is
/// "never panic", so a future regression upstream should degrade to
/// lossy-but-safe output, not crash the hook.
pub fn payload_field(prepared: &PreparedPayload) -> Option<String> {
    match prepared {
        PreparedPayload::EnvelopeOnly => None,
        PreparedPayload::Included { bytes, .. } => {
            Some(String::from_utf8_lossy(bytes).into_owned())
        }
    }
}

/// Fold a [`PreparedPayload`] into the frame's `redaction_version` field
/// (spec 12 §2 `[SPEC]`, D-019): `None` for [`PreparedPayload::EnvelopeOnly`]
/// (a denied event's payload is never scanned, so no scanner version
/// applies), `Some(redaction_version)` for [`PreparedPayload::Included`].
pub fn redaction_version_field(prepared: &PreparedPayload) -> Option<u32> {
    match prepared {
        PreparedPayload::EnvelopeOnly => None,
        PreparedPayload::Included {
            redaction_version, ..
        } => Some(*redaction_version),
    }
}

/// Whether this event's paths or tool name match the configured deny-list.
fn is_denied(paths: &[String], tool_name: Option<&str>, deny: &SpoolConfig) -> bool {
    paths.iter().any(|p| path_denied(p, &deny.deny_paths))
        || tool_name.is_some_and(|t| deny.deny_tools.iter().any(|d| d == t))
}

/// Whether `path` lies under (or exactly equals) one of `deny_paths`,
/// component-wise — a directory-prefix match, never a substring match, so a
/// deny entry `secrets` matches `secrets/api.key` but not `not-secrets/x.txt`.
fn path_denied(path: &str, deny_paths: &[String]) -> bool {
    let path_components: Vec<&str> = path.split('/').collect();
    deny_paths.iter().any(|entry| {
        let entry_components: Vec<&str> = entry.split('/').collect();
        !entry_components.is_empty()
            && path_components.len() >= entry_components.len()
            && path_components[..entry_components.len()] == entry_components[..]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_deny() -> SpoolConfig {
        SpoolConfig::default()
    }

    #[test]
    fn a_path_that_merely_shares_a_prefix_string_is_not_denied() {
        let deny = SpoolConfig {
            deny_paths: vec!["secrets".to_string()],
            deny_tools: vec![],
        };
        assert!(path_denied("secrets/api.key", &deny.deny_paths));
        assert!(path_denied("secrets", &deny.deny_paths));
        assert!(!path_denied("not-secrets/x.txt", &deny.deny_paths));
        assert!(!path_denied("src/secretsmanager.rs", &deny.deny_paths));
    }

    #[test]
    fn clean_payload_within_cap_passes_through_unredacted() {
        let scanner = Scanner::new();
        let result = prepare_payload(
            "{\"tool_input\":{\"command\":\"echo hi\"}}",
            &[],
            Some("Bash"),
            &no_deny(),
            &scanner,
        );
        match result {
            PreparedPayload::Included {
                bytes,
                secrets_found,
                truncation,
                ..
            } => {
                assert_eq!(secrets_found, 0);
                assert!(truncation.is_none());
                assert_eq!(
                    String::from_utf8(bytes).unwrap(),
                    "{\"tool_input\":{\"command\":\"echo hi\"}}"
                );
            }
            other => panic!("expected Included, got {other:?}"),
        }
    }

    #[test]
    fn payload_field_is_none_for_envelope_only() {
        assert_eq!(payload_field(&PreparedPayload::EnvelopeOnly), None);
    }

    #[test]
    fn payload_field_wraps_included_bytes_as_a_string() {
        let prepared = PreparedPayload::Included {
            bytes: b"{\"k\":\"v\"}".to_vec(),
            redaction_version: 1,
            secrets_found: 0,
            truncation: None,
        };
        assert_eq!(payload_field(&prepared), Some("{\"k\":\"v\"}".to_string()));
    }

    #[test]
    fn redaction_version_field_is_none_for_envelope_only() {
        assert_eq!(
            redaction_version_field(&PreparedPayload::EnvelopeOnly),
            None
        );
    }

    #[test]
    fn redaction_version_field_is_some_scanner_version_for_included() {
        let scanner = Scanner::new();
        let prepared = prepare_payload("clean text", &[], None, &no_deny(), &scanner);
        assert_eq!(redaction_version_field(&prepared), Some(scanner.version()));
    }
}
