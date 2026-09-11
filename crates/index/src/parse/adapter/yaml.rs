//! The YAML tree-sitter adapter (ADR-0015, T24-05; ADR-0002 derivation).
//!
//! The `tree-sitter-yaml` grammar is used for every `.yaml`/`.yml` file. It is
//! pinned at `0.7` (ABI 14) to pair with the workspace `tree-sitter 0.24` core, on
//! the terms every grammar before it is held to (see the dependency allowlist in
//! CONTRIBUTING.md). The declared `grammar_version`/`query_version` (1/1) are
//! reconciled to the pinned crate — a documented, non-silent binding (spec 03
//! §2.3.1).
//!
//! Its units are [`UnitKind::ConfigSection`], like TOML's and unlike the six code
//! languages' (ADR-0015 Decision 3), and it overrides
//! [`LanguageSpec::parent_unit_kinds`] so sections may parent one another.
//!
//! Decisions, each measured against the real grammar before it was written and
//! pinned by a test below:
//!
//! - **A `document` is a unit, named by its 1-based index.** This is the one
//!   synthesized name in group 24, and it is forced rather than chosen: the engine's
//!   route derivation requires *every* ancestor to have a safe name, so an unnamed
//!   document would collapse every key beneath it to an ordinal anchor and names —
//!   the point of this card — would be lost. It is also what separates two
//!   documents' identical `apiVersion` keys, which is the acceptance.
//! - **Only structural keys are units** (owner decision): a `block_mapping_pair`
//!   is a section when its value is a mapping or a sequence. A leaf scalar stays as
//!   text inside its enclosing section. The rule lives in `yaml.scm`, because
//!   `classify_capture` is fixed per capture name and cannot decline a match.
//! - **A quoted key is unquoted before it names a unit**, the rule `toml.rs`
//!   applies. Unlike TOML, a non-ASCII YAML key needs no quoting (`café:` is a plain
//!   scalar), so this matters for `"quoted":` rather than for Unicode.
//! - **A key containing `/` is left to an ordinal anchor.**
//!   `nginx.ingress.kubernetes.io/config` is a real Kubernetes annotation and `/` is
//!   a route separator, so naming that section would corrupt the route. The fix is
//!   an honest ordinal, never a weakened `is_safe_segment`.

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
const QUERY_SRC: &str = include_str!("yaml.scm");

/// A YAML parser adapter over the `tree-sitter-yaml` grammar.
pub struct YamlParser {
    language: tree_sitter::Language,
    query: Query,
}

impl YamlParser {
    /// Compile the grammar and query once. The query is a build-time constant, so a
    /// compile failure is a bug (panics).
    pub fn new() -> Self {
        let language: tree_sitter::Language = tree_sitter_yaml::LANGUAGE.into();
        let query = Query::new(&language, QUERY_SRC).expect("the bundled YAML query must compile");
        Self { language, query }
    }
}

impl Default for YamlParser {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageParser for YamlParser {
    fn language(&self) -> LanguageId {
        LanguageId::Yaml
    }

    fn parse(&self, source: &[u8]) -> ParseOutput {
        parse_with(self, source)
    }
}

impl LanguageSpec for YamlParser {
    fn language(&self) -> LanguageId {
        LanguageId::Yaml
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
            "decl.document" => section("document"),
            "decl.key" => section("key"),
            _ => CaptureRole::Ignore,
        }
    }

