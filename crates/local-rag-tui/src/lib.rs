//! Library half of `local-rag-tui`: screen data/compute/render transforms shared between the
//! binary ([`main`](../main/index.html)) and its tests (mirrors `local-rag-hook`'s own lib/bin
//! split — the same shape, adopted here for the same reason: `tests/*.rs` integration tests only
//! see a package's library target, never its binary target, and T18-02's live-subprocess test
//! must call this crate's own screen-compute code directly against a real `local-rag serve`).
//!
//! [`status`] is T18-02's deliverable — the Status screen (spec 11 §7): daemon identity/mode
//! (best-effort against `store.lock`, live-probed via `local_rag::daemon::fetch_welcome` when the
//! lock says ready) plus durable counts read directly from `state.sqlite`. Later T18-03+ cards add
//! their own sibling modules here, not inside this one.

pub mod status;
