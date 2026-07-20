//! The shared, language-agnostic tree-sitter engine (ADR-0002).
//!
//! [`parse_with`] turns exact source bytes into a canonical [`ParseOutput`] given
//! a [`LanguageSpec`]. The engine owns everything language-independent — running
//! the query, collecting units, the file unit, the error/fallback pass, canonical
//! ordering, parent inference, and the `syntax_path`/ordinal anchor derivation —
//! so a new language (T04-04 JavaScript, T04-05 Rust) supplies only a
//! [`LanguageSpec`] (grammar, query, capture map, name/signature/reference hooks),
//! honoring ADR-0001 ("the choice lives in data/config, not the parser core").

pub mod typescript;

use std::collections::{HashMap, HashSet};

use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Parser, Query, QueryCursor};

use crate::parse::language::LanguageId;
use crate::parse::locator::SyntaxAnchor;
use crate::parse::output::{ByteSpan, ParseOutput, ParsedUnitDraft, ReferenceKind, UnresolvedRef};
use crate::parse::signature;
use local_rag_store::code::UnitKind;

/// The fixed anchor of the whole-file unit (a delimiter-safe token).
const FILE_ANCHOR: &str = "file";

/// The role of a query capture, decided by the [`LanguageSpec`].
pub enum CaptureRole {
    /// A declaration node that becomes a `symbol` unit with this language kind.
    Decl {
        /// The schema-level unit kind (`symbol` for code declarations).
        unit_kind: UnitKind,
        /// The language-level kind label (`function`, `class`, …).
        lang_kind: &'static str,
    },
    /// A module-specifier node that becomes an [`UnresolvedRef`].
    Reference,
    /// A capture the engine ignores.
    Ignore,
}

/// A per-language description the shared engine drives (ADR-0001 seam).
pub trait LanguageSpec {
    /// The language this spec handles.
    fn language(&self) -> LanguageId;
    /// The compiled tree-sitter grammar.
    fn ts_language(&self) -> &tree_sitter::Language;
    /// The compiled, versioned query (`queries=` dimension of the fingerprint).
    fn query(&self) -> &Query;
    /// Classify a capture name into its [`CaptureRole`].
    fn classify_capture(&self, capture_name: &str) -> CaptureRole;
    /// The unit's safe local name, or `None` if it is anonymous / non-identifier
    /// (⇒ the engine uses an ordinal anchor).
    fn local_name(&self, decl: Node, src: &[u8]) -> Option<String>;
    /// The canonical signature descriptor of a declaration (hashed into `sig`).
    fn signature_descriptor(
        &self,
        decl: Node,
        unit_kind: UnitKind,
        lang_kind: &str,
        src: &[u8],
    ) -> String;
    /// Resolve a reference capture into `(kind, unquoted specifier text)`.
    fn reference(
        &self,
        capture_name: &str,
        node: Node,
        src: &[u8],
    ) -> Option<(ReferenceKind, String)>;
    /// The language-level kind label used for the whole-file unit's descriptor.
    fn file_lang_kind(&self) -> &'static str {
        "file"
    }
    /// The language-level kind label used for a fallback (error) unit.
    fn fallback_lang_kind(&self) -> &'static str {
        "error"
    }
}

/// An internal working unit, before parent/anchor assignment.
struct Raw {
    span: ByteSpan,
    unit_kind: UnitKind,
    lang_kind: Option<String>,
    local_name: Option<String>,
    signature_fingerprint: String,
    /// A unit that must use an ordinal anchor (fallback chunks).
    force_ordinal: bool,
    /// The single whole-file unit (fixed anchor, never a parent).
    is_file: bool,
}

