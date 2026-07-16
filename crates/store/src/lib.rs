//! `local-rag` durable storage layer (`state.sqlite` / `cache.sqlite`).
//!
//! Scaffold only (T00-02): no storage logic yet. Re-exports the workspace
//! version so downstream crates share a single source of truth.

pub use local_rag_core::VERSION;
