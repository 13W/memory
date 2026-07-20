//! The language-agnostic **parse-output contract** (spec 03 §2.3–2.4; 06 §2.1;
//! ADR-0002).
//!
//! This is the pure, DB-free product of parsing exact source bytes — the T04-03
//! boundary. A [`crate::parse::LanguageParser::parse`] call turns
//! `source: &[u8]` (the exact `source_blob`) into a [`ParseOutput`] and touches
//! no database, no id source, and no clock. Persistence — minting `unit_id`,
//! deriving each unit's `blob_id`, atomic create/reuse, dedup — is T04-06.
//!
//! ## Two byte worlds
//!
//! Byte spans ([`ByteSpan`]) index the **exact source bytes** (tree-sitter's
//! `start_byte`/`end_byte`), never normalized text. A unit's `blob_id` (assigned
//! later, T04-06) hashes the **normalized** text of `source_blob[span]`. The two
//! are intentionally different and must never be conflated (spec 03 §2.3.1: spans
//! address `source_blob`; §4.2: `blob_id` is over normalized text).
//!
//! ## Canonical order `[SPEC]` (ADR-0002)
//!
//! [`ParseOutput::units`] is always in a canonical, insertion-order-independent
//! order: ascending `span.start`, then descending `span.end` (an enclosing unit
//! sorts before the units it contains), then `unit_kind`, then `lang_kind`, then
//! `local_name`. Because a parent encloses its children and starts no later, this
//! guarantees `parent < child` by index — the property T04-06 relies on for
//! deterministic persistence.

use crate::parse::language::LanguageId;
use crate::parse::locator::{SyntaxAnchor, SyntaxLocatorDraft};
use local_rag_store::code::UnitKind;

/// A byte span into the exact source bytes: `[start, end)` (spec 03 §2.3
/// `parsed_unit.span_start`/`span_end`). `end >= start` always.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ByteSpan {
    /// Inclusive start byte offset.
    pub start: u32,
    /// Exclusive end byte offset (`>= start`).
    pub end: u32,
}

impl ByteSpan {
    /// Construct a span, asserting `end >= start` in debug builds.
    pub fn new(start: u32, end: u32) -> Self {
        debug_assert!(end >= start, "byte span end {end} < start {start}");
        Self { start, end }
    }

    /// The span length in bytes.
    pub fn len(self) -> u32 {
        self.end - self.start
    }

    /// Whether the span is empty (`start == end`), e.g. the file unit of an empty
    /// file.
    pub fn is_empty(self) -> bool {
        self.end == self.start
    }

    /// Whether `self` strictly or equally encloses `other` (used for parent
    /// inference). A unit is considered to enclose another if it starts no later
    /// and ends no earlier.
    pub fn encloses(self, other: ByteSpan) -> bool {
        self.start <= other.start && self.end >= other.end
    }
}

/// The kind of an unresolved reference (`unresolved_reference.reference_kind`).
///
/// v0 covers only module specifiers (import resolution / the dependency graph is
/// post-v0 per ADR-0001); the enum is extensible for later reference kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReferenceKind {
    /// A value `import … from "X"` (or bare `import "X"`).
    Import,
    /// A type-only `import type … from "X"`.
    TypeImport,
    /// A re-export `export … from "X"` / `export * from "X"`.
    Reexport,
}

impl ReferenceKind {
    /// The stored `unresolved_reference.reference_kind` value.
    pub fn as_str(self) -> &'static str {
        match self {
            ReferenceKind::Import => "import",
            ReferenceKind::TypeImport => "type_import",
            ReferenceKind::Reexport => "reexport",
        }
    }

    /// Parse a stored/fixture value back into a [`ReferenceKind`].
    pub fn from_str_value(value: &str) -> Option<ReferenceKind> {
        match value {
            "import" => Some(ReferenceKind::Import),
            "type_import" => Some(ReferenceKind::TypeImport),
            "reexport" => Some(ReferenceKind::Reexport),
            _ => None,
        }
    }
}

/// One parser-produced unit, **without** the persistence-assigned `unit_id`,
/// `file_revision_id`, or `blob_id` (those are T04-06).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedUnitDraft {
    /// The schema-level unit kind (`parsed_unit.unit_kind`).
    pub unit_kind: UnitKind,
    /// Byte span into the exact source (`span_start`/`span_end`).
    pub span: ByteSpan,
    /// The unit's local (unqualified) name, if it has a safe one
    /// (`parsed_unit.local_name`).
    pub local_name: Option<String>,
    /// The language-level kind label (`parsed_unit.kind`, e.g. `function`,
    /// `class`, `interface`).
    pub lang_kind: Option<String>,
    /// The path-free structural anchor of this unit's [`SyntaxLocatorDraft`].
    pub anchor: SyntaxAnchor,
    /// The `sig` field of this unit's [`SyntaxLocatorDraft`] (ADR-0002).
    pub signature_fingerprint: String,
    /// Index into [`ParseOutput::units`] of the enclosing unit, or `None` for a
    /// top-level unit / the file unit. Always `< self`'s index (canonical order).
    pub parent: Option<usize>,
}

impl ParsedUnitDraft {
    /// Assemble the path-free, blob-free [`SyntaxLocatorDraft`] for this unit.
    /// T04-06 completes it into a full `SyntaxLocator` with the derived `blob_id`.
    pub fn locator_draft(&self, language: LanguageId) -> SyntaxLocatorDraft {
        SyntaxLocatorDraft {
            language,
            anchor: self.anchor.clone(),
            signature_fingerprint: self.signature_fingerprint.clone(),
        }
    }
}

/// An unresolved reference emitted by the parser (`unresolved_reference`), parse-
/// local per file revision (spec 03 §2.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnresolvedRef {
    /// Index into [`ParseOutput::units`] of the unit the reference originates
    /// from (mapped to `unresolved_reference.source_unit_id` at persistence).
    pub source_unit: usize,
    /// The referenced module specifier text, unquoted (`reference_text`).
    pub reference_text: String,
    /// The kind of reference (`reference_kind`).
    pub reference_kind: ReferenceKind,
}

/// The complete, deterministic product of one parse (spec 06 §2.1: same
/// `(content, parser_fingerprint)` ⇒ byte-identical output).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseOutput {
    /// The parsed units in canonical order (see the module docs).
    pub units: Vec<ParsedUnitDraft>,
    /// The unresolved references, ordered by source position.
    pub unresolved: Vec<UnresolvedRef>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_span_helpers() {
        let s = ByteSpan::new(3, 7);
        assert_eq!(s.len(), 4);
        assert!(!s.is_empty());
        assert!(ByteSpan::new(5, 5).is_empty());
        assert!(ByteSpan::new(0, 10).encloses(ByteSpan::new(2, 8)));
        assert!(ByteSpan::new(0, 10).encloses(ByteSpan::new(0, 10)));
        assert!(!ByteSpan::new(2, 8).encloses(ByteSpan::new(0, 10)));
        assert!(!ByteSpan::new(0, 5).encloses(ByteSpan::new(3, 8)));
    }

    #[test]
    fn reference_kind_round_trips() {
        for k in [
            ReferenceKind::Import,
            ReferenceKind::TypeImport,
            ReferenceKind::Reexport,
        ] {
            assert_eq!(ReferenceKind::from_str_value(k.as_str()), Some(k));
        }
        assert_eq!(ReferenceKind::from_str_value("bogus"), None);
    }
}
