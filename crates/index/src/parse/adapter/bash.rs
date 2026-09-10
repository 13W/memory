//! The Bash tree-sitter adapter (ADR-0015, T24-02; ADR-0002 derivation).
//!
//! The `tree-sitter-bash` grammar is used for every `.sh`/`.bash` file. It is
//! pinned at `0.23` (ABI 14) to pair with the workspace `tree-sitter 0.24` core,
//! for the reason the four earlier grammars are (see the dependency allowlist in
//! CONTRIBUTING.md). The declared `grammar_version`/`query_version` (1/1) are
//! reconciled to the pinned crate — a documented, non-silent binding (spec 03
//! §2.3.1), matching T04-03/04/05 and T24-01.
//!
//! The shared engine ([`parse_with`]) owns everything language-independent; this
//! adapter supplies only the grammar, the Bash query set (`bash.scm`), the capture
//! map, and the name/signature/reference hooks. Bash-specific decisions, each
//! measured against the real grammar before it was written and pinned by a test
//! below:
//!
//! - **All three function forms are one node kind.** `greet() { … }`,
//!   `function greet { … }` and `function greet() { … }` all parse to a
//!   `function_definition` with the same `name:` field (a `word`). The surface
//!   form is deliberately **not** part of the signature: the three spell the same
//!   function, so a cosmetic rewrite must not move `sig`.
//! - **One unit per declaration COMMAND, not per assignment** (owner decision,
//!   T24-02). `declare a=1 b=2` is a single node and a single unit, named by its
//!   first declared variable, with every declared name in the signature. The
//!   command is captured **at any depth**, so `f() { local x=1; }` yields both
//!   `function:f` and `function:f/variable:x`.
//! - **The declaration keyword and its flags are signature-bearing.** `local x=1`,
//!   `export x=1` and `declare -r x=1` are different declarations of the same name.
//! - **Only the first argument of `source`/`.` is a reference.**
//!   `source lib.sh arg1` passes `arg1` to the sourced script; it is not a second
//!   import.

use tree_sitter::{Node, Query};

use crate::parse::adapter::{
    CaptureRole, LanguageSpec, body_member_count, is_identifier_kind, is_safe_segment, parse_with,
};
use crate::parse::language::LanguageId;
use crate::parse::output::{ParseOutput, ReferenceKind};
use crate::parse::parser::LanguageParser;
use crate::parse::signature::SignatureDescriptor;
use local_rag_store::code::UnitKind;

/// The versioned query set (`queries=1`), embedded at build time.
const QUERY_SRC: &str = include_str!("bash.scm");

/// A Bash parser adapter over the `tree-sitter-bash` grammar.
pub struct BashParser {
    language: tree_sitter::Language,
    query: Query,
}

impl BashParser {
    /// Compile the grammar and query once. The query is a build-time constant, so a
    /// compile failure is a bug (panics).
    pub fn new() -> Self {
        let language: tree_sitter::Language = tree_sitter_bash::LANGUAGE.into();
        let query = Query::new(&language, QUERY_SRC).expect("the bundled Bash query must compile");
        Self { language, query }
    }
}

impl Default for BashParser {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageParser for BashParser {
    fn language(&self) -> LanguageId {
        LanguageId::Bash
    }

    fn parse(&self, source: &[u8]) -> ParseOutput {
        parse_with(self, source)
    }
}

impl LanguageSpec for BashParser {
    fn language(&self) -> LanguageId {
        LanguageId::Bash
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
            "decl.variable" => decl("variable"),
            "ref.import" => CaptureRole::Reference,
            _ => CaptureRole::Ignore,
        }
    }