/// Parse `source` with `spec` into a canonical, deterministic [`ParseOutput`].
pub fn parse_with(spec: &dyn LanguageSpec, source: &[u8]) -> ParseOutput {
    let mut parser = Parser::new();
    if parser.set_language(spec.ts_language()).is_err() {
        return file_only(spec, source);
    }
    let Some(tree) = parser.parse(source, None) else {
        return file_only(spec, source);
    };
    let root = tree.root_node();

    let mut raws: Vec<Raw> = Vec::new();
    raws.push(file_raw(spec, source));

    let mut refs: Vec<(u32, ReferenceKind, String)> = Vec::new();

    let query = spec.query();
    let names = query.capture_names();
    let mut cursor = QueryCursor::new();
    let mut seen_decls: HashSet<usize> = HashSet::new();
    let mut matches = cursor.matches(query, root, source);
    while let Some(m) = matches.next() {
        for cap in m.captures {
            let capture_name = names[cap.index as usize];
            match spec.classify_capture(capture_name) {
                CaptureRole::Decl {
                    unit_kind,
                    lang_kind,
                } => {
                    let node = cap.node;
                    if !seen_decls.insert(node.id()) {
                        continue;
                    }
                    let span = ByteSpan::new(node.start_byte() as u32, node.end_byte() as u32);
                    let local_name = spec.local_name(node, source);
                    let descriptor = spec.signature_descriptor(node, unit_kind, lang_kind, source);
                    raws.push(Raw {
                        span,
                        unit_kind,
                        lang_kind: Some(lang_kind.to_string()),
                        local_name,
                        signature_fingerprint: signature::fingerprint(&descriptor),
                        force_ordinal: false,
                        is_file: false,
                    });
                }
                CaptureRole::Reference => {
                    if let Some((kind, text)) = spec.reference(capture_name, cap.node, source) {
                        refs.push((cap.node.start_byte() as u32, kind, text));
                    }
                }
                CaptureRole::Ignore => {}
            }
        }
    }

    collect_fallbacks(spec, root, &mut raws);

    raws.sort_by(canonical_cmp);
    let units = finalize(&raws);

    refs.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(a.1.as_str().cmp(b.1.as_str()))
            .then(a.2.cmp(&b.2))
    });
    let file_idx = units
        .iter()
        .position(|u| u.unit_kind == UnitKind::File)
        .unwrap_or(0);
    let unresolved = refs
        .into_iter()
        .map(|(_, reference_kind, reference_text)| UnresolvedRef {
            source_unit: file_idx,
            reference_text,
            reference_kind,
        })
        .collect();

    ParseOutput { units, unresolved }
}

/// The whole-file unit (always present), spanning the entire source.
fn file_raw(spec: &dyn LanguageSpec, source: &[u8]) -> Raw {
    let lang = spec.language();
    let mut descriptor = signature::SignatureDescriptor::new(
        lang.as_str(),
        UnitKind::File.as_str(),
        spec.file_lang_kind(),
    );
    descriptor.push("");
    Raw {
        span: ByteSpan::new(0, source.len() as u32),
        unit_kind: UnitKind::File,
        lang_kind: None,
        local_name: None,
        signature_fingerprint: descriptor.fingerprint(),
        force_ordinal: false,
        is_file: true,
    }
}

/// A degraded output with only the file unit (grammar load / parse failure).
fn file_only(spec: &dyn LanguageSpec, source: &[u8]) -> ParseOutput {
    let raws = vec![file_raw(spec, source)];
    ParseOutput {
        units: finalize(&raws),
        unresolved: Vec::new(),
    }
}

/// Emit a `fallback_chunk` unit for each outermost ERROR/MISSING node.
fn collect_fallbacks(spec: &dyn LanguageSpec, node: Node, out: &mut Vec<Raw>) {
    let lang = spec.language();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.is_error() || child.is_missing() {
            let mut descriptor = signature::SignatureDescriptor::new(
                lang.as_str(),
                UnitKind::FallbackChunk.as_str(),
                spec.fallback_lang_kind(),
            );
            descriptor.push("");
            out.push(Raw {
                span: ByteSpan::new(child.start_byte() as u32, child.end_byte() as u32),
                unit_kind: UnitKind::FallbackChunk,
                lang_kind: Some(spec.fallback_lang_kind().to_string()),
                local_name: None,
                signature_fingerprint: descriptor.fingerprint(),
                force_ordinal: true,
                is_file: false,
            });
            // Outermost only: do not descend into an error region.
        } else {
            collect_fallbacks(spec, child, out);
        }
    }
}

/// The canonical total order: `span.start` asc, `span.end` desc (enclosing
/// first), then `unit_kind`, `lang_kind`, `local_name`, `sig`.
fn canonical_cmp(a: &Raw, b: &Raw) -> std::cmp::Ordering {
    a.span
        .start
        .cmp(&b.span.start)
        .then(b.span.end.cmp(&a.span.end))
        .then(a.unit_kind.as_str().cmp(b.unit_kind.as_str()))
        .then(
            a.lang_kind
                .as_deref()
                .unwrap_or("")
                .cmp(b.lang_kind.as_deref().unwrap_or("")),
        )
        .then(
            a.local_name
                .as_deref()
                .unwrap_or("")
                .cmp(b.local_name.as_deref().unwrap_or("")),
        )
        .then(a.signature_fingerprint.cmp(&b.signature_fingerprint))
}

