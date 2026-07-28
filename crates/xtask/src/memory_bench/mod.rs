//! The memory-router benchmark (spec 08 §7, 14 §2's `memory-quality` row) —
//! T14-07.
//!
//! Split the same way `crate::bench` is (see that module's own doc): the
//! memory-router benchmark runner is `cargo xtask memory-bench`
//! (`crates/xtask/src/memory_bench/`), split so everything *scored* is an
//! ordinary offline test and only the end-to-end run needs the installed
//! GGUF weights and the `llama-cpp-2` toolchain: [`corpus`] loads the
//! labeled observation-stream fixture cases, [`score`] holds the op-kind
//! matching and precision/recall math, [`report`] shapes the output,
//! [`gate`] turns a report plus versioned P/R thresholds into a verdict, and
//! [`run`] is the only piece that needs the real model.

pub mod corpus;
pub mod gate;
pub mod report;
pub mod run;
pub mod score;

use std::path::{Path, PathBuf};

/// The repository's `fixtures/` root, resolved from this crate's manifest so
/// it works regardless of the caller's working directory.
pub fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

/// The case-index fixture the router-op cases live in (see `corpus`'s module
/// doc for why this is `memory/index.json`, not a new top-level family).
pub fn case_index_path() -> PathBuf {
    fixtures_root().join("memory/index.json")
}

/// The versioned gate thresholds (spec 14 §2's `P`/`R`).
pub fn thresholds_path() -> PathBuf {
    fixtures_root().join("memory/baseline/thresholds.json")
}

/// Where recorded runs live.
pub fn baseline_dir() -> PathBuf {
    fixtures_root().join("memory/baseline")
}
