//! The Rust tree-sitter adapter (ADR-0001 third language, dogfooding; ADR-0002
//! derivation).
//!
//! The `tree-sitter-rust` grammar is used for every `.rs` file. It is pinned at
//! `0.23` (ABI 14) to pair with the workspace `tree-sitter 0.24` core — the newer
//! `0.24+` grammar is ABI 15, which the core refuses to load (it would silently
//! degrade to a file-only parse), the same reason the TypeScript/JavaScript grammars
//! are held at `0.23` (see the dependency allowlist in CONTRIBUTING.md). The declared
//! `grammar_version`/`query_version` (1/1) are reconciled to the pinned crate — a
//! documented, non-silent binding (spec 03 §2.3.1), matching T04-03/T04-04.
//!
//! The shared engine ([`parse_with`]) owns everything language-independent; this
//! adapter supplies only the grammar, the Rust query set (`rust.scm`), the capture
//! map, and the name/signature/reference hooks. Primitives identical across
//! languages (`field_text`, `body_member_count`, …) come from
//! [`crate::parse::adapter`]; Rust-specific bits (impl Self-type naming, the
//! visibility/keyword modifier set) live here.

use tree_sitter::{Node, Query};

use crate::parse::adapter::{
    CaptureRole, LanguageSpec, body_member_count, field_text, is_identifier_kind, is_safe_segment,
    parse_with,
};
use crate::parse::language::LanguageId;
use crate::parse::output::{ParseOutput, ReferenceKind};
use crate::parse::parser::LanguageParser;
use crate::parse::signature::SignatureDescriptor;
use local_rag_store::code::UnitKind;

/// The versioned query set (`queries=1`), embedded at build time.
const QUERY_SRC: &str = include_str!("rust.scm");

/// A Rust parser adapter over the `tree-sitter-rust` grammar.
pub struct RustParser {
    language: tree_sitter::Language,
    query: Query,
}

impl RustParser {
    /// Compile the grammar and query once. The query is a build-time constant, so a
    /// compile failure is a bug (panics).
    pub fn new() -> Self {
        let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(&language, QUERY_SRC).expect("the bundled Rust query must compile");
        Self { language, query }
    }
}

impl Default for RustParser {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageParser for RustParser {
    fn language(&self) -> LanguageId {
        LanguageId::Rust
    }

    fn parse(&self, source: &[u8]) -> ParseOutput {
        parse_with(self, source)
    }
}

impl LanguageSpec for RustParser {
    fn language(&self) -> LanguageId {
        LanguageId::Rust
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
            "decl.struct" => decl("struct"),
            "decl.enum" => decl("enum"),
            "decl.union" => decl("union"),
            "decl.trait" => decl("trait"),
            "decl.impl" => decl("impl"),
            "decl.mod" => decl("mod"),
            "decl.const" => decl("const"),
            "decl.static" => decl("static"),
            "decl.type_alias" => decl("type_alias"),
            "decl.macro" => decl("macro"),
            "ref.use" => CaptureRole::Reference,
            _ => CaptureRole::Ignore,
        }
    }

