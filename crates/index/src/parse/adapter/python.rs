//! The Python tree-sitter adapter (ADR-0015 first post-v0 language, T24-01;
//! ADR-0002 derivation).
//!
//! The `tree-sitter-python` grammar is used for every `.py`/`.pyi` file. It is
//! pinned at `0.23` (ABI 14) to pair with the workspace `tree-sitter 0.24` core —
//! the grammar's `0.25` line is ABI 15, which the core refuses to load (it would
//! silently degrade to a file-only parse), the same reason the three v0 grammars
//! are held at `0.23` (see the dependency allowlist in CONTRIBUTING.md). The
//! declared `grammar_version`/`query_version` (1/1) are reconciled to the pinned
//! crate — a documented, non-silent binding (spec 03 §2.3.1), matching T04-03/04/05.
//!
//! The shared engine ([`parse_with`]) owns everything language-independent; this
//! adapter supplies only the grammar, the Python query set (`python.scm`), the
//! capture map, and the name/signature/reference hooks. Primitives identical across
//! languages (`field_text`, `modifiers`, `body_member_count`, …) come from
//! [`crate::parse::adapter`]; Python-specific bits live here:
//!
//! - A **decorated definition is one unit**, captured at its `decorated_definition`
//!   wrapper (the query never captures the inner definition), so the unit owns the
//!   decorators' span while its `lang_kind` and route are those of the definition —
//!   adding a decorator does not move `class:Foo/function:bar`. The hooks unwrap the
//!   wrapper through [`definition_node`], the same shape as the Rust adapter's
//!   `impl_item` special case.
//! - A **module-level assignment to a bare identifier** is a `variable` symbol; any
//!   other target (tuple, attribute, subscript) is not a unit.
//! - Every import specifier is one `ReferenceKind::Import` (`import os, sys` is
//!   two); `from __future__ import …` is not a reference.
//! - Docstrings are **not** extracted: no doc-comment mechanism exists in the parse
//!   output for any language (card T24-01, out of scope).

use tree_sitter::{Node, Query};

use crate::parse::adapter::{
    CaptureRole, LanguageSpec, body_member_count, field_text, is_identifier_kind, is_safe_segment,
    modifiers, parse_with,
};
use crate::parse::language::LanguageId;
use crate::parse::output::{ParseOutput, ReferenceKind};
use crate::parse::parser::LanguageParser;
use crate::parse::signature::SignatureDescriptor;
use local_rag_store::code::UnitKind;

/// The versioned query set (`queries=1`), embedded at build time.
const QUERY_SRC: &str = include_str!("python.scm");

/// A Python parser adapter over the `tree-sitter-python` grammar.
pub struct PythonParser {
    language: tree_sitter::Language,
    query: Query,
}

impl PythonParser {
    /// Compile the grammar and query once. The query is a build-time constant, so a
    /// compile failure is a bug (panics).
    pub fn new() -> Self {
        let language: tree_sitter::Language = tree_sitter_python::LANGUAGE.into();
        let query =
            Query::new(&language, QUERY_SRC).expect("the bundled Python query must compile");
        Self { language, query }
    }
}

impl Default for PythonParser {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageParser for PythonParser {
    fn language(&self) -> LanguageId {
        LanguageId::Python
    }

    fn parse(&self, source: &[u8]) -> ParseOutput {
        parse_with(self, source)
    }
}

impl LanguageSpec for PythonParser {
    fn language(&self) -> LanguageId {
        LanguageId::Python
    }

    fn ts_language(&self) -> &tree_sitter::Language {
        &self.language
    }

    fn query(&self) -> &Query {
        &self.query
    }

    fn classify_capture(&self, capture_name: &str) -> CaptureRole {
        let decl = |lang_kind| CaptureRole::Decl {
            unit_kind: UnitKind::Symbol,
            lang_kind,
        };
        match capture_name {
            "decl.function" => decl("function"),
            "decl.class" => decl("class"),
            "decl.variable" => decl("variable"),
            "ref.import" => CaptureRole::Reference,
            _ => CaptureRole::Ignore,
        }
    }

