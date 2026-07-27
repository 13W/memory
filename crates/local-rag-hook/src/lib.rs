//! Library half of `local-rag-hook`: the spool-writer transforms shared between
//! the hook binary ([`main`](../main/index.html)) and its tests.
//!
//! [`payload`] owns the REDACTION step of the hook write path (spec 07 §2) —
//! deny-list exclusion and size-capping before anything is handed to the (T13-02)
//! segment writer. Parsing Claude Code's hook JSON, computing `source_event_id`,
//! and building/writing the frame itself are later group-13 tasks.

pub mod payload;
