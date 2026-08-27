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
//! T22-15 added a third thing to the same delivery half: the ONNX Runtime is now
//! an artifact of first run too (spec 10 §5 `[FIXED, ADR-0013]`), pinned in
//! [`ort_catalog`], taken out of an upstream release archive by [`archive`], and
//! installed by [`install::install_ort`] beside the weights. Nothing in the
//! product calls it yet — `local-rag init`/`doctor` are T22-16's, and this card
//! deliberately shipped the mechanism and the resolution rung without a
//! trigger.
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
//! crate provides the typed API it will call, and T22-16 extends that surface to
//! the runtime. Exercising the runtime on every platform is T22-17's CI matrix:
//! this machine verified `darwin-arm64` end to end (download, extraction,
//! `ort::init_from`, real inference) and the other four only structurally, by
//! extracting each archive and comparing digests — the same honest split
//! `D-029` already recorded.

pub mod archive;
pub mod catalog;
pub mod fetch;
pub mod install;
pub mod manifest;
pub mod onnx;
pub mod ort_catalog;

pub use archive::{ArchiveError, ArchiveFormat, Limits as ArchiveLimits, extract_member};
pub use catalog::{
    AssetFile, CATALOG, DEFAULT_MODEL_ID, DEFAULT_MODEL_LICENSE, DEFAULT_MODEL_LICENSE_URL,
    DEFAULT_MODEL_REVISION, DEFAULT_MODEL_SOURCE, EMBEDDINGGEMMA_300M, ModelCatalogEntry, find,
};
pub use fetch::{AssetFetcher, FetchError, HttpFetcher, LocalFetcher};
pub use install::{
    InstallError, InstallReport, MANIFEST_FILE, OK_MARKER, OrtInstallReport, PART_SUFFIX,
    install_model, install_ort, is_installed, ort_dylib_path, ort_is_installed,
    write_license_notice,
};
pub use manifest::{ManifestFile, ModelManifest, OrtManifest};
pub use onnx::{MAX_SEQUENCE_TOKENS, OnnxEmbedder, OnnxError, POOLED_OUTPUT};
pub use ort_catalog::{ORT_ASSETS, OrtAsset, for_current_platform};

pub use local_rag_core::VERSION;
