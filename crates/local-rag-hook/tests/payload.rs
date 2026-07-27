//! Integration tests for the T13-01 REDACTION step (spec 07 §2, 12 §2):
//! deny-list exclusion, secret redaction, and the 256 KiB payload cap.

use local_rag_core::config::SpoolConfig;
use local_rag_core::identity::domain::truncated_excerpt;
use local_rag_core::redaction::Scanner;
use local_rag_hook::payload::{PAYLOAD_CAP_BYTES, PreparedPayload, prepare_payload};

fn no_deny() -> SpoolConfig {
    SpoolConfig::default()
}

/// Minimal JSON string escaping (only `\` and `"`, the only characters these
/// tests' payloads use) — enough to build a realistic JSON-escaped payload
/// without pulling in a `serde_json` dependency this crate doesn't otherwise
/// need.
fn json_string_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn json_payload(command: &str) -> String {
    format!(
        "{{\"tool_input\":{{\"command\":\"{}\"}}}}",
        json_string_escape(command)
    )
}

#[test]
fn credential_and_high_entropy_secrets_are_redacted_inside_a_json_payload() {
    let scanner = Scanner::new();
    let cred = "AKIAIOSFODNN7EXAMPLE";
    let b64 = "aGVsbG9Xb3JsZERlYWRCZWVmQ2FmZUJhYmVMMzM3SHVudGVy";
    // A realistic shell command whose secrets were themselves quoted, then
    // JSON-escaped when embedded as the "command" field's string value — the
    // exact context the group-13 card's "credential/high-entropy patterns"
    // bullet targets.
    let payload = json_payload(&format!(
        "export API_KEY=\"{cred}\" && echo token=\"{b64}\""
    ));

    let result = prepare_payload(&payload, &[], Some("Bash"), &no_deny(), &scanner);
    let PreparedPayload::Included {
        bytes,
        secrets_found,
        redaction_version,
        truncation,
    } = result
    else {
        panic!("expected Included");
    };
    let text = String::from_utf8(bytes).expect("valid utf-8");
    assert!(!text.contains(cred), "credential token must not survive");
    assert!(!text.contains(b64), "high-entropy string must not survive");
    assert_eq!(secrets_found, 2);
    assert_eq!(redaction_version, scanner.version());
    assert!(truncation.is_none());
}

#[test]
fn false_positive_code_is_not_redacted() {
    let scanner = Scanner::new();
    // Unquoted assignment, short quoted value, and a hex SHA — the same
    // false-positive shapes T03-02 already proved quiet, now inside a JSON
    // payload context.
    let payload = json_payload(
        "let password = user.password; let short = \"abc\"; let rev = da39a3ee5e6b4b0d3255bfef95601890afd80709aa",
    );

    let result = prepare_payload(&payload, &[], Some("Read"), &no_deny(), &scanner);
    let PreparedPayload::Included {
        bytes,
        secrets_found,
        truncation,
        ..
    } = result
    else {
        panic!("expected Included");
    };
    assert_eq!(secrets_found, 0);
    assert!(truncation.is_none());
    assert_eq!(String::from_utf8(bytes).unwrap(), payload, "byte-identical");
}

#[test]
fn payload_at_exactly_the_cap_is_not_truncated() {
    let scanner = Scanner::new();
    let payload = "a".repeat(PAYLOAD_CAP_BYTES);
    let result = prepare_payload(&payload, &[], None, &no_deny(), &scanner);
    let PreparedPayload::Included {
        bytes, truncation, ..
    } = result
    else {
        panic!("expected Included");
    };
    assert_eq!(bytes.len(), PAYLOAD_CAP_BYTES);
    assert!(truncation.is_none());
}

#[test]
fn one_byte_over_the_cap_is_truncated_with_hash_and_original_size() {
    let scanner = Scanner::new();
    let payload = "a".repeat(PAYLOAD_CAP_BYTES + 1);
    let result = prepare_payload(&payload, &[], None, &no_deny(), &scanner);
    let PreparedPayload::Included {
        bytes, truncation, ..
    } = result
    else {
        panic!("expected Included");
    };
    assert_eq!(bytes.len(), PAYLOAD_CAP_BYTES);
    let t = truncation.expect("truncated");
    assert_eq!(t.original_size, PAYLOAD_CAP_BYTES as u64 + 1);
    assert_eq!(
        t.hash,
        truncated_excerpt(payload.as_bytes()),
        "hash covers the full pre-cap bytes"
    );
}

