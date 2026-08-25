//! The `parser_fingerprint` builder (spec 03 §2.3.1) `[FIXED semantics, format
//! [SPEC]]`.
//!
//! `parser_fingerprint` is a **canonical sorted `key=value` string** covering
//! everything that affects unit boundaries:
//! `chunk=<policy_ver>;grammar=<name>@<ver>;lang=<language_id>;norm=<boundary_norm_ver>;queries=<ts_query_ver>`.
//! It is a plain, human-inspectable TEXT value in `file_revision`'s
//! `UNIQUE (content_hash, parser_fingerprint)` — **not** a hash — so no
//! `Domain` variant is involved. `store` consumes it as an opaque `&str`.
//!
//! ## Format `[SPEC]`
//!
//! Keys are sorted in **ascending ASCII byte order** and joined with `;` with
//! **no trailing separator** (matching the concrete example in spec 03 §2.3.1).
//! Sorting makes the value **order-independent by construction**: however the
//! components are assembled, the same inputs render the same bytes.
//!
//! ## Versions are our boundary counters, reconciled to pinned crates
//!
//! `grammar_version`/`query_version` are **our** boundary-version counters, not
//! the upstream crate semver. T04-03 links the first real grammar (TypeScript,
//! `tsx` variant) and **reconciles them to `1`/`1`** against the pinned crates
//! `tree-sitter 0.24` / `tree-sitter-typescript 0.23` (see [`descriptor`]);
//! because no units are persisted before T04-06, the goldens below stay green —
//! this is the deliberate, documented reconciliation, never a silent bump
//! (ADR-0002). A later grammar/query change that shifts unit boundaries is a
//! deliberate bump (a rebuild event); the goldens and
//! `version_constants_and_descriptors_are_pinned` are the tripwire. JavaScript
//! and Rust grammars are not linked until T04-04/T04-05.

use crate::parse::language::{LanguageId, SourceDialect};

/// The chunking-policy version — how oversized/opaque content is split into
/// `fallback_chunk` units. The `chunk=` fingerprint field.
///
/// v0 semantics (ADR-0002): a `fallback_chunk` is emitted only for outermost
/// ERROR/MISSING spans; there is no size-based splitting. Bumping it moves unit
/// boundaries ⇒ new `(content_hash, parser_fingerprint)` keys ⇒ a full rebuild. A
/// deliberate, version-gated event, never an implementation convenience.
pub const CHUNK_POLICY_VERSION: u32 = 1;

/// The boundary-normalization version — pre-parse normalization that shifts where
/// unit boundaries fall. The `norm=` fingerprint field.
///
/// v0 semantics (ADR-0002): **identity** — the grammar parses the raw source
/// bytes, so spans address the exact `source_blob`. **Distinct** from
/// `local_rag_store`'s `NORMALIZATION_VERSION`: that versions `content_blob` text
/// identity and never affects boundaries, whereas this versions normalization that
/// changes the parse itself. Bumping it is a rebuild event on the same terms as
/// [`CHUNK_POLICY_VERSION`].
pub const BOUNDARY_NORM_VERSION: u32 = 1;

/// The boundary-affecting, per-language grammar/query metadata — the "data/config"
/// of ADR-0001 ("the choice lives in data/config, not in the parser abstraction").
///
/// `grammar_version`/`query_version` are our boundary counters. TypeScript's are
/// reconciled to the pinned real grammar in T04-03 (see [`descriptor`]); the
/// JavaScript/Rust rows stay declared constants until T04-04/T04-05. See the
/// module docs on the rebuild event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LanguageDescriptor {
    /// The grammar name (the `<name>` in `grammar=<name>@<ver>`).
    pub grammar_name: &'static str,
    /// The grammar version (the `<ver>` in `grammar=<name>@<ver>`).
    pub grammar_version: u32,
    /// The tree-sitter query-set version (the `queries=` field).
    pub query_version: u32,
}

