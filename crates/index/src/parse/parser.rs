//! The language-agnostic parser seam (spec 03 §2.3.1; ADR-0001).
//!
//! [`LanguageParser`] is the abstraction the per-language tree-sitter adapters of
//! T04-03/04/05 implement. Honoring ADR-0001 ("parser core stays language-
//! agnostic: the choice lives in data/config, not in the parser abstraction"), the
//! trait names no concrete language and the fingerprint comes from the shared
//! descriptor table, not per-adapter code.
//!
//! The parse **output** contract — unit byte spans, parents, unresolved
//! references — is intentionally NOT defined here; that is T04-03 scope. This task
//! fixes only the identity seam every adapter shares.

use crate::parse::fingerprint;
use crate::parse::language::LanguageId;

/// A per-language parser adapter (implemented in T04-03+).
pub trait LanguageParser {
    /// The language this parser handles.
    fn language(&self) -> LanguageId;

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

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal stand-in that exercises the trait and its default method until the
    /// real tree-sitter adapters land (T04-03+).
    struct StubTypeScript;

    impl LanguageParser for StubTypeScript {
        fn language(&self) -> LanguageId {
            LanguageId::TypeScript
        }
    }

    #[test]
    fn language_parser_reports_language_and_default_fingerprint() {
        let parser = StubTypeScript;
        assert_eq!(parser.language(), LanguageId::TypeScript);
        assert_eq!(
            parser.parser_fingerprint(),
            fingerprint::parser_fingerprint(LanguageId::TypeScript)
        );
    }
}