    fn local_name(&self, decl: Node, src: &[u8]) -> Option<String> {
        // `impl_item` has no `name` field; derive the Self-type's base identifier so
        // its members get a `impl:<Type>/…` route. Everything else has a `name`.
        let name = if decl.kind() == "impl_item" {
            impl_self_type_identifier(decl)?
        } else {
            decl.child_by_field_name("name")?
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
        let mut d =
            SignatureDescriptor::new(LanguageId::Rust.as_str(), unit_kind.as_str(), lang_kind);
        d.push(self.local_name(decl, src).unwrap_or_default());
        d.push(rust_modifiers(decl, src));
        d.push(field_text(decl, "type_parameters", src));
        d.push(field_text(decl, "parameters", src));
        d.push(field_text(decl, "return_type", src));
        // `type` = const/static value type, `type_item` aliased type, or impl Self-type.
        d.push(field_text(decl, "type", src));
        d.push(field_text(decl, "trait", src));
        d.push(field_text(decl, "bounds", src));
        d.push(body_member_count(decl).to_string());
        d.fingerprint()
    }

    fn reference(
        &self,
        capture_name: &str,
        node: Node,
        src: &[u8],
    ) -> Option<(ReferenceKind, String)> {
        match capture_name {
            "ref.use" => {
                let text = field_text(node, "argument", src);
                if text.is_empty() {
                    return None;
                }
                // `pub use …` re-exports; a plain `use …` is an import.
                let kind = if has_visibility_modifier(node) {
                    ReferenceKind::Reexport
                } else {
                    ReferenceKind::Import
                };
                Some((kind, text))
            }
            _ => None,
        }
    }
}

/// The base `type_identifier` of an `impl_item`'s Self type (its `type` field),
/// unwrapping a single `generic_type` layer (`impl Foo<T>` → `Foo`). Returns `None`
/// for a non-identifier Self type (tuple/reference/…), which yields an ordinal anchor.
fn impl_self_type_identifier(decl: Node) -> Option<Node> {
    let ty = decl.child_by_field_name("type")?;
    match ty.kind() {
        "type_identifier" => Some(ty),
        "generic_type" => {
            let base = ty.child_by_field_name("type")?;
            (base.kind() == "type_identifier").then_some(base)
        }
        _ => None,
    }
}

/// The sorted, de-duplicated modifier set of a Rust item: the `visibility_modifier`
/// text (`pub`, `pub(crate)`, …) plus the anonymous keyword tokens that qualify an
/// item, joined with `,`. Rust's `pub` is a *named* node, so the shared `modifiers`
/// helper (anonymous-token only) does not apply.
fn rust_modifiers(node: Node, src: &[u8]) -> String {
    const KEYWORDS: &[&str] = &["async", "const", "unsafe", "default", "extern", "mut"];
    let mut found: Vec<String> = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.is_named() {
            if child.kind() == "visibility_modifier" {
                found.push(child.utf8_text(src).unwrap_or("").to_string());
            }
        } else if KEYWORDS.contains(&child.kind()) {
            found.push(child.kind().to_string());
        }
    }
    found.sort_unstable();
    found.dedup();
    found.join(",")
}

