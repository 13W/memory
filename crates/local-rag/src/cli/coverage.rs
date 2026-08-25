//! Is every file of the worktree accounted for by the index? — `D-096`.
//!
//! Three commands need the same two answers and must not disagree about them:
//! `project list` and `doctor` report the **durable** half (what the active
//! generation indexed and what it skipped, per reason), and `project coverage`
//! reports the **measured** half (files present in the tree that the generation
//! placed in neither table). This module owns both computations; the commands
//! only format them — the same division `freshness.rs` (X-008) already sets.
//!
//! # Why this module exists at all
//!
//! Until `D-096` the loss was unreportable. A file whose extension selects no
//! v0 language is counted `BuildOutcome::files_deferred` and written **nowhere**
//! (spec 06 §2's own as-built note says so), and no production command read
//! `skipped_file` either. On the owner's store that made 3446 of firefly's
//! 13728 tracked files invisible — a quarter of the repository — and the only
//! way to learn it was hand-written SQL against `state.sqlite`. The measured
//! half below is exactly that hand-written query, made a command.
//!
//! # Cost, and why the split is where it is
//!
//! The durable half is two aggregate queries against indexed columns, so
//! `doctor` and `project list` can afford it on every invocation. The measured
//! half walks the tree, which is why it lives behind an explicit command rather
//! than inside a health report. It uses
//! [`scan_paths`](local_rag_index::scan::scan_paths), which shares its walk with
//! the real scan and reads no file content — the set of names is all the
//! difference needs, and hashing a worktree to learn a set of names would be the
//! wrong price for a diagnostic.

use std::collections::BTreeSet;
use std::path::Path;

use local_rag_core::identity::path::CaseSensitivity;
use local_rag_store::{
    SkipTally, WorktreeKind, generation_accounted_paths, generation_file_count,
    generation_skip_tally,
};

/// How many files the active generation indexed, and what it skipped.
///
/// Read back from the two membership tables rather than remembered from the
/// build, so it answers for the generation search is actually serving — which is
/// the question a user asking "why is this file not found" is really asking.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GenerationCoverage {
    /// `generation_file` rows — files that are searchable.
    pub indexed: usize,
    /// `skipped_file` rows, per reason.
    pub skipped: SkipTally,
}

impl GenerationCoverage {
    /// Read the coverage of one generation.
    pub fn read(
        conn: &rusqlite::Connection,
        generation_id: &str,
    ) -> Result<GenerationCoverage, String> {
        let indexed = generation_file_count(conn, generation_id)
            .map_err(|e| format!("could not count {generation_id}'s indexed files: {e}"))?;
        let skipped = generation_skip_tally(conn, generation_id)
            .map_err(|e| format!("could not count {generation_id}'s skipped files: {e}"))?;
        Ok(GenerationCoverage { indexed, skipped })
    }

    /// Every file the generation examined — the denominator the measured half
    /// subtracts from the tree.
    pub fn accounted(&self) -> usize {
        self.indexed + self.skipped.total()
    }

    /// `10247 indexed, 46 skipped (43 secret, 3 huge)` — one wording, so the
    /// three commands that print it cannot drift apart.
    pub fn render(&self) -> String {
        if self.skipped.is_empty() {
            return format!("{} indexed, 0 skipped", self.indexed);
        }
        format!(
            "{} indexed, {} skipped ({})",
            self.indexed,
            self.skipped.total(),
            self.skipped.render()
        )
    }
}

/// How many files of the tree the generation never accounted for, and which.
///
/// `total == 0` is the healthy answer and the invariant `D-098` is required to
/// establish permanently: every scanned file lands in `generation_file` or in
/// `skipped_file`, never in neither.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UnaccountedFiles {
    /// How many candidate paths are in neither membership table.
    pub total: usize,
    /// Their extensions, largest group first; ties broken alphabetically so the
    /// output is deterministic. `(none)` collects extensionless names.
    pub by_extension: Vec<(String, usize)>,
    /// The first few paths, in sorted order — enough to recognize what class of
    /// file is missing without printing thousands of lines.
    pub examples: Vec<String>,
}

/// How many example paths [`UnaccountedFiles`] carries. Small on purpose: the
/// extension histogram is what identifies the cause, the examples only confirm
/// it.
pub const EXAMPLE_COUNT: usize = 10;

impl UnaccountedFiles {
    /// The difference `candidates ∖ accounted`, summarized.
    ///
    /// `candidates` is the scan's own candidate list (already normalized), and
    /// `accounted` the union of both membership tables. Both sides use the same
    /// normalized-path spelling, which is what makes the subtraction exact
    /// rather than approximate.
    pub fn from_sets(candidates: &[String], accounted: &BTreeSet<String>) -> UnaccountedFiles {
        let missing: Vec<&String> = candidates
            .iter()
            .filter(|p| !accounted.contains(*p))
            .collect();

        let mut counts: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        for path in &missing {
            *counts.entry(extension_of(path)).or_default() += 1;
        }
        let mut by_extension: Vec<(String, usize)> = counts.into_iter().collect();
        by_extension.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

        UnaccountedFiles {
            total: missing.len(),
            by_extension,
            examples: missing
                .iter()
                .take(EXAMPLE_COUNT)
                .map(|p| (*p).clone())
                .collect(),
        }
    }

