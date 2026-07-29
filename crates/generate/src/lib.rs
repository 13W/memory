//! `local-rag` local generative model assets and provider — T14-07/ADR-0006.
//!
//! This crate closes the **local generator crate** half of open question O3
//! (spec 15 §4), the last of its three halves — the other two (default
//! embedding model, weights delivery) were closed by ADR-0004/ADR-0005 in
//! `crates/models`. Two halves, one crate, mirroring that precedent:
//!
//! * [`install`] implements spec 10 §5's `[FIXED policy]` for a generative
//!   model exactly as `local_rag_models::install` does for the embedding
//!   one — a checksum-verified, atomic, resumable download (`.part` → fsync
//!   → rename → `.ok`) recording source/size/sha256/license in
//!   `models/<model_id>/manifest.json`. Duplicated rather than shared with
//!   `local_rag_models` (see this crate's own `Cargo.toml` comment for why);
//! * [`llama`] is the [`local_rag_embed::Generator`] `[FIXED]` provider (spec
//!   10 §1), loading the model ADR-0006 selected through `llama-cpp-2`
//!   (Rust bindings to llama.cpp).
//!
//! # Why this is not part of `crates/models`
//!
//! `crates/models`' own name, module docs and dependency table are
//! ONNX/embedding-specific (ADR-0004/0005); folding an unrelated runtime in
//! would misdescribe already-shipped code. More concretely, `llama-cpp-2`
//! needs a materially different build toolchain (`cmake` + `libclang` for
//! `bindgen`) than ONNX's `load-dynamic` (no C++ toolchain at all) — mixing
//! them would force every ONNX-only contributor to also carry a C++
//! toolchain they do not need. A third heavy-dependency island gets a third
//! isolated crate, continuing the isolation `spike/` and `crates/embed`
//! vs. `crates/models` already established rather than breaking it.
//!
//! # Why `crates/memory` (the router) never depends on this crate directly
//!
//! `local_rag_memory` depends on [`local_rag_embed::Generator`] (the trait)
//! and [`local_rag_embed::GeneratorPool`], never on this crate — the same
//! shape `crates/search` uses for `Embedder` (it never depends on
//! `crates/embed`/`crates/models` either; the concrete provider is wired in
//! only at the outermost composition point). Swapping `llama-cpp-2` for a
//! different runtime later is "write a new `Generator` impl in a new/
//! different crate," never a `crates/memory` rewrite.

pub mod catalog;
pub mod chat_template;
pub mod fetch;
pub mod install;
pub mod llama;
pub mod manifest;

pub use catalog::{
    AssetFile, CATALOG, DEFAULT_MODEL_ID, GEMMA4_E2B_IT_Q4_0, GeneratorCatalogEntry,
    PHI3_MINI_4K_INSTRUCT_Q4, QWEN2_5_0_5B_INSTRUCT_Q4KM, QWEN2_5_1_5B_INSTRUCT_Q4KM, find,
};
pub use chat_template::ChatTemplateError;
pub use fetch::{AssetFetcher, FetchError, HttpFetcher, LocalFetcher};
pub use install::{
    InstallError, InstallReport, MANIFEST_FILE, OK_MARKER, PART_SUFFIX, install_model,
    is_installed, write_license_notice,
};
pub use llama::{LlamaError, LlamaGenerator, default_model_path};
pub use manifest::{GeneratorManifest, GeneratorManifestFile};

pub use local_rag_core::VERSION;
