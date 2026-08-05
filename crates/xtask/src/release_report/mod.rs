//! The versioned release report (spec 14 §2's 9 acceptance gates) — T17-05.
//!
//! Split the same way [`crate::bench`]/[`crate::memory_bench`] already are:
//! [`resources`] and [`latency`] are the two genuinely new measurement legs
//! (idle RAM, index bytes/symbol, embedding-cache-budget adherence, source/
//! worktree byte ratio; one-file and branch-checkout reconcile p95) that have
//! no prior art anywhere in this repository (spec 14 §2's `latency`/
//! `resources` rows, `[BASELINE]`-pending since T12-05). [`report`] shapes
//! the combined output, [`gate`] evaluates only the two sub-gates that
//! already have a committed pass/fail threshold (`quality`/`memory-quality`
//! — `latency`/`resources` are recorded as this release's first-established
//! v2 baseline, never gated, the same precedent T10's dense-backend spike
//! metrics already set), and [`run`] is the only piece that needs a real
//! index, real weights, and real time.
//!
//! Deliberately **not** part of `cargo xtask ci` — same reasoning as
//! `bench`/`memory-bench` (real model weights, a real corpus checkout, and
//! now also a real compiled `local-rag` binary sibling to `xtask`'s own,
//! none of which a per-commit check should require).

pub mod gate;
pub mod latency;
pub mod report;
pub mod resources;
pub mod run;

use std::path::PathBuf;

/// Where recorded release reports live.
pub fn baseline_dir() -> PathBuf {
    crate::bench::fixtures_root().join("release")
}
