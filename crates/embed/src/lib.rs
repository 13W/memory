//! `local-rag` embedding provider pool (spec 10 §1) — T11-03.
//!
//! Spec 10 §1 `[FIXED]` fixes three things this crate implements:
//!
//! 1. the [`Embedder`] trait shape (`embed(EmbedRequest) -> Result<Vec<Vector>>`
//!    plus `key() -> RepresentationKey`);
//! 2. "**the local backend is the working default**; Ollama/remote providers are
//!    strictly optional" — so the pool is *ordered*, local-first, and works with
//!    no network and no external daemon at all;
//! 3. "every remote call is gated by the effective `data_policy` (02 §3.2)
//!    **before** the provider is selected; `local_only` never falls back to
//!    remote" — the [`policy`] guard runs before [`ProviderPool`] looks at a
//!    single candidate, and a blocked remote is a typed
//!    [`ErrorCode::PolicyBlockedRemote`](local_rag_protocol::ErrorCode), never a
//!    silent downgrade (spec 12 §1, 02 §6).
//!
//! Primary/fallback + retry semantics are "inherited from the v1 behavioral
//! contract" `[FIXED]` (01 §7). That contract exists in this repository as
//! implementation-neutral fixtures — `fixtures/fault/index.json`, family
//! `fault.llm.*` (retry on 5xx/429/network, honor `Retry-After`, never retry a
//! 4xx, throw after exhausting attempts) — and `crates/embed/tests/retry.rs`
//! replays every one of those cases against this pool.
//!
//! # What this task deliberately does **not** ship
//!
//! No ONNX/`candle` runtime is linked here, and no model weights are loaded.
//! Spec 10 §5 puts weight delivery (`local-rag init --download-models`,
//! checksum-verified manifest, atomic `.part → fsync → rename → .ok`) in
//! **T11-06**, and this crate has no way to obtain weights before that task
//! exists; wiring an inference runtime against absent assets would be undead
//! code. The default model choice itself *is* decided here — ADR-0004 closes the
//! embedding half of open question O3 — and the split is recorded as `D-008` in
//! `docs/implementation-plan/DEVIATIONS.md`, not left implicit.
//!
//! What ships instead is [`HashingEmbedder`]: a real, dependency-free,
//! byte-deterministic in-process [`Embedder`] under its own bootstrap
//! `model_id`. It keeps "the local backend is the working default" literally
//! true today, it is the deterministic model fixture every test in this crate
//! embeds with, and it is *not* the model ADR-0004 selects — a representation
//! registered under a different `model_id` can never be confused with the
//! production one, because `model_id` is one of the six fields of the canonical
//! [`RepresentationKey`](local_rag_store::RepresentationKey) (spec 03 §2.2).

pub mod backfill;

mod contract;
mod local;
mod pool;
mod registry;

pub mod policy;

pub use backfill::{
    BackfillError, BackfillParams, BackfillReport, DEFAULT_EMBED_BATCH, DEFAULT_WRITE_BATCH_ROWS,
    InFlight, promote_if_covered, run_backfill,
};
pub use contract::{EmbedError, EmbedRequest, Embedder, ProviderFailure, Vector};
pub use local::{
    HashingEmbedder, LOCAL_BOOTSTRAP_DIMENSIONS, LOCAL_BOOTSTRAP_MODEL_ID,
    LOCAL_BOOTSTRAP_NORMALIZATION_VERSION, LOCAL_BOOTSTRAP_REPRESENTATION_VERSION,
    model_assets_dir, require_model_assets,
};
pub use policy::{Locality, allows};
pub use pool::{
    DEFAULT_RETRY_BASE_MS, DEFAULT_RETRY_MAX_ATTEMPTS, DEFAULT_RETRY_MAX_MS, ProviderEntry,
    ProviderPool, RetryPolicy, Sleeper, ThreadSleeper, retry_delay_ms,
};
pub use registry::{RegistryMismatch, register_embedder_representation, verify_registered_key};

pub use local_rag_core::VERSION;
