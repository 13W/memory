//! The labelled precision/recall corpus for the shared secret scanner — D-097.
//!
//! Loads `fixtures/adversarial/index.json` at runtime and asserts every
//! `adversarial.redaction.*` case, both halves together. That pairing is the
//! whole point: a scanner can be made quiet by weakening it and loud by
//! tightening it, and only a corpus carrying both directions catches either
//! mistake. The `expected.has_secret` labels come from a live measurement on a
//! real repository (see the fixture's own `description`), not from taste.
//!
//! Why a runtime loader when the neighbouring `adversarial.*` cases are
//! hand-written tests: those describe end-to-end behavior of different
//! subsystems and have nothing to share. These are one function applied to one
//! string, so the corpus **is** the test, and adding a case must not require
//! touching Rust — that is what keeps the corpus growable as new false positives
//! are measured.

use local_rag_core::redaction::Scanner;

/// Every `adversarial.redaction.*` case, as `(id, text, expected_has_secret)`.
fn corpus() -> Vec<(String, String, bool)> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/adversarial/index.json"
    );
    let raw = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("read the adversarial fixture index at {path}: {e}"));
    let doc: serde_json::Value =
        serde_json::from_str(&raw).expect("the adversarial fixture index is valid JSON");

    doc["cases"]
        .as_array()
        .expect("cases is an array")
        .iter()
        .filter(|c| {
            c["id"]
                .as_str()
                .is_some_and(|id| id.starts_with("adversarial.redaction."))
        })
        .map(|c| {
            let id = c["id"].as_str().expect("id is a string").to_string();
            let text = c["input"]["text"]
                .as_str()
                .unwrap_or_else(|| panic!("{id}: input.text must be a string"))
                .to_string();
            let expected = c["expected"]["has_secret"]
                .as_bool()
                .unwrap_or_else(|| panic!("{id}: expected.has_secret must be a bool"));
            (id, text, expected)
        })
        .collect()
}

/// The gate: every labelled case gets the verdict its label says, and the corpus
/// carries enough of both kinds to be worth gating on.
#[test]
fn the_labelled_corpus_holds() {
    let scanner = Scanner::new();
    let cases = corpus();

    let (positives, negatives): (Vec<_>, Vec<_>) = cases.iter().partition(|(_, _, e)| *e);
    assert!(
        positives.len() >= 10 && negatives.len() >= 10,
        "the corpus must carry both directions in force — a one-sided corpus \
         cannot fail in the direction it omits (true: {}, false: {})",
        positives.len(),
        negatives.len(),
    );

    let mut wrong: Vec<String> = Vec::new();
    for (id, text, expected) in &cases {
        let actual = scanner.has_secret(text);
        if actual != *expected {
            wrong.push(format!(
                "{id}: expected has_secret={expected}, got {actual}"
            ));
        }
    }
    assert!(
        wrong.is_empty(),
        "{} of {} labelled cases disagree with the scanner:\n  {}",
        wrong.len(),
        cases.len(),
        wrong.join("\n  "),
    );
}

/// A finding must span bytes of the input, and `redact` must remove them.
///
/// Precision work is where a scanner most easily starts reporting a *verdict*
/// without a usable *span* — the spool and remote-transmission flows rewrite
/// payloads by span (spec 07 §2, 12 §1), so a true positive with a bad span is a
/// leak dressed as a detection.
#[test]
fn every_true_positive_yields_a_usable_span() {
    let scanner = Scanner::new();
    for (id, text, expected) in corpus() {
        if !expected {
            continue;
        }
        let findings = scanner.scan(&text);
        assert!(!findings.is_empty(), "{id}: scan() found nothing");
        for f in &findings {
            assert!(f.start < f.end, "{id}: empty span {f:?}");
            assert!(f.end <= text.len(), "{id}: span past the end {f:?}");
            assert!(
                text.is_char_boundary(f.start) && text.is_char_boundary(f.end),
                "{id}: span is not on char boundaries {f:?}",
            );
        }
        let redacted = scanner.redact(&text);
        assert!(redacted.findings > 0, "{id}: redact replaced nothing");
        assert_ne!(redacted.text, text, "{id}: redact left the text unchanged");
    }
}

/// A false-positive case must be left byte-identical by `redact`, not merely
/// unflagged by `has_secret`: the two must agree, or the file classifier and the
/// payload rewriter would disagree about the same bytes.
#[test]
fn every_true_negative_survives_redaction_unchanged() {
    let scanner = Scanner::new();
    for (id, text, expected) in corpus() {
        if expected {
            continue;
        }
        assert!(scanner.scan(&text).is_empty(), "{id}: scan() found a span");
        let redacted = scanner.redact(&text);
        assert_eq!(redacted.findings, 0, "{id}: redact reported a finding");
        assert_eq!(redacted.text, text, "{id}: redact rewrote clean text");
    }
}
