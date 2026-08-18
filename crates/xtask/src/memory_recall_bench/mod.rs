//! Retrieval-quality benchmark for memory recall (spec 08 §6) — X-010.
//!
//! No Hit@K/MRR-style benchmark existed for `local_rag_memory::recall::
//! pipeline::recall` before this: `cargo xtask memory-bench`
//! (`crate::memory_bench`) scores the *consolidation router*
//! (create/reinforce/supersede/noop), never recall itself — see
//! `crates/memory/src/recall/fusion.rs`'s own module doc, which states
//! plainly that no such fixture set exists yet. This is that fixture set's
//! harness, split the same way `crate::bench`/`crate::memory_bench` are:
//! [`corpus`] loads and validates the `[FIXED for this run]` bilingual
//! fixture, [`score`] holds the matching semantics and metric math,
//! [`report`] shapes the output, and [`run`] is the only piece that needs a
//! real store, a real ONNX session and real time. There is no `gate` module
//! here (unlike `bench`/`memory_bench`): nothing has measured this pipeline
//! before, so there is no baseline to gate against yet — this run *is* the
//! evidence a future threshold would be derived from (the same "collect
//! metrics, never invent thresholds" principle spec 08 §2's own as-built note
//! states for confidence weights, O2).
//!
//! Invoked as `cargo xtask memory-recall-bench`.

pub mod corpus;
pub mod report;
pub mod run;
pub mod score;

use std::path::{Path, PathBuf};

/// The repository's `fixtures/` root, resolved from this crate's manifest so
/// it works regardless of the caller's working directory.
pub fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

/// The bilingual memory-recall corpus fixture.
pub fn corpus_fixture_path() -> PathBuf {
    fixtures_root().join("memory-recall/corpus.json")
}

/// Where recorded runs live.
pub fn baseline_dir() -> PathBuf {
    fixtures_root().join("memory-recall/baseline")
}
