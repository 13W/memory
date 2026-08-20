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
//! Nothing in this module reads or writes `state.sqlite`: the storage and its
//! guards are `local_rag_store::memory::normalization` (T21-01, inverted by
//! T21-13 — English is the canon, so that table records what the *author*
//! wrote). Since ADR-0011 the callers are the boundaries: the write path
//! (T21-14), the query paths (T21-15/T21-19) and the one-time backfill
//! (T21-17). The daemon's remaining sweep only runs [`detect`], never
//! [`translate`].

pub mod detect;
pub mod translate;
