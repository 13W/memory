//! The JavaScript tree-sitter adapter (ADR-0001 second language; ADR-0002
//! derivation).
//!
//! The standalone `tree-sitter-javascript` grammar is used for every JavaScript
//! extension (`.js`/`.jsx`/`.mjs`/`.cjs`). It is pinned at `0.23` (ABI 14) to pair
//! with the workspace `tree-sitter 0.24` core — the newer `0.25` grammar is ABI 15,
//! which the core refuses to load (it would silently degrade to a file-only parse),
//! the same reason `tree-sitter-typescript` is held at `0.23` (see the dependency
//! allowlist in CONTRIBUTING.md). The declared `grammar_version`/`query_version`
//! (1/1) are reconciled to the pinned crate — a documented, non-silent binding
//! (spec 03 §2.3.1), matching the TypeScript reconciliation of T04-03.
//!
//! The shared engine ([`parse_with`]) owns everything language-independent; this
//! adapter supplies only the grammar, the JavaScript query set (`javascript.scm`),
//! the capture map, and the name/signature/reference hooks. Primitives identical
//! across languages (`modifiers`, `field_text`, …) come from [`crate::parse::adapter`].

use tree_sitter::{Node, Query};

use crate::parse::adapter::{
    CaptureRole, LanguageSpec, body_member_count, field_text, heritage_text, is_identifier_kind,
    is_safe_segment, modifiers, parse_with,
};
use crate::parse::language::LanguageId;
use crate::parse::output::{ParseOutput, ReferenceKind};
use crate::parse::parser::LanguageParser;
use crate::parse::signature::SignatureDescriptor;
use local_rag_store::code::UnitKind;

/// The versioned query set (`queries=1`), embedded at build time.
const QUERY_SRC: &str = include_str!("javascript.scm");

/// A JavaScript parser adapter over the `tree-sitter-javascript` grammar.
pub struct JavaScriptParser {
    language: tree_sitter::Language,
    query: Query,
}

impl JavaScriptParser {
    /// Compile the grammar and query once. The query is a build-time constant, so
    /// a compile failure is a bug (panics).
    pub fn new() -> Self {
        let language: tree_sitter::Language = tree_sitter_javascript::LANGUAGE.into();
        let query =
            Query::new(&language, QUERY_SRC).expect("the bundled JavaScript query must compile");
        Self { language, query }
    }
}

impl Default for JavaScriptParser {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageParser for JavaScriptParser {
    fn language(&self) -> LanguageId {
        LanguageId::JavaScript
    }

    fn parse(&self, source: &[u8]) -> ParseOutput {
        parse_with(self, source)
    }
}

impl LanguageSpec for JavaScriptParser {
    fn language(&self) -> LanguageId {
        LanguageId::JavaScript
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
            "decl.method" => decl("method"),
            "decl.const" => decl("const"),
            "ref.import" | "ref.reexport" => CaptureRole::Reference,
            _ => CaptureRole::Ignore,
        }
    }

    fn local_name(&self, decl: Node, src: &[u8]) -> Option<String> {
        let name = decl.child_by_field_name("name")?;
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
        // JavaScript has no type annotations, so the descriptor carries no
        // type_parameters/return_type/type-alias fields — only the name, the
        // modifier set, the parameter text, the value shape (for function/class-
        // valued `const`), heritage, and the body member count.
        let mut d = SignatureDescriptor::new(
            LanguageId::JavaScript.as_str(),
            unit_kind.as_str(),
            lang_kind,
        );
        d.push(self.local_name(decl, src).unwrap_or_default());
        d.push(modifiers(decl));
        d.push(field_text(decl, "parameters", src));
        match decl.child_by_field_name("value") {
            Some(value) => {
                d.push(value.kind());
                d.push(field_text(value, "parameters", src));
            }
            None => {
                d.push("");
                d.push("");
            }
        }
        d.push(heritage_text(decl, src));
        d.push(body_member_count(decl).to_string());
        d.fingerprint()
    }

