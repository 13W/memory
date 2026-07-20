//! `local-rag` code indexing / reconcile layer.
//!
//! Hosts the [`classify`] module — deterministic file classification into a
//! searchable member or a single skip reason (spec 06 §2.2) — atop the code
//! storage in `local-rag-store`. Still re-exports the workspace version so
//! downstream crates share a single source of truth.

pub mod classify;

pub use local_rag_core::VERSION;
