//! `local-rag` code indexing / reconcile layer.
//!
//! Hosts the [`classify`] module — deterministic file classification into a
//! searchable member or a single skip reason (spec 06 §2.2) — the [`parse`]
//! module — parser identity (language selector, `parser_fingerprint`, path-free
//! `SyntaxLocator`) and the parser seam (spec 03 §2.3.1, §2.4) — and the [`scan`]
//! module — the authoritative gitignore-aware tree scan that produces the
//! reconcile candidate manifest (spec 06 §1–2) — and the [`reconcile`] module —
//! the generation builder that turns that manifest into a durable generation with
//! structural sharing (spec 06 §2) — atop the code storage in `local-rag-store`.
//! Still re-exports the workspace version so downstream crates share a single
//! source of truth.

pub mod classify;
pub mod parse;
pub mod reconcile;
pub mod scan;

pub use local_rag_core::VERSION;
