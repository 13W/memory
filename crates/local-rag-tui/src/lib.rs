//! Library half of `local-rag-tui`: screen data/compute/render transforms shared between the
//! binary ([`main`](../main/index.html)) and its tests (mirrors `local-rag-hook`'s own lib/bin
//! split — the same shape, adopted here for the same reason: `tests/*.rs` integration tests only
//! see a package's library target, never its binary target, and T18-02's live-subprocess test
//! must call this crate's own screen-compute code directly against a real `local-rag serve`).
//!
//! [`status`] is T18-02's deliverable — the Status screen (spec 11 §7): daemon identity/mode
//! (best-effort against `store.lock`, live-probed via `local_rag::daemon::fetch_welcome` when the
//! lock says ready) plus durable counts read directly from `state.sqlite`. [`repositories`] is
//! T18-03's — the Repositories screen: browse registered repositories, drill into a repository's
//! worktrees, then into one worktree's own detail. [`memory`] is T18-04's read paths plus T18-05's
//! mutation actions (approve/reject/edit/retract/merge) — the Memory screen: browse memory
//! entries/candidates with kind/state/scope filters and pagination, drill into an entry's own
//! detail + evidence, mutate through the same primitives `cli/memory.rs` uses. [`store_read`] is a
//! T18-04 extraction — the offline-safe `state.sqlite` read dance shared by every screen above,
//! moved out once a third screen needed it. [`store_write`] is T18-05's write-side counterpart —
//! the same offline-safe precaution, returning a write-capable `StateDb`. `rt` (crate-internal,
//! not re-exported) is T18-05's single-shot tokio runtime for driving a mutation's
//! `StateWriter::transaction` from this crate's otherwise fully synchronous event loop. Later
//! T18-06+ cards add their own sibling modules here, not inside these.

pub mod memory;
pub mod repositories;
mod rt;
pub mod status;
pub mod store_read;
pub mod store_write;
