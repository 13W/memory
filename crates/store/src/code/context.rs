//! The `code_context` subject serialization (spec 09 §3, 03 §4.2) — D-016.
//!
//! Spec 09 §3 left the shape of a `code_context` subject `[OPEN]`, to be
//! "decided by the benchmark". The benchmark decided: T12-05 measured v2 at
//! MRR 0.5646 against the v1 baseline's 0.6963, and reading v1's own indexer
//! showed it never embedded bare code. It embedded a **labelled context
//! envelope** (`scripts/benchmark.ts::buildEmbedCtx`):
//!
//! ```text
//! File: {path}
//! Type: {chunkType}
//! Name: {name}
//! JSDoc: {doc}        // when present
//! Sig: {signature}    // when present
//! Code:
//! {content}
//! ```
//!
//! So the missing signal was never "documentation" alone — it was path, kind,
//! name, doc *and* signature, none of which v2's `code_raw` vector carries.
//!
//! # Why this is a representation, not a wider span
//!
//! The obvious-looking fix — grow a unit's span to swallow its doc comment —
//! would conflate two different things: `content_blob` is the unit's **content
//! identity** (shared across paths, spec 03 §4.2 `[FIXED]`), while this is a
//! **retrieval representation** (path-dependent by construction). Growing spans
//! would also move `parsed_unit` boundaries, forcing a
//! `CHUNK_POLICY_VERSION` bump and a full re-parse, change every T12-04 snippet,
//! and *still* not supply path/name/signature. `code_context` is the slot the
//! architecture already reserved for exactly this, down to
//! `Domain::SubjectOccurrenceContext` carrying its own `context_version`.
//!
//! # Reproducing v1's fields without touching the schema
//!
//! Both derived fields come from bytes already stored, so nothing here needs a
//! migration or a parser change:
//!
//! - **signature** is v1's `getSignature` verbatim: the unit's first line,
//!   trimmed, capped at [`SIGNATURE_CAP_CHARS`] with an ellipsis.
//! - **doc** is v1's `extractJsDoc`/`extractLineComments`: walk back over blank
//!   lines from the unit's start, then take either a `/** … */` block or a
//!   contiguous run of `//` lines. The walk additionally stops at
//!   `previous_unit_end`, so a comment that trails the *previous* declaration is
//!   never stolen by the next one — an ambiguity v1 simply had.

use std::collections::HashMap;

use local_rag_core::identity::domain::subject_occurrence_context;

use crate::code::{FtsSourceRow, UnitKind, derive_content_blob};

/// The serialization-format version, hashed into the subject
/// (`subject_occurrence_context(CONTEXT_VERSION, …)`). Bumping it re-keys every
/// context subject, which is the intended effect of changing the envelope.
pub const CONTEXT_VERSION: u32 = 1;

/// Longest signature line kept, matching v1's `getSignature` cap.
pub const SIGNATURE_CAP_CHARS: usize = 200;

/// Everything the envelope needs about one occurrence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextInput<'a> {
    /// The occurrence's path within its worktree.
    pub normalized_path: &'a str,
    /// The unit's schema kind.
    pub unit_kind: UnitKind,
    /// The language-level kind (`function`, `class`, …), when the grammar
    /// exposes one.
    pub lang_kind: Option<&'a str>,
    /// The unit's local name, when it has one.
    pub local_name: Option<&'a str>,
    /// The unit's normalized text — the same text `code_raw` embeds.
    pub normalized_text: &'a str,
    /// The revision's exact stored bytes, for deriving the doc block.
    pub source: &'a [u8],
    /// The unit's span within `source`.
    pub span: (usize, usize),
    /// Where the previous unit of the same file ends, so a trailing comment is
    /// not attributed to this unit. `0` when this is the first.
    pub previous_unit_end: usize,
}

/// Render the `code_context` envelope for one occurrence.
///
/// Absent fields are **omitted entirely** rather than emitted empty: a `Doc:`
/// line with nothing after it is a token the model has to explain away, and it
/// would make "has no documentation" indistinguishable from "documented with the
/// empty string".
pub fn serialize(input: &ContextInput<'_>) -> String {
    let mut out = String::with_capacity(input.normalized_text.len() + 128);
    out.push_str("File: ");
    out.push_str(input.normalized_path);
    out.push('\n');

    out.push_str("Type: ");
    out.push_str(input.unit_kind.as_str());
    if let Some(lang_kind) = input.lang_kind.filter(|k| !k.is_empty()) {
        out.push('/');
        out.push_str(lang_kind);
    }
    out.push('\n');

    if let Some(name) = input.local_name.filter(|n| !n.is_empty()) {
        out.push_str("Name: ");
        out.push_str(name);
        out.push('\n');
    }

    if let Some(doc) = doc_block(input.source, input.span.0, input.previous_unit_end) {
        out.push_str("Doc: ");
        out.push_str(&doc);
        out.push('\n');
    }

    if let Some(sig) = signature(input.normalized_text) {
        out.push_str("Sig: ");
        out.push_str(&sig);
        out.push('\n');
    }

    out.push_str("Code:\n");
    out.push_str(input.normalized_text);
    out
}

