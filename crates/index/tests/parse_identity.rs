//! Cross-crate parser-identity coverage (T04-02).
//!
//! Guards the coupling that unit tests inside the crate cannot see on their own:
//! the closed [`LanguageId`] set MUST agree with the `index.languages` config
//! default (spec 02 §3.1, ADR-0001). Mirrors the `language_coverage.rs`
//! (T04-01) approach of checking the code set against the typed config.

use std::collections::BTreeSet;

use local_rag_core::config::Config;
use local_rag_index::parse::LanguageId;

#[test]
fn language_ids_match_config_language_set() {
    let code_set: BTreeSet<String> = LanguageId::ALL
        .iter()
        .map(|l| l.as_str().to_string())
        .collect();

    let config_set: BTreeSet<String> = Config::default().index.languages.into_iter().collect();

    let expected: BTreeSet<String> = ["typescript", "javascript", "rust"]
        .into_iter()
        .map(String::from)
        .collect();

    assert_eq!(
        code_set, expected,
        "LanguageId set diverged from ADR-0001 (typescript, javascript, rust)"
    );
    assert_eq!(
        config_set, code_set,
        "config default `index.languages` diverged from the LanguageId set"
    );
}

#[test]
fn every_config_language_parses_back_to_a_language_id() {
    // Each canonical config token round-trips through `from_str_value`, proving the
    // selector/fingerprint domain covers exactly the configured languages.
    for token in Config::default().index.languages {
        assert!(
            LanguageId::from_str_value(&token).is_some(),
            "config language `{token}` has no LanguageId"
        );
    }
}