    fn local_name(&self, decl: Node, src: &[u8]) -> Option<String> {
        // A decorated definition is named by the definition it wraps; an assignment
        // by its `left` target (the query admits only a bare identifier there).
        let inner = definition_node(decl);
        let name = if inner.kind() == "assignment" {
            inner.child_by_field_name("left")?
        } else {
            inner.child_by_field_name("name")?
        };
        if !is_identifier_kind(name.kind()) {
            return None;
        }
        let text = name.utf8_text(src).ok()?;
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
        let inner = definition_node(decl);
        let mut d =
            SignatureDescriptor::new(LanguageId::Python.as_str(), unit_kind.as_str(), lang_kind);
        d.push(self.local_name(decl, src).unwrap_or_default());
        d.push(decorator_text(decl, src));
        // `async` is the only Python token in the shared modifier set.
        d.push(modifiers(inner));
        d.push(field_text(inner, "type_parameters", src));
        d.push(field_text(inner, "parameters", src));
        d.push(field_text(inner, "return_type", src));
        // A class's bases; `heritage_text` does not recognize Python's
        // `argument_list`, so the field is read directly.
        d.push(field_text(inner, "superclasses", src));
        // A PEP 526 annotation on an assignment (`X: int = 1`).
        d.push(field_text(inner, "type", src));
        // The kind of an assignment's value (`integer`, `call`, `lambda`, …).
        d.push(
            inner
                .child_by_field_name("right")
                .map(|n| n.kind().to_string())
                .unwrap_or_default(),
        );
        d.push(body_member_count(inner).to_string());
        d.fingerprint()
    }

