//! Language identity and the language-by-path selector (spec 03 §2.3.1, 06 §2.1;
//! ADR-0001).
//!
//! [`LanguageId`] is the closed v0 set fixed by ADR-0001 (closes O4). Its
//! canonical strings ([`LanguageId::as_str`]) are the same tokens as the
//! `index.languages` config array (spec 02 §3.1) and the `lang=` field of a
//! `parser_fingerprint` (spec 03 §2.3.1). [`select_language`] realizes the
//! deferred "precise selector is T04-02" from ADR-0001: language is chosen by
//! file extension, so byte-identical source under different-language extensions
//! yields different file revisions `[FIXED]`.

use std::path::Path;

use local_rag_store::code::UnitKind;

/// The first-release language set (ADR-0001, closes O4) `[FIXED]`.
///
/// A closed enum — not an open string — so the selector, the fingerprint, and the
/// descriptor table all range over exactly the languages the project supports in
/// v0. Adding a language after v0 is additive (a new variant + adapter + goldens),
/// with no schema or identity change (spec 03 §2.3.1 keys on `lang`/`grammar`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LanguageId {
    /// TypeScript (`.ts` `.tsx` `.mts` `.cts`).
    TypeScript,
    /// JavaScript (`.js` `.jsx` `.mjs` `.cjs`).
    JavaScript,
    /// Rust (`.rs`).
    Rust,
}

impl LanguageId {
    /// Every language in the closed v0 set, in a stable order.
    pub const ALL: [LanguageId; 3] = [
        LanguageId::TypeScript,
        LanguageId::JavaScript,
        LanguageId::Rust,
    ];

    /// The canonical language id string (spec 02 §3.1; the `lang=` fingerprint
    /// field, spec 03 §2.3.1).
    ///
    /// MUST equal the corresponding `index.languages` config token; the
    /// integration test `language_ids_match_config_language_set` guards this.
    pub const fn as_str(self) -> &'static str {
        match self {
            LanguageId::TypeScript => "typescript",
            LanguageId::JavaScript => "javascript",
            LanguageId::Rust => "rust",
        }
    }

    /// Parse a canonical language string back into a [`LanguageId`], or `None` if
    /// it is outside the closed set.
    ///
    /// The inverse of [`as_str`](LanguageId::as_str); mirrors
    /// `DataPolicy::from_str_value` so callers raise a typed error rather than
    /// silently defaulting.
    pub fn from_str_value(value: &str) -> Option<LanguageId> {
        match value {
            "typescript" => Some(LanguageId::TypeScript),
            "javascript" => Some(LanguageId::JavaScript),
            "rust" => Some(LanguageId::Rust),
            _ => None,
        }
    }
}

/// Select the language for a file by its extension (spec 03 §2.3.1, 06 §2.1;
/// ADR-0001 extension table) `[FIXED]`.
///
/// Extension-only and case-insensitive. A path outside the v0 set — an unknown
/// extension, a dotfile, an extensionless name — yields `None`; the caller routes
/// `None` to the language-agnostic / skip path (`config_section | text_section |
/// fallback_chunk`, spec 06 §2.1), which is specified by a later task. Uses
/// [`Path::extension`], so `foo.d.ts` selects on `ts`, `.gitignore`/`Makefile`/
/// `foo.` have no extension, and a non-UTF-8 extension yields `None`.
pub fn select_language(path: &Path) -> Option<LanguageId> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "ts" | "tsx" | "mts" | "cts" => Some(LanguageId::TypeScript),
        "js" | "jsx" | "mjs" | "cjs" => Some(LanguageId::JavaScript),
        "rs" => Some(LanguageId::Rust),
        _ => None,
    }
}

/// What the language-agnostic path chunked a file as (D-098; spec 06 §2.1's
/// `config_section | text_section | fallback_chunk`).
///
/// **Not a language.** ADR-0001's `[FIXED]` v0 language set is [`LanguageId`] and
/// is untouched by this: these are the dialects of the *universal* path, the one
/// spec 06 §2.1 has always required ("all kinds are indexed — v1 parity
/// requirement") and that no task owned until D-098. Keeping them a sibling enum
/// rather than three more `LanguageId` variants is what lets the language set stay
/// closed and the ADR stay true.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UniversalKind {
    /// Structured configuration — sections are top-level keys.
    Config,
    /// Prose — sections are heading spans.
    Text,
    /// Anything else textual — sections are size-bounded line windows.
    Fallback,
}

impl UniversalKind {
    /// Every universal dialect, in a stable order.
    pub const ALL: [UniversalKind; 3] = [
        UniversalKind::Config,
        UniversalKind::Text,
        UniversalKind::Fallback,
    ];

    /// The canonical dialect string (the `lang=` fingerprint/locator field).
    pub const fn as_str(self) -> &'static str {
        match self {
            UniversalKind::Config => "config",
            UniversalKind::Text => "text",
            UniversalKind::Fallback => "fallback",
        }
    }

    /// The `parsed_unit.unit_kind` this dialect produces for its sections.
    pub const fn unit_kind(self) -> UnitKind {
        match self {
            UniversalKind::Config => UnitKind::ConfigSection,
            UniversalKind::Text => UnitKind::TextSection,
            UniversalKind::Fallback => UnitKind::FallbackChunk,
        }
    }
}

/// The `lang=` value of a `parser_fingerprint` and a `SyntaxLocator`: one of the
/// closed v0 languages, or one of the universal path's dialects (D-098).
///
/// The two live in one type because they occupy one field. They are not peers in
/// meaning: a [`LanguageId`] names a tree-sitter grammar, a [`UniversalKind`]
/// names a chunking policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceDialect {
    /// Parsed by the tree-sitter adapter for this language.
    Language(LanguageId),
    /// Chunked by the universal path under this policy.
    Universal(UniversalKind),
}