    fn local_name(&self, decl: Node, src: &[u8]) -> Option<String> {
        let name = if decl.kind() == "declaration_command" {
            // Named by the first variable it declares (`declare a=1 b=2` → `a`);
            // the full list is in the signature.
            declared_name_nodes(decl).into_iter().next()?
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
            SignatureDescriptor::new(LanguageId::Bash.as_str(), unit_kind.as_str(), lang_kind);
        d.push(self.local_name(decl, src).unwrap_or_default());
        // The declaring keyword (`local`/`export`/`declare`/`readonly`/`typeset`)
        // and its option words: `local x=1` and `export x=1` are different
        // declarations, and `declare -r x=1` is a different one again.
        d.push(declaration_keyword(decl));
        d.push(declaration_flags(decl, src));
        // Every declared name, so `declare a=1 b=2` and `declare a=1 c=2` differ.
        d.push(
            declared_name_nodes(decl)
                .iter()
                .map(|n| n.utf8_text(src).unwrap_or("").to_string())
                .collect::<Vec<_>>()
                .join(","),
        );
        // The kind of each assigned value (`number`, `string`, `array`, …), never
        // the value text — the same shape as the Python adapter's `right` field.
        d.push(assigned_value_kinds(decl));
        // Bash functions have no parameter list; the body's statement count is the
        // only structural signal available. The surface form of the definition
        // (`name()` vs `function name`) is deliberately absent.
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
            "ref.import" => {
                // Only the first argument of `source`/`.` names the sourced file;
                // any further argument is passed to that script.
                if !is_first_argument(node) {
                    return None;
                }
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

/// The name nodes a `declaration_command` declares, in source order: the `name`
/// field of each `variable_assignment` child plus each bare `variable_name` child
/// (`local Z`). Option words (`-r`, `-a`) are not names. Any other node kind is
/// not a declaration and yields nothing.
fn declared_name_nodes(decl: Node<'_>) -> Vec<Node<'_>> {
    if decl.kind() != "declaration_command" {
        return Vec::new();
    }
    let mut cursor = decl.walk();
    decl.named_children(&mut cursor)
        .filter_map(|child| match child.kind() {
            "variable_assignment" => child.child_by_field_name("name"),
            "variable_name" => Some(child),
            _ => None,
        })
        .collect()
}

/// The declaring keyword of a `declaration_command` (`local`, `export`, `declare`,
/// `readonly`, `typeset`), or `""` for any other node. It is the command's first
/// child and an anonymous token, so it is read by kind rather than by text.
fn declaration_keyword(decl: Node) -> String {
    if decl.kind() != "declaration_command" {
        return String::new();
    }
    decl.child(0)
        .filter(|c| !c.is_named())
        .map(|c| c.kind().to_string())
        .unwrap_or_default()
}

/// The option words of a `declaration_command` (`declare -r -x` → `-r,-x`) in
/// source order, or `""`.
fn declaration_flags(decl: Node, src: &[u8]) -> String {
    if decl.kind() != "declaration_command" {
        return String::new();
    }
    let mut cursor = decl.walk();
    decl.named_children(&mut cursor)
        .filter(|c| c.kind() == "word")
        .map(|c| c.utf8_text(src).unwrap_or("").to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// The `value` field kind of each `variable_assignment` under a
/// `declaration_command`, in source order (a bare `local Z` contributes `""`).
fn assigned_value_kinds(decl: Node) -> String {
    if decl.kind() != "declaration_command" {
        return String::new();
    }
    let mut cursor = decl.walk();
    decl.named_children(&mut cursor)
        .filter(|c| matches!(c.kind(), "variable_assignment" | "variable_name"))
        .map(|c| {
            c.child_by_field_name("value")
                .map(|v| v.kind().to_string())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// Whether `node` is the first `argument:` child of its parent command.
fn is_first_argument(node: Node) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    for i in 0..parent.child_count() {
        if parent.field_name_for_child(i as u32) == Some("argument") {
            return parent.child(i) == Some(node);
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::language::SourceDialect;
    use crate::parse::locator::SyntaxAnchor;

    fn parse(src: &str) -> ParseOutput {
        BashParser::new().parse(src.as_bytes())
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
        let out = parse("present() {\n  :\n}\n");
        assert!(
            out.units.iter().any(|u| u.unit_kind == UnitKind::Symbol),
            "the Bash grammar must load and produce symbols (not just a file unit)"
        );
    }

    #[test]
    fn every_file_gets_exactly_one_file_unit() {
        let src = "export X=1\n";
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
    fn all_three_function_forms_yield_one_identically_signed_unit() {
        // `greet() { … }`, `function greet { … }` and `function greet() { … }` are
        // the same function written three ways. Each is one `function_definition`
        // with a `word` name, and the surface form is deliberately NOT part of the
        // signature — a cosmetic rewrite must not move `sig`.
        let bodies = [
            "greet() {\n  :\n}\n",
            "function greet {\n  :\n}\n",
            "function greet() {\n  :\n}\n",
        ];
        let mut sigs = Vec::new();
        for src in bodies {
            let out = parse(src);
            assert_eq!(
                symbols(&out),
                vec![(Some("greet".to_string()), Some("function".to_string()))],
                "one named function unit for {src:?}"
            );
            let greet = find(&out, UnitKind::Symbol, "greet");
            assert_eq!(
                greet.anchor,
                SyntaxAnchor::Path("function:greet".to_string())
            );
            assert_eq!(greet.span.start, 0);
            assert_eq!(greet.span.end, src.len() as u32 - 1);
            sigs.push(greet.signature_fingerprint.clone());
        }
        assert_eq!(sigs[0], sigs[1]);
        assert_eq!(sigs[1], sigs[2]);
    }

    #[test]
    fn extracts_core_declaration_kinds_with_lang_kind() {
        // Covers every classify_capture declaration branch: both function forms,
        // an assigning declaration and a bare one.
        let out =
            parse("greet() {\n  echo hi\n}\nfunction other {\n  :\n}\nexport API=1\nlocal Z\n");
        for (name, kind) in [
            ("greet", "function"),
            ("other", "function"),
            ("API", "variable"),
            ("Z", "variable"),
        ] {
            assert_eq!(
                find(&out, UnitKind::Symbol, name).lang_kind.as_deref(),
                Some(kind),
                "kind for {name}"
            );
        }
        assert_eq!(symbols(&out).len(), 4);
    }

    #[test]
    fn byte_spans_are_exact() {
        let src = "greet() {\n  :\n}";
        let out = parse(src);
        let greet = find(&out, UnitKind::Symbol, "greet");
        assert_eq!(greet.span.start, 0);
        assert_eq!(greet.span.end, src.len() as u32);
    }

    #[test]
    fn declarations_are_captured_at_any_depth_and_nest() {
        // The T24-02 decision (owner): a declaration command is a unit wherever it
        // appears, so `local` inside a function body routes under that function.
        let out = parse("outer() {\n  local x=1\n  inner() {\n    local y=2\n  }\n}\n");
        let outer_idx = index_of(&out, find(&out, UnitKind::Symbol, "outer"));
        let x = find(&out, UnitKind::Symbol, "x");
        assert_eq!(x.parent, Some(outer_idx));
        assert_eq!(
            x.anchor,
            SyntaxAnchor::Path("function:outer/variable:x".to_string())
        );
        let inner = find(&out, UnitKind::Symbol, "inner");
        assert_eq!(inner.parent, Some(outer_idx));
        let y = find(&out, UnitKind::Symbol, "y");
        assert_eq!(y.parent, Some(index_of(&out, inner)));
        assert_eq!(
            y.anchor,
            SyntaxAnchor::Path("function:outer/function:inner/variable:y".to_string())
        );
    }

    #[test]
    fn one_unit_per_declaration_command_named_by_its_first_variable() {
        // `declare a=1 b=2` is ONE node and one unit; `b` lives in the signature,
        // not in a second unit. An operand-less declaration and a plain assignment
        // are not units at all — no ordinal noise, the rule `python.scm` applies to
        // non-identifier assignment targets.
        let out = parse("declare a=1 b=2\ndeclare -p\nPLAIN=1\nlocal\n");
        assert_eq!(
            symbols(&out),
            vec![(Some("a".to_string()), Some("variable".to_string()))]
        );
        let a = find(&out, UnitKind::Symbol, "a");
        assert_eq!(a.span.start, 0);
        assert_eq!(a.span.end, "declare a=1 b=2".len() as u32);
    }

    #[test]
    fn declaration_signature_carries_keyword_flags_names_and_value_kinds() {
        // Four ways for two declarations of the same name to differ, each of which
        // must move `sig` while the route stays `variable:x`.
        let sig = |src: &str| {
            let out = parse(src);
            let x = find(&out, UnitKind::Symbol, "x");
            assert_eq!(x.anchor, SyntaxAnchor::Path("variable:x".to_string()));
            x.signature_fingerprint.clone()
        };
        let base = sig("local x=1\n");
        assert_ne!(
            base,
            sig("export x=1\n"),
            "the keyword is signature-bearing"
        );
        assert_ne!(base, sig("local -r x=1\n"), "flags are signature-bearing");
        assert_ne!(base, sig("local x=1 y=2\n"), "every declared name counts");
        assert_ne!(
            base,
            sig("local x=(1 2)\n"),
            "the assigned value's kind counts"
        );
        // The value itself does not: `1` and `2` are both `number`.
        assert_eq!(base, sig("local x=2\n"));
    }

    #[test]
    fn indistinguishable_sibling_declarations_demote_to_ordinals() {
        // Two declarations the engine cannot tell apart (same parent, kind, route
        // and sig) lose their named route rather than colliding on one anchor.
        let out = parse("f() {\n  local x=1\n  local x=2\n}\n");
        let xs: Vec<_> = out
            .units
            .iter()
            .filter(|u| u.local_name.as_deref() == Some("x"))
            .collect();
        assert_eq!(xs.len(), 2);
        assert_eq!(xs[0].anchor, SyntaxAnchor::LocalOrdinal(0));
        assert_eq!(xs[1].anchor, SyntaxAnchor::LocalOrdinal(1));
    }

    #[test]
    fn only_source_and_dot_produce_references_from_their_first_argument() {
        let out =
            parse("source lib.sh\n. ./other.sh\necho hi\nls -la\ngrep x f\nsource extra.sh arg1\n");
        let texts: Vec<&str> = out
            .unresolved
            .iter()
            .map(|r| r.reference_text.as_str())
            .collect();
        assert_eq!(texts, vec!["lib.sh", "./other.sh", "extra.sh"]);
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
    fn an_unevaluable_sourced_path_is_not_a_reference() {
        // A quoted or expanded specifier is a `string`, not a `word`: the path is
        // not knowable without evaluating the shell, so nothing is recorded rather
        // than recording a specifier that no resolver could ever match.
        let out = parse("source \"$DIR/lib.sh\"\nsource plain.sh\n");
        let texts: Vec<&str> = out
            .unresolved
            .iter()
            .map(|r| r.reference_text.as_str())
            .collect();
        assert_eq!(texts, vec!["plain.sh"]);
    }

    #[test]
    fn error_input_yields_fallback_chunk_and_names_the_definition_inside_it() {
        // Measured, not assumed (T24-02): after an unclosed body, tree-sitter-bash
        // parks the rest of the file in an ERROR node and parses the following
        // definition INSIDE it. The query matches at any depth, so `g` keeps its
        // name and overlaps the fallback chunk (ADR-0002).
        let src = "f() {\n  echo unclosed\n\ng() { :; }\n";
        let out = parse(src);
        let chunk = out
            .units
            .iter()
            .find(|u| u.unit_kind == UnitKind::FallbackChunk)
            .expect("malformed input must produce a fallback chunk");
        let g = find(&out, UnitKind::Symbol, "g");
        assert_eq!(g.anchor, SyntaxAnchor::Path("function:g".to_string()));
        assert_eq!(g.span.start, src.find("g() {").unwrap() as u32);
        assert!(g.span.start >= chunk.span.start && g.span.end <= chunk.span.end);
        // A fallback chunk is never a parent.
        assert_eq!(g.parent, None);
    }

    #[test]
    fn parse_is_deterministic() {
        let src = "source lib.sh\nreadonly A=1\nrun() {\n  local x=1\n  echo \"$x\"\n}\n";
        let first = parse(src);
        for _ in 0..4 {
            assert_eq!(parse(src), first);
        }
        assert_eq!(BashParser::new().parse(src.as_bytes()), first);
    }

    #[test]
    fn signature_descriptor_field_order_is_pinned() {
        // The hex goldens below are opaque; this states what they are made of, so
        // a reordered or dropped descriptor field fails with a readable diff
        // rather than an unexplained hash change. Note the double hash: every
        // adapter's `signature_descriptor` returns `SignatureDescriptor::fingerprint`
        // (already a hash), which the shared engine then hashes again as the unit's
        // opaque descriptor — the v0 shape, kept here rather than diverged from.
        use crate::parse::signature::{FIELD_SEP, fingerprint};
        let sep = FIELD_SEP.to_string();

        let out = parse("declare -r -a arr=(1 2)\n");
        let arr = find(&out, UnitKind::Symbol, "arr");
        let expected = [
            "bash", "symbol", "variable", // head: language, unit kind, lang kind
            "arr",      // local name
            "declare",  // the declaring keyword
            "-r,-a",    // its option words, in source order
            "arr",      // every declared name
            "array",    // the assigned value's node kind
            "0",        // body member count (a declaration has no body)
        ]
        .join(&sep);
        assert_eq!(
            arr.signature_fingerprint,
            fingerprint(&fingerprint(&expected))
        );

        let out = parse("greet() {\n  echo a\n  echo b\n}\n");
        let greet = find(&out, UnitKind::Symbol, "greet");
        let expected = [
            "bash", "symbol", "function", //
            "greet",    // local name
            "",         // no declaring keyword
            "",         // no option words
            "",         // no declared names
            "",         // no assigned value
            "2",        // the body's named children: the two commands
        ]
        .join(&sep);
        assert_eq!(
            greet.signature_fingerprint,
            fingerprint(&fingerprint(&expected))
        );
    }

    #[test]
    fn locator_and_signature_goldens() {
        // Implementation-specific tripwire: pins the exact serialized locator (with a
        // fixed placeholder blob_id) and the exact `sig` hex, which the neutral
        // fixtures deliberately omit. A change to the sig algorithm, the descriptor
        // field order, or anchor formatting must update these deliberately.
        use crate::parse::locator::SyntaxLocator;
        let out = parse("deploy() {\n  local target=prod\n  echo \"$target\"\n}\n");
        let deploy = find(&out, UnitKind::Symbol, "deploy");
        assert_eq!(
            deploy.signature_fingerprint,
            "7b66f952c8034af2cdc4ddf69e4248f5305712915192c7c84d7ceddbc488bcb8"
        );
        let locator = SyntaxLocator::from_draft(
            deploy.locator_draft(SourceDialect::Language(LanguageId::Bash)),
            "b10b1d".to_string(),
        );
        assert_eq!(
            locator.serialize(),
            "anchor=p:function:deploy;blob=b10b1d;lang=bash;\
             sig=7b66f952c8034af2cdc4ddf69e4248f5305712915192c7c84d7ceddbc488bcb8"
        );
        let target = find(&out, UnitKind::Symbol, "target");
        assert_eq!(
            target.anchor,
            SyntaxAnchor::Path("function:deploy/variable:target".to_string())
        );
        assert_eq!(
            target.signature_fingerprint,
            "be0ad631ceeb4daa4b9f8f0c133852fa10f56d095779f62fa0a58f06c9bf7243"
        );
    }

    #[test]
    fn unicode_spans_are_byte_offsets() {
        // "café☕" is 8 bytes; a bash function name is a `word`, which accepts
        // non-ASCII, so both the offset and the name are exercised here.
        let src = "S=\"café☕\"\ncafé() {\n  :\n}\n";
        let out = parse(src);
        let cafe = find(&out, UnitKind::Symbol, "café");
        assert_eq!(cafe.span.start, src.find("café() {").unwrap() as u32);
        assert_eq!(cafe.anchor, SyntaxAnchor::Path("function:café".to_string()));
    }
}
