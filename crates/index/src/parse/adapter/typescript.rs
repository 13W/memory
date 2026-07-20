//! The TypeScript tree-sitter adapter (ADR-0001 first language; ADR-0002
//! derivation).
//!
//! One grammar variant — `tsx` — is used for every TypeScript extension
//! (`.ts`/`.tsx`/`.mts`/`.cts`). The `parser_fingerprint` is identical across
//! them and [`LanguageParser::parse`] never sees the path, so a single grammar is
//! required for determinism (spec 06 §2.1); `tsx` is the practical superset that
//! also parses JSX (ADR-0002). The declared `grammar_version`/`query_version`
//! (1/1) are reconciled to the pinned crates `tree-sitter 0.24` /
//! `tree-sitter-typescript 0.23` — a documented, non-silent binding (spec 03
//! §2.3.1).

use tree_sitter::{Node, Query};

use crate::parse::adapter::{CaptureRole, LanguageSpec, parse_with};
use crate::parse::language::LanguageId;
use crate::parse::output::{ParseOutput, ReferenceKind};
use crate::parse::parser::LanguageParser;
use crate::parse::signature::SignatureDescriptor;
use local_rag_store::code::UnitKind;

/// The versioned query set (`queries=1`), embedded at build time.
const QUERY_SRC: &str = include_str!("typescript.scm");

/// A TypeScript parser adapter over the `tsx` grammar.
pub struct TypeScriptParser {
    language: tree_sitter::Language,
    query: Query,
}

impl TypeScriptParser {
    /// Compile the grammar and query once. The query is a build-time constant, so
    /// a compile failure is a bug (panics).
    pub fn new() -> Self {
        let language: tree_sitter::Language = tree_sitter_typescript::LANGUAGE_TSX.into();
        let query =
            Query::new(&language, QUERY_SRC).expect("the bundled TypeScript query must compile");
        Self { language, query }
    }
}

impl Default for TypeScriptParser {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageParser for TypeScriptParser {
    fn language(&self) -> LanguageId {
        LanguageId::TypeScript
    }

    fn parse(&self, source: &[u8]) -> ParseOutput {
        parse_with(self, source)
    }
}

impl LanguageSpec for TypeScriptParser {
    fn language(&self) -> LanguageId {
        LanguageId::TypeScript
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
            "decl.interface" => decl("interface"),
            "decl.enum" => decl("enum"),
            "decl.type_alias" => decl("type_alias"),
            "decl.namespace" => decl("namespace"),
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
        let mut d = SignatureDescriptor::new(
            LanguageId::TypeScript.as_str(),
            unit_kind.as_str(),
            lang_kind,
        );
        d.push(self.local_name(decl, src).unwrap_or_default());
        d.push(modifiers(decl));
        d.push(field_text(decl, "type_parameters", src));
        d.push(field_text(decl, "parameters", src));
        d.push(field_text(decl, "return_type", src));
        match decl.child_by_field_name("value") {
            Some(value) => {
                d.push(value.kind());
                d.push(field_text(value, "parameters", src));
                d.push(field_text(value, "return_type", src));
                // The aliased type is small and identity-relevant; a function/class
                // value's body is not, so only type aliases carry the value text.
                if lang_kind == "type_alias" {
                    d.push(value.utf8_text(src).unwrap_or(""));
                } else {
                    d.push("");
                }
            }
            None => {
                d.push("");
                d.push("");
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
            "ref.import" => {
                // string_fragment -> string -> import_statement
                let is_type = node
                    .parent()
                    .and_then(|s| s.parent())
                    .map(has_type_token)
                    .unwrap_or(false);
                let kind = if is_type {
                    ReferenceKind::TypeImport
                } else {
                    ReferenceKind::Import
                };
                Some((kind, text))
            }
            "ref.reexport" => Some((ReferenceKind::Reexport, text)),
            _ => None,
        }
    }
}

/// Identifier-family node kinds whose text is a safe path segment.
fn is_identifier_kind(kind: &str) -> bool {
    matches!(
        kind,
        "identifier"
            | "type_identifier"
            | "property_identifier"
            | "private_property_identifier"
            | "shorthand_property_identifier"
    )
}

/// Whether a name is a delimiter-safe `syntax_path` segment (no `;`/`=`/`/`/`:`,
/// no whitespace or control). Identifier-family names always satisfy this; the
/// check is a defensive backstop for the `SyntaxLocator::serialize` invariant.
fn is_safe_segment(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| !matches!(c, ';' | '=' | '/' | ':') && !c.is_whitespace() && !c.is_control())
}

/// The text of `node`'s `field` child, or `""`.
fn field_text(node: Node, field: &str, src: &[u8]) -> String {
    node.child_by_field_name(field)
        .and_then(|n| n.utf8_text(src).ok())
        .unwrap_or("")
        .to_string()
}

/// The sorted, de-duplicated set of modifier keyword tokens directly under
/// `node` (async/static/abstract/get/set/…), joined with `,`.
fn modifiers(node: Node) -> String {
    const MODS: &[&str] = &[
        "async",
        "*",
        "static",
        "abstract",
        "readonly",
        "get",
        "set",
        "public",
        "private",
        "protected",
        "override",
        "declare",
        "const",
    ];
    let mut found: Vec<&str> = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if !child.is_named() && MODS.contains(&child.kind()) {
            found.push(child.kind());
        }
    }
    found.sort_unstable();
    found.dedup();
    found.join(",")
}