    /// A config section parents another config section; nothing here is a `Symbol`.
    fn parent_unit_kinds(&self) -> &'static [UnitKind] {
        &[UnitKind::ConfigSection]
    }

    fn local_name(&self, decl: Node, src: &[u8]) -> Option<String> {
        if decl.kind() == "document" {
            return Some(document_index(decl).to_string());
        }
        let key = decl.child_by_field_name("key")?;
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
            SignatureDescriptor::new(LanguageId::Yaml.as_str(), unit_kind.as_str(), lang_kind);
        d.push(self.local_name(decl, src).unwrap_or_default());
        // How the key was written: `a:` and `"a":` name the same key in YAML's own
        // semantics but are different text, and the shape is what a reader sees.
        d.push(key_scalar_kind(decl));
        // Whether the section holds a mapping or a sequence — the structural shape,
        // never the values, so an edited scalar does not turn a section into a
        // different unit.
        d.push(value_shape(decl));
        d.push(member_count(decl).to_string());
        d.fingerprint()
    }

    fn reference(
        &self,
        _capture_name: &str,
        _node: Node,
        _src: &[u8],
    ) -> Option<(ReferenceKind, String)> {
        // YAML declares no imports. An anchor/alias (`*defaults`) is a reference
        // *within* the document, not to another file, and `ReferenceKind`'s closed
        // `Import | TypeImport | Reexport` set has no counterpart for it.
        None
    }
}

/// The 1-based position of a `document` among its stream's documents.
fn document_index(decl: Node) -> usize {
    let mut n = 1;
    let mut cursor = decl.prev_named_sibling();
    while let Some(prev) = cursor {
        if prev.kind() == "document" {
            n += 1;
        }
        cursor = prev.prev_named_sibling();
    }
    n
}

/// A quoted key's content, or the text unchanged (`"a"` → `a`, `'a'` → `a`).
fn unquote(text: &str) -> &str {
    for quote in ['"', '\''] {
        if text.len() >= 2 && text.starts_with(quote) && text.ends_with(quote) {
            return &text[1..text.len() - 1];
        }
    }
    text
}

/// The scalar kind inside a pair's `key` (`plain_scalar`, `double_quote_scalar`,
/// `single_quote_scalar`), or `""` for a document.
fn key_scalar_kind(decl: Node) -> String {
    let Some(key) = decl.child_by_field_name("key") else {
        return String::new();
    };
    let mut cursor = key.walk();
    key.named_children(&mut cursor)
        .next()
        .map(|c| c.kind().to_string())
        .unwrap_or_default()
}

/// Whether the section's content is a `block_mapping` or a `block_sequence`.
///
/// For a document this is the shape of its root node; for a key, of its value.
fn value_shape(decl: Node) -> String {
    let content = if decl.kind() == "document" {
        first_block_node(decl)
    } else {
        decl.child_by_field_name("value")
    };
    let Some(node) = content else {
        return String::new();
    };
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|c| matches!(c.kind(), "block_mapping" | "block_sequence"))
        .map(|c| c.kind().to_string())
        .unwrap_or_default()
}

/// The `block_node` a document wraps, if any (an empty document has none).
fn first_block_node(decl: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = decl.walk();
    decl.named_children(&mut cursor)
        .find(|c| c.kind() == "block_node")
}

