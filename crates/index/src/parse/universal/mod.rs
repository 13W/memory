//! The language-agnostic indexing path — `config_section | text_section |
//! fallback_chunk` (spec 06 §2.1 `[FIXED]`, ADR-0012; D-098).
//!
//! # What this closes
//!
//! spec 06 §2.1 has always declared, `[FIXED]`, that **all five unit kinds are
//! indexed** — a v1 parity requirement. Three of them had no producer. ADR-0001's
//! own "Scope boundary" pointed the universal path at "T04-06 / groups 05 and 08";
//! T04-06 turned out to be deterministic persistence, and groups 05 and 08 both
//! closed `PASS` having deferred it by design, so the pointer dangled and no card
//! owned the requirement. What that cost, measured on the owner's store: 3455 of
//! firefly's files and 246 of this repository's own were in neither
//! `generation_file` nor `skipped_file` — including all 119 `.md` files, so the
//! product's entire specification was absent from its own code search.
//!
//! # The shape of the answer
//!
//! [`chunk`] is a pure function of the exact bytes: same input, byte-identical
//! [`ParseOutput`] (spec 06 §2.1's determinism gate, 14 §5). It takes on **no
//! dependency** — no YAML, JSON or Markdown parser — for two reasons. The
//! obvious one is the T10 guardrail. The load-bearing one is that a real parser
//! does not give byte spans into the original text, and spec 03 §2.3.1 requires
//! spans that address the exact `source_blob`; a chunker that re-serializes its
//! input cannot satisfy that. Line-oriented scanning can, exactly.
//!
//! Every file yields a [`UnitKind::File`] unit spanning the whole content — the
//! same shape the tree-sitter adapters produce — plus zero or more sections
//! parented to it. A blank or empty file is the file unit alone; nothing here can
//! produce zero units, so no accepted file is ever indexed with no occurrences.
//!
//! # Why the anchors are what they are
//!
//! A [`SyntaxAnchor`] must be path-free (ADR-0002, spec 01 §5.1) and stable under
//! edits elsewhere in the file. Headings and top-level keys satisfy both: they are
//! structural routes inside the content, they survive an unrelated edit, and they
//! are what a reader would cite. A fallback chunk has no such structure, so it
//! gets an ordinal — which is honest about being positional, and is exactly what
//! [`SyntaxAnchor::LocalOrdinal`] exists for.

use local_rag_store::code::UnitKind;

use crate::parse::language::UniversalKind;
use crate::parse::locator::SyntaxAnchor;
use crate::parse::output::{ByteSpan, ParseOutput, ParsedUnitDraft};
use crate::parse::signature::SignatureDescriptor;

/// The largest section this path emits, in bytes of exact source.
///
/// A section longer than this is split into consecutive parts on line boundaries.
/// Chosen, not derived: it is the same order of magnitude as the largest units the
/// tree-sitter adapters produce for real code, so the universal path does not hand
/// the embedder inputs of a different scale than the language path does. A single
/// line longer than the cap is its own section — splitting mid-line would put a
/// span boundary inside a token for no benefit.
///
/// Changing it moves unit boundaries, so it is gated by
/// [`UNIVERSAL_POLICY_VERSION`](crate::parse::fingerprint::UNIVERSAL_POLICY_VERSION)
/// exactly as a grammar change is: a bump, then a rebuild.
pub const MAX_SECTION_BYTES: usize = 2048;

/// Chunk `source` under `kind` (spec 06 §2.1; ADR-0012).
///
/// `source` is the exact `source_blob` and is guaranteed valid UTF-8 by the
/// classifier (spec 06 §2.2 rejects anything else as `encoding`). Invalid UTF-8 is
/// still handled rather than trusted: it yields the file unit alone, because a
/// chunker is not the right place to discover a classifier bug by panicking.
pub fn chunk(kind: UniversalKind, source: &[u8]) -> ParseOutput {
    let file_unit = file_draft(kind, source.len());
    let Ok(text) = std::str::from_utf8(source) else {
        return ParseOutput {
            units: vec![file_unit],
            unresolved: Vec::new(),
        };
    };

    let named = match kind {
        UniversalKind::Config => config_sections(text),
        UniversalKind::Text => text_sections(text),
        UniversalKind::Fallback => Vec::new(),
    };
    let sections = split_oversized(text, named);

    let mut units = Vec::with_capacity(sections.len() + 1);
    units.push(file_unit);
    let mut seen: Vec<(String, usize)> = Vec::new();
    for (ordinal, section) in sections.into_iter().enumerate() {
        units.push(section_draft(kind, section, ordinal, &mut seen));
    }
    ParseOutput {
        units,
        unresolved: Vec::new(),
    }
}