/// The unit's first non-empty line, trimmed and capped — v1's `getSignature`.
fn signature(normalized_text: &str) -> Option<String> {
    let first = normalized_text
        .lines()
        .find(|l| !l.trim().is_empty())?
        .trim();
    if first.is_empty() {
        return None;
    }
    if first.chars().count() <= SIGNATURE_CAP_CHARS {
        return Some(first.to_string());
    }
    let cut: String = first.chars().take(SIGNATURE_CAP_CHARS).collect();
    Some(cut + "...")
}

/// The documentation block immediately preceding `span_start`, if any.
///
/// `floor` (the previous unit's end) bounds the backwards walk.
fn doc_block(source: &[u8], span_start: usize, floor: usize) -> Option<String> {
    let head = source.get(..span_start.min(source.len()))?;
    let text = std::str::from_utf8(head).ok()?;
    // Only the region after the previous unit is ours to read.
    let floor = floor.min(text.len());
    let region = text.get(floor..)?;

    let mut lines: Vec<&str> = region.lines().collect();
    // The unit starts mid-line only if the source has no newline before it; the
    // final element is then the partial line the unit begins on, never a comment.
    if !region.ends_with('\n') {
        lines.pop();
    }
    // Skip blank lines between the doc and the declaration (v1 does the same).
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    let last = lines.last()?.trim_end();

    if last.ends_with("*/") {
        // A `/** … */` block: walk back to its opening line.
        let start = lines
            .iter()
            .rposition(|l| l.trim_start().starts_with("/**"))?;
        let block = &lines[start..];
        return Some(join_trimmed(block));
    }

    // A contiguous run of `//` lines (covers `///` and `//!` too).
    let mut start = lines.len();
    while start > 0 && lines[start - 1].trim_start().starts_with("//") {
        start -= 1;
    }
    if start == lines.len() {
        return None;
    }
    Some(join_trimmed(&lines[start..]))
}

/// Join comment lines into one line, trimmed, so the envelope stays a compact
/// labelled record rather than reproducing the source's own layout.
fn join_trimmed(lines: &[&str]) -> String {
    lines
        .iter()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// One occurrence's `code_context` subject: its envelope and the subject hash
/// that keys it in `embedding_cache`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextSubject {
    /// The occurrence the envelope describes.
    pub occurrence_id: String,
    /// `H(subject/occurrence_context, CONTEXT_VERSION, envelope)`.
    pub subject_hash: String,
    /// The envelope text — what actually gets embedded.
    pub serialization: String,
}

