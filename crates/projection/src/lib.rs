//! `local-rag` projection layer (per-worktree dense shard + FTS view).
//!
//! Scaffold only (T00-02): no projection logic yet. Re-exports the workspace
//! version so downstream crates share a single source of truth.

pub use local_rag_core::VERSION;