/// One candidate section: its byte span and the structural name it was found by,
/// if any.
struct Section {
    start: usize,
    end: usize,
    name: Option<String>,
}

/// The whole-file unit every universal file gets.
fn file_draft(kind: UniversalKind, len: usize) -> ParsedUnitDraft {
    let mut descriptor = SignatureDescriptor::new(kind.as_str(), UnitKind::File.as_str(), "file");
    descriptor.push("");
    ParsedUnitDraft {
        unit_kind: UnitKind::File,
        span: ByteSpan::new(0, len as u32),
        local_name: None,
        lang_kind: None,
        anchor: SyntaxAnchor::Path("file".to_string()),
        signature_fingerprint: descriptor.fingerprint(),
        parent: None,
    }
}

/// One section unit, parented to the file unit (index 0).
///
/// `seen` carries the names already emitted so a repeated name (two `## Usage`
/// headings, two `name:` keys at the same level) becomes `Usage#2` rather than a
/// duplicate anchor. Duplicate anchors would not be a correctness bug — the
/// `parsed_unit` unique key also carries the span — but they would make two
/// distinct sections indistinguishable in a locator, which is the one thing a
/// locator exists to prevent.
fn section_draft(
    kind: UniversalKind,
    section: Section,
    ordinal: usize,
    seen: &mut Vec<(String, usize)>,
) -> ParsedUnitDraft {
    let unit_kind = kind.unit_kind();
    let lang_kind = match kind {
        UniversalKind::Config => "key",
        UniversalKind::Text => "heading",
        UniversalKind::Fallback => "chunk",
    };

    let anchor = match &section.name {
        Some(name) => {
            let count = match seen.iter_mut().find(|(n, _)| n == name) {
                Some((_, c)) => {
                    *c += 1;
                    *c
                }
                None => {
                    seen.push((name.clone(), 1));
                    1
                }
            };
            SyntaxAnchor::Path(if count == 1 {
                name.clone()
            } else {
                format!("{name}#{count}")
            })
        }
        None => SyntaxAnchor::LocalOrdinal(ordinal as u32),
    };

    let mut descriptor = SignatureDescriptor::new(kind.as_str(), unit_kind.as_str(), lang_kind);
    descriptor.push(section.name.as_deref().unwrap_or(""));
    match &anchor {
        SyntaxAnchor::Path(p) => descriptor.push(p),
        SyntaxAnchor::LocalOrdinal(o) => descriptor.push(o.to_string()),
    }

    ParsedUnitDraft {
        unit_kind,
        span: ByteSpan::new(section.start as u32, section.end as u32),
        local_name: section.name,
        lang_kind: Some(lang_kind.to_string()),
        anchor,
        signature_fingerprint: descriptor.fingerprint(),
        parent: Some(0),
    }
}

/// `(byte offset, line including its newline)` for every line of `text`.
fn lines_with_offsets(text: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut offset = 0usize;
    for line in text.split_inclusive('\n') {
        out.push((offset, line));
        offset += line.len();
    }
    out
}

/// Sections of a prose file: one per ATX heading, plus any preamble before the
/// first heading.
///
/// The name is the heading trail (`Install/From source`), built from the enclosing
/// heading stack, so a section is identified the way a reader would cite it and a
/// nested heading is not confused with a top-level one of the same text.
fn text_sections(text: &str) -> Vec<Section> {
    let lines = lines_with_offsets(text);
    let mut starts: Vec<(usize, String)> = Vec::new();
    let mut stack: Vec<(usize, String)> = Vec::new();

    for (offset, line) in &lines {
        let Some((level, title)) = atx_heading(line) else {
            continue;
        };
        stack.retain(|(l, _)| *l < level);
        stack.push((level, title));
        let trail = stack
            .iter()
            .map(|(_, t)| t.as_str())
            .collect::<Vec<_>>()
            .join("/");
        starts.push((*offset, trail));
    }

    spans_from_starts(text, starts)
}