/// Whether `node` has a direct `visibility_modifier` child (`pub …`).
fn has_visibility_modifier(node: Node) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .any(|c| c.kind() == "visibility_modifier")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::language::SourceDialect;
    use crate::parse::locator::SyntaxAnchor;

    fn parse(src: &str) -> ParseOutput {
        RustParser::new().parse(src.as_bytes())
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
        let out = parse("fn present() {}\n");
        assert!(
            out.units.iter().any(|u| u.unit_kind == UnitKind::Symbol),
            "the Rust grammar must load and produce symbols (not just a file unit)"
        );
    }

    #[test]
    fn every_file_gets_exactly_one_file_unit() {
        let out = parse("const X: u8 = 1;\n");
        let files: Vec<_> = out
            .units
            .iter()
            .filter(|u| u.unit_kind == UnitKind::File)
            .collect();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].span.start, 0);
        assert_eq!(files[0].span.end, "const X: u8 = 1;\n".len() as u32);
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
        // Covers every classify_capture branch, including union, macro, and the
        // trait method *signature* (`function_signature_item` → function).
        let out = parse(
            "fn foo(a: u8) {}\nstruct Bar {}\nenum E { A }\nunion U { f: u8 }\ntrait T { fn required(&self); }\nmod m {}\nconst C: u8 = 1;\nstatic S: u8 = 2;\ntype Alias = u8;\nmacro_rules! mac { () => {} }\n",
        );
        for (name, kind) in [
            ("foo", "function"),
            ("Bar", "struct"),
            ("E", "enum"),
            ("U", "union"),
            ("T", "trait"),
            ("required", "function"),
            ("m", "mod"),
            ("C", "const"),
            ("S", "static"),
            ("Alias", "type_alias"),
            ("mac", "macro"),
        ] {
            assert_eq!(
                find(&out, UnitKind::Symbol, name).lang_kind.as_deref(),
                Some(kind),
                "kind for {name}"
            );
        }
    }

    #[test]
    fn byte_spans_are_exact() {
        let src = "fn foo() {}";
        let out = parse(src);
        let foo = find(&out, UnitKind::Symbol, "foo");
        assert_eq!(foo.span.start, 0);
        assert_eq!(foo.span.end, src.len() as u32);
    }

    #[test]
    fn impl_method_parent_and_route() {
        let out = parse("struct Host;\nimpl Host {\n  fn method(&self) {}\n}\n");
        let host_impl = out
            .units
            .iter()
            .find(|u| u.lang_kind.as_deref() == Some("impl"))
            .expect("impl unit present");
        assert_eq!(host_impl.local_name.as_deref(), Some("Host"));
        let impl_idx = out
            .units
            .iter()
            .position(|u| std::ptr::eq(u, host_impl))
            .unwrap();
        let method = find(&out, UnitKind::Symbol, "method");
        assert_eq!(method.parent, Some(impl_idx));
        assert_eq!(
            method.anchor,
            SyntaxAnchor::Path("impl:Host/function:method".to_string())
        );
    }

    #[test]
    fn mod_nesting_parent() {
        let out = parse("mod outer {\n  fn inner() {}\n}\n");
        let outer = find(&out, UnitKind::Symbol, "outer");
        let outer_idx = out
            .units
            .iter()
            .position(|u| std::ptr::eq(u, outer))
            .unwrap();
        let inner = find(&out, UnitKind::Symbol, "inner");
        assert_eq!(inner.parent, Some(outer_idx));
        assert_eq!(
            inner.anchor,
            SyntaxAnchor::Path("mod:outer/function:inner".to_string())
        );
    }

    #[test]
    fn anonymous_impl_self_type_falls_back_to_ordinal() {
        // A non-identifier Self type (tuple) has no safe name → ordinal anchor.
        let out = parse("trait X {}\nimpl X for (u8, u8) {}\n");
        let anon = out
            .units
            .iter()
            .find(|u| u.lang_kind.as_deref() == Some("impl"))
            .expect("impl unit present");
        assert!(anon.local_name.is_none());
        assert!(matches!(anon.anchor, SyntaxAnchor::LocalOrdinal(_)));
    }

    #[test]
    fn unresolved_references_are_classified() {
        let out = parse("use std::fmt;\npub use crate::x::Y;\n");
        let by_text = |t: &str| {
            out.unresolved
                .iter()
                .find(|r| r.reference_text == t)
                .unwrap()
        };
        assert_eq!(by_text("std::fmt").reference_kind, ReferenceKind::Import);
        assert_eq!(
            by_text("crate::x::Y").reference_kind,
            ReferenceKind::Reexport
        );
        let file_idx = out
            .units
            .iter()
            .position(|u| u.unit_kind == UnitKind::File)
            .unwrap();
        assert!(out.unresolved.iter().all(|r| r.source_unit == file_idx));
    }

    #[test]
    fn error_input_yields_fallback_chunk_and_recovers_symbols() {
        let out = parse("fn ok() {}\nfn @@@ broken(\n");
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
        let src = "pub struct A;\nimpl A { fn m(&self) {} }\nuse std::io;\n";
        let first = parse(src);
        for _ in 0..4 {
            assert_eq!(parse(src), first);
        }
        assert_eq!(RustParser::new().parse(src.as_bytes()), first);
    }

    #[test]
    fn locator_and_signature_goldens() {
        // Implementation-specific tripwire: pins the exact serialized locator (with a
        // fixed placeholder blob_id) and the exact `sig` hex, which the neutral
        // fixtures deliberately omit. A change to the sig algorithm or anchor
        // formatting must update these deliberately.
        use crate::parse::locator::SyntaxLocator;
        let out = parse("fn foo(a: u8) -> u8 { a }\n");
        let foo = find(&out, UnitKind::Symbol, "foo");
        assert_eq!(
            foo.signature_fingerprint,
            "2e25289b78331871a4e4f216d8f00356d7ff2749f88ca57dbd8f8b3dda08330f"
        );
        let locator = SyntaxLocator::from_draft(
            foo.locator_draft(SourceDialect::Language(LanguageId::Rust)),
            "b10b1d".to_string(),
        );
        assert_eq!(
            locator.serialize(),
            "anchor=p:function:foo;blob=b10b1d;lang=rust;\
             sig=2e25289b78331871a4e4f216d8f00356d7ff2749f88ca57dbd8f8b3dda08330f"
        );
    }

    #[test]
    fn unicode_spans_are_byte_offsets() {
        // "café" is 5 bytes (é = 2 bytes); the fn name follows a multi-byte string
        // literal, so a char-based offset would be wrong.
        let src = "const S: &str = \"café☕\";\nfn after() {}\n";
        let out = parse(src);
        let after = find(&out, UnitKind::Symbol, "after");
        let expected_start = src.find("fn after").unwrap() as u32;
        assert_eq!(after.span.start, expected_start);
    }
}
