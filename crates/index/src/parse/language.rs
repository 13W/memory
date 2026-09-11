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

/// The supported language set: the first-release set (ADR-0001, closes O4)
/// `[FIXED]`, extended post-v0 by ADR-0015 (one variant per group-24 card).
///
/// A closed enum — not an open string — so the selector, the fingerprint, and the
/// descriptor table all range over exactly the languages the project supports.
/// Adding a language after v0 is additive (a new variant + adapter + goldens),
/// with no schema or identity change (spec 03 §2.3.1 keys on `lang`/`grammar`);
/// the v0 three keep their meaning as the set that shipped v0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LanguageId {
    /// TypeScript (`.ts` `.tsx` `.mts` `.cts`).
    TypeScript,
    /// JavaScript (`.js` `.jsx` `.mjs` `.cjs`).
    JavaScript,
    /// Rust (`.rs`).
    Rust,
    /// Python (`.py` `.pyi`) — ADR-0015, T24-01.
    Python,
    /// Bash (`.sh` `.bash`) — ADR-0015, T24-02.
    Bash,
    /// Go (`.go`) — ADR-0015, T24-03.
    Go,
    /// TOML (`.toml`) — ADR-0015, T24-04. Sections, not symbols.
    Toml,
    /// YAML (`.yaml` `.yml`) — ADR-0015, T24-05. Sections, not symbols.
    Yaml,
}

impl LanguageId {
    /// Every language in the closed set, in a stable order (the v0 three first,
    /// then the ADR-0015 additions in card order).
    pub const ALL: [LanguageId; 8] = [
        LanguageId::TypeScript,
        LanguageId::JavaScript,
        LanguageId::Rust,
        LanguageId::Python,
        LanguageId::Bash,
        LanguageId::Go,
        LanguageId::Toml,
        LanguageId::Yaml,
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
            LanguageId::Python => "python",
            LanguageId::Bash => "bash",
            LanguageId::Go => "go",
            LanguageId::Toml => "toml",
            LanguageId::Yaml => "yaml",
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
            "python" => Some(LanguageId::Python),
            "bash" => Some(LanguageId::Bash),
            "go" => Some(LanguageId::Go),
            "toml" => Some(LanguageId::Toml),
            "yaml" => Some(LanguageId::Yaml),
            _ => None,
        }
    }
}

/// Select the language for a file by its extension (spec 03 §2.3.1, 06 §2.1;
/// ADR-0001 extension table, extended by ADR-0015's) `[FIXED]`.
///
/// Extension-only and case-insensitive. A path outside the set — an unknown
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
        "py" | "pyi" => Some(LanguageId::Python),
        "sh" | "bash" => Some(LanguageId::Bash),
        "go" => Some(LanguageId::Go),
        "toml" => Some(LanguageId::Toml),
        "yaml" | "yml" => Some(LanguageId::Yaml),
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
    "json",
    "jsonc",
    "json5",
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
            // ADR-0015 (T24-01).
            ("a.py", LanguageId::Python),
            ("stubs/typed.pyi", LanguageId::Python),
            // ADR-0015 (T24-02).
            ("deploy.sh", LanguageId::Bash),
            ("scripts/ci.bash", LanguageId::Bash),
            // ADR-0015 (T24-03).
            ("main.go", LanguageId::Go),
            ("internal/server/handler.go", LanguageId::Go),
            // ADR-0015 (T24-04): `toml` left `CONFIG_EXTENSIONS` in the same commit.
            ("Cargo.toml", LanguageId::Toml),
            ("crates/index/Cargo.toml", LanguageId::Toml),
            // ADR-0015 (T24-05): and `yaml`/`yml` did the same.
            ("deploy/values.yaml", LanguageId::Yaml),
            (".github/workflows/ci.yml", LanguageId::Yaml),
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
            select_language(Path::new("app.PY")),
            Some(LanguageId::Python)
        );
        assert_eq!(
            select_language(Path::new("Deploy.SH")),
            Some(LanguageId::Bash)
        );
        assert_eq!(select_language(Path::new("Main.GO")), Some(LanguageId::Go));
        assert_eq!(
            select_language(Path::new("Cargo.TOML")),
            Some(LanguageId::Toml)
        );
        assert_eq!(
            select_language(Path::new("Deploy.YML")),
            Some(LanguageId::Yaml)
        );
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

    /// ADR-0015 Decision 1: an extension belongs to **exactly one** selector.
    ///
    /// `select_dialect` consults `select_language` before `universal_kind`, so an
    /// extension listed in both would be silently shadowed — the language would win
    /// and the universal table would be a lie. Group 24 moves one extension per
    /// card (`toml` in T24-04, `yaml`/`yml` in T24-05), and this is the standing
    /// guard over every row of both tables, not just the row a card touches.
    #[test]
    fn no_extension_is_claimed_by_both_selectors() {
        for ext in CONFIG_EXTENSIONS.iter().chain(TEXT_EXTENSIONS) {
            let path = format!("sample.{ext}");
            assert_eq!(
                select_language(Path::new(&path)),
                None,
                "`{ext}` is in a universal table AND in select_language"
            );
            // …and the universal side still claims it, so the row is not orphaned.
            assert_ne!(
                universal_kind(Path::new(&path)),
                UniversalKind::Fallback,
                "`{ext}` is listed but classifies as Fallback"
            );
        }
        // The converse direction, stated on the extensions the languages own: each
        // resolves to a language dialect, never to a universal one.
        for path in [
            "a.ts", "a.tsx", "a.mts", "a.cts", "a.js", "a.jsx", "a.mjs", "a.cjs", "a.rs", "a.py",
            "a.pyi", "a.sh", "a.bash", "a.go", "a.toml", "a.yaml", "a.yml",
        ] {
            assert!(
                matches!(select_dialect(Path::new(path)), SourceDialect::Language(_)),
                "{path} must select a language dialect"
            );
        }
    }

    /// `toml` left `CONFIG_EXTENSIONS` in T24-04 and `yaml`/`yml` in T24-05
    /// (ADR-0015 Decision 1), which finishes the move for group 24. The formats
    /// that stay on the universal path are untouched.
    #[test]
    fn the_group_24_extensions_left_the_universal_config_table() {
        for (ext, path, language) in [
            ("toml", "Cargo.toml", LanguageId::Toml),
            ("yaml", "deploy/values.yaml", LanguageId::Yaml),
            ("yml", ".github/workflows/ci.yml", LanguageId::Yaml),
        ] {
            assert!(
                !CONFIG_EXTENSIONS.contains(&ext),
                "`{ext}` must have left the universal Config table"
            );
            assert_eq!(
                select_dialect(Path::new(path)),
                SourceDialect::Language(language)
            );
        }
        for ext in [
            "json",
            "jsonc",
            "json5",
            "ini",
            "cfg",
            "conf",
            "properties",
            "env",
        ] {
            assert!(
                CONFIG_EXTENSIONS.contains(&ext),
                "`{ext}` must stay on the universal Config path"
            );
        }
        // The universal Config policy is narrowed, not retired.
        assert!(!CONFIG_EXTENSIONS.is_empty());
    }
}
