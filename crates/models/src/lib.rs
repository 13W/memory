//! `local-rag` model assets: the installer and the ONNX embedding provider — T11-06.
//!
//! This crate closes the **delivery half** of open question O3 (spec 15 §4) and
//! the deferral D-008 recorded when T11-03 shipped the provider pool without a
//! runtime. Two halves, one card:
//!
//! * [`install`] implements spec 10 §5's `[FIXED policy]` — a checksum-verified,
//!   atomic, resumable download (`.part` → fsync → rename → `.ok`) that records
//!   source/size/sha256/license in `models/<model_id>/manifest.json` and surfaces
//!   the license before the first byte moves;
//! * [`onnx`] is the in-process provider spec 10 §1 `[FIXED]` requires, loading
//!   the model ADR-0004 selected through `ort`'s `load-dynamic` feature so the
//!   build stays offline (ADR-0005).
//!
//! # Why this is not part of `crates/embed`
//!
//! `crates/embed` carries a structural guarantee — a test asserts its manifest
//! declares no network client and no model runtime (T11-03, tightened by D-010).
//! That guarantee is still true and still worth having: the provider *pool*, the
//! policy guard and the retry contract have no business linking TLS or ONNX. The
//! heavy dependencies live here instead, the same way `spike/` isolates the
//! dense-backend candidates, and `crates/embed` keeps its lint intact rather
//! than being weakened to accommodate this crate.
//!
//! # What stays elsewhere
//!
//! The `local-rag init --download-models` *command* is T15-07's CLI card; this
//! crate provides the typed API it will call. Verifying ORT bundling across the
//! whole platform matrix is T17-03's "before the final CI matrix", and excluding
//! weights from the npm packages is T17-01's packaging test.

pub mod catalog;
pub mod fetch;
pub mod install;
pub mod manifest;
pub mod onnx;

pub use catalog::{
    AssetFile, CATALOG, DEFAULT_MODEL_ID, DEFAULT_MODEL_LICENSE, DEFAULT_MODEL_LICENSE_URL,
    DEFAULT_MODEL_REVISION, DEFAULT_MODEL_SOURCE, EMBEDDINGGEMMA_300M, ModelCatalogEntry, find,
};
pub use fetch::{AssetFetcher, FetchError, HttpFetcher, LocalFetcher};
pub use install::{
    InstallError, InstallReport, MANIFEST_FILE, OK_MARKER, PART_SUFFIX, install_model,
    is_installed, write_license_notice,
};
pub use manifest::{ManifestFile, ModelManifest};
pub use onnx::{MAX_SEQUENCE_TOKENS, OnnxEmbedder, OnnxError, POOLED_OUTPUT};

pub use local_rag_core::VERSION;