/// The `(level, title)` of an ATX heading line, or `None`.
///
/// Requires whitespace after the `#` run, which is what separates a heading from
/// a `#!/bin/sh` shebang or a `#region` marker.
fn atx_heading(line: &str) -> Option<(usize, String)> {
    let trimmed = line.trim_start();
    let hashes = trimmed.chars().take_while(|c| *c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &trimmed[hashes..];
    if !rest.starts_with([' ', '\t']) {
        return None;
    }
    let title = rest.trim().trim_end_matches('#').trim();
    (!title.is_empty()).then(|| (hashes, title.to_string()))
}

/// Sections of a structured-config file: one per top-level key.
///
/// "Top level" is the **minimum indentation over the file's own key lines**, not
/// column zero. That one choice is what makes a single rule serve YAML (keys at
/// column 0), pretty-printed JSON (keys at column 2 inside the root object), and
/// an INI/`.env` file alike, without a per-format parser — the file states its own
/// top level by where its keys sit.
fn config_sections(text: &str) -> Vec<Section> {
    let lines = lines_with_offsets(text);
    let keys: Vec<(usize, usize, String)> = lines
        .iter()
        .filter_map(|(offset, line)| config_key(line).map(|(ind, k)| (*offset, ind, k)))
        .collect();
    let Some(top) = keys.iter().map(|(_, ind, _)| *ind).min() else {
        return Vec::new();
    };
    let starts: Vec<(usize, String)> = keys
        .into_iter()
        .filter(|(_, ind, _)| *ind == top)
        .map(|(offset, _, key)| (offset, key))
        .collect();
    spans_from_starts(text, starts)
}

/// The `(indent, key)` of a `key:`/`key =` line, or `None`.
fn config_key(line: &str) -> Option<(usize, String)> {
    let body = line.trim_end_matches(['\n', '\r']);
    let indent = body.len() - body.trim_start().len();
    let mut rest = body.trim_start();
    if rest.starts_with(['#', ';']) {
        return None;
    }
    let quote = rest.starts_with(['"', '\'']).then(|| {
        let q = rest.as_bytes()[0];
        rest = &rest[1..];
        q
    });
    let key_len = rest
        .find(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '$' | '/')))
        .unwrap_or(rest.len());
    if key_len == 0 {
        return None;
    }
    let key = &rest[..key_len];
    let mut after = &rest[key_len..];
    if let Some(q) = quote {
        if !after.starts_with(q as char) {
            return None;
        }
        after = &after[1..];
    }
    let after = after.trim_start();
    (after.starts_with(':') || after.starts_with('=')).then(|| (indent, key.to_string()))
}

/// Turn section start offsets into spans covering the whole file.
///
/// Content before the first start becomes an unnamed preamble section (a YAML
/// document marker, a licence header above the first heading) unless it is only
/// whitespace. Nothing is dropped: the sections together tile the file.
fn spans_from_starts(text: &str, starts: Vec<(usize, String)>) -> Vec<Section> {
    if starts.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(starts.len() + 1);
    if starts[0].0 > 0 && !text[..starts[0].0].trim().is_empty() {
        out.push(Section {
            start: 0,
            end: starts[0].0,
            name: None,
        });
    }
    for (i, (offset, name)) in starts.iter().enumerate() {
        let end = starts.get(i + 1).map_or(text.len(), |(next, _)| *next);
        out.push(Section {
            start: *offset,
            end,
            name: Some(name.clone()),
        });
    }
    out
}