    fn reference(
        &self,
        capture_name: &str,
        node: Node,
        src: &[u8],
    ) -> Option<(ReferenceKind, String)> {
        let text = node.utf8_text(src).ok()?.to_string();
        match capture_name {
            // JavaScript has no `import type`; every specifier is a value import.
            "ref.import" => Some((ReferenceKind::Import, text)),
            "ref.reexport" => Some((ReferenceKind::Reexport, text)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::locator::SyntaxAnchor;

    fn parse(src: &str) -> ParseOutput {
        JavaScriptParser::new().parse(src.as_bytes())
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

    #[test]
    fn grammar_loads_and_extracts_symbols() {
        // Guard: if the grammar failed to load (e.g. an ABI mismatch), the engine
        // degrades to a file-only parse. A non-empty source MUST yield a symbol.
        let out = parse("function present() {}\n");
        assert!(
            out.units.iter().any(|u| u.unit_kind == UnitKind::Symbol),
            "the JavaScript grammar must load and produce symbols (not just a file unit)"
        );
    }

    #[test]
    fn every_file_gets_exactly_one_file_unit() {
        let out = parse("const x = 1;\n");
        let files: Vec<_> = out
            .units
            .iter()
            .filter(|u| u.unit_kind == UnitKind::File)
            .collect();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].span.start, 0);
        assert_eq!(files[0].span.end, "const x = 1;\n".len() as u32);
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
        let out = parse(
            "export function foo(a) {}\nclass Bar {}\nfunction* gen() {}\nexport const g = (x) => x;\n",
        );
        assert_eq!(
            find(&out, UnitKind::Symbol, "foo").lang_kind.as_deref(),
            Some("function")
        );
        assert_eq!(
            find(&out, UnitKind::Symbol, "Bar").lang_kind.as_deref(),
            Some("class")
        );
        assert_eq!(
            find(&out, UnitKind::Symbol, "gen").lang_kind.as_deref(),
            Some("function")
        );
        assert_eq!(
            find(&out, UnitKind::Symbol, "g").lang_kind.as_deref(),
            Some("const")
        );
    }

    #[test]
    fn byte_spans_are_exact() {
        let src = "function foo() {}";
        let out = parse(src);
        let foo = find(&out, UnitKind::Symbol, "foo");
        assert_eq!(foo.span.start, 0);
        assert_eq!(foo.span.end, src.len() as u32);
    }

    #[test]
    fn method_parent_is_the_class() {
        let out = parse("class Host {\n  method(x) {}\n}\n");
        let host = find(&out, UnitKind::Symbol, "Host");
        let host_idx = out
            .units
            .iter()
            .position(|u| std::ptr::eq(u, host))
            .unwrap();
        let method = find(&out, UnitKind::Symbol, "method");
        assert_eq!(method.parent, Some(host_idx));
        assert_eq!(method.lang_kind.as_deref(), Some("method"));
    }

    #[test]
    fn named_route_and_ordinal_fallback() {
        let out = parse("class Foo {\n  bar() {}\n  [Symbol.iterator]() {}\n}\n");
        let bar = find(&out, UnitKind::Symbol, "bar");
        assert_eq!(
            bar.anchor,
            SyntaxAnchor::Path("class:Foo/method:bar".to_string())
        );
        // Computed method name → ordinal fallback (no local_name).
        let computed = out
            .units
            .iter()
            .find(|u| {
                u.unit_kind == UnitKind::Symbol
                    && u.local_name.is_none()
                    && u.lang_kind.as_deref() == Some("method")
            })
            .expect("computed-name method present");
        assert!(matches!(computed.anchor, SyntaxAnchor::LocalOrdinal(_)));
    }

    #[test]
    fn top_level_function_const_is_captured_but_value_const_is_not() {
        let out = parse("export const f = (x) => x;\nexport const obj = { a: 1 };\nconst n = 5;\n");
        assert_eq!(
            find(&out, UnitKind::Symbol, "f").lang_kind.as_deref(),
            Some("const")
        );
        assert!(
            !out.units
                .iter()
                .any(|u| u.local_name.as_deref() == Some("obj")),
            "object-valued const is not a symbol"
        );
        assert!(
            !out.units
                .iter()
                .any(|u| u.local_name.as_deref() == Some("n"))
        );
    }

    #[test]
    fn unresolved_references_are_classified() {
        let out = parse("import { A } from \"./mod\";\nexport * from \"./re\";\n");
        let by_text = |t: &str| {
            out.unresolved
                .iter()
                .find(|r| r.reference_text == t)
                .unwrap()
        };
        assert_eq!(by_text("./mod").reference_kind, ReferenceKind::Import);
        assert_eq!(by_text("./re").reference_kind, ReferenceKind::Reexport);
        // All refs originate from the file unit.
        let file_idx = out
            .units
            .iter()
            .position(|u| u.unit_kind == UnitKind::File)
            .unwrap();
        assert!(out.unresolved.iter().all(|r| r.source_unit == file_idx));
    }

    #[test]
    fn error_input_yields_fallback_chunk_and_recovers_symbols() {
        let out = parse("function ok() {}\nfunction @@@ broken(\n");
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
    fn parse_is_deterministic() {
        let src = "export class A { m() {} }\nimport { X } from \"./x\";\n";
        let first = parse(src);
        for _ in 0..4 {
            assert_eq!(parse(src), first);
        }
        // A fresh parser instance gives identical output.
        assert_eq!(JavaScriptParser::new().parse(src.as_bytes()), first);
    }

    #[test]
    fn locator_and_signature_goldens() {
        // Implementation-specific tripwire: pins the exact serialized locator (with
        // a fixed placeholder blob_id) and the exact `sig` hex, which the neutral
        // fixtures deliberately omit. A change to the sig algorithm or anchor
        // formatting must update these deliberately.
        use crate::parse::locator::SyntaxLocator;
        let out = parse("function foo(a) {}\n");
        let foo = find(&out, UnitKind::Symbol, "foo");
        assert_eq!(
            foo.signature_fingerprint,
            "ef7dab787bf7f0827f1e8cefac975d9e32c3f341c3b292ccdd84256bbd694a60"
        );
        let locator = SyntaxLocator::from_draft(
            foo.locator_draft(LanguageId::JavaScript),
            "b10b1d".to_string(),
        );
        assert_eq!(
            locator.serialize(),
            "anchor=p:function:foo;blob=b10b1d;lang=javascript;\
             sig=ef7dab787bf7f0827f1e8cefac975d9e32c3f341c3b292ccdd84256bbd694a60"
        );
    }

    #[test]
    fn unicode_spans_are_byte_offsets() {
        // "café" is 5 bytes (é = 2 bytes); the function name follows a multi-byte
        // string literal, so a char-based offset would be wrong.
        let src = "const s = \"café\";\nfunction after() {}\n";
        let out = parse(src);
        let after = find(&out, UnitKind::Symbol, "after");
        let expected_start = src.find("function after").unwrap() as u32;
        assert_eq!(after.span.start, expected_start);
    }
}
