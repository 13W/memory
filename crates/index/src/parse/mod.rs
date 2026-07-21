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
//! - [`output`] — the parse-output contract ([`ParseOutput`], byte spans,
//!   parents, unresolved references), T04-03.
//! - [`signature`] — the `signature_fingerprint` descriptor/derivation (ADR-0002).
//! - [`adapter`] — the shared tree-sitter engine and the per-language adapters
//!   (TypeScript in T04-03; JS/Rust in T04-04/05).
//!
//! # Scope
//!
//! T04-03 links the first real tree-sitter grammar (TypeScript, ADR-0001) and
//! defines the parse-output contract and the `syntax_path`/`signature_fingerprint`
//! derivation (ADR-0002, closes O7). Deterministic persistence of these units
//! (id minting, `blob_id` derivation, create/reuse, dedup) is T04-06.

pub mod adapter;
pub mod fingerprint;
pub mod language;
pub mod locator;
pub mod output;
pub mod parser;
pub mod signature;

pub use adapter::javascript::JavaScriptParser;
pub use adapter::typescript::TypeScriptParser;
pub use fingerprint::{
    BOUNDARY_NORM_VERSION, CHUNK_POLICY_VERSION, FingerprintComponents, LanguageDescriptor,
    canonical_kv, descriptor, parser_fingerprint,
};
pub use language::{LanguageId, select_language};
pub use locator::{LocatorParseError, SyntaxAnchor, SyntaxLocator, SyntaxLocatorDraft};
pub use output::{ByteSpan, ParseOutput, ParsedUnitDraft, ReferenceKind, UnresolvedRef};
pub use parser::LanguageParser;
