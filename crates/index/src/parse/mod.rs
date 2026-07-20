//! Parser identity and the language-agnostic parser seam (spec 03 §2.3.1, §2.4;
//! 06 §2.1; ADR-0001).
//!
//! This module builds the **producer** of a `parser_fingerprint` (which `store`
//! consumes as an opaque string) and fixes the value shapes the parser layer keys
//! on:
//!
//! - [`language`] — the closed v0 [`LanguageId`] set and the extension-based
//!   [`select_language`] selector.
//! - [`fingerprint`] — the canonical sorted [`parser_fingerprint`] over all
//!   boundary-affecting versions.
//! - [`locator`] — the path-free [`SyntaxLocator`] and its canonical
//!   serialization.
//! - [`parser`] — the [`LanguageParser`] trait the tree-sitter adapters implement.
//!
//! # Scope
//!
//! No tree-sitter grammar is linked (T10 guardrail): the real per-language
//! adapters, byte spans, parents, unresolved references, and the derivation of
//! `syntax_path`/`signature_fingerprint` are T04-03+. Grammar/query versions are
//! declared constants here.

pub mod fingerprint;
pub mod language;
pub mod locator;
pub mod parser;

pub use fingerprint::{
    BOUNDARY_NORM_VERSION, CHUNK_POLICY_VERSION, FingerprintComponents, LanguageDescriptor,
    canonical_kv, descriptor, parser_fingerprint,
};
pub use language::{LanguageId, select_language};
pub use locator::{LocatorParseError, SyntaxAnchor, SyntaxLocator};
pub use parser::LanguageParser;
