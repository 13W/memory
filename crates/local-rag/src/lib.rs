//! Library half of `local-rag`: the daemon lifecycle transforms shared
//! between the binary ([`main`](../main/index.html)) and its tests.
//!
//! [`daemon`] is T15-01's deliverable — store lock (spec 02 §2, §4.1), the
//! startup/shutdown sequence, idle-shutdown gating, and the two startup
//! catch-up resume passes. The versioned proxy protocol, MCP tools, and the
//! rest of the CLI surface are later group-15 cards.

pub mod daemon;