/// Build the `code_context` subject for every occurrence of `generation_id`.
///
/// Reads the generation's occurrences (the same join FTS materialization uses),
/// resolves each unit's normalized text and its revision's exact bytes, and
/// renders the envelope. Revisions are read once each: a file's units all share
/// one, and `source_bytes` decompresses the whole revision per call.
///
/// An occurrence whose bytes or span cannot produce an envelope is **skipped**
/// rather than embedded from a partial input — a vector built from the wrong
/// text is worse than a missing one, which coverage will report honestly.
pub fn context_subjects_for_generation(
    conn: &rusqlite::Connection,
    generation_id: &str,
) -> rusqlite::Result<Vec<ContextSubject>> {
    let rows = super::occurrences_for_fts(conn, generation_id)?;

    // `previous_unit_end` is per file, in span order, so the doc walk of one
    // unit never reads through the unit above it.
    let mut by_path: HashMap<&str, Vec<&FtsSourceRow>> = HashMap::new();
    for row in &rows {
        by_path
            .entry(row.normalized_path.as_str())
            .or_default()
            .push(row);
    }
    for units in by_path.values_mut() {
        units.sort_by_key(|r| (r.span_start, r.span_end));
    }

    let mut bytes_by_revision: HashMap<&str, Option<Vec<u8>>> = HashMap::new();
    let mut out = Vec::with_capacity(rows.len());
    for units in by_path.values() {
        let mut previous_end = 0usize;
        for row in units {
            if !bytes_by_revision.contains_key(row.file_revision_id.as_str()) {
                let bytes = super::source_bytes(conn, &row.file_revision_id)?;
                bytes_by_revision.insert(row.file_revision_id.as_str(), bytes);
            }
            let Some(Some(source)) = bytes_by_revision.get(row.file_revision_id.as_str()) else {
                continue;
            };
            // Normalized text is recomputed from the very bytes the doc block
            // is read from, rather than read out of `normalized_text_cache`:
            // it keeps this reader dependent on `state.sqlite` alone, and it
            // removes any chance of the envelope's `Code:` disagreeing with the
            // span its `Doc:` was derived from.
            let Some(slice) = source.get(row.span_start as usize..row.span_end as usize) else {
                continue;
            };
            let Ok(unit_text) = std::str::from_utf8(slice) else {
                continue;
            };
            let text = derive_content_blob(&row.language, unit_text).normalized_text;
            let serialization = serialize(&ContextInput {
                normalized_path: &row.normalized_path,
                unit_kind: row.unit_kind,
                lang_kind: row.lang_kind.as_deref(),
                local_name: row.local_name.as_deref(),
                normalized_text: &text,
                source,
                span: (row.span_start as usize, row.span_end as usize),
                previous_unit_end: previous_end,
            });
            out.push(ContextSubject {
                occurrence_id: row.occurrence_id.clone(),
                subject_hash: subject_occurrence_context(CONTEXT_VERSION, serialization.as_bytes()),
                serialization,
            });
            previous_end = row.span_end.max(0) as usize;
        }
    }
    // Deterministic order, like every other reader in this module.
    out.sort_by(|a, b| a.occurrence_id.cmp(&b.occurrence_id));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input<'a>(source: &'a str, span: (usize, usize), text: &'a str) -> ContextInput<'a> {
        ContextInput {
            normalized_path: "src/indexer/embedder.ts",
            unit_kind: UnitKind::Symbol,
            lang_kind: Some("function"),
            local_name: Some("embedBatch"),
            normalized_text: text,
            source: source.as_bytes(),
            span,
            previous_unit_end: 0,
        }
    }

    // ---- the envelope ------------------------------------------------------

    #[test]
    fn the_envelope_carries_every_field_v1_embedded() {
        let source = "/** Embed a batch of texts. */\nfunction embedBatch(t) {\n  return t;\n}\n";
        let unit = "function embedBatch(t) {\n  return t;\n}";
        let span = (source.find("function").expect("present"), source.len() - 1);
        let out = serialize(&input(source, span, unit));

        assert_eq!(
            out,
            "File: src/indexer/embedder.ts\n\
             Type: symbol/function\n\
             Name: embedBatch\n\
             Doc: /** Embed a batch of texts. */\n\
             Sig: function embedBatch(t) {\n\
             Code:\n\
             function embedBatch(t) {\n  return t;\n}"
        );
    }

    /// Absent fields are omitted, not emitted empty — otherwise "undocumented"
    /// and "documented with nothing" look identical to the model.
    #[test]
    fn absent_fields_are_omitted_entirely() {
        let source = "function anon() {}\n";
        let mut i = input(source, (0, 18), "function anon() {}");
        i.local_name = None;
        i.lang_kind = None;
        let out = serialize(&i);

        assert!(!out.contains("Doc:"), "{out}");
        assert!(!out.contains("Name:"), "{out}");
        assert_eq!(out.lines().next(), Some("File: src/indexer/embedder.ts"));
        assert!(out.contains("Type: symbol\n"), "no lang kind ⇒ bare kind");
    }

    #[test]
    fn the_code_body_is_the_same_text_code_raw_embeds() {
        let unit = "const x = 1;";
        let out = serialize(&input("const x = 1;", (0, 12), unit));
        assert!(out.ends_with("Code:\nconst x = 1;"), "{out}");
    }

    // ---- signature ---------------------------------------------------------

    #[test]
    fn the_signature_is_the_first_line_trimmed() {
        assert_eq!(
            signature("  function f(a, b) {\n  body\n}").as_deref(),
            Some("function f(a, b) {")
        );
        assert_eq!(signature("\n\nclass A {}").as_deref(), Some("class A {}"));
        assert_eq!(signature("   \n  \n").as_deref(), None);
        assert_eq!(signature("").as_deref(), None);
    }

    #[test]
    fn a_long_signature_is_capped_with_an_ellipsis() {
        let long = "f".repeat(SIGNATURE_CAP_CHARS + 50);
        let sig = signature(&long).expect("some");
        assert_eq!(sig.chars().count(), SIGNATURE_CAP_CHARS + 3);
        assert!(sig.ends_with("..."));
    }

    // ---- doc extraction ----------------------------------------------------

    #[test]
    fn a_jsdoc_block_is_collected_whole() {
        let source = "/**\n * Embeds a batch.\n * @param t texts\n */\nfunction f() {}\n";
        let start = source.find("function").expect("present");
        assert_eq!(
            doc_block(source.as_bytes(), start, 0).as_deref(),
            Some("/** * Embeds a batch. * @param t texts */")
        );
    }

    #[test]
    fn a_run_of_line_comments_is_collected() {
        let source = "// first\n// second\nfn f() {}\n";
        let start = source.find("fn f").expect("present");
        assert_eq!(
            doc_block(source.as_bytes(), start, 0).as_deref(),
            Some("// first // second")
        );
    }

    /// Rust's `///` and `//!` are `//` runs, so they need no separate rule.
    #[test]
    fn rust_doc_comments_are_line_comment_runs() {
        let source = "/// Does a thing.\nfn f() {}\n";
        let start = source.find("fn f").expect("present");
        assert_eq!(
            doc_block(source.as_bytes(), start, 0).as_deref(),
            Some("/// Does a thing.")
        );
    }

    #[test]
    fn blank_lines_between_doc_and_declaration_are_skipped() {
        let source = "// doc\n\n\nfn f() {}\n";
        let start = source.find("fn f").expect("present");
        assert_eq!(
            doc_block(source.as_bytes(), start, 0).as_deref(),
            Some("// doc")
        );
    }

    #[test]
    fn a_declaration_with_no_preceding_comment_has_no_doc() {
        let source = "fn f() {}\n";
        assert_eq!(doc_block(source.as_bytes(), 0, 0), None);

        let source = "const a = 1;\n\nfn f() {}\n";
        let start = source.find("fn f").expect("present");
        assert_eq!(doc_block(source.as_bytes(), start, 0), None);
    }

    /// What the floor actually guarantees: the backwards walk never reads
    /// *through* the previous unit. A comment run that begins above the previous
    /// declaration is cut at its end, so one unit's documentation can never leak
    /// into the next.
    #[test]
    fn the_walk_stops_at_the_previous_units_end() {
        //            0        9
        let source = "// for a\nfn a() {}\n// for b\nfn b() {}\n";
        let previous_end = source.find("fn b").expect("present") - "// for b\n".len();
        let start = source.find("fn b").expect("present");

        assert_eq!(
            doc_block(source.as_bytes(), start, previous_end).as_deref(),
            Some("// for b"),
            "only the comment below the previous unit is this unit's"
        );
        // Without the floor the walk would run past `fn a() {}`… it does not,
        // because a non-comment line terminates the run — but a floor of 0 also
        // exposes the run to anything above, which is what the floor removes.
        assert_eq!(
            doc_block(source.as_bytes(), start, 0).as_deref(),
            Some("// for b"),
            "a declaration between the two comment runs already terminates the walk"
        );
    }

    /// The ambiguity this deliberately does **not** resolve, stated rather than
    /// hidden: a comment on the line after a declaration reads as documentation
    /// for the *next* one. v1 behaved identically, and disambiguating it needs
    /// the previous unit's own end line, which the envelope's caller supplies
    /// only as a byte floor.
    #[test]
    fn a_trailing_comment_is_attributed_to_the_following_unit() {
        let source = "fn a() {}\n// ambiguous\n\nfn b() {}\n";
        let previous_end = source.find('\n').expect("newline") + 1;
        let start = source.find("fn b").expect("present");
        assert_eq!(
            doc_block(source.as_bytes(), start, previous_end).as_deref(),
            Some("// ambiguous"),
            "documented as a known limitation, not a silent surprise"
        );
    }

    #[test]
    fn a_span_start_beyond_the_source_yields_no_doc() {
        assert_eq!(doc_block(b"short", 999, 0), None);
    }

    #[test]
    fn non_utf8_source_yields_no_doc_rather_than_panicking() {
        assert_eq!(doc_block(&[0xFF, 0xFE, 0x41], 3, 0), None);
    }

    /// The envelope is a pure function of its input — the property the subject
    /// hash depends on.
    #[test]
    fn serialization_is_deterministic() {
        let source = "/** doc */\nfn f() {}\n";
        let i = input(source, (11, 20), "fn f() {}");
        let first = serialize(&i);
        for _ in 0..5 {
            assert_eq!(serialize(&i), first);
        }
    }
}