#[test]
fn a_cap_inside_a_multibyte_character_moves_back_to_a_boundary() {
    let scanner = Scanner::new();
    let mut payload = "a".repeat(PAYLOAD_CAP_BYTES - 3);
    payload.push('😀'); // 4 bytes: the cap falls inside it
    payload.push_str("tail");

    let result = prepare_payload(&payload, &[], None, &no_deny(), &scanner);
    let PreparedPayload::Included {
        bytes, truncation, ..
    } = result
    else {
        panic!("expected Included");
    };
    assert_eq!(
        bytes.len(),
        PAYLOAD_CAP_BYTES - 3,
        "straddling char dropped whole"
    );
    let text = String::from_utf8(bytes).expect("still valid utf-8");
    assert!(text.chars().all(|c| c == 'a'));
    assert!(truncation.is_some());
}

#[test]
fn no_raw_secret_survives_redaction_and_capping_of_an_oversized_payload() {
    let scanner = Scanner::new();
    let cred_early = "AKIAIOSFODNN7EXAMPLE";
    let cred_late = "ghp_012345678901234567890123456789012345";
    let padding = "x".repeat(PAYLOAD_CAP_BYTES);
    // A space precedes each credential: `=` is itself a token character (so it
    // does not break a token run on its own — established T03-02 scanner
    // behavior, not something this task changes), so a realistic separator is
    // needed for the token boundary to isolate the credential correctly.
    let payload = format!("before= {cred_early} pad={padding} after= {cred_late}");
    assert!(
        payload.len() > PAYLOAD_CAP_BYTES,
        "sanity: definitely over cap"
    );

    let result = prepare_payload(&payload, &[], None, &no_deny(), &scanner);
    let PreparedPayload::Included {
        bytes,
        truncation,
        secrets_found,
        ..
    } = result
    else {
        panic!("expected Included");
    };
    let text = String::from_utf8(bytes).expect("valid utf-8");
    assert!(!text.contains(cred_early));
    assert!(!text.contains(cred_late));
    assert!(text.len() <= PAYLOAD_CAP_BYTES);
    assert!(truncation.is_some());
    // Redaction runs over the *whole* text before the cap is ever applied, so
    // both secrets are found regardless of which survives the byte cut.
    assert_eq!(secrets_found, 2);
}

#[test]
fn deny_listed_path_becomes_envelope_only() {
    let scanner = Scanner::new();
    let deny = SpoolConfig {
        deny_paths: vec!["secrets".to_string()],
        deny_tools: vec![],
    };
    let paths = vec!["secrets/api.key".to_string()];
    let result = prepare_payload("irrelevant content", &paths, Some("Read"), &deny, &scanner);
    assert_eq!(result, PreparedPayload::EnvelopeOnly);
}

#[test]
fn deny_listed_tool_becomes_envelope_only() {
    let scanner = Scanner::new();
    let deny = SpoolConfig {
        deny_paths: vec![],
        deny_tools: vec!["Bash".to_string()],
    };
    let result = prepare_payload("irrelevant content", &[], Some("Bash"), &deny, &scanner);
    assert_eq!(result, PreparedPayload::EnvelopeOnly);
}

#[test]
fn a_path_that_merely_shares_a_prefix_string_is_not_denied() {
    let scanner = Scanner::new();
    let deny = SpoolConfig {
        deny_paths: vec!["secrets".to_string()],
        deny_tools: vec![],
    };
    let paths = vec!["not-secrets/x.txt".to_string()];
    let result = prepare_payload("hello", &paths, Some("Read"), &deny, &scanner);
    assert!(matches!(result, PreparedPayload::Included { .. }));
}

/// The card's own test bullet, verbatim: "instrumentation proves raw secret
/// never reaches spool builder/remote sink." `prepare_payload` is the sole
/// producer of the bytes any future spool builder (T13-02) will ever see, so
/// proving the raw secret is absent from its output — in both the redacted
/// and the envelope-only path — is exactly that proof at this layer.
#[test]
fn instrumentation_proves_raw_secret_never_reaches_the_prepared_bytes() {
    let scanner = Scanner::new();
    let secret = "AKIAIOSFODNN7EXAMPLE";
    let payload = format!("token = {secret}");

    // Included path: the secret is redacted, never present in the output bytes.
    let result = prepare_payload(&payload, &[], None, &no_deny(), &scanner);
    let PreparedPayload::Included { bytes, .. } = result else {
        panic!("expected Included");
    };
    assert!(!String::from_utf8(bytes).unwrap().contains(secret));

    // Envelope-only path: no payload bytes are ever produced at all — the
    // deny-list check runs before the scanner is even invoked, so the raw
    // payload text is dropped in its entirety, not merely redacted.
    let deny = SpoolConfig {
        deny_paths: vec!["secret-dir".to_string()],
        deny_tools: vec![],
    };
    let result = prepare_payload(
        &payload,
        &["secret-dir/file".to_string()],
        None,
        &deny,
        &scanner,
    );
    assert_eq!(result, PreparedPayload::EnvelopeOnly);
}
