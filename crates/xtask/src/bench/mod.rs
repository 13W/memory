//! The 49-query search benchmark (spec 14 §7) — T12-05.
//!
//! Split so the scored part is testable without a store: [`corpus`] loads and
//! validates the `[FIXED]` input, [`score`] holds the matching semantics and
//! metric math, [`report`] shapes the output and the v1 diff, [`gate`] turns a
//! report plus versioned thresholds into a verdict, and [`run`] is the only
//! piece that needs a real index, real weights and real time.

pub mod corpus;
pub mod gate;
pub mod report;
pub mod run;
pub mod score;

use std::path::{Path, PathBuf};

/// The repository's `fixtures/` root, resolved from this crate's manifest so it
/// works regardless of the caller's working directory.
pub fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

/// The 49-query corpus fixture.
pub fn corpus_fixture_path() -> PathBuf {
    fixtures_root().join("search/corpus.json")
}

/// The versioned gate thresholds (spec 14 §2's `X`/`Y`).
pub fn thresholds_path() -> PathBuf {
    fixtures_root().join("search/baseline/thresholds.json")
}

/// Where recorded runs live.
pub fn baseline_dir() -> PathBuf {
    fixtures_root().join("search/baseline")
}