/// The concatenated heritage clauses (`extends`/`implements`) of `node`, or `""`.
fn heritage_text(node: Node, src: &[u8]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let kind = child.kind();
        if kind == "class_heritage" || kind.contains("extends") || kind.contains("implements") {
            parts.push(child.utf8_text(src).unwrap_or("").to_string());
        }
    }
    parts.join("|")
}

/// The count of named members in `node`'s body (class/interface/enum/namespace).
fn body_member_count(node: Node) -> usize {
    let body = node.child_by_field_name("body").or_else(|| {
        let mut cursor = node.walk();
        node.children(&mut cursor)
            .find(|c| c.kind().ends_with("_body") || c.kind() == "statement_block")
    });
    match body {
        Some(b) => {
            let mut cursor = b.walk();
            b.children(&mut cursor).filter(|c| c.is_named()).count()
        }
        None => 0,
    }
}

/// Whether `stmt` has a direct anonymous `type` token (an `import type …`).
fn has_type_token(stmt: Node) -> bool {
    let mut cursor = stmt.walk();
    stmt.children(&mut cursor)
        .any(|c| !c.is_named() && c.kind() == "type")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::locator::SyntaxAnchor;

    fn parse(src: &str) -> ParseOutput {
        TypeScriptParser::new().parse(src.as_bytes())
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
            "export function foo(a: number): void {}\nclass Bar {}\ninterface I {}\nenum E { A }\ntype T = number;\nnamespace N {}\n",
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
            find(&out, UnitKind::Symbol, "I").lang_kind.as_deref(),
            Some("interface")
        );
        assert_eq!(
            find(&out, UnitKind::Symbol, "E").lang_kind.as_deref(),
            Some("enum")
        );
        assert_eq!(
            find(&out, UnitKind::Symbol, "T").lang_kind.as_deref(),
            Some("type_alias")
        );
        assert_eq!(
            find(&out, UnitKind::Symbol, "N").lang_kind.as_deref(),
            Some("namespace")
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
        let out = parse("class Host {\n  method(x: number) {}\n}\n");
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
    fn overloads_differ_by_signature() {
        let out = parse(
            "function ov(a: number): number;\nfunction ov(a: string): string;\nfunction ov(a: any): any { return a; }\n",
        );
        let sigs: Vec<&str> = out
            .units
            .iter()
            .filter(|u| u.local_name.as_deref() == Some("ov"))
            .map(|u| u.signature_fingerprint.as_str())
            .collect();
        assert_eq!(sigs.len(), 3);
        let mut uniq = sigs.clone();
        uniq.sort_unstable();
        uniq.dedup();
        assert_eq!(uniq.len(), 3, "overloads must have distinct signatures");
    }

    #[test]
    fn top_level_function_const_is_captured_but_value_const_is_not() {
        let out = parse(
            "export const f = (x: number) => x;\nexport const obj = { a: 1 };\nconst n = 5;\n",
        );
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
        let out = parse(
            "import { A } from \"./mod\";\nimport type { T } from \"./types\";\nexport * from \"./re\";\n",
        );
        let by_text = |t: &str| {
            out.unresolved
                .iter()
                .find(|r| r.reference_text == t)
                .unwrap()
        };
        assert_eq!(by_text("./mod").reference_kind, ReferenceKind::Import);
        assert_eq!(by_text("./types").reference_kind, ReferenceKind::TypeImport);
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
        assert_eq!(TypeScriptParser::new().parse(src.as_bytes()), first);
    }

    #[test]
    fn locator_and_signature_goldens() {
        // Implementation-specific tripwire: pins the exact serialized locator (with
        // a fixed placeholder blob_id) and the exact `sig` hex, which the neutral
        // fixtures deliberately omit. A change to the sig algorithm or anchor
        // formatting must update these deliberately.
        use crate::parse::locator::SyntaxLocator;
        let out = parse("function foo(a: number): void {}\n");
        let foo = find(&out, UnitKind::Symbol, "foo");
        assert_eq!(
            foo.signature_fingerprint,
            "120e9865e8347d390dfc37e67dd1d75882d5348a2d0891cf01ca58deaf4ea8b4"
        );
        let locator = SyntaxLocator::from_draft(
            foo.locator_draft(LanguageId::TypeScript),
            "b10b1d".to_string(),
        );
        assert_eq!(
            locator.serialize(),
            "anchor=p:function:foo;blob=b10b1d;lang=typescript;\
             sig=120e9865e8347d390dfc37e67dd1d75882d5348a2d0891cf01ca58deaf4ea8b4"
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