/// The boundary-affecting descriptor for a language (ADR-0001 grammar mapping).
///
/// TypeScript is reconciled (T04-03, ADR-0002) to the linked crates
/// `tree-sitter 0.24` + `tree-sitter-typescript 0.23` (`tsx` grammar variant);
/// `grammar_version=1`/`query_version=1` are the boundary counters for that
/// binding, bumped deliberately on any boundary-shifting upgrade.
pub const fn descriptor(language: LanguageId) -> LanguageDescriptor {
    match language {
        LanguageId::TypeScript => LanguageDescriptor {
            grammar_name: "tree-sitter-typescript",
            grammar_version: 1,
            query_version: 1,
        },
        LanguageId::JavaScript => LanguageDescriptor {
            grammar_name: "tree-sitter-javascript",
            grammar_version: 1,
            query_version: 1,
        },
        LanguageId::Rust => LanguageDescriptor {
            grammar_name: "tree-sitter-rust",
            grammar_version: 1,
            query_version: 1,
        },
    }
}

/// Render `(key, value)` pairs as a canonical sorted string: sort by ascending
/// ASCII key, join `key=value` with `;`, no trailing separator (spec 03 §2.3.1
/// format `[SPEC]`).
///
/// Order-independent by construction — the caller may build the pairs in any
/// order. Reused by [`SyntaxLocator`](crate::parse::locator::SyntaxLocator)
/// serialization.
pub fn canonical_kv(pairs: &mut [(&str, String)]) -> String {
    pairs.sort_by(|a, b| a.0.cmp(b.0));
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(";")
}

/// The boundary-affecting components of a `parser_fingerprint` (spec 03 §2.3.1).
///
/// Named fields carry no ordering; [`to_canonical_string`](Self::to_canonical_string)
/// sorts, so a caller cannot accidentally produce a non-canonical value.
pub struct FingerprintComponents<'a> {
    /// The `chunk=` value ([`CHUNK_POLICY_VERSION`]).
    pub chunk_policy_version: u32,
    /// The grammar name in `grammar=<name>@<ver>`.
    pub grammar_name: &'a str,
    /// The grammar version in `grammar=<name>@<ver>`.
    pub grammar_version: u32,
    /// The `lang=` value ([`LanguageId::as_str`]).
    pub language_id: &'a str,
    /// The `norm=` value ([`BOUNDARY_NORM_VERSION`]).
    pub boundary_norm_version: u32,
    /// The `queries=` value.
    pub query_version: u32,
}

impl FingerprintComponents<'_> {
    /// Render the canonical `parser_fingerprint` string (spec 03 §2.3.1).
    pub fn to_canonical_string(&self) -> String {
        let mut pairs = [
            ("chunk", self.chunk_policy_version.to_string()),
            (
                "grammar",
                format!("{}@{}", self.grammar_name, self.grammar_version),
            ),
            ("lang", self.language_id.to_string()),
            ("norm", self.boundary_norm_version.to_string()),
            ("queries", self.query_version.to_string()),
        ];
        canonical_kv(&mut pairs)
    }
}

/// The canonical `parser_fingerprint` for a language (spec 03 §2.3.1), assembled
/// from the shared version constants and the language [`descriptor`].
pub fn parser_fingerprint(language: LanguageId) -> String {
    dialect_fingerprint(SourceDialect::Language(language))
}

/// The universal chunker's boundary-version counter — how the language-agnostic
/// path splits a file into sections (D-098). The `grammar=universal@<ver>` field.
///
/// Named `grammar` because it occupies the grammar slot of the `[SPEC]` format,
/// which this task does not change; semantically it versions a **chunking policy**,
/// not a parser. Bumping it moves unit boundaries ⇒ new
/// `(content_hash, parser_fingerprint)` keys ⇒ a rebuild of every universally
/// indexed file, on exactly the same terms as a grammar bump.
pub const UNIVERSAL_POLICY_VERSION: u32 = 1;

