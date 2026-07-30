//! Library half of `local-rag-hook`: the spool-writer transforms shared between
//! the hook binary ([`main`](../main/index.html)) and its tests.
//!
//! [`payload`] owns the REDACTION step of the hook write path (spec 07 §2) —
//! deny-list exclusion and size-capping before anything is handed to the
//! segment writer. [`event`] parses Claude Code's real hook JSON; [`identity`]
//! computes `source_event_id`/`dedup_key` (spec 07 §4); [`segment`] builds and
//! durably appends the LRSP frame. The wire-format primitives themselves
//! (`FramePayload`, frame/header encode, CRC-32C) live in
//! `local_rag_core::spool` (T13-03 relocated them there from this crate's own
//! former `frame` module, so the daemon-side decoder — `local_rag_store::spool`
//! — shares exactly one implementation with this write path rather than
//! risking two that could drift). [`recall`] is the read-only recall RPC +
//! `additionalContext` injection for `SessionStart`/`UserPromptSubmit` (spec
//! 11 §3.2/§5, T15-06) — runs after the spool append above, never before.

pub mod clock;
pub mod event;
pub mod identity;
pub mod payload;
pub mod recall;
pub mod segment;
pub mod subagent_counter;
