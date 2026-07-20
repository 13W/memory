//! Path-only gitignore matching for the `ignored` skip reason (spec 06 §2.2
//! "gitignore + configured excludes").
//!
//! [`GitignoreSet`] wraps [`ignore::gitignore::Gitignore`] and applies standard
//! Git precedence for **nested** `.gitignore` files: the matcher closest to the
//! file wins, and `!`-negation re-includes an otherwise-ignored path. Configured
//! excludes act as an additional root-level layer.
//!
//! This is deliberately just the *matcher*, not a directory walk — the
//! authoritative tree scan (`ignore::Walk`) is T05-02. The set is built from
//! in-memory `(dir, contents)` sources against a synthetic root, so tests need no
//! filesystem and are fully deterministic.

use std::path::{Path, PathBuf};

use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};

/// One `.gitignore` (or the excludes layer) rooted at a directory.
struct Layer {
    base: PathBuf,
    depth: usize,
    matcher: Gitignore,
}

/// A set of gitignore matchers evaluated with Git precedence (deepest wins).
///
/// Build one with [`GitignoreSetBuilder`]; query with [`GitignoreSet::is_ignored`].
pub struct GitignoreSet {
    root: PathBuf,
    /// Deepest-first, so the first non-`None` verdict is the closest matcher.
    layers: Vec<Layer>,
}

impl GitignoreSet {
    /// A matcher set that ignores nothing (no `.gitignore`, no excludes).
    pub fn empty() -> GitignoreSet {
        GitignoreSet {
            root: PathBuf::from("/"),
            layers: Vec::new(),
        }
    }

    /// Whether `rel_path` (forward-slash, relative to the repository root) is
    /// ignored. `is_dir` selects directory-only patterns (`build/`).
    ///
    /// Evaluates matchers closest-first; the first `Ignore`/`Whitelist` decides,
    /// so a deep negation re-includes a path ignored higher up.
    pub fn is_ignored(&self, rel_path: &str, is_dir: bool) -> bool {
        let abs = self.root.join(rel_path);
        for layer in &self.layers {
            if !abs.starts_with(&layer.base) {
                continue;
            }
            match layer.matcher.matched_path_or_any_parents(&abs, is_dir) {
                Match::Ignore(_) => return true,
                Match::Whitelist(_) => return false,
                Match::None => {}
            }
        }
        false
    }
}

/// Accumulates gitignore sources, then [`build`](GitignoreSetBuilder::build)s a
/// [`GitignoreSet`] against a synthetic `root`.
pub struct GitignoreSetBuilder {
    root: PathBuf,
    sources: Vec<(PathBuf, usize, Vec<String>)>,
}

impl GitignoreSetBuilder {
    /// Start a builder rooted at `root` (may be synthetic, e.g. `/repo` in tests;
    /// the real worktree root in production). No filesystem access occurs.
    pub fn new(root: impl AsRef<Path>) -> GitignoreSetBuilder {
        GitignoreSetBuilder {
            root: root.as_ref().to_path_buf(),
            sources: Vec::new(),
        }
    }

    /// Add a `.gitignore` located in `dir` (relative to the root; `""` or `"."`
    /// for the root itself) with the given file `contents`.
    pub fn add_gitignore(&mut self, dir: &str, contents: &str) -> &mut GitignoreSetBuilder {
        let (base, depth) = self.base_of(dir);
        let lines = contents.lines().map(str::to_string).collect();
        self.sources.push((base, depth, lines));
        self
    }

    /// Add configured excludes as a root-level ignore layer (spec 06 §2.2). Each
    /// entry is one gitignore-syntax pattern.
    pub fn add_excludes(&mut self, patterns: &[&str]) -> &mut GitignoreSetBuilder {
        let lines = patterns.iter().map(|p| (*p).to_string()).collect();
        self.sources.push((self.root.clone(), 0, lines));
        self
    }

    /// Resolve a relative `dir` to an absolute base and its component depth.
    fn base_of(&self, dir: &str) -> (PathBuf, usize) {
        let trimmed = dir.trim_matches('/');
        if trimmed.is_empty() || trimmed == "." {
            (self.root.clone(), 0)
        } else {
            let depth = trimmed.split('/').filter(|s| !s.is_empty()).count();
            (self.root.join(trimmed), depth)
        }
    }

    /// Compile every source into a [`GitignoreSet`], ordered deepest-first.
    ///
    /// Fails if a gitignore line is malformed (a typed [`ignore::Error`]).
    pub fn build(self) -> Result<GitignoreSet, ignore::Error> {
        let mut layers = Vec::with_capacity(self.sources.len());
        for (base, depth, lines) in self.sources {
            let mut builder = GitignoreBuilder::new(&base);
            for line in &lines {
                builder.add_line(None, line)?;
            }
            layers.push(Layer {
                base,
                depth,
                matcher: builder.build()?,
            });
        }
        // Deepest first; equal-depth layers keep insertion order (stable sort), so
        // a directory's own `.gitignore` is consulted before the excludes layer.
        layers.sort_by_key(|l| std::cmp::Reverse(l.depth));
        Ok(GitignoreSet {
            root: self.root,
            layers,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(sources: &[(&str, &str)]) -> GitignoreSet {
        let mut b = GitignoreSetBuilder::new("/repo");
        for (dir, contents) in sources {
            b.add_gitignore(dir, contents);
        }
        b.build().expect("valid gitignore")
    }

    #[test]
    fn empty_set_ignores_nothing() {
        let set = GitignoreSet::empty();
        assert!(!set.is_ignored("anything.rs", false));
    }

    #[test]
    fn root_gitignore_basic_patterns() {
        let set = build(&[(".", "target/\n*.log\n")]);
        assert!(set.is_ignored("target", true));
        assert!(set.is_ignored("app.log", false));
        assert!(set.is_ignored("nested/deep/app.log", false));
        assert!(!set.is_ignored("src/main.rs", false));
    }

    #[test]
    fn nested_gitignore_deeper_wins_via_negation() {
        // Root ignores all *.txt; a nested .gitignore re-includes keep.txt under
        // docs/ — the deeper (closer) matcher takes precedence.
        let set = build(&[(".", "*.txt\n"), ("docs", "!keep.txt\n")]);
        assert!(set.is_ignored("notes.txt", false));
        assert!(set.is_ignored("docs/other.txt", false));
        assert!(
            !set.is_ignored("docs/keep.txt", false),
            "deep negation re-includes"
        );
    }

    #[test]
    fn nested_gitignore_deeper_can_add_ignores() {
        let set = build(&[(".", "\n"), ("src/gen", "*.rs\n")]);
        assert!(!set.is_ignored("src/main.rs", false));
        assert!(set.is_ignored("src/gen/table.rs", false));
    }

    #[test]
    fn configured_excludes_are_a_root_layer() {
        let mut b = GitignoreSetBuilder::new("/repo");
        b.add_gitignore(".", "\n");
        b.add_excludes(&["vendor/", "*.min.js"]);
        let set = b.build().expect("valid");
        assert!(set.is_ignored("vendor", true));
        assert!(set.is_ignored("web/app.min.js", false));
        assert!(!set.is_ignored("web/app.js", false));
    }

    #[test]
    fn malformed_line_is_a_typed_error() {
        let mut b = GitignoreSetBuilder::new("/repo");
        // An unclosed alternate group is rejected by the globset compiler; the
        // error propagates as a typed `ignore::Error` from `build`.
        b.add_gitignore(".", "src/{a\n");
        assert!(b.build().is_err());
    }
}