    /// Walk `root` and compute the difference against `generation_id`.
    ///
    /// Read-only in both halves: [`scan_paths`](local_rag_index::scan::scan_paths)
    /// reads no content and the store side is two `SELECT`s.
    pub fn measure(
        conn: &rusqlite::Connection,
        generation_id: &str,
        root: &Path,
        kind: WorktreeKind,
        case: CaseSensitivity,
        prune_roots: &[String],
    ) -> Result<UnaccountedFiles, String> {
        let candidates = local_rag_index::scan::scan_paths(root, kind, case, prune_roots)
            .map_err(|e| format!("could not scan {}: {e}", root.display()))?;
        let accounted = generation_accounted_paths(conn, generation_id)
            .map_err(|e| format!("could not read {generation_id}'s membership: {e}"))?;
        Ok(UnaccountedFiles::from_sets(&candidates, &accounted))
    }

    /// `yaml 584, gql 560, svg 384` — the histogram, capped for one line.
    pub fn render_extensions(&self, cap: usize) -> String {
        if self.by_extension.is_empty() {
            return "none".to_string();
        }
        let shown: Vec<String> = self
            .by_extension
            .iter()
            .take(cap)
            .map(|(ext, n)| format!("{ext} {n}"))
            .collect();
        let rest = self.by_extension.len().saturating_sub(cap);
        if rest == 0 {
            shown.join(", ")
        } else {
            format!("{}, +{rest} more", shown.join(", "))
        }
    }
}

/// The lowercased final `.`-delimited suffix of a path's last component, or
/// `(none)` when it has none.
///
/// Deliberately the same "final suffix of the last component" rule
/// `select_language` and `BINARY_EXTENSIONS` already apply, so the histogram
/// groups files the way the code that dropped them grouped them.
fn extension_of(path: &str) -> String {
    let last = path.rsplit('/').next().unwrap_or(path);
    match last.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() => ext.to_ascii_lowercase(),
        _ => "(none)".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use local_rag_store::SkipReason;

    #[test]
    fn coverage_renders_totals_and_the_reason_breakdown() {
        let mut skipped = SkipTally::default();
        for _ in 0..43 {
            skipped.add(SkipReason::Secret);
        }
        for _ in 0..3 {
            skipped.add(SkipReason::Huge);
        }
        let c = GenerationCoverage {
            indexed: 10247,
            skipped,
        };
        // The exact live shape this defect was found in.
        assert_eq!(c.render(), "10247 indexed, 46 skipped (43 secret, 3 huge)");
        assert_eq!(c.accounted(), 10293);
    }

    #[test]
    fn coverage_with_no_skips_still_names_the_zero() {
        let c = GenerationCoverage {
            indexed: 7,
            skipped: SkipTally::default(),
        };
        // Never silent: "0 skipped" is an answer, an omitted clause is not.
        assert_eq!(c.render(), "7 indexed, 0 skipped");
    }

    #[test]
    fn the_difference_is_exact_and_grouped_by_extension() {
        let candidates = vec![
            "a.rs".to_string(),
            "docs/one.md".to_string(),
            "docs/two.md".to_string(),
            "deploy/values.yaml".to_string(),
            "makefile".to_string(),
        ];
        let accounted: BTreeSet<String> = ["a.rs".to_string()].into_iter().collect();

        let u = UnaccountedFiles::from_sets(&candidates, &accounted);
        assert_eq!(u.total, 4);
        // Largest group first, ties alphabetical — deterministic output.
        assert_eq!(
            u.by_extension,
            vec![
                ("md".to_string(), 2),
                ("(none)".to_string(), 1),
                ("yaml".to_string(), 1),
            ]
        );
        assert_eq!(u.render_extensions(2), "md 2, (none) 1, +1 more");
        assert_eq!(u.examples.len(), 4);
    }

    #[test]
    fn a_fully_accounted_tree_reports_nothing() {
        let candidates = vec!["a.rs".to_string(), "b.png".to_string()];
        let accounted: BTreeSet<String> = candidates.iter().cloned().collect();
        let u = UnaccountedFiles::from_sets(&candidates, &accounted);
        assert_eq!(u.total, 0);
        assert!(u.by_extension.is_empty());
        assert_eq!(u.render_extensions(5), "none");
    }

    #[test]
    fn extensions_are_taken_from_the_last_component_and_lowercased() {
        assert_eq!(extension_of("a/b/c.YAML"), "yaml");
        assert_eq!(extension_of("archive.tar.gz"), "gz");
        // A dotfile has no extension, matching `Path::extension`'s own rule —
        // the one `select_language` applies when it drops the file.
        assert_eq!(extension_of(".gitignore"), "(none)");
        assert_eq!(extension_of("dir.d/Makefile"), "(none)");
        assert_eq!(extension_of("trailing."), "(none)");
    }
}
