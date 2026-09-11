//! The TOML tree-sitter adapter (ADR-0015, T24-04; ADR-0002 derivation).
//!
//! The `tree-sitter-toml-ng` grammar is used for every `.toml` file. It is pinned at
//! `0.7` (ABI 14) to pair with the workspace `tree-sitter 0.24` core, on the terms
//! every grammar before it is held to (see the dependency allowlist in
//! CONTRIBUTING.md). The declared `grammar_version`/`query_version` (1/1) are
//! reconciled to the pinned crate — a documented, non-silent binding (spec 03
//! §2.3.1).
//!
//! This is the first adapter whose units are **not** `Symbol`: a table, a table
//! array element and a key each emit [`UnitKind::ConfigSection`] (ADR-0015
//! Decision 3). It is therefore also the first adapter to override
//! [`LanguageSpec::parent_unit_kinds`], without which every config section would come
//! out flat and `table:dependencies/key:serde` would be unreachable.
//!
//! Decisions, each measured against the real grammar before it was written and
//! pinned by a test below:
//!
//! - **Every `pair` is a unit, at any depth** (owner decision, T24-04). A `table`
//!   node's span covers its own pairs, so nesting comes from span containment; an
//!   inline table's pairs are `pair` nodes as well and nest one level further.
//! - **A sub-table is a sibling, not a child.** `[a.b]` is its own `table` node
//!   named by a `dotted_key`, so the route is `table:a.b` rather than
//!   `table:a/table:b` — the grammar's shape, not a choice this adapter makes.
//! - **A quoted key is unquoted before it names a unit.** `["café"]` is the only
//!   legal way to write a non-ASCII TOML key (a bare key admits `A-Za-z0-9_-` only,
//!   and `[café]` is a parse error), so without stripping the quotes the common
//!   non-ASCII case would have no usable name. A key whose content is still not a
//!   safe path segment — `["quoted key"]` — falls back to an ordinal anchor, which
//!   is honest rather than silently mangled.
//! - **A value's node kind is signature-bearing, its text is not.** `serde = "1"`
//!   and `serde = "2"` are the same key with a changed value, and a unit keeps its
//!   identity when its content changes — the same rule that keeps a function's body
//!   out of its signature.

use tree_sitter::{Node, Query};

use crate::parse::adapter::{
    CaptureRole, LanguageSpec, is_identifier_kind, is_safe_segment, parse_with,
};
use crate::parse::language::LanguageId;
use crate::parse::output::{ParseOutput, ReferenceKind};
use crate::parse::parser::LanguageParser;
use crate::parse::signature::SignatureDescriptor;
use local_rag_store::code::UnitKind;

/// The versioned query set (`queries=1`), embedded at build time.
const QUERY_SRC: &str = include_str!("toml.scm");

/// A TOML parser adapter over the `tree-sitter-toml-ng` grammar.
pub struct TomlParser {
    language: tree_sitter::Language,
    query: Query,
}

impl TomlParser {
    /// Compile the grammar and query once. The query is a build-time constant, so a
    /// compile failure is a bug (panics).
    pub fn new() -> Self {
        let language: tree_sitter::Language = tree_sitter_toml_ng::LANGUAGE.into();
        let query = Query::new(&language, QUERY_SRC).expect("the bundled TOML query must compile");
        Self { language, query }
    }
}

impl Default for TomlParser {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageParser for TomlParser {
    fn language(&self) -> LanguageId {
        LanguageId::Toml
    }

    fn parse(&self, source: &[u8]) -> ParseOutput {
        parse_with(self, source)
    }
}

impl LanguageSpec for TomlParser {
    fn language(&self) -> LanguageId {
        LanguageId::Toml
    }

    fn ts_language(&self) -> &tree_sitter::Language {
        &self.language
    }

    fn query(&self) -> &Query {
        &self.query
    }

    fn classify_capture(&self, capture_name: &str) -> CaptureRole {
        let section = |lang_kind| CaptureRole::Decl {
            unit_kind: UnitKind::ConfigSection,
            lang_kind,
        };
        match capture_name {
            "decl.table" => section("table"),
            "decl.table_array" => section("table_array"),
            "decl.key" => section("key"),
            _ => CaptureRole::Ignore,
        }
    }

    /// A config section parents another config section; nothing here is a `Symbol`.
    fn parent_unit_kinds(&self) -> &'static [UnitKind] {
        &[UnitKind::ConfigSection]
    }

    fn local_name(&self, decl: Node, src: &[u8]) -> Option<String> {
        let key = key_node(decl)?;
        if !is_identifier_kind(key.kind()) {
            return None;
        }
        let text = unquote(key.utf8_text(src).ok()?);
        if is_safe_segment(text) {
            Some(text.to_string())
        } else {
            None
        }
    }