    fn reference(
        &self,
        capture_name: &str,
        node: Node,
        src: &[u8],
    ) -> Option<(ReferenceKind, String)> {
        match capture_name {
            "ref.import" => {
                // The captured node is the specifier itself (`dotted_name` or
                // `relative_import`); Python has no re-export or type-only form.
                let text = node.utf8_text(src).ok()?;
                if text.is_empty() {
                    return None;
                }
                Some((ReferenceKind::Import, text.to_string()))
            }
            _ => None,
        }
    }
}

/// The definition a `decorated_definition` wraps (its `definition` field); any other
/// node is its own definition.
fn definition_node(decl: Node) -> Node {
    if decl.kind() == "decorated_definition" {
        decl.child_by_field_name("definition").unwrap_or(decl)
    } else {
        decl
    }
}

/// The decorators of a `decorated_definition` in source order, joined with `,`, or
/// `""` for an undecorated node. Order is part of the signature (`@a @b` and
/// `@b @a` compose differently).
fn decorator_text(decl: Node, src: &[u8]) -> String {
    if decl.kind() != "decorated_definition" {
        return String::new();
    }
    let mut cursor = decl.walk();
    decl.children(&mut cursor)
        .filter(|c| c.kind() == "decorator")
        .map(|c| c.utf8_text(src).unwrap_or("").to_string())
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::language::SourceDialect;
    use crate::parse::locator::SyntaxAnchor;

    fn parse(src: &str) -> ParseOutput {
        PythonParser::new().parse(src.as_bytes())
    }

    fn find<'a>(
        out: &'a ParseOutput,
        kind: UnitKind,
        name: &str,
    ) -> &'a crate::parse::output::ParsedUnitDraft {
        out.units
            .iter()
            .find(|u| u.unit_kind == kind && u.local_name.as_deref() == Some(name))
            .unwrap_or_else(|| panic!("no {kind:?} unit named {name}"))
    }

    fn index_of(out: &ParseOutput, unit: &crate::parse::output::ParsedUnitDraft) -> usize {
        out.units
            .iter()
            .position(|u| std::ptr::eq(u, unit))
            .unwrap()
    }

    #[test]
    fn grammar_loads_and_extracts_symbols() {
        // Guard: if the grammar failed to load (e.g. an ABI mismatch), the engine
        // degrades to a file-only parse. A non-empty source MUST yield a symbol.
        let out = parse("def present():\n    pass\n");
        assert!(
            out.units.iter().any(|u| u.unit_kind == UnitKind::Symbol),
            "the Python grammar must load and produce symbols (not just a file unit)"
        );
    }

    #[test]
    fn every_file_gets_exactly_one_file_unit() {
        let src = "X = 1\n";
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
        assert_eq!(files[0].parent, None);
    }

    #[test]
    fn empty_file_is_only_a_file_unit() {
        let out = parse("");
        assert_eq!(out.units.len(), 1);
        assert_eq!(out.units[0].unit_kind, UnitKind::File);
        assert!(out.units[0].span.is_empty());
        assert!(out.unresolved.is_empty());
    }

    #[test]
    fn extracts_core_declaration_kinds_with_lang_kind() {
        // Covers every classify_capture declaration branch: plain and async
        // functions, a class, and module-level plain / annotated assignments.
        let out = parse(
            "def foo(a):\n    return a\nasync def bar():\n    pass\nclass Baz:\n    pass\nX = 1\nY: int = 2\n",
        );
        for (name, kind) in [
            ("foo", "function"),
            ("bar", "function"),
            ("Baz", "class"),
            ("X", "variable"),
            ("Y", "variable"),
        ] {
            assert_eq!(
                find(&out, UnitKind::Symbol, name).lang_kind.as_deref(),
                Some(kind),
                "kind for {name}"
            );
        }
        assert_eq!(
            out.units
                .iter()
                .filter(|u| u.unit_kind == UnitKind::Symbol)
                .count(),
            5
        );
    }

    #[test]
    fn byte_spans_are_exact() {
        let src = "def foo():\n    pass";
        let out = parse(src);
        let foo = find(&out, UnitKind::Symbol, "foo");
        assert_eq!(foo.span.start, 0);
        assert_eq!(foo.span.end, src.len() as u32);
    }

    #[test]
    fn method_parent_and_route() {
        let out = parse("class Host:\n    def method(self):\n        pass\n");
        let host = find(&out, UnitKind::Symbol, "Host");
        let host_idx = index_of(&out, host);
        let method = find(&out, UnitKind::Symbol, "method");
        assert_eq!(method.parent, Some(host_idx));
        assert_eq!(
            method.anchor,
            SyntaxAnchor::Path("class:Host/function:method".to_string())
        );
    }

    #[test]
    fn nested_def_route() {
        let out = parse("def outer():\n    def inner():\n        pass\n    return inner\n");
        let outer = find(&out, UnitKind::Symbol, "outer");
        let outer_idx = index_of(&out, outer);
        let inner = find(&out, UnitKind::Symbol, "inner");
        assert_eq!(inner.parent, Some(outer_idx));
        assert_eq!(
            inner.anchor,
            SyntaxAnchor::Path("function:outer/function:inner".to_string())
        );
    }

    #[test]
    fn decorated_definition_yields_exactly_one_unit_owning_the_decorator_span() {
        // The T24-01 decision: a decorated method is ONE unit whose span starts at
        // the first decorator, not two overlapping units (`decorated_definition`
        // plus the inner `function_definition`, which would nest as
        // `function:bar/function:bar`). The route is the same as undecorated.
        let plain = "class Foo:\n    def bar(self):\n        pass\n";
        let decorated =
            "class Foo:\n    @staticmethod\n    @other.wrap(1)\n    def bar(self):\n        pass\n";
        let out = parse(decorated);
        let bars: Vec<_> = out
            .units
            .iter()
            .filter(|u| u.local_name.as_deref() == Some("bar"))
            .collect();
        assert_eq!(
            bars.len(),
            1,
            "one unit for a decorated def: {:?}",
            out.units
        );
        let bar = bars[0];
        assert_eq!(
            bar.span.start,
            decorated.find("@staticmethod").unwrap() as u32
        );
        assert_eq!(bar.span.end, decorated.len() as u32 - 1);
        assert_eq!(bar.lang_kind.as_deref(), Some("function"));
        assert_eq!(
            bar.anchor,
            SyntaxAnchor::Path("class:Foo/function:bar".to_string())
        );
        assert_eq!(
            bar.parent,
            Some(index_of(&out, find(&out, UnitKind::Symbol, "Foo")))
        );
        // Exactly two symbols in the file: the class and the method.
        assert_eq!(
            out.units
                .iter()
                .filter(|u| u.unit_kind == UnitKind::Symbol)
                .count(),
            2
        );
        // The route is decoration-independent; the signature is not.
        let plain_bar = parse(plain);
        let plain_bar = find(&plain_bar, UnitKind::Symbol, "bar");
        assert_eq!(plain_bar.anchor, bar.anchor);
        assert_ne!(plain_bar.signature_fingerprint, bar.signature_fingerprint);

        // A decorated class is one unit too, and still parents its methods.
        let out = parse("@dataclass\nclass Point:\n    def norm(self):\n        pass\n");
        let points: Vec<_> = out
            .units
            .iter()
            .filter(|u| u.local_name.as_deref() == Some("Point"))
            .collect();
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].span.start, 0);
        assert_eq!(points[0].lang_kind.as_deref(), Some("class"));
        let norm = find(&out, UnitKind::Symbol, "norm");
        assert_eq!(norm.parent, Some(index_of(&out, points[0])));
        assert_eq!(
            norm.anchor,
            SyntaxAnchor::Path("class:Point/function:norm".to_string())
        );
    }

    #[test]
    fn only_bare_identifier_targets_become_variable_units() {
        // Tuple / attribute / subscript targets, class attributes, and augmented
        // assignments are not units — not even ordinal ones.
        let out = parse(
            "a, b = 1, 2\nobj.attr = 3\nd[\"k\"] = 4\nx += 1\nclass C:\n    attr = 5\nX = 1\n",
        );
        let symbols: Vec<_> = out
            .units
            .iter()
            .filter(|u| u.unit_kind == UnitKind::Symbol)
            .map(|u| (u.local_name.clone(), u.lang_kind.clone()))
            .collect();
        assert_eq!(
            symbols,
            vec![
                (Some("C".to_string()), Some("class".to_string())),
                (Some("X".to_string()), Some("variable".to_string())),
            ]
        );
    }

    #[test]
    fn unresolved_references_are_classified() {
        let out = parse(
            "import os, sys\nimport numpy as np\nfrom pkg.mod import a, b\nfrom . import sibling\nfrom ..up import thing\n",
        );
        let texts: Vec<&str> = out
            .unresolved
            .iter()
            .map(|r| r.reference_text.as_str())
            .collect();
        assert_eq!(texts, vec!["os", "sys", "numpy", "pkg.mod", ".", "..up"]);
        assert!(
            out.unresolved
                .iter()
                .all(|r| r.reference_kind == ReferenceKind::Import)
        );
        let file_idx = out
            .units
            .iter()
            .position(|u| u.unit_kind == UnitKind::File)
            .unwrap();
        assert!(out.unresolved.iter().all(|r| r.source_unit == file_idx));
    }

    #[test]
    fn future_import_is_not_a_reference() {
        let out = parse("from __future__ import annotations\nimport os\n");
        let texts: Vec<&str> = out
            .unresolved
            .iter()
            .map(|r| r.reference_text.as_str())
            .collect();
        assert_eq!(texts, vec!["os"]);
    }

    #[test]
    fn error_input_yields_fallback_chunk_and_recovers_symbols() {
        let out = parse("def ok():\n    pass\ndef @@@ broken(\n");
        assert!(
            out.units
                .iter()
                .any(|u| u.local_name.as_deref() == Some("ok"))
        );
        assert!(
            out.units
                .iter()
                .any(|u| u.unit_kind == UnitKind::FallbackChunk),
            "malformed input must produce a fallback chunk"
        );
    }

    #[test]
    fn error_before_a_definition_still_recovers_it() {
        // The broken line PRECEDES the good definitions. tree-sitter-python recovers
        // by parking the garbage in an `ERROR` *sibling* at module level, so the
        // definitions stay direct children of `module` and keep their names; the
        // garbage becomes a fallback chunk that precedes them in canonical order.
        let src = "@@@\ndef ok():\n    pass\nclass K:\n    pass\n";
        let out = parse(src);
        let ok = find(&out, UnitKind::Symbol, "ok");
        assert_eq!(ok.span.start, src.find("def ok").unwrap() as u32);
        assert_eq!(ok.lang_kind.as_deref(), Some("function"));
        assert_eq!(ok.anchor, SyntaxAnchor::Path("function:ok".to_string()));
        let k = find(&out, UnitKind::Symbol, "K");
        assert_eq!(k.lang_kind.as_deref(), Some("class"));
        let chunk = out
            .units
            .iter()
            .find(|u| u.unit_kind == UnitKind::FallbackChunk)
            .expect("malformed input must produce a fallback chunk");
        assert_eq!(chunk.span.start, 0);
        assert!(chunk.span.end <= ok.span.start);
    }

    #[test]
    fn parse_is_deterministic() {
        let src =
            "import os\nclass A:\n    @property\n    def m(self):\n        return 1\nX = A()\n";
        let first = parse(src);
        for _ in 0..4 {
            assert_eq!(parse(src), first);
        }
        assert_eq!(PythonParser::new().parse(src.as_bytes()), first);
    }

    #[test]
    fn locator_and_signature_goldens() {
        // Implementation-specific tripwire: pins the exact serialized locator (with a
        // fixed placeholder blob_id) and the exact `sig` hex, which the neutral
        // fixtures deliberately omit. A change to the sig algorithm, the descriptor
        // field order, or anchor formatting must update these deliberately.
        use crate::parse::locator::SyntaxLocator;
        let out = parse("def foo(a: int) -> int:\n    return a\n");
        let foo = find(&out, UnitKind::Symbol, "foo");
        assert_eq!(
            foo.signature_fingerprint,
            "d766e830f96e33a3232e92a4a8e229680de0d14df379273b0376844ce3fe9731"
        );
        let locator = SyntaxLocator::from_draft(
            foo.locator_draft(SourceDialect::Language(LanguageId::Python)),
            "b10b1d".to_string(),
        );
        assert_eq!(
            locator.serialize(),
            "anchor=p:function:foo;blob=b10b1d;lang=python;\
             sig=d766e830f96e33a3232e92a4a8e229680de0d14df379273b0376844ce3fe9731"
        );
    }

    #[test]
    fn unicode_spans_are_byte_offsets() {
        // "café" is 5 bytes (é = 2 bytes); the def follows a multi-byte string
        // literal, so a char-based offset would be wrong.
        let src = "S = \"café☕\"\ndef after():\n    pass\n";
        let out = parse(src);
        let after = find(&out, UnitKind::Symbol, "after");
        let expected_start = src.find("def after").unwrap() as u32;
        assert_eq!(after.span.start, expected_start);
    }
}
