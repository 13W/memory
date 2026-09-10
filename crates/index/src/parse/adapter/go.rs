//! The Go tree-sitter adapter (ADR-0015, T24-03; ADR-0002 derivation).
//!
//! The `tree-sitter-go` grammar is used for every `.go` file. It is pinned at
//! `0.23` (ABI 14) to pair with the workspace `tree-sitter 0.24` core, for the
//! reason every grammar before it is (see the dependency allowlist in
//! CONTRIBUTING.md). The declared `grammar_version`/`query_version` (1/1) are
//! reconciled to the pinned crate — a documented, non-silent binding (spec 03
//! §2.3.1), matching T04-03/04/05 and T24-01/02.
//!
//! The shared engine ([`parse_with`]) owns everything language-independent; this
//! adapter supplies only the grammar, the Go query set (`go.scm`), the capture map,
//! and the name/signature/reference hooks. Go-specific decisions, each measured
//! against the real grammar before it was written and pinned by a test below:
//!
//! - **The spec is the unit, not the declaration** (owner decision, T24-03). A
//!   grouped `type ( Server …; Client … )` yields two named units rather than one,
//!   at the price of the `type` keyword falling outside the span.
//! - **A spec that declares several names is one unit named by the first**
//!   (`const C, D = 3, 4` → `const:C`), with every name in the signature — the rule
//!   T24-02 settled for Bash. The grammar labels a multi-name spec's separating
//!   comma `name:` as well, so name fields are filtered by node kind.
//! - **A method is `method`, a function is `function`.** Methods are top-level
//!   nodes, so `finalize` gives them no parent and the route is flat; the receiver
//!   goes into the signature, which is what keeps `func (a A) Do()` and
//!   `func (b B) Do()` distinct rather than collapsing them onto ordinals.
//! - **A type's shape is in the signature by node kind, never by text.** Pushing
//!   `field_text(spec, "type")` for a `type_spec` would put an entire struct body
//!   into the descriptor; the kind (`struct_type`, `interface_type`, …) is pushed
//!   instead. For a `const`/`var` spec the type text is short and is pushed as-is.

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
const QUERY_SRC: &str = include_str!("go.scm");

/// A Go parser adapter over the `tree-sitter-go` grammar.
pub struct GoParser {
    language: tree_sitter::Language,
    query: Query,
}

impl GoParser {
    /// Compile the grammar and query once. The query is a build-time constant, so a
    /// compile failure is a bug (panics).
    pub fn new() -> Self {
        let language: tree_sitter::Language = tree_sitter_go::LANGUAGE.into();
        let query = Query::new(&language, QUERY_SRC).expect("the bundled Go query must compile");
        Self { language, query }
    }
}

impl Default for GoParser {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageParser for GoParser {
    fn language(&self) -> LanguageId {
        LanguageId::Go
    }

    fn parse(&self, source: &[u8]) -> ParseOutput {
        parse_with(self, source)
    }
}

impl LanguageSpec for GoParser {
    fn language(&self) -> LanguageId {
        LanguageId::Go
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
            "decl.method" => decl("method"),
            "decl.type" => decl("type"),
            "decl.const" => decl("const"),
            "decl.var" => decl("variable"),
            "ref.import" => CaptureRole::Reference,
            _ => CaptureRole::Ignore,
        }
    }