    fn signature_descriptor(
        &self,
        decl: Node,
        unit_kind: UnitKind,
        lang_kind: &str,
        src: &[u8],
    ) -> String {
        let mut d =
            SignatureDescriptor::new(LanguageId::Toml.as_str(), unit_kind.as_str(), lang_kind);
        d.push(self.local_name(decl, src).unwrap_or_default());
        // The key's node kind: `[a.b]` and `["a.b"]` name the same section in TOML's
        // own semantics but are written differently, and the shape is what a reader
        // sees.
        d.push(key_node(decl).map(|k| k.kind()).unwrap_or_default());
        // The value's node kind only — never its text, so a changed version string
        // does not turn a key into a different unit.
        d.push(value_kind(decl));
        d.push(member_count(decl).to_string());
        d.fingerprint()
    }

    fn reference(
        &self,
        _capture_name: &str,
        _node: Node,
        _src: &[u8],
    ) -> Option<(ReferenceKind, String)> {
        // TOML declares no imports: `parse::output`'s `Import | TypeImport | Reexport`
        // has no TOML counterpart, and inventing one from a key that happens to hold
        // a path would be a guess.
        None
    }
}

/// The key node of a table, table array element or pair: its first key-shaped child.
///
/// The grammar gives a table's key no field name — it is simply the first named child
/// after the anonymous `[` — so the key is found by kind rather than by field.
fn key_node(decl: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = decl.walk();
    decl.named_children(&mut cursor)
        .find(|c| matches!(c.kind(), "bare_key" | "dotted_key" | "quoted_key"))
}

/// A quoted key's content, or the text unchanged.
///
/// `"café"` → `café`, `'lit'` → `lit`. Only a matched pair of surrounding quotes is
/// removed, so a bare or dotted key passes through untouched.
fn unquote(text: &str) -> &str {
    for quote in ['"', '\''] {
        if text.len() >= 2 && text.starts_with(quote) && text.ends_with(quote) {
            return &text[1..text.len() - 1];
        }
    }
    text
}

/// The node kind of a pair's value (`string`, `integer`, `array`, `inline_table`, …),
/// or `""` for a table header, which has no value of its own.
fn value_kind(decl: Node) -> String {
    if decl.kind() != "pair" {
        return String::new();
    }
    let mut cursor = decl.walk();
    decl.named_children(&mut cursor)
        .find(|c| {
            !matches!(
                c.kind(),
                "bare_key" | "dotted_key" | "quoted_key" | "comment"
            )
        })
        .map(|c| c.kind().to_string())
        .unwrap_or_default()
}

