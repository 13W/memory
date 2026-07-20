//! `local-rag` code indexing / reconcile layer.
//!
//! Hosts the [`classify`] module — deterministic file classification into a
//! searchable member or a single skip reason (spec 06 §2.2) — and the [`parse`]
//! module — parser identity (language selector, `parser_fingerprint`, path-free
//! `SyntaxLocator`) and the parser seam (spec 03 §2.3.1, §2.4) — atop the code
//! storage in `local-rag-store`. Still re-exports the workspace version so
//! downstream crates share a single source of truth.

pub mod classify;
pub mod parse;

pub use local_rag_core::VERSION;
