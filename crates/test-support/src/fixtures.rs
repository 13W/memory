//! Locate and read the behavioral fixtures imported by T00-01.
//!
//! The fixtures live at the workspace root under `fixtures/` (see
//! `fixtures/README.md`). This module resolves that directory relative to the
//! crate source and reads files as raw bytes/strings. Typed parsing is
//! deliberately *not* provided here: the workspace is dependency-free, and
//! pulling in a JSON parser is deferred to the first task that actually consumes
//! typed fixtures (and must justify the dependency per `CONTRIBUTING.md`).

use std::io;
use std::path::{Path, PathBuf};

/// Absolute path of the workspace `fixtures/` directory.
///
/// Resolved from this crate's manifest directory (`crates/test-support`) by
/// walking up to the workspace root, so it is independent of the current
/// working directory and of `$HOME`.
pub fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
}

/// Resolve a fixture path relative to [`fixtures_root`].
pub fn fixture_path(rel: impl AsRef<Path>) -> PathBuf {
    fixtures_root().join(rel)
}

/// Whether a fixture exists at `rel` (relative to [`fixtures_root`]).
pub fn fixture_exists(rel: impl AsRef<Path>) -> bool {
    fixture_path(rel).exists()
}

/// Read a fixture file as a UTF-8 string.
pub fn read_fixture(rel: impl AsRef<Path>) -> io::Result<String> {
    std::fs::read_to_string(fixture_path(rel))
}

/// Read a fixture file as raw bytes.
pub fn read_fixture_bytes(rel: impl AsRef<Path>) -> io::Result<Vec<u8>> {
    std::fs::read(fixture_path(rel))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixtures_root_points_at_imported_corpus() {
        // The manifest imported by T00-01 must be reachable.
        assert!(
            fixture_exists("manifest.json"),
            "expected fixtures/manifest.json at {}",
            fixtures_root().display()
        );
    }

    #[test]
    fn read_fixture_returns_contents() {
        let manifest = read_fixture("manifest.json").expect("read manifest");
        assert!(manifest.trim_start().starts_with('{'));
    }
}