/// How many entries the section holds directly: a mapping's pairs or a sequence's
/// items.
///
/// The shared `body_member_count` finds nothing here — a YAML section has no `body`
/// field and no `*_body` child — so without this a section's `sig` would not move
/// when it gained a key.
fn member_count(decl: Node) -> usize {
    let content = if decl.kind() == "document" {
        first_block_node(decl)
    } else {
        decl.child_by_field_name("value")
    };
    let Some(node) = content else {
        return 0;
    };
    let mut cursor = node.walk();
    let Some(collection) = node
        .named_children(&mut cursor)
        .find(|c| matches!(c.kind(), "block_mapping" | "block_sequence"))
    else {
        return 0;
    };
    let mut cursor = collection.walk();
    collection
        .named_children(&mut cursor)
        .filter(|c| matches!(c.kind(), "block_mapping_pair" | "block_sequence_item"))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::language::SourceDialect;
    use crate::parse::locator::SyntaxAnchor;

    fn parse(src: &str) -> ParseOutput {
        YamlParser::new().parse(src.as_bytes())
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

    fn names(out: &ParseOutput) -> Vec<(Option<String>, Option<String>)> {
        out.units
            .iter()
            .filter(|u| u.unit_kind == UnitKind::ConfigSection)
            .map(|u| (u.local_name.clone(), u.lang_kind.clone()))
            .collect()
    }

    const MANIFEST: &str = "apiVersion: v1\nkind: Service\nspec:\n  ports:\n    - 80\n\
                            ---\napiVersion: apps/v1\nkind: Deployment\n---\nkind: Ingress\n";

    #[test]
    fn grammar_loads_and_extracts_sections() {
        // Guard: if the grammar failed to load (e.g. an ABI mismatch), the engine
        // degrades to a file-only parse. A non-empty source MUST yield a section.
        let out = parse("present:\n  k: 1\n");
        assert!(
            out.units
                .iter()
                .any(|u| u.unit_kind == UnitKind::ConfigSection),
            "the YAML grammar must load and produce config sections"
        );
    }

    #[test]
    fn documents_stop_merging_into_one_flat_key_list() {
        // THE CARD'S ACCEPTANCE. `parse::universal::config_key` knows nothing about
        // `---`, so this manifest used to come out as one flat list —
        // `apiVersion, kind, spec, ports, apiVersion, kind, kind` — in which the
        // three documents' identical keys are indistinguishable.
        let out = parse(MANIFEST);
        let docs: Vec<&str> = out
            .units
            .iter()
            .filter(|u| u.lang_kind.as_deref() == Some("document"))
            .map(|u| u.local_name.as_deref().unwrap_or(""))
            .collect();
        assert_eq!(docs, vec!["1", "2", "3"]);
        // Each document's own span, in source order and non-overlapping.
        let spans: Vec<(u32, u32)> = out
            .units
            .iter()
            .filter(|u| u.lang_kind.as_deref() == Some("document"))
            .map(|u| (u.span.start, u.span.end))
            .collect();
        assert!(spans[0].1 <= spans[1].0 && spans[1].1 <= spans[2].0);
    }

    #[test]
    fn a_key_routes_under_its_own_document() {
        // Two documents with the same key no longer collide: the route says which
        // document the key belongs to.
        let out = parse("a:\n  k: 1\n---\na:\n  k: 1\n");
        let keys: Vec<_> = out
            .units
            .iter()
            .filter(|u| u.lang_kind.as_deref() == Some("key"))
            .collect();
        assert_eq!(keys.len(), 2);
        assert_eq!(
            keys[0].anchor,
            SyntaxAnchor::Path("document:1/key:a".to_string())
        );
        assert_eq!(
            keys[1].anchor,
            SyntaxAnchor::Path("document:2/key:a".to_string())
        );
    }

    #[test]
    fn equal_span_ancestors_still_route() {
        // `D-135`: a document whose single top-level key spans the whole document is
        // the ordinary shape of a `values.yaml`. Before the fix the engine dropped a
        // parent candidate of equal span, so this file routed as `key:metadata`
        // while the same file with a second top-level key routed as
        // `document:1/key:metadata` — two shapes, one language. The nearest ancestor
        // also wins among equal spans, which is why `labels` sits under `metadata`
        // rather than under the document.
        let out = parse("metadata:\n  name: app\n  labels:\n    app: demo\n");
        let doc = find(&out, "1");
        let meta = find(&out, "metadata");
        assert_eq!(doc.span, meta.span, "the case only exists when spans match");
        assert_eq!(meta.parent, Some(index_of(&out, doc)));
        assert_eq!(
            meta.anchor,
            SyntaxAnchor::Path("document:1/key:metadata".to_string())
        );
        let labels = find(&out, "labels");
        assert_eq!(labels.parent, Some(index_of(&out, meta)));
        assert_eq!(
            labels.anchor,
            SyntaxAnchor::Path("document:1/key:metadata/key:labels".to_string())
        );
    }

    #[test]
    fn only_structural_keys_are_sections() {
        // The T24-05 decision (owner): a key is a section when its value is a
        // mapping or a sequence. A leaf scalar is a field, not a section — YAML
        // nests far deeper than TOML, where T24-04 made every pair a unit.
        let out = parse(
            "apiVersion: v1\nmetadata:\n  name: app\nspec:\n  containers:\n    - name: app\n",
        );
        assert_eq!(
            names(&out),
            vec![
                (Some("1".to_string()), Some("document".to_string())),
                (Some("metadata".to_string()), Some("key".to_string())),
                (Some("spec".to_string()), Some("key".to_string())),
                (Some("containers".to_string()), Some("key".to_string())),
            ],
            "apiVersion and name are leaves, not sections"
        );
        assert_eq!(
            find(&out, "containers").anchor,
            SyntaxAnchor::Path("document:1/key:spec/key:containers".to_string()),
            "ADR-0015's own example route"
        );
        // A sequence value counts as structure just as a mapping does.
        let out = parse("items:\n  - one\n  - two\n");
        assert_eq!(
            find(&out, "items").anchor,
            SyntaxAnchor::Path("document:1/key:items".to_string())
        );
    }

    #[test]
    fn a_key_containing_a_slash_falls_back_to_an_ordinal() {
        // The card's named regression. `/` is the route separator, so naming this
        // section would corrupt the anchor; the fix is an honest ordinal, never a
        // weakened `is_safe_segment` — which this test also pins directly.
        assert!(!is_safe_segment("nginx.ingress.kubernetes.io/config"));
        let out = parse(
            "metadata:\n  annotations:\n    nginx.ingress.kubernetes.io/config:\n      a: 1\n",
        );
        let unnamed: Vec<_> = out
            .units
            .iter()
            .filter(|u| u.unit_kind == UnitKind::ConfigSection && u.local_name.is_none())
            .collect();
        assert_eq!(unnamed.len(), 1);
        assert_eq!(unnamed[0].anchor, SyntaxAnchor::LocalOrdinal(0));
        // Its named ancestors are unaffected.
        assert_eq!(
            find(&out, "annotations").anchor,
            SyntaxAnchor::Path("document:1/key:metadata/key:annotations".to_string())
        );
    }

    #[test]
    fn quoted_keys_are_unquoted_and_non_ascii_needs_no_quoting() {
        let out = parse("\"quoted\":\n  x: 1\n");
        assert_eq!(
            find(&out, "quoted").anchor,
            SyntaxAnchor::Path("document:1/key:quoted".to_string())
        );
        // Unlike TOML, where `[café]` is a parse error, a YAML plain scalar key may
        // be non-ASCII as written.
        let out = parse("café:\n  s: 1\n");
        assert_eq!(
            find(&out, "café").anchor,
            SyntaxAnchor::Path("document:1/key:café".to_string())
        );
    }

    #[test]
    fn every_unit_is_a_config_section_and_none_is_a_symbol() {
        let out = parse(MANIFEST);
        for u in &out.units {
            assert!(
                matches!(u.unit_kind, UnitKind::ConfigSection | UnitKind::File),
                "unexpected kind {:?}",
                u.unit_kind
            );
        }
        assert!(
            out.unresolved.is_empty(),
            "YAML declares no imports; an alias is a reference within the document"
        );
    }

    #[test]
    fn every_file_gets_exactly_one_file_unit() {
        let src = "a:\n  k: 1\n";
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
    fn a_file_with_no_content_has_no_document_at_all() {
        // Measured: the grammar emits a `stream` with no `document` child for an
        // empty, whitespace-only or comment-only file, so there is nothing to name.
        for src in ["", "\n\n   \n", "# just a comment\n"] {
            let out = parse(src);
            assert_eq!(out.units.len(), 1, "for {src:?}");
            assert_eq!(out.units[0].unit_kind, UnitKind::File);
        }
    }

    #[test]
    fn byte_spans_are_exact() {
        let src = "spec:\n  replicas: 2\n";
        let out = parse(src);
        let spec = find(&out, "spec");
        assert_eq!(spec.span.start, 0);
        assert_eq!(spec.span.end, src.len() as u32);
    }

    #[test]
    fn error_input_yields_fallback_chunk_and_recovers_the_next_key() {
        let src = "a: 1\n\t- broken\n  : :\nb:\n  c: 2\n";
        let out = parse(src);
        let chunk = out
            .units
            .iter()
            .find(|u| u.unit_kind == UnitKind::FallbackChunk)
            .expect("malformed input must produce a fallback chunk");
        let b = find(&out, "b");
        assert_eq!(b.anchor, SyntaxAnchor::Path("document:1/key:b".to_string()));
        assert!(b.span.start >= chunk.span.end);
    }

    #[test]
    fn parse_is_deterministic() {
        let first = parse(MANIFEST);
        for _ in 0..4 {
            assert_eq!(parse(MANIFEST), first);
        }
        assert_eq!(YamlParser::new().parse(MANIFEST.as_bytes()), first);
    }

    #[test]
    fn a_section_gaining_a_key_moves_its_signature() {
        // `body_member_count` finds nothing in a YAML mapping, which is why this
        // adapter counts members itself.
        let sig = |src: &str| find(&parse(src), "t").signature_fingerprint.clone();
        assert_ne!(sig("t:\n  a: 1\n"), sig("t:\n  a: 1\n  b: 2\n"));
        // A changed leaf value does not: a section keeps its identity when its
        // content changes.
        assert_eq!(sig("t:\n  a: 1\n"), sig("t:\n  a: 2\n"));
        // A mapping and a sequence are different shapes.
        assert_ne!(sig("t:\n  a: 1\n"), sig("t:\n  - a\n"));
    }

    #[test]
    fn signature_descriptor_field_order_is_pinned() {
        // The hex goldens below are opaque; this states what they are made of. Note
        // the double hash: every adapter's `signature_descriptor` returns
        // `SignatureDescriptor::fingerprint` (already a hash), which the shared
        // engine then hashes again.
        use crate::parse::signature::{FIELD_SEP, fingerprint};
        let sep = FIELD_SEP.to_string();

        let out = parse("spec:\n  replicas: 2\n  selector:\n    a: b\n");
        let expected = [
            "yaml",
            "config_section",
            "key",           // head: language, unit kind, lang kind
            "spec",          // local name
            "plain_scalar",  // how the key was written
            "block_mapping", // the section's shape, never its values
            "2",             // members: `replicas` and `selector`
        ]
        .join(&sep);
        assert_eq!(
            find(&out, "spec").signature_fingerprint,
            fingerprint(&fingerprint(&expected))
        );

        let expected = [
            "yaml",
            "config_section",
            "document", //
            "1",
            "",
            "block_mapping",
            "1",
        ]
        .join(&sep);
        assert_eq!(
            find(&out, "1").signature_fingerprint,
            fingerprint(&fingerprint(&expected))
        );
    }

    #[test]
    fn locator_and_signature_goldens() {
        // Implementation-specific tripwire: pins the exact serialized locator (with a
        // fixed placeholder blob_id) and the exact `sig` hex, which the neutral
        // fixtures deliberately omit.
        use crate::parse::locator::SyntaxLocator;
        let out = parse("spec:\n  containers:\n    - name: app\n");
        let containers = find(&out, "containers");
        assert_eq!(
            containers.signature_fingerprint,
            "629b0cc0dba6bdec7c6bdcf545faed9874e77d4fb6962d2bcb574dd47ba753bf"
        );
        let locator = SyntaxLocator::from_draft(
            containers.locator_draft(SourceDialect::Language(LanguageId::Yaml)),
            "b10b1d".to_string(),
        );
        assert_eq!(
            locator.serialize(),
            "anchor=p:document:1/key:spec/key:containers;blob=b10b1d;lang=yaml;\
             sig=629b0cc0dba6bdec7c6bdcf545faed9874e77d4fb6962d2bcb574dd47ba753bf"
        );
    }

    #[test]
    fn unicode_spans_are_byte_offsets() {
        let src = "café:\n  s: \"café☕\"\nafter:\n  k: 1\n";
        let out = parse(src);
        let after = find(&out, "after");
        assert_eq!(after.span.start, src.find("after:").unwrap() as u32);
    }
}