/// Enforce [`MAX_SECTION_BYTES`] on line boundaries, and produce the fallback
/// windowing when `sections` is empty.
///
/// One function for both because they are the same operation: "cover this range
/// with line-aligned pieces no larger than the cap". A whitespace-only piece is
/// dropped — it would carry no searchable text and would still cost an embedding.
fn split_oversized(text: &str, sections: Vec<Section>) -> Vec<Section> {
    let ranges = if sections.is_empty() {
        vec![Section {
            start: 0,
            end: text.len(),
            name: None,
        }]
    } else {
        sections
    };

    let mut out = Vec::new();
    for section in ranges {
        if section.end <= section.start {
            continue;
        }
        let body = &text[section.start..section.end];
        if body.trim().is_empty() {
            continue;
        }
        if body.len() <= MAX_SECTION_BYTES {
            out.push(section);
            continue;
        }
        let mut part_start = section.start;
        let mut cursor = section.start;
        for (offset, line) in lines_with_offsets(body) {
            let line_start = section.start + offset;
            let line_end = line_start + line.len();
            if line_end - part_start > MAX_SECTION_BYTES && cursor > part_start {
                out.push(Section {
                    start: part_start,
                    end: cursor,
                    name: section.name.clone(),
                });
                part_start = cursor;
            }
            cursor = line_end;
        }
        if cursor > part_start {
            out.push(Section {
                start: part_start,
                end: cursor,
                name: section.name.clone(),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(out: &ParseOutput) -> Vec<UnitKind> {
        out.units.iter().map(|u| u.unit_kind).collect()
    }

    fn anchors(out: &ParseOutput) -> Vec<String> {
        out.units
            .iter()
            .map(|u| match &u.anchor {
                SyntaxAnchor::Path(p) => p.clone(),
                SyntaxAnchor::LocalOrdinal(o) => format!("#{o}"),
            })
            .collect()
    }

    /// Every span must address the exact bytes it was cut from — the invariant
    /// spec 03 §2.3.1 states and the whole reason this chunker is line-oriented
    /// rather than parser-based.
    fn assert_spans_address_the_source(out: &ParseOutput, source: &str) {
        for u in &out.units {
            let (s, e) = (u.span.start as usize, u.span.end as usize);
            assert!(e <= source.len(), "span past the end: {:?}", u.span);
            assert!(
                source.is_char_boundary(s) && source.is_char_boundary(e),
                "span is not on char boundaries: {:?}",
                u.span,
            );
        }
        let file = &out.units[0];
        assert_eq!(file.unit_kind, UnitKind::File);
        assert_eq!(file.span, ByteSpan::new(0, source.len() as u32));
    }

    #[test]
    fn an_empty_file_is_the_file_unit_alone() {
        for kind in UniversalKind::ALL {
            let out = chunk(kind, b"");
            assert_eq!(kinds(&out), vec![UnitKind::File]);
            assert_eq!(out.units[0].span, ByteSpan::new(0, 0));
            assert!(out.unresolved.is_empty());
        }
    }

    #[test]
    fn a_whitespace_only_file_produces_no_sections() {
        for kind in UniversalKind::ALL {
            // Never zero units: the file unit always exists, so an accepted file
            // can never be indexed with nothing searchable in it.
            assert_eq!(kinds(&chunk(kind, b"\n\n   \n")), vec![UnitKind::File]);
        }
    }

    #[test]
    fn markdown_sections_follow_the_heading_trail() {
        let src = "intro\n\n# Install\ntext\n\n## From source\nmore\n\n# Usage\nu\n";
        let out = chunk(UniversalKind::Text, src.as_bytes());
        assert_eq!(
            kinds(&out),
            vec![
                UnitKind::File,
                UnitKind::TextSection, // preamble
                UnitKind::TextSection,
                UnitKind::TextSection,
                UnitKind::TextSection,
            ]
        );
        // The trail, not the bare heading: a nested section is not confused with a
        // top-level one of the same text.
        assert_eq!(
            anchors(&out),
            vec![
                "file".to_string(),
                "#0".to_string(),
                "Install".to_string(),
                "Install/From source".to_string(),
                "Usage".to_string(),
            ]
        );
        assert_spans_address_the_source(&out, src);

        // The sections tile the file: no byte of content is dropped.
        let covered: usize = out.units[1..].iter().map(|u| u.span.len() as usize).sum();
        assert_eq!(covered, src.len());
    }

    #[test]
    fn a_hash_without_whitespace_is_not_a_heading() {
        // A shebang and a `#region` marker are not headings. With no heading in
        // the file there is no *named* section — but the content is still covered,
        // by an ordinal-anchored window. "Not a heading" must never mean "not
        // indexed"; that conflation is the whole of D-098.
        let src = "#!/bin/sh\n#region setup\necho hi\n";
        let out = chunk(UniversalKind::Text, src.as_bytes());
        assert_eq!(kinds(&out), vec![UnitKind::File, UnitKind::TextSection]);
        assert_eq!(anchors(&out), vec!["file".to_string(), "#0".to_string()]);
        assert_eq!(out.units[1].span, ByteSpan::new(0, src.len() as u32));
    }

    #[test]
    fn yaml_sections_are_its_column_zero_keys() {
        let src = "image: x\nports:\n  - 80\n  - 443\nenv:\n  A: 1\n";
        let out = chunk(UniversalKind::Config, src.as_bytes());
        assert_eq!(
            anchors(&out),
            vec![
                "file".to_string(),
                "image".to_string(),
                "ports".to_string(),
                "env".to_string(),
            ]
        );
        // The nested `A: 1` is part of `env`, not a section of its own.
        assert_eq!(out.units[3].unit_kind, UnitKind::ConfigSection);
        assert_spans_address_the_source(&out, src);
    }

    #[test]
    fn json_sections_are_the_keys_of_the_root_object() {
        // The rule that makes one implementation serve YAML and JSON alike: "top
        // level" is the minimum indentation of the file's own key lines, so a
        // pretty-printed root object at indent 2 states its own top level.
        let src = "{\n  \"name\": \"x\",\n  \"scripts\": {\n    \"build\": \"tsc\"\n  }\n}\n";
        let out = chunk(UniversalKind::Config, src.as_bytes());
        assert_eq!(
            anchors(&out),
            vec![
                "file".to_string(),
                "#0".to_string(), // the opening brace
                "name".to_string(),
                "scripts".to_string(),
            ]
        );
        assert!(
            !anchors(&out).contains(&"build".to_string()),
            "a nested key is not a top-level section",
        );
        assert_spans_address_the_source(&out, src);
    }

    #[test]
    fn a_config_comment_is_not_a_key() {
        let src = "# not: a key\n; nor = this\nreal: 1\n";
        let out = chunk(UniversalKind::Config, src.as_bytes());
        assert_eq!(
            anchors(&out),
            vec!["file".to_string(), "#0".to_string(), "real".to_string()],
        );
    }

    #[test]
    fn a_config_file_with_no_keys_falls_back_to_windows() {
        // No key lines at all: the file is still covered, by size-bounded windows
        // under its own kind. Nothing is left unchunked.
        let src = "just\nsome\nlines\n";
        let out = chunk(UniversalKind::Config, src.as_bytes());
        assert_eq!(kinds(&out), vec![UnitKind::File, UnitKind::ConfigSection]);
        assert_eq!(anchors(&out), vec!["file".to_string(), "#0".to_string()]);
    }

    #[test]
    fn repeated_names_get_distinct_anchors() {
        let src = "# Notes\na\n# Notes\nb\n# Notes\nc\n";
        let out = chunk(UniversalKind::Text, src.as_bytes());
        assert_eq!(
            anchors(&out),
            vec![
                "file".to_string(),
                "Notes".to_string(),
                "Notes#2".to_string(),
                "Notes#3".to_string(),
            ],
            "two sections must never share a locator anchor",
        );
    }

    #[test]
    fn oversized_sections_are_split_on_line_boundaries() {
        let line = "x".repeat(100);
        let body: String = std::iter::repeat_n(line.as_str(), 60)
            .map(|l| format!("{l}\n"))
            .collect();
        let src = format!("# Big\n{body}");
        let out = chunk(UniversalKind::Text, src.as_bytes());

        let sections = &out.units[1..];
        assert!(sections.len() > 1, "one 6 KB section must be split");
        for u in sections {
            assert!(
                u.span.len() as usize <= MAX_SECTION_BYTES,
                "a part exceeds the cap: {}",
                u.span.len(),
            );
            // Every part starts at a line boundary.
            let start = u.span.start as usize;
            assert!(
                start == 0 || src.as_bytes()[start - 1] == b'\n',
                "part starts mid-line at {start}",
            );
        }
        // The parts still tile the section, and carry its name.
        assert_eq!(
            sections
                .iter()
                .map(|u| u.span.len() as usize)
                .sum::<usize>(),
            src.len(),
        );
        assert!(anchors(&out).contains(&"Big".to_string()));
        assert!(anchors(&out).contains(&"Big#2".to_string()));
    }

    #[test]
    fn a_single_line_longer_than_the_cap_is_its_own_section() {
        // Splitting mid-line would put a span boundary inside a token for no
        // benefit, so the cap yields to the line.
        let src = format!("{}\n", "y".repeat(MAX_SECTION_BYTES * 2));
        let out = chunk(UniversalKind::Fallback, src.as_bytes());
        assert_eq!(kinds(&out), vec![UnitKind::File, UnitKind::FallbackChunk]);
        assert_eq!(out.units[1].span.len() as usize, src.len());
    }

    #[test]
    fn fallback_windows_are_ordinal_anchored_and_cover_everything() {
        // Enough lines to exceed MAX_SECTION_BYTES several times over: ~9 bytes a
        // line means 200 lines is still one window, which is what the first
        // version of this test got wrong.
        let src: String = (0..2000).map(|i| format!("line {i}\n")).collect();
        assert!(src.len() > MAX_SECTION_BYTES * 3);
        let out = chunk(UniversalKind::Fallback, src.as_bytes());
        let sections = &out.units[1..];
        assert!(sections.len() > 1);
        for (i, u) in sections.iter().enumerate() {
            assert_eq!(u.anchor, SyntaxAnchor::LocalOrdinal(i as u32));
            assert_eq!(u.unit_kind, UnitKind::FallbackChunk);
            assert_eq!(u.parent, Some(0));
        }
        assert_eq!(
            sections
                .iter()
                .map(|u| u.span.len() as usize)
                .sum::<usize>(),
            src.len(),
        );
        assert_spans_address_the_source(&out, &src);
    }

    #[test]
    fn output_is_byte_identical_on_re_chunking() {
        // The determinism gate (spec 06 §2.1, 14 §5), applied to this path.
        let cases: &[(UniversalKind, &str)] = &[
            (UniversalKind::Config, "a: 1\nb:\n  c: 2\n"),
            (UniversalKind::Text, "# H\ntext\n## H2\nmore\n"),
            (UniversalKind::Fallback, "query Q { a b c }\n"),
            (UniversalKind::Text, "тема\n# Заголовок\nтекст\n"),
        ];
        for (kind, src) in cases {
            let first = chunk(*kind, src.as_bytes());
            for _ in 0..8 {
                assert_eq!(chunk(*kind, src.as_bytes()), first, "{src:?}");
            }
        }
    }

    #[test]
    fn multibyte_content_keeps_spans_on_char_boundaries() {
        let src = "# Заголовок\n\nтекст с юникодом — тире\n\n## Подраздел\nещё\n";
        let out = chunk(UniversalKind::Text, src.as_bytes());
        assert_spans_address_the_source(&out, src);
        assert!(anchors(&out).contains(&"Заголовок".to_string()));
        assert!(anchors(&out).contains(&"Заголовок/Подраздел".to_string()));
    }

    #[test]
    fn invalid_utf8_yields_the_file_unit_rather_than_a_panic() {
        // The classifier guarantees UTF-8; a chunker is not the place to discover
        // a classifier bug by panicking.
        let out = chunk(UniversalKind::Text, &[0xff, 0xfe, 0x00]);
        assert_eq!(kinds(&out), vec![UnitKind::File]);
    }

    #[test]
    fn every_section_is_parented_to_the_file_unit_in_canonical_order() {
        let src = "# A\nx\n# B\ny\n";
        let out = chunk(UniversalKind::Text, src.as_bytes());
        for (i, u) in out.units.iter().enumerate() {
            match u.parent {
                None => assert_eq!(i, 0, "only the file unit is parentless"),
                Some(p) => assert!(p < i, "parent index must precede its child"),
            }
        }
        // Ascending start, and the file unit first — the canonical order T04-06
        // relies on for deterministic persistence.
        let starts: Vec<u32> = out.units[1..].iter().map(|u| u.span.start).collect();
        assert!(starts.windows(2).all(|w| w[0] <= w[1]));
    }
}
