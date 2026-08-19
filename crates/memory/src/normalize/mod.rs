//! English normalization of durable memory text (ADR-0010) — group 21.
//!
//! The pieces land one card at a time and are deliberately independent:
//!
//! - [`detect`] (T21-03) — is this text written in a non-Latin script at all?
//!   Pure, model-free, and the reason an already-English store costs zero
//!   inference.
//! - [`translate`] (T21-04) — the one component that spends inference, and the
//!   one that trusts neither its input (an entry's text is data, encoded as a
//!   JSON string, never prompt structure) nor its output (every answer is
//!   validated before anyone may store it).
//!
//! The write order (T21-05) and the daemon worker (T21-06) join them here. Nothing in this module reads or writes
//! `state.sqlite`: the storage and its guards are
//! `local_rag_store::memory::normalization`/`effective_text` (T21-01/T21-02).

pub mod detect;
pub mod translate;