/// The `parser_fingerprint` for any dialect — a v0 language or a universal
/// chunking policy (D-098).
///
/// The format is unchanged (spec 03 §2.3.1 `[SPEC]`): the universal side fills the
/// same five keys, with `grammar=universal@1` and `queries=0` — zero because the
/// universal path runs no tree-sitter query set at all, and saying so is more
/// honest than borrowing `1` from a query file that does not exist.
pub fn dialect_fingerprint(dialect: SourceDialect) -> String {
    match dialect {
        SourceDialect::Language(language) => {
            let d = descriptor(language);
            FingerprintComponents {
                chunk_policy_version: CHUNK_POLICY_VERSION,
                grammar_name: d.grammar_name,
                grammar_version: d.grammar_version,
                language_id: language.as_str(),
                boundary_norm_version: BOUNDARY_NORM_VERSION,
                query_version: d.query_version,
            }
            .to_canonical_string()
        }
        SourceDialect::Universal(kind) => FingerprintComponents {
            chunk_policy_version: CHUNK_POLICY_VERSION,
            grammar_name: "universal",
            grammar_version: UNIVERSAL_POLICY_VERSION,
            language_id: kind.as_str(),
            boundary_norm_version: BOUNDARY_NORM_VERSION,
            query_version: 0,
        }
        .to_canonical_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typescript_fingerprint_is_exact_golden() {
        // Golden pins the FIXED key set, the ASCII-sorted order, the `;` separator
        // with no trailing separator, and the v0 declared versions. A real grammar
        // in T04-03 that bumps a version updates this deliberately (rebuild event).
        assert_eq!(
            parser_fingerprint(LanguageId::TypeScript),
            "chunk=1;grammar=tree-sitter-typescript@1;lang=typescript;norm=1;queries=1"
        );
    }

    #[test]
    fn javascript_and_rust_goldens() {
        assert_eq!(
            parser_fingerprint(LanguageId::JavaScript),
            "chunk=1;grammar=tree-sitter-javascript@1;lang=javascript;norm=1;queries=1"
        );
        assert_eq!(
            parser_fingerprint(LanguageId::Rust),
            "chunk=1;grammar=tree-sitter-rust@1;lang=rust;norm=1;queries=1"
        );
    }

    #[test]
    fn all_languages_have_distinct_fingerprints() {
        let fps: Vec<String> = LanguageId::ALL
            .iter()
            .map(|&l| parser_fingerprint(l))
            .collect();
        let mut sorted = fps.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            fps.len(),
            "fingerprints must be distinct per language"
        );
    }

    #[test]
    fn canonical_kv_is_order_independent() {
        // The same components in a scrambled order render byte-identically, and
        // equal the golden. This is the structural guarantee behind "reordered
        // config gives the same value".
        let mut scrambled = [
            ("queries", "1".to_string()),
            ("chunk", "1".to_string()),
            ("lang", "typescript".to_string()),
            ("grammar", "tree-sitter-typescript@1".to_string()),
            ("norm", "1".to_string()),
        ];
        let mut ordered = [
            ("chunk", "1".to_string()),
            ("grammar", "tree-sitter-typescript@1".to_string()),
            ("lang", "typescript".to_string()),
            ("norm", "1".to_string()),
            ("queries", "1".to_string()),
        ];
        let from_scrambled = canonical_kv(&mut scrambled);
        assert_eq!(from_scrambled, canonical_kv(&mut ordered));
        assert_eq!(from_scrambled, parser_fingerprint(LanguageId::TypeScript));
    }

    #[test]
    fn fingerprint_keys_are_ascii_sorted_and_complete() {
        let fp = parser_fingerprint(LanguageId::Rust);
        let keys: Vec<&str> = fp
            .split(';')
            .map(|kv| kv.split('=').next().unwrap())
            .collect();
        assert_eq!(keys, ["chunk", "grammar", "lang", "norm", "queries"]);
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        assert_eq!(keys, sorted, "keys must be in ascending ASCII order");
    }

    #[test]
    fn bumping_any_boundary_version_changes_fingerprint() {
        // A component builder with every field explicit; each variant perturbs
        // exactly one field so we prove every field independently keys the value.
        fn build(
            chunk: u32,
            grammar_name: &'static str,
            grammar_version: u32,
            lang: &'static str,
            norm: u32,
            queries: u32,
        ) -> String {
            FingerprintComponents {
                chunk_policy_version: chunk,
                grammar_name,
                grammar_version,
                language_id: lang,
                boundary_norm_version: norm,
                query_version: queries,
            }
            .to_canonical_string()
        }

        let base = build(1, "tree-sitter-typescript", 1, "typescript", 1, 1);

        let variants = [
            build(2, "tree-sitter-typescript", 1, "typescript", 1, 1), // chunk
            build(1, "tree-sitter-typescript-x", 1, "typescript", 1, 1), // grammar name
            build(1, "tree-sitter-typescript", 2, "typescript", 1, 1), // grammar version
            build(1, "tree-sitter-typescript", 1, "javascript", 1, 1), // lang
            build(1, "tree-sitter-typescript", 1, "typescript", 2, 1), // norm
            build(1, "tree-sitter-typescript", 1, "typescript", 1, 2), // queries
        ];

        for v in &variants {
            assert_ne!(*v, base, "a bumped version must change the fingerprint");
        }
        // Base + all six single-field variants are pairwise distinct.
        let mut all = variants.to_vec();
        all.push(base);
        all.sort();
        all.dedup();
        assert_eq!(all.len(), 7, "each field independently changes the value");
    }

    #[test]
    fn version_constants_and_descriptors_are_pinned() {
        // Tripwire: T04-03+ reconciling to a real grammar/query version must bump
        // these deliberately and update the goldens (a documented rebuild event).
        assert_eq!(CHUNK_POLICY_VERSION, 1);
        assert_eq!(BOUNDARY_NORM_VERSION, 1);

        let expected = [
            (LanguageId::TypeScript, "tree-sitter-typescript"),
            (LanguageId::JavaScript, "tree-sitter-javascript"),
            (LanguageId::Rust, "tree-sitter-rust"),
        ];
        for (lang, name) in expected {
            let d = descriptor(lang);
            assert_eq!(d.grammar_name, name);
            assert_eq!(d.grammar_version, 1);
            assert_eq!(d.query_version, 1);
        }
    }

    #[test]
    fn universal_fingerprints_are_exact_goldens_and_distinct() {
        use crate::parse::language::UniversalKind;
        // The same five keys, ASCII-sorted, no trailing separator — the format is
        // untouched by D-098; only the values are new.
        assert_eq!(
            dialect_fingerprint(SourceDialect::Universal(UniversalKind::Config)),
            "chunk=1;grammar=universal@1;lang=config;norm=1;queries=0"
        );
        assert_eq!(
            dialect_fingerprint(SourceDialect::Universal(UniversalKind::Text)),
            "chunk=1;grammar=universal@1;lang=text;norm=1;queries=0"
        );
        assert_eq!(
            dialect_fingerprint(SourceDialect::Universal(UniversalKind::Fallback)),
            "chunk=1;grammar=universal@1;lang=fallback;norm=1;queries=0"
        );

        // Every dialect, language and universal alike, has its own fingerprint —
        // the property structural sharing depends on: identical bytes indexed
        // under two policies must be two revisions, not one.
        let mut all: Vec<String> = LanguageId::ALL
            .iter()
            .map(|&l| dialect_fingerprint(SourceDialect::Language(l)))
            .chain(
                UniversalKind::ALL
                    .iter()
                    .map(|&u| dialect_fingerprint(SourceDialect::Universal(u))),
            )
            .collect();
        let total = all.len();
        all.sort();
        all.dedup();
        assert_eq!(all.len(), total, "two dialects share a fingerprint");
    }

    #[test]
    fn the_language_wrapper_agrees_with_the_dialect_builder() {
        for l in LanguageId::ALL {
            assert_eq!(
                parser_fingerprint(l),
                dialect_fingerprint(SourceDialect::Language(l)),
            );
        }
    }

    #[test]
    fn parser_fingerprint_is_deterministic() {
        let first = parser_fingerprint(LanguageId::TypeScript);
        for _ in 0..8 {
            assert_eq!(parser_fingerprint(LanguageId::TypeScript), first);
        }
    }
}
