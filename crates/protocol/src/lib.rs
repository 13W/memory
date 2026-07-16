//! `local-rag` protocol types (proxy <-> daemon handshake, MCP tool contracts).
//!
//! Scaffold only (T00-02): no protocol logic yet. Re-exports the workspace
//! version so downstream crates share a single source of truth.

pub use local_rag_core::VERSION;