    fn local_name(&self, decl: Node, src: &[u8]) -> Option<String> {
        let name = declared_name_nodes(decl).into_iter().next()?;
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
            SignatureDescriptor::new(LanguageId::Go.as_str(), unit_kind.as_str(), lang_kind);
        d.push(self.local_name(decl, src).unwrap_or_default());
        // The receiver is what makes `func (a A) Do()` and `func (b B) Do()` two
        // different methods under one flat route.
        d.push(field_text(decl, "receiver", src));
        d.push(field_text(decl, "type_parameters", src));
        d.push(field_text(decl, "parameters", src));
        d.push(field_text(decl, "result", src));
        // Every declared name, so `const C, D` and `const C, E` differ.
        d.push(
            declared_name_nodes(decl)
                .iter()
                .map(|n| n.utf8_text(src).unwrap_or("").to_string())
                .collect::<Vec<_>>()
                .join(","),
        );
        let (type_kind, type_text) = type_field(decl, src);
        d.push(type_kind);
        d.push(type_text);
        // The kinds of the initialising expressions (`int_literal`, `iota`, …),
        // never their text — the shape the Python adapter uses for `right`.
        d.push(value_kinds(decl));
        d.push(member_count(decl).to_string());
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
                // The captured node is the literal's content, so the path arrives
                // already unquoted. Go has no re-export or type-only import form.
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

/// The name nodes a declaration node declares, in source order.
///
/// One for a function, method or type spec; possibly several for a `const`/`var`
/// spec (`var a, b int`). The grammar labels a multi-name spec's separating comma
/// with the `name` field too, so anonymous nodes are filtered out here rather than
/// surfacing as an ordinal anchor.
fn declared_name_nodes(decl: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = decl.walk();
    let names: Vec<Node<'_>> = decl
        .children_by_field_name("name", &mut cursor)
        .filter(|n| n.is_named())
        .collect();
    names
}

/// The `type` field's `(node kind, text)`, where the text is kept only when it is a
/// type *reference* rather than a type *definition*.
///
/// A `const`/`var` spec's type is a short name (`int`, `[]byte`) and belongs in the
/// descriptor verbatim. A `type_spec`'s type is the whole definition — an entire
/// struct or interface body — so only its kind is kept, and the member count picks
/// up the rest.
fn type_field(decl: Node, src: &[u8]) -> (String, String) {
    let Some(node) = decl.child_by_field_name("type") else {
        return (String::new(), String::new());
    };
    let kind = node.kind().to_string();
    let text = if decl.kind() == "type_spec" {
        String::new()
    } else {
        node.utf8_text(src).unwrap_or("").to_string()
    };
    (kind, text)
}

/// How many members a declaration has: a function or method body's statements, or
/// a struct's fields / an interface's elements.
///
/// The shared [`body_member_count`] only finds a `body` field or a `*_body` /
/// `statement_block` child, which covers Go functions but not a `type_spec` — its
/// members live under `type: (struct_type (field_declaration_list …))`. Without
/// this branch a struct's `sig` would not move when the struct gains a field.
fn member_count(decl: Node) -> usize {
    let Some(type_node) = decl.child_by_field_name("type") else {
        return body_member_count(decl);
    };
    if decl.kind() != "type_spec" {
        return body_member_count(decl);
    }
    let mut cursor = type_node.walk();
    let field_list = type_node
        .named_children(&mut cursor)
        .find(|c| c.kind() == "field_declaration_list");
    let counted = field_list.unwrap_or(type_node);
    let mut cursor = counted.walk();
    counted.named_children(&mut cursor).count()
}

/// The node kinds of a spec's initialising expressions, joined with `,`.
///
/// The `value` field is an `expression_list` even for a single value, so its named
/// children are what carry information (`int_literal`, `iota`, `func_literal`, …).
fn value_kinds(decl: Node) -> String {
    let Some(value) = decl.child_by_field_name("value") else {
        return String::new();
    };
    if value.kind() != "expression_list" {
        return value.kind().to_string();
    }
    let mut cursor = value.walk();
    value
        .named_children(&mut cursor)
        .map(|c| c.kind().to_string())
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::language::SourceDialect;
    use crate::parse::locator::SyntaxAnchor;

    fn parse(src: &str) -> ParseOutput {
        GoParser::new().parse(src.as_bytes())
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

    fn symbols(out: &ParseOutput) -> Vec<(Option<String>, Option<String>)> {
        out.units
            .iter()
            .filter(|u| u.unit_kind == UnitKind::Symbol)
            .map(|u| (u.local_name.clone(), u.lang_kind.clone()))
            .collect()
    }

    #[test]
    fn grammar_loads_and_extracts_symbols() {
        // Guard: if the grammar failed to load (e.g. an ABI mismatch), the engine
        // degrades to a file-only parse. A non-empty source MUST yield a symbol.
        let out = parse("package main\n\nfunc Present() {}\n");
        assert!(
            out.units.iter().any(|u| u.unit_kind == UnitKind::Symbol),
            "the Go grammar must load and produce symbols (not just a file unit)"
        );
    }

    #[test]
    fn every_file_gets_exactly_one_file_unit() {
        let src = "package main\n\nvar X = 1\n";
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
        // Covers every classify_capture declaration branch.
        let out = parse(
            "package main\n\nfunc Run() {}\n\nfunc (s *Server) Start() {}\n\n\
             type Foo struct{}\n\nconst K = 1\n\nvar V string\n",
        );
        for (name, kind) in [
            ("Run", "function"),
            ("Start", "method"),
            ("Foo", "type"),
            ("K", "const"),
            ("V", "variable"),
        ] {
            assert_eq!(
                find(&out, UnitKind::Symbol, name).lang_kind.as_deref(),
                Some(kind),
                "kind for {name}"
            );
        }
        assert_eq!(symbols(&out).len(), 5);
        // The package clause is not a unit: it names the file's package, not a
        // declaration anyone searches for by name.
        assert!(
            !out.units
                .iter()
                .any(|u| u.local_name.as_deref() == Some("main"))
        );
    }

    #[test]
    fn a_method_is_named_by_its_field_identifier() {
        // The card's named regression: a method's name node is a `field_identifier`,
        // which only reaches `local_name` because T24-01 widened the allowlist. A
        // narrower allowlist would silently give this unit an ordinal anchor.
        let out = parse("package main\n\nfunc (s *Server) Start() error {\n\treturn nil\n}\n");
        let start = find(&out, UnitKind::Symbol, "Start");
        assert_eq!(start.lang_kind.as_deref(), Some("method"));
        assert_eq!(start.anchor, SyntaxAnchor::Path("method:Start".to_string()));
        // Methods are top-level nodes in Go, so the route is flat, not
        // `type:Server/method:Start` — the receiver is in the signature instead.
        assert_eq!(start.parent, None);
    }

    #[test]
    fn identically_named_methods_differ_by_receiver_not_by_anchor() {
        // Two receivers, one method name: both keep the named route (the locator
        // separates them by `sig`), which is only true because the receiver is a
        // descriptor field. The same method declared twice on the SAME receiver is
        // genuinely indistinguishable and demotes to ordinals instead.
        let out = parse("package main\n\nfunc (a A) Do() {}\n\nfunc (b B) Do() {}\n");
        let dos: Vec<_> = out
            .units
            .iter()
            .filter(|u| u.local_name.as_deref() == Some("Do"))
            .collect();
        assert_eq!(dos.len(), 2);
        assert!(
            dos.iter()
                .all(|u| u.anchor == SyntaxAnchor::Path("method:Do".to_string()))
        );
        assert_ne!(dos[0].signature_fingerprint, dos[1].signature_fingerprint);

        let same = parse("package main\n\nfunc (a A) Do() {}\n\nfunc (a A) Do() {}\n");
        let dos: Vec<_> = same
            .units
            .iter()
            .filter(|u| u.local_name.as_deref() == Some("Do"))
            .collect();
        assert_eq!(dos[0].anchor, SyntaxAnchor::LocalOrdinal(1));
        assert_eq!(dos[1].anchor, SyntaxAnchor::LocalOrdinal(2));
    }

    #[test]
    fn a_grouped_declaration_yields_one_unit_per_spec() {
        // The T24-03 decision (owner): the spec is the unit, so every name in a
        // grouped block is findable. The `type`/`const`/`var` keyword is outside the
        // span, which is the accepted cost.
        let src =
            "package main\n\ntype (\n\tServer struct{ A int }\n\tClient interface{ Do() }\n)\n";
        let out = parse(src);
        assert_eq!(
            symbols(&out),
            vec![
                (Some("Server".to_string()), Some("type".to_string())),
                (Some("Client".to_string()), Some("type".to_string())),
            ]
        );
        let server = find(&out, UnitKind::Symbol, "Server");
        assert_eq!(server.span.start, src.find("Server").unwrap() as u32);

        // An ungrouped declaration is the same unit shape, one spec.
        let src = "package main\n\ntype Foo struct{}\n";
        let out = parse(src);
        let foo = find(&out, UnitKind::Symbol, "Foo");
        assert_eq!(foo.span.start, src.find("Foo").unwrap() as u32);
        assert_eq!(foo.span.end, src.len() as u32 - 1);
    }

    #[test]
    fn a_spec_with_several_names_is_one_unit_named_by_the_first() {
        // `const C, D = 3, 4` is one `const_spec`; `D` lives in the signature, not
        // in a second unit. The grammar also labels the separating comma `name:`,
        // so a comma must never become the unit's name.
        let out = parse("package main\n\nconst C, D = 3, 4\n\nvar a, b int\n");
        assert_eq!(
            symbols(&out),
            vec![
                (Some("C".to_string()), Some("const".to_string())),
                (Some("a".to_string()), Some("variable".to_string())),
            ]
        );
        // The second name is signature-bearing.
        let two = find(&out, UnitKind::Symbol, "C")
            .signature_fingerprint
            .clone();
        let one = parse("package main\n\nconst C = 3\n");
        assert_ne!(two, find(&one, UnitKind::Symbol, "C").signature_fingerprint);
    }

    #[test]
    fn declarations_are_captured_at_any_depth_and_nest() {
        let out = parse(
            "package main\n\nfunc Outer() {\n\ttype Local struct{}\n\tvar z int\n\t_ = z\n}\n",
        );
        let outer_idx = index_of(&out, find(&out, UnitKind::Symbol, "Outer"));
        let local = find(&out, UnitKind::Symbol, "Local");
        assert_eq!(local.parent, Some(outer_idx));
        assert_eq!(
            local.anchor,
            SyntaxAnchor::Path("function:Outer/type:Local".to_string())
        );
        let z = find(&out, UnitKind::Symbol, "z");
        assert_eq!(
            z.anchor,
            SyntaxAnchor::Path("function:Outer/variable:z".to_string())
        );
        // A `func_literal` has no name and is deliberately not a unit.
        let out = parse("package main\n\nfunc Outer() {\n\tinner := func() {}\n\t_ = inner\n}\n");
        assert_eq!(
            symbols(&out),
            vec![(Some("Outer".to_string()), Some("function".to_string()))]
        );
    }

    #[test]
    fn byte_spans_are_exact() {
        let src = "package main\n\nfunc Run() {}";
        let out = parse(src);
        let run = find(&out, UnitKind::Symbol, "Run");
        assert_eq!(run.span.start, src.find("func Run").unwrap() as u32);
        assert_eq!(run.span.end, src.len() as u32);
    }

    #[test]
    fn every_import_spec_is_one_reference_to_its_path() {
        // The alias is never the reference: `f`, `_` and `.` all name the same
        // dependency as the path beside them. Both literal forms are covered.
        let out = parse(
            "package main\n\nimport \"fmt\"\n\nimport (\n\t\"os\"\n\tf \"path/filepath\"\n\
             \t_ \"embed\"\n\t. \"strings\"\n)\n\nimport `text/template`\n",
        );
        let texts: Vec<&str> = out
            .unresolved
            .iter()
            .map(|r| r.reference_text.as_str())
            .collect();
        assert_eq!(
            texts,
            vec![
                "fmt",
                "os",
                "path/filepath",
                "embed",
                "strings",
                "text/template"
            ]
        );
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
    fn error_input_yields_fallback_chunk_and_recovers_the_next_declaration() {
        let src = "package main\n\ntype = = =\n\nfunc Ok() {}\n";
        let out = parse(src);
        let chunk = out
            .units
            .iter()
            .find(|u| u.unit_kind == UnitKind::FallbackChunk)
            .expect("malformed input must produce a fallback chunk");
        assert_eq!(chunk.span.start, src.find("type = = =").unwrap() as u32);
        let ok = find(&out, UnitKind::Symbol, "Ok");
        assert_eq!(ok.anchor, SyntaxAnchor::Path("function:Ok".to_string()));
        assert!(ok.span.start >= chunk.span.end);
    }

    #[test]
    fn an_unclosed_parameter_list_swallows_what_follows() {
        // Measured, and recorded because it is a real limit rather than a bug to
        // fix here: after `func broken( {`, tree-sitter-go does NOT recover the next
        // declaration as a `function_declaration` — it parses `func Ok()` as a type
        // inside the unterminated parameter list, with ERROR nodes around it. So
        // `Ok` gets no unit, and the fallback chunks land *inside* `broken`'s span.
        let out = parse("package main\n\nfunc broken( {\n\nfunc Ok() {}\n");
        assert_eq!(
            symbols(&out),
            vec![(Some("broken".to_string()), Some("function".to_string()))]
        );
        let broken_idx = index_of(&out, find(&out, UnitKind::Symbol, "broken"));
        let chunks: Vec<_> = out
            .units
            .iter()
            .filter(|u| u.unit_kind == UnitKind::FallbackChunk)
            .collect();
        assert!(!chunks.is_empty());
        assert!(chunks.iter().all(|c| c.parent == Some(broken_idx)));
    }

    #[test]
    fn parse_is_deterministic() {
        let src = "package main\n\nimport \"fmt\"\n\ntype S struct{ A int }\n\n\
                   func (s *S) Print() {\n\tfmt.Println(s.A)\n}\n";
        let first = parse(src);
        for _ in 0..4 {
            assert_eq!(parse(src), first);
        }
        assert_eq!(GoParser::new().parse(src.as_bytes()), first);
    }

    #[test]
    fn signature_descriptor_field_order_is_pinned() {
        // The hex goldens below are opaque; this states what they are made of, so a
        // reordered or dropped descriptor field fails with a readable diff rather
        // than an unexplained hash change. Note the double hash: every adapter's
        // `signature_descriptor` returns `SignatureDescriptor::fingerprint` (already
        // a hash), which the shared engine then hashes again.
        use crate::parse::signature::{FIELD_SEP, fingerprint};
        let sep = FIELD_SEP.to_string();

        // Go allows no type parameters on a method, so the receiver and the type
        // parameter list are pinned on two declarations rather than one.
        let out = parse("package main\n\nfunc (s *Server) Start(p int) error {\n\treturn nil\n}\n");
        let start = find(&out, UnitKind::Symbol, "Start");
        let expected = [
            "go",
            "symbol",
            "method",      // head: language, unit kind, lang kind
            "Start",       // local name
            "(s *Server)", // receiver — what separates two types' `Start`
            "",            // no type parameters (a method may not have them)
            "(p int)",     // parameters
            "error",       // result
            "Start",       // every declared name (one, for a method)
            "",            // no `type` field on a method
            "",            // …so no type text either
            "",            // no initialising value
            "1",           // members: the body's single return statement
        ]
        .join(&sep);
        assert_eq!(
            start.signature_fingerprint,
            fingerprint(&fingerprint(&expected))
        );

        let out = parse("package main\n\nfunc Map[T any](xs []T) []T {\n\treturn xs\n}\n");
        let map = find(&out, UnitKind::Symbol, "Map");
        let expected = [
            "go", "symbol", "function", //
            "Map", "", "[T any]", "(xs []T)", "[]T", "Map", "", "", "", "1",
        ]
        .join(&sep);
        assert_eq!(
            map.signature_fingerprint,
            fingerprint(&fingerprint(&expected))
        );

        let out = parse("package main\n\nvar a, b []byte = nil, nil\n");
        let a = find(&out, UnitKind::Symbol, "a");
        let expected = [
            "go",
            "symbol",
            "variable", //
            "a",        // local name: the first declared
            "",
            "",
            "",
            "",           // no receiver, type parameters, parameters or result
            "a,b",        // every declared name
            "slice_type", // the `type` field's node kind
            "[]byte",     // …and its text, which is short for a var spec
            "nil,nil",    // the initialising expressions, by kind
            "0",          // no members
        ]
        .join(&sep);
        assert_eq!(
            a.signature_fingerprint,
            fingerprint(&fingerprint(&expected))
        );
    }

    #[test]
    fn a_types_shape_is_in_the_signature_by_kind_not_by_text() {
        // A struct's field list must not enter the descriptor: `field_text(spec,
        // "type")` would put the whole body there. The kind plus the member count
        // still separate a struct from an interface, and separate two structs whose
        // field counts differ.
        let struct_sig = |src: &str| {
            let out = parse(src);
            find(&out, UnitKind::Symbol, "T")
                .signature_fingerprint
                .clone()
        };
        let one = struct_sig("package main\n\ntype T struct{ A int }\n");
        assert_ne!(
            one,
            struct_sig("package main\n\ntype T interface{ Do() }\n")
        );
        assert_ne!(
            one,
            struct_sig("package main\n\ntype T struct{ A int\nB int }\n")
        );
        // Renaming a field does not move the sig — the accepted consequence of
        // keeping type text out of the descriptor.
        assert_eq!(one, struct_sig("package main\n\ntype T struct{ Z int }\n"));
    }

    #[test]
    fn locator_and_signature_goldens() {
        // Implementation-specific tripwire: pins the exact serialized locator (with a
        // fixed placeholder blob_id) and the exact `sig` hex, which the neutral
        // fixtures deliberately omit. A change to the sig algorithm, the descriptor
        // field order, or anchor formatting must update these deliberately.
        use crate::parse::locator::SyntaxLocator;
        let out = parse("package main\n\nfunc Run(a int) error {\n\treturn nil\n}\n");
        let run = find(&out, UnitKind::Symbol, "Run");
        assert_eq!(
            run.signature_fingerprint,
            "0b13c2fb0c68f77cda3ec03b3a9a9a9a3f6218e6a4dec4a690fba67cfb0bdcd1"
        );
        let locator = SyntaxLocator::from_draft(
            run.locator_draft(SourceDialect::Language(LanguageId::Go)),
            "b10b1d".to_string(),
        );
        assert_eq!(
            locator.serialize(),
            "anchor=p:function:Run;blob=b10b1d;lang=go;\
             sig=0b13c2fb0c68f77cda3ec03b3a9a9a9a3f6218e6a4dec4a690fba67cfb0bdcd1"
        );
    }

    #[test]
    fn unicode_spans_are_byte_offsets() {
        // "café☕" is 8 bytes; a Go identifier admits non-ASCII letters, so both the
        // offset and the name are exercised here.
        let src = "package main\n\nvar S = \"café☕\"\n\nfunc café() {}\n";
        let out = parse(src);
        let cafe = find(&out, UnitKind::Symbol, "café");
        assert_eq!(cafe.span.start, src.find("func café").unwrap() as u32);
        assert_eq!(cafe.anchor, SyntaxAnchor::Path("function:café".to_string()));
    }
}
