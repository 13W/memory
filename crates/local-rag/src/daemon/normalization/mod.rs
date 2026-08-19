//! Applying English normalization to durable memory (ADR-0010) — group 21.
//!
//! - [`write`] (T21-05) — one entry: translate, embed under the new subject
//!   hash, then commit the normalization row. The order is the module's whole
//!   reason to exist.
//!
//! The worker that decides *which* entries to hand it, in what batches and with
//! what backoff, is T21-06's.

pub mod write;
