//! T21-03: the script detector against the real bilingual corpus
//! (`fixtures/memory-recall/corpus.json`, X-010).
//!
//! The unit table in `normalize::detect` pins hand-written cases; this pins the
//! detector against text nobody wrote for it — the same 24 memory entries the
//! recall benchmark measures, each carrying a Russian original and its English
//! translation. Two properties matter:
//!
//! - it never panics on any of them (the module is pure, but "pure" is not
//!   "total" until something checks every real input);
//! - it is actually *useful* on them: the originals read as non-Latin, the
//!   translations as English. A detector that answered `Undetermined` for real
//!   entries would pass its own unit table and still be worthless.
//!
//! Deterministic: reads a committed fixture from disk, no clock, no network.

use std::path::{Path, PathBuf};

use local_rag_memory::normalize::detect::{ScriptClass, script_class, script_stats};

fn corpus_path() -> PathBuf {
    // Same resolution `xtask`'s own fixture readers use.
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/memory-recall/corpus.json")
}

fn entries() -> Vec<(String, String, String)> {
    let raw = std::fs::read_to_string(corpus_path()).expect("read the memory-recall corpus");
    let json: serde_json::Value = serde_json::from_str(&raw).expect("corpus is valid json");
    json["entries"]
        .as_array()
        .expect("corpus.entries is an array")
        .iter()
        .map(|e| {
            (
                e["id"].as_str().expect("id").to_string(),
                e["text_original"].as_str().expect("original").to_string(),
                e["text_english"].as_str().expect("english").to_string(),
            )
        })
        .collect()
}

#[test]
fn the_detector_survives_every_corpus_text() {
    let entries = entries();
    assert!(
        entries.len() >= 20,
        "the corpus must actually have been read, got {}",
        entries.len()
    );
    for (id, original, english) in &entries {
        for text in [original, english] {
            // The assertion is that these calls return at all — a panic here is
            // the failure this test exists to catch.
            let stats = script_stats(text);
            let class = script_class(text);
            assert_eq!(
                stats.considered(),
                stats.latin + stats.non_latin,
                "{id}: the counts must agree with their own sum",
            );
            assert!(
                matches!(
                    class,
                    ScriptClass::English | ScriptClass::NonLatin | ScriptClass::Undetermined
                ),
                "{id}: every text lands in some class",
            );
        }
    }
}

/// The detector must separate the two halves of the corpus — that separation is
/// the whole reason it exists.
///
/// X-010's corpus is deliberately bilingual in a specific way: half of its
/// entries were written in Russian and carry a translation, the other half were
/// already English and carry themselves (`text_original == text_english`, the
/// en-en group the benchmark scores against). Both halves are pinned here,
/// because both are load-bearing — the first is what the translator is for, and
/// the second is ADR-0010 Decision 8's own case: already-English text must cost
/// zero inference.
#[test]
fn the_detector_splits_the_corpus_exactly_along_its_bilingual_seam() {
    let mut needs_translation = 0usize;
    let mut already_english = 0usize;

    for (id, original, english) in entries() {
        assert_eq!(
            script_class(&english),
            ScriptClass::English,
            "{id}: the English side must cost zero inference (stats: {:?})",
            script_stats(&english),
        );

        if original == english {
            already_english += 1;
            assert_eq!(
                script_class(&original),
                ScriptClass::English,
                "{id}: an entry that was already English must not be translated \
                 (stats: {:?})",
                script_stats(&original),
            );
        } else {
            needs_translation += 1;
            assert_eq!(
                script_class(&original),
                ScriptClass::NonLatin,
                "{id}: the Russian original must be worth translating (stats: {:?})",
                script_stats(&original),
            );
        }
    }

    assert!(
        needs_translation > 0 && already_english > 0,
        "both halves must be exercised: {needs_translation} non-Latin, \
         {already_english} already-English",
    );
}
