//! Library half of `local-rag`: the transforms shared between the binary
//! ([`main`](../main/index.html)), its tests, and — since group 20 — each
//! other.
//!
//! [`daemon`] is T15-01's deliverable — store lock (spec 02 §2, §4.1), the
//! startup/shutdown sequence, idle-shutdown gating, and the two startup
//! catch-up resume passes. The versioned proxy protocol, MCP tools, and the
//! rest of the CLI surface are later group-15 cards.
//!
//! [`indexing`] is T20-02's: the reconcile → embed → activate → materialize
//! pipeline (spec 06 §1–2, spec 05 §5), previously `pub(crate)` inside the
//! binary's `cli/index.rs` and therefore unreachable from any library code.
//! It has two callers now — the CLI (`cli::index`/`cli::watch`/
//! `cli::rebuild`) and the daemon's future per-worktree background tasks
//! (`daemon::indexing`, T20-05/T20-06, not implemented yet) — so it lives
//! where both can link against it.

pub mod daemon;
pub mod indexing;
