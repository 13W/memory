//! `local-rag` durable memory + observations layer.
//!
//! T14-07 turns this from a scaffold into the local router (spec 08 §4 step
//! 3, closing the generator half of open item O3): [`router::route`] is the
//! `generate` closure [`local_rag_store::run_once`] is generic over. Module
//! layout mirrors the design's own staging:
//!
//! - [`schema`] — the wire JSON shape the model must emit ([`schema::RawRouterOp`]).
//! - [`prompt`] — the system/user messages built from a
//!   [`local_rag_store::ConsolidationWindow`] plus [`recall`]'s candidate set.
//! - [`parse`] — text → `Vec<RawRouterOp>`, tier-1 malformed-output handling.
//! - [`guard`] — code-level enforcement of spec 08 §4's two `[FIXED]`
//!   placement rules; turns one `RawRouterOp` into one
//!   [`local_rag_store::GeneratedOp`], including tier-2 (referential)
//!   degradation.
//! - [`recall`] — the pre-generation candidate conflict set and the
//!   post-generation fresh-target resolution [`guard`] uses.
//! - [`router`] — ties the above together into the one function the runner
//!   calls.
//!
//! # Why this crate depends on `local-rag-embed`, never `local-rag-generate`
//!
//! [`router::route`] takes `&`[`local_rag_embed::GeneratorPool`] — the
//! trait-level seam, not any concrete runtime. The actual `llama.cpp`-backed
//! [`Generator`](local_rag_embed::Generator) implementation
//! (`local_rag_generate::LlamaGenerator`) is wired in only at the daemon/
//! `xtask` composition point, exactly like `crates/search` never depends on
//! `embed`/`models` directly. This is what keeps swapping the local runtime
//! a "write a new `Generator` impl" change, never a `local-rag-memory`
//! rewrite.
//!
//! # Why the store-level backstop still matters
//!
//! [`guard`] enforces spec 12 §4's "model-claims are never auto-promoted to
//! facts" *before* an op is ever proposed, but
//! [`local_rag_store::memory::op`]'s own `ModelClaimOnlyProvenance` check
//! (T14-07, spec 12 §4, `crates/store/src/memory/op.rs`) is the one that
//! actually can't be bypassed — [`local_rag_store::run_once`] is generic
//! over *any* `generate` closure, so a future generator, a bug in this
//! crate, or a direct `commit_apply_run` call would otherwise slip past this
//! module entirely. Both layers exist on purpose; this crate is not the only
//! place spec 12 §4 is real.

pub mod guard;
pub mod normalize;
pub mod parse;
pub mod prompt;
pub mod recall;
pub mod router;
pub mod schema;

pub use local_rag_core::VERSION;