/// Assign parents (by span containment) and anchors (named route or ordinal),
/// then materialize the [`ParsedUnitDraft`] list. `raws` is already in canonical
/// order.
fn finalize(raws: &[Raw]) -> Vec<ParsedUnitDraft> {
    let n = raws.len();

    // Parent = nearest strictly-enclosing `symbol` unit (never the file or a
    // fallback). Canonical order guarantees a parent's index is smaller.
    let mut parent: Vec<Option<usize>> = vec![None; n];
    for i in 0..n {
        if raws[i].is_file {
            continue;
        }
        let mut best: Option<usize> = None;
        for j in 0..n {
            if j == i {
                continue;
            }
            let candidate = &raws[j];
            if candidate.is_file || candidate.unit_kind != UnitKind::Symbol {
                continue;
            }
            if candidate.span == raws[i].span || !candidate.span.encloses(raws[i].span) {
                continue;
            }
            match best {
                None => best = Some(j),
                Some(b) => {
                    if candidate.span.len() < raws[b].span.len() {
                        best = Some(j);
                    }
                }
            }
        }
        parent[i] = best;
    }

    // Ordinal = position among the parent's direct children in canonical order.
    let mut ordinal: Vec<u32> = vec![0; n];
    let mut counters: HashMap<Option<usize>, u32> = HashMap::new();
    for i in 0..n {
        let c = counters.entry(parent[i]).or_insert(0);
        ordinal[i] = *c;
        *c += 1;
    }

    // Named route from the parent chain, or `None` if any link is unsafe.
    let routes: Vec<Option<String>> = (0..n)
        .map(|i| {
            if raws[i].is_file {
                Some(FILE_ANCHOR.to_string())
            } else {
                named_route(i, raws, &parent)
            }
        })
        .collect();

    // Demote whole colliding groups (same parent, kind, route, sig) to ordinals.
    let mut groups: HashMap<(Option<usize>, &str, &str, &str), Vec<usize>> = HashMap::new();
    for i in 0..n {
        if raws[i].is_file {
            continue;
        }
        if let Some(route) = &routes[i] {
            groups
                .entry((
                    parent[i],
                    raws[i].unit_kind.as_str(),
                    route.as_str(),
                    raws[i].signature_fingerprint.as_str(),
                ))
                .or_default()
                .push(i);
        }
    }
    let mut collided: HashSet<usize> = HashSet::new();
    for members in groups.values() {
        if members.len() > 1 {
            collided.extend(members.iter().copied());
        }
    }

    (0..n)
        .map(|i| {
            let anchor = match &routes[i] {
                Some(route) if !collided.contains(&i) => SyntaxAnchor::Path(route.clone()),
                _ => SyntaxAnchor::LocalOrdinal(ordinal[i]),
            };
            ParsedUnitDraft {
                unit_kind: raws[i].unit_kind,
                span: raws[i].span,
                local_name: raws[i].local_name.clone(),
                lang_kind: raws[i].lang_kind.clone(),
                anchor,
                signature_fingerprint: raws[i].signature_fingerprint.clone(),
                parent: parent[i],
            }
        })
        .collect()
}

/// The named-declaration route `<kind>:<name>/…` from the outermost ancestor down
/// to unit `i`, or `None` if `i` is a fallback or any link lacks a safe name.
fn named_route(i: usize, raws: &[Raw], parent: &[Option<usize>]) -> Option<String> {
    if raws[i].force_ordinal {
        return None;
    }
    let mut chain = Vec::new();
    let mut cur = Some(i);
    while let Some(k) = cur {
        chain.push(k);
        cur = parent[k];
    }
    chain.reverse();
    let mut segments = Vec::with_capacity(chain.len());
    for &k in &chain {
        let name = raws[k].local_name.as_deref()?;
        let kind = raws[k].lang_kind.as_deref().unwrap_or("");
        segments.push(format!("{kind}:{name}"));
    }
    Some(segments.join("/"))
}