impl SourceDialect {
    /// The canonical `lang=` string.
    pub const fn as_str(self) -> &'static str {
        match self {
            SourceDialect::Language(l) => l.as_str(),
            SourceDialect::Universal(u) => u.as_str(),
        }
    }

    /// Parse a canonical string back, or `None` if it names neither a v0 language
    /// nor a universal dialect.
    pub fn from_str_value(value: &str) -> Option<SourceDialect> {
        if let Some(l) = LanguageId::from_str_value(value) {
            return Some(SourceDialect::Language(l));
        }
        match value {
            "config" => Some(SourceDialect::Universal(UniversalKind::Config)),
            "text" => Some(SourceDialect::Universal(UniversalKind::Text)),
            "fallback" => Some(SourceDialect::Universal(UniversalKind::Fallback)),
            _ => None,
        }
    }
}

/// Extensions chunked as [`UniversalKind::Config`] (ADR-0012) `[SPEC]`.
///
/// Structured, key-bearing formats. Extension-only and case-insensitive, the same
/// rule [`select_language`] applies, so the two selectors cannot disagree about
/// what a path is.
const CONFIG_EXTENSIONS: &[&str] = &[
    "yaml",
    "yml",
    "json",
    "jsonc",
    "json5",
    "toml",
    "ini",
    "cfg",
    "conf",
    "properties",
    "env",
    "tf",
    "tfvars",
    "hcl",
    "tfstate",
];

/// Extensions chunked as [`UniversalKind::Text`] (ADR-0012) `[SPEC]`.
const TEXT_EXTENSIONS: &[&str] = &["md", "mdx", "markdown", "txt", "rst", "adoc", "asciidoc"];

/// Which universal policy chunks `path` (ADR-0012) `[SPEC]`.
///
/// Total by construction: an extension outside both tables — and an extensionless
/// name, and a dotfile — is [`UniversalKind::Fallback`]. That totality is the
/// point. `select_language` returning `None` used to mean "written nowhere"
/// (spec 06 §2's deferral note, D-098); it now means "chunked by policy", and
/// there is no third outcome for a file the classifier accepted.
pub fn universal_kind(path: &Path) -> UniversalKind {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return UniversalKind::Fallback;
    };
    let ext = ext.to_ascii_lowercase();
    if CONFIG_EXTENSIONS.contains(&ext.as_str()) {
        UniversalKind::Config
    } else if TEXT_EXTENSIONS.contains(&ext.as_str()) {
        UniversalKind::Text
    } else {
        UniversalKind::Fallback
    }
}

/// The dialect that indexes `path` — a v0 language when its extension selects
/// one, otherwise the universal policy for it (D-098).
///
/// Total: every path this is called with gets a dialect, which is what makes
/// "every classified file is either indexed or skipped" an invariant rather than
/// an aspiration.
pub fn select_dialect(path: &Path) -> SourceDialect {
    match select_language(path) {
        Some(language) => SourceDialect::Language(language),
        None => SourceDialect::Universal(universal_kind(path)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_id_string_round_trips_and_rejects_bogus() {
        for l in LanguageId::ALL {
            assert_eq!(LanguageId::from_str_value(l.as_str()), Some(l));
        }
        // Case-sensitive canonical tokens; anything outside the set is `None`.
        assert_eq!(LanguageId::from_str_value("Rust"), None);
        assert_eq!(LanguageId::from_str_value("ts"), None);
        assert_eq!(LanguageId::from_str_value(""), None);
        assert_eq!(LanguageId::from_str_value("c"), None);
    }

    #[test]
    fn select_language_maps_every_adr_extension() {
        let cases = [
            ("a.ts", LanguageId::TypeScript),
            ("a.tsx", LanguageId::TypeScript),
            ("a.mts", LanguageId::TypeScript),
            ("a.cts", LanguageId::TypeScript),
            ("a.js", LanguageId::JavaScript),
            ("a.jsx", LanguageId::JavaScript),
            ("a.mjs", LanguageId::JavaScript),
            ("a.cjs", LanguageId::JavaScript),
            ("a.rs", LanguageId::Rust),
            ("nested/dir/module.rs", LanguageId::Rust),
            ("types/foo.d.ts", LanguageId::TypeScript),
        ];
        for (path, expected) in cases {
            assert_eq!(
                select_language(Path::new(path)),
                Some(expected),
                "extension mapping for {path}"
            );
        }
    }

    #[test]
    fn select_language_is_case_insensitive() {
        assert_eq!(
            select_language(Path::new("Foo.TS")),
            Some(LanguageId::TypeScript)
        );
        assert_eq!(select_language(Path::new("M.Rs")), Some(LanguageId::Rust));
        assert_eq!(
            select_language(Path::new("x.JsX")),
            Some(LanguageId::JavaScript)
        );
    }

    #[test]
    fn select_language_returns_none_for_unknown_and_pathological() {
        for path in [
            "main.c",         // not in the v0 set
            "main.cpp",       // the §2.3.1 .c/.cpp example — both out of set here
            "data.json",      // universal path, not a tree-sitter language
            "notes.md",       // universal path
            "Makefile",       // no extension
            ".gitignore",     // dotfile → no extension
            "foo.",           // trailing dot → no extension
            "noext",          // no extension at all
            "archive.tar.gz", // last extension `gz` is not in the set
        ] {
            assert_eq!(
                select_language(Path::new(path)),
                None,
                "expected no language for {path}"
            );
        }
    }
}