/// How many keys a section holds directly: a table's or table array element's own
/// pairs, or an inline table's pairs for a `pair`.
///
/// The shared `body_member_count` finds nothing here — a TOML section has no `body`
/// field and no `*_body` child — so without this a table's `sig` would not move when
/// the table gained a key.
fn member_count(decl: Node) -> usize {
    let mut cursor = decl.walk();
    let direct = decl
        .named_children(&mut cursor)
        .filter(|c| c.kind() == "pair")
        .count();
    if direct > 0 || decl.kind() != "pair" {
        return direct;
    }
    // A `pair` whose value is an inline table: count the table's own pairs.
    let mut cursor = decl.walk();
    let inline = decl
        .named_children(&mut cursor)
        .find(|c| c.kind() == "inline_table");
    match inline {
        Some(t) => {
            let mut cursor = t.walk();
            t.named_children(&mut cursor)
                .filter(|c| c.kind() == "pair")
                .count()
        }
        None => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::language::SourceDialect;
    use crate::parse::locator::SyntaxAnchor;

    fn parse(src: &str) -> ParseOutput {
        TomlParser::new().parse(src.as_bytes())
    }

    fn find<'a>(out: &'a ParseOutput, name: &str) -> &'a crate::parse::output::ParsedUnitDraft {
        out.units
            .iter()
            .find(|u| u.local_name.as_deref() == Some(name))
            .unwrap_or_else(|| panic!("no unit named {name}"))
    }

    fn index_of(out: &ParseOutput, unit: &crate::parse::output::ParsedUnitDraft) -> usize {
        out.units
            .iter()
            .position(|u| std::ptr::eq(u, unit))
            .unwrap()
    }

    fn routes(out: &ParseOutput) -> Vec<(Option<String>, Option<String>, String)> {
        out.units
            .iter()
            .map(|u| {
                let anchor = match &u.anchor {
                    SyntaxAnchor::Path(p) => format!("p:{p}"),
                    SyntaxAnchor::LocalOrdinal(o) => format!("o:{o}"),
                };
                (u.local_name.clone(), u.lang_kind.clone(), anchor)
            })
            .collect()
    }

    const CARGO: &str = "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\n\
                         [dependencies]\nserde = \"1\"\n\n[[bin]]\nname = \"a\"\n";

    #[test]
    fn grammar_loads_and_extracts_sections() {
        // Guard: if the grammar failed to load (e.g. an ABI mismatch), the engine
        // degrades to a file-only parse. A non-empty source MUST yield a section.
        let out = parse("[present]\nk = 1\n");
        assert!(
            out.units
                .iter()
                .any(|u| u.unit_kind == UnitKind::ConfigSection),
            "the TOML grammar must load and produce config sections"
        );
    }

    #[test]
    fn a_table_header_is_the_section_the_line_scanner_missed() {
        // THE CARD'S ACCEPTANCE. `parse::universal::config_key` recognizes only
        // `key:`/`key =` lines, so this exact input used to come out as
        // `name, version, serde, name` with `[package]`, `[dependencies]` and
        // `[[bin]]` never starting a section. Now the headers are the sections.
        let out = parse(CARGO);
        let headers: Vec<&str> = out
            .units
            .iter()
            .filter(|u| matches!(u.lang_kind.as_deref(), Some("table") | Some("table_array")))
            .map(|u| u.local_name.as_deref().unwrap_or(""))
            .collect();
        assert_eq!(headers, vec!["package", "dependencies", "bin"]);
        assert_eq!(find(&out, "package").lang_kind.as_deref(), Some("table"));
        assert_eq!(
            find(&out, "bin").lang_kind.as_deref(),
            Some("table_array"),
            "`[[bin]]` is an array element, not a table, and its route says so"
        );
        // The table's span reaches to the next header, so its keys are inside it.
        let package = find(&out, "package");
        assert_eq!(package.span.start, 0);
        assert_eq!(
            package.span.end,
            CARGO.find("[dependencies]").unwrap() as u32
        );
    }

    #[test]
    fn every_unit_is_a_config_section_and_none_is_a_symbol() {
        // ADR-0015 Decision 3: a TOML key is not a symbol. The file unit is the only
        // other kind here.
        let out = parse(CARGO);
        for u in &out.units {
            assert!(
                matches!(u.unit_kind, UnitKind::ConfigSection | UnitKind::File),
                "unexpected kind {:?}",
                u.unit_kind
            );
        }
        assert!(out.unresolved.is_empty(), "TOML declares no imports");
    }

    #[test]
    fn a_key_inside_a_table_nests_under_it() {
        // The parent seam (`parent_unit_kinds`): without it every section would be
        // flat and this route would read `key:serde`.
        let out = parse(CARGO);
        let deps = find(&out, "dependencies");
        let serde = find(&out, "serde");
        assert_eq!(serde.parent, Some(index_of(&out, deps)));
        assert_eq!(
            serde.anchor,
            SyntaxAnchor::Path("table:dependencies/key:serde".to_string())
        );
    }

    #[test]
    fn an_inline_tables_keys_nest_one_level_further() {
        let out = parse("[dependencies]\ntokio = { version = \"1\", features = [\"full\"] }\n");
        assert_eq!(
            find(&out, "features").anchor,
            SyntaxAnchor::Path("table:dependencies/key:tokio/key:features".to_string())
        );
        // The inline table is the key's value, so the key's member count sees it.
        let tokio = find(&out, "tokio");
        let plain = parse("[dependencies]\ntokio = \"1\"\n");
        assert_ne!(
            tokio.signature_fingerprint,
            find(&plain, "tokio").signature_fingerprint
        );
    }

    #[test]
    fn a_sub_table_is_a_sibling_named_by_its_dotted_key() {
        // The grammar's shape, not a choice: `[a.b]` is its own `table` node, so the
        // route is `table:a.b` rather than `table:a/table:b`.
        let out = parse("[a]\nx = 1\n\n[a.b]\ny = 2\n");
        let ab = find(&out, "a.b");
        assert_eq!(ab.parent, None);
        assert_eq!(ab.anchor, SyntaxAnchor::Path("table:a.b".to_string()));
        assert_eq!(
            find(&out, "y").anchor,
            SyntaxAnchor::Path("table:a.b/key:y".to_string())
        );
    }

    #[test]
    fn a_key_before_the_first_table_is_a_top_level_section() {
        let out = parse("root = true\n\n[a]\nk = 1\n");
        let root = find(&out, "root");
        assert_eq!(root.parent, None);
        assert_eq!(root.anchor, SyntaxAnchor::Path("key:root".to_string()));
    }

    #[test]
    fn quoted_keys_are_unquoted_and_unsafe_ones_take_an_ordinal() {
        // `["café"]` is the ONLY legal way to write a non-ASCII TOML key — a bare key
        // admits `A-Za-z0-9_-`, and `[café]` is a parse error (measured) — so without
        // unquoting, the common non-ASCII case would have no usable name.
        let out = parse("bare = 1\n\"quoted\" = 2\ndotted.key = 3\n'lit' = 4\n");
        let names: Vec<&str> = out
            .units
            .iter()
            .filter(|u| u.unit_kind == UnitKind::ConfigSection)
            .map(|u| u.local_name.as_deref().unwrap_or("<none>"))
            .collect();
        assert_eq!(names, vec!["bare", "quoted", "dotted.key", "lit"]);

        // A key whose content is still not a safe path segment falls back honestly.
        let out = parse("[\"quoted key\"]\nz = 3\n");
        let table = out
            .units
            .iter()
            .find(|u| u.lang_kind.as_deref() == Some("table"))
            .expect("a table");
        assert_eq!(table.local_name, None);
        assert_eq!(table.anchor, SyntaxAnchor::LocalOrdinal(0));
    }

    #[test]
    fn repeated_array_elements_are_indistinguishable_and_take_ordinals() {
        // Two `[[bin]]` elements have the same name, the same key shape, no value of
        // their own and the same member count — nothing in a signature can separate
        // them, so the engine demotes the pair to ordinals rather than letting them
        // share one anchor. Their keys keep named routes; those two rows stay
        // distinct by span and by the locator's own `blob` (spec 06 §2.1's T24-04
        // amendment says so).
        let out = parse("[[bin]]\nname = \"a\"\n\n[[bin]]\nname = \"b\"\n");
        let bins: Vec<_> = out
            .units
            .iter()
            .filter(|u| u.lang_kind.as_deref() == Some("table_array"))
            .collect();
        assert_eq!(bins.len(), 2);
        assert_eq!(bins[0].anchor, SyntaxAnchor::LocalOrdinal(1));
        assert_eq!(bins[1].anchor, SyntaxAnchor::LocalOrdinal(2));
        assert_eq!(bins[0].signature_fingerprint, bins[1].signature_fingerprint);
        // A single `[[bin]]` keeps its name.
        let out = parse("[[bin]]\nname = \"a\"\n");
        assert_eq!(
            find(&out, "bin").anchor,
            SyntaxAnchor::Path("table_array:bin".to_string())
        );
    }

    #[test]
    fn a_changed_value_keeps_the_key_but_a_changed_value_kind_does_not() {
        // A unit keeps its identity when its content changes — the rule that keeps a
        // function's body out of its signature. The value's *kind* is another matter.
        let sig = |src: &str| find(&parse(src), "k").signature_fingerprint.clone();
        let base = sig("k = 1\n");
        assert_eq!(base, sig("k = 2\n"), "the value's text is not in the sig");
        assert_ne!(base, sig("k = \"1\"\n"), "its kind is");
        assert_ne!(base, sig("k = [1]\n"));
        assert_ne!(base, sig("k = { a = 1 }\n"));
    }

    #[test]
    fn a_table_gaining_a_key_moves_its_signature() {
        // `body_member_count` finds nothing in a TOML section (no `body` field, no
        // `*_body` child), which is why this adapter counts members itself.
        let sig = |src: &str| find(&parse(src), "t").signature_fingerprint.clone();
        assert_ne!(sig("[t]\na = 1\n"), sig("[t]\na = 1\nb = 2\n"));
    }

    #[test]
    fn every_file_gets_exactly_one_file_unit() {
        let src = "k = 1\n";
        let out = parse(src);
        let files: Vec<_> = out
            .units
            .iter()
            .filter(|u| u.unit_kind == UnitKind::File)
            .collect();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].span.start, 0);
        assert_eq!(files[0].span.end, src.len() as u32);
        assert_eq!(files[0].anchor, SyntaxAnchor::Path("file".to_string()));
    }

    #[test]
    fn empty_and_comment_only_files_are_only_a_file_unit() {
        for src in ["", "\n\n   \n", "# just a comment\n"] {
            let out = parse(src);
            assert_eq!(out.units.len(), 1, "for {src:?}");
            assert_eq!(out.units[0].unit_kind, UnitKind::File);
        }
    }

    #[test]
    fn byte_spans_are_exact() {
        let src = "[t]\nk = 1";
        let out = parse(src);
        let t = find(&out, "t");
        assert_eq!(t.span.start, 0);
        assert_eq!(t.span.end, src.len() as u32);
        let k = find(&out, "k");
        assert_eq!(k.span.start, src.find("k = 1").unwrap() as u32);
        assert_eq!(k.span.end, src.len() as u32);
    }

    #[test]
    fn error_input_yields_fallback_chunk_and_recovers_the_next_table() {
        let src = "[unclosed\nkey = = =\n\n[ok]\nk = 1\n";
        let out = parse(src);
        let chunk = out
            .units
            .iter()
            .find(|u| u.unit_kind == UnitKind::FallbackChunk)
            .expect("malformed input must produce a fallback chunk");
        assert_eq!(chunk.span.start, 0);
        let ok = find(&out, "ok");
        assert_eq!(ok.anchor, SyntaxAnchor::Path("table:ok".to_string()));
        assert!(ok.span.start >= chunk.span.end);
    }

    #[test]
    fn parse_is_deterministic() {
        let first = parse(CARGO);
        for _ in 0..4 {
            assert_eq!(parse(CARGO), first);
        }
        assert_eq!(TomlParser::new().parse(CARGO.as_bytes()), first);
        // And the canonical order is stable enough to state outright.
        assert_eq!(
            routes(&first)
                .into_iter()
                .map(|(n, _, a)| format!("{}@{a}", n.unwrap_or_default()))
                .collect::<Vec<_>>(),
            vec![
                "@p:file",
                "package@p:table:package",
                "name@p:table:package/key:name",
                "version@p:table:package/key:version",
                "dependencies@p:table:dependencies",
                "serde@p:table:dependencies/key:serde",
                "bin@p:table_array:bin",
                "name@p:table_array:bin/key:name",
            ]
        );
    }

    #[test]
    fn signature_descriptor_field_order_is_pinned() {
        // The hex goldens below are opaque; this states what they are made of. Note
        // the double hash: every adapter's `signature_descriptor` returns
        // `SignatureDescriptor::fingerprint` (already a hash), which the shared engine
        // then hashes again.
        use crate::parse::signature::{FIELD_SEP, fingerprint};
        let sep = FIELD_SEP.to_string();

        let out = parse("[deps]\nserde = \"1\"\n");
        let expected = [
            "toml",
            "config_section",
            "table",    // head: language, unit kind, lang kind
            "deps",     // local name
            "bare_key", // the key's node kind: `[a.b]` and `["a.b"]` differ
            "",         // a header has no value of its own
            "1",        // members: the table's own keys
        ]
        .join(&sep);
        assert_eq!(
            find(&out, "deps").signature_fingerprint,
            fingerprint(&fingerprint(&expected))
        );

        let expected = [
            "toml",
            "config_section",
            "key", //
            "serde",
            "bare_key",
            "string",
            "0",
        ]
        .join(&sep);
        assert_eq!(
            find(&out, "serde").signature_fingerprint,
            fingerprint(&fingerprint(&expected))
        );
    }

    #[test]
    fn locator_and_signature_goldens() {
        // Implementation-specific tripwire: pins the exact serialized locator (with a
        // fixed placeholder blob_id) and the exact `sig` hex, which the neutral
        // fixtures deliberately omit.
        use crate::parse::locator::SyntaxLocator;
        let out = parse("[dependencies]\nserde = \"1\"\n");
        let serde = find(&out, "serde");
        assert_eq!(
            serde.signature_fingerprint,
            "128237e65201323ce7ab5b9d4b86f0b233d7195da7cec5f790825e949b995755"
        );
        let locator = SyntaxLocator::from_draft(
            serde.locator_draft(SourceDialect::Language(LanguageId::Toml)),
            "b10b1d".to_string(),
        );
        assert_eq!(
            locator.serialize(),
            "anchor=p:table:dependencies/key:serde;blob=b10b1d;lang=toml;\
             sig=128237e65201323ce7ab5b9d4b86f0b233d7195da7cec5f790825e949b995755"
        );
    }

    #[test]
    fn unicode_spans_are_byte_offsets() {
        let src = "[\"café\"]\ns = \"café☕\"\n";
        let out = parse(src);
        let cafe = find(&out, "café");
        assert_eq!(cafe.anchor, SyntaxAnchor::Path("table:café".to_string()));
        let s = find(&out, "s");
        assert_eq!(s.span.start, src.find("s = ").unwrap() as u32);
        assert_eq!(s.anchor, SyntaxAnchor::Path("table:café/key:s".to_string()));
    }
}
