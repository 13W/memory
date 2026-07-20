//! The language-agnostic parser seam (spec 03 §2.3.1, §2.4; 06 §2.1; ADR-0001,
//! ADR-0002).
//!
//! [`LanguageParser`] is the abstraction the per-language tree-sitter adapters of
//! T04-03/04/05 implement. Honoring ADR-0001 ("parser core stays language-
//! agnostic: the choice lives in data/config, not in the parser abstraction"), the
//! trait names no concrete language and the fingerprint comes from the shared
//! descriptor table, not per-adapter code.
//!
//! The parse **output** contract — [`ParseOutput`], byte spans, parents,
//! unresolved references — is defined in [`crate::parse::output`] (T04-03).

use crate::parse::fingerprint;
use crate::parse::language::LanguageId;
use crate::parse::output::ParseOutput;

/// A per-language parser adapter.
pub trait LanguageParser {
    /// The language this parser handles.
    fn language(&self) -> LanguageId;

    /// Parse exact source bytes into a deterministic, DB-free [`ParseOutput`]
    /// (spec 06 §2.1: same `(content, parser_fingerprint)` ⇒ byte-identical
    /// output). `source` is the exact `source_blob` and is guaranteed valid UTF-8
    /// (the classifier skips non-UTF-8 as `encoding`, spec 06 §2.2). The adapter
    /// mints no ids and touches no database — persistence is T04-06.
    fn parse(&self, source: &[u8]) -> ParseOutput;

    /// The `parser_fingerprint` keying every `file_revision` this parser produces
    /// (spec 03 §2.3.1).
    ///
    /// The default wires to the shared descriptor table so the fingerprint is
    /// derived from data/config, not re-implemented per adapter. An adapter should
    /// not normally override it.
    fn parser_fingerprint(&self) -> String {
        fingerprint::parser_fingerprint(self.language())
    }
}
