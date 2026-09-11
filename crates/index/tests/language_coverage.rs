//! Corpus/manifest language coverage (T04-01 acceptance).
//!
//! ADR-0001 closes O4 by fixing the v0 language set. The corpus has no `language`
//! field and the benchmark is single-language (TypeScript), so "coverage" is the
//! following invariant, verified here offline and deterministically:
//!
//!   (a) the benchmark language (`manifest.json` `source.language`) MUST be in the
//!       selected set — otherwise the search gates (T12-05) are unmeasurable;
//!   (b) the set is consistent across the normative spec text and the code default
//!       (`Config::default`);
//!   (c) it is non-vacuous: languages the corpus does NOT cover are explicitly
//!       acknowledged in ADR-0001, not silently included.
//!
//! Parsing stays dependency-free (no `serde_json`): the manifest language and the
//! spec `languages = [...]` array are read with small string helpers, mirroring the
//! `crates/xtask/tests/ci_config.rs` style; the config set comes typed from
//! `local_rag_core`.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use local_rag_core::config::Config;
use local_rag_test_support::fixtures::read_fixture;

fn workspace_root() -> PathBuf {
    // crates/index -> crates -> workspace root
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
}

fn read_doc(rel: &str) -> String {
    let path = workspace_root().join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Value of the first `"language": "..."` string in a JSON document.
fn json_string_value(json: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let after_key = &json[json.find(&needle)? + needle.len()..];
    let after_colon = &after_key[after_key.find(':')? + 1..];
    let open = after_colon.find('"')? + 1;
    let rest = &after_colon[open..];
    let close = rest.find('"')?;
    Some(rest[..close].to_string())
}

/// Parse the `languages = ["a", "b", ...]` array from a TOML/spec snippet.
fn parse_languages_array(text: &str) -> BTreeSet<String> {
    let line = text
        .lines()
        .find(|l| l.trim_start().starts_with("languages") && l.contains('['))
        .unwrap_or_else(|| panic!("no `languages = [...]` line found"));
    let inner = &line[line.find('[').unwrap() + 1..line.find(']').unwrap()];
    inner
        .split(',')
        .map(|s| s.trim().trim_matches('"').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// ADR-0001's v0 three plus ADR-0015's additions (one per group-24 card).
fn expected_set() -> BTreeSet<String> {
    [
        "typescript",
        "javascript",
        "rust",
        "python",
        "bash",
        "go",
        "toml",
        "yaml",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

#[test]
fn benchmark_language_is_in_the_selected_set() {
    let manifest = read_fixture("manifest.json").expect("read manifest.json");
    let benchmark = json_string_value(&manifest, "language")
        .expect("manifest source.language")
        .to_lowercase();
    // The imported corpus/baseline is this one language; without it the gates
    // cannot be measured.
    assert_eq!(
        benchmark, "typescript",
        "benchmark language changed unexpectedly"
    );

    let spec = read_doc("docs/specification/02-architecture.md");
    let spec_langs = parse_languages_array(&spec);
    assert!(
        spec_langs.contains(&benchmark),
        "benchmark language `{benchmark}` must be in the v0 set {spec_langs:?}"
    );
}

#[test]
fn language_set_is_consistent_across_spec_and_code() {
    let spec_langs = parse_languages_array(&read_doc("docs/specification/02-architecture.md"));
    let config_langs: BTreeSet<String> = Config::default().index.languages.into_iter().collect();

    assert_eq!(
        spec_langs,
        expected_set(),
        "spec 02 §3.1 language set diverged from ADR-0001"
    );
    assert_eq!(
        config_langs, spec_langs,
        "config default `index.languages` diverged from the spec set"
    );
}

#[test]
fn non_corpus_languages_are_acknowledged_in_adr() {
    let adr = read_doc("docs/adr/0001-first-release-language-set.md");
    // Everything except the benchmark language lacks a corpus in v0; the ADR must
    // own that limitation explicitly rather than let it pass silently.
    let mut non_corpus: Vec<String> = expected_set().into_iter().collect();
    non_corpus.retain(|l| l != "typescript");
    assert!(
        !non_corpus.is_empty(),
        "expected at least one non-corpus language"
    );

    assert!(
        adr.to_lowercase().contains("no benchmark corpus"),
        "ADR-0001 must acknowledge the missing benchmark corpus"
    );
    for lang in non_corpus {
        assert!(
            adr_names_language(&adr, &lang),
            "ADR-0001 must name the non-corpus language `{lang}`"
        );
    }
}

/// Whether `adr` names `lang` as a **word**, not as a fragment of a longer one
/// (`D-134`).
///
/// The former predicate was `adr.to_lowercase().contains(lang)`, which goes
/// vacuous for a short token: ADR-0001 contains `goldens`, `going` and
/// `algorithm`, every one of which contains `go`, so the guard would have passed
/// for Go while proving only that the ADR is written in English — and the same
/// trap waits for any future `c`, `r`, `d` or `ml`. A character continues a token
/// when it is ASCII alphanumeric or `_`; both neighbours of a hit must fail that
/// test. Hand-written rather than a regex: this file is deliberately
/// dependency-free.
fn adr_names_language(adr: &str, lang: &str) -> bool {
    let haystack = adr.to_lowercase();
    let needle = lang.to_lowercase();
    if needle.is_empty() {
        return false;
    }
    let bytes = haystack.as_bytes();
    let is_token_byte = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    haystack.match_indices(&needle).any(|(start, hit)| {
        let end = start + hit.len();
        let before_ok = start == 0 || !is_token_byte(bytes[start - 1]);
        let after_ok = end == bytes.len() || !is_token_byte(bytes[end]);
        before_ok && after_ok
    })
}

/// `D-134`: the corrected acknowledgment predicate must be demonstrable as a
/// guard — a test that cannot be shown to fail is not one.
#[test]
fn adr_acknowledgment_predicate_rejects_a_fragment_match() {
    // The exact text the old `contains` predicate accepted: three words that
    // contain `go`, and no standalone `go` anywhere.
    let without = "The set is TypeScript, JavaScript and Rust. \
                   A new adapter + query set + goldens; the algorithm is going to stay.";
    assert!(
        without.to_lowercase().contains("go"),
        "the former substring predicate accepted this text — that is what D-134 is"
    );
    assert!(
        !adr_names_language(without, "go"),
        "a fragment of `goldens`/`going`/`algorithm` must not count as naming Go"
    );
    // The same text once the ADR actually names the language, in the amendment's
    // own bold form — `*` is not a token character, so the word boundary holds.
    let with = format!("{without} ADR-0015 adds **python**, **go** and **bash**.");
    assert!(adr_names_language(&with, "go"));

    // Longer names are unaffected, and a fragment of one still does not count.
    assert!(adr_names_language("… rust …", "rust"));
    assert!(!adr_names_language("rustacean", "rust"));
    assert!(!adr_names_language("go_lang", "go"));
    assert!(adr_names_language("`go`, `bash`", "bash"));
    assert!(!adr_names_language("", "go"));
}

#[test]
fn coverage_string_helpers_are_correct() {
    let manifest = r#"{ "source": { "language": "TypeScript", "note": "language x" } }"#;
    assert_eq!(
        json_string_value(manifest, "language").as_deref(),
        Some("TypeScript")
    );
    assert_eq!(json_string_value(manifest, "missing"), None);

    let toml = "  languages = [\"typescript\", \"javascript\", \"rust\", \"python\", \"bash\", \
                 \"go\", \"toml\", \"yaml\"]\n";
    assert_eq!(parse_languages_array(toml), expected_set());
    // Reordering does not change the parsed set.
    let reordered = "languages = [\"rust\",\"go\",\"toml\",\"yaml\",\"bash\",\"python\",\
                     \"typescript\" , \"javascript\"]";
    assert_eq!(parse_languages_array(reordered), expected_set());
}
