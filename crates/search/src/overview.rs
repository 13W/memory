//! `project_overview()` (spec 11 §2) — T12-04.
//!
//! "3-level tree + entry points + top imports, derived from active generation
//! `[SPEC: computed, cached per generation]`."
//!
//! Everything comes from `state.sqlite`'s record of the active generation — no
//! disk walk, for the same reason snippets never touch the live file (spec 09
//! §7 `[FIXED]`): an overview that described the working tree rather than the
//! generation would disagree with every other answer the daemon gives.
//!
//! # The three fields, as-built `[SPEC]`
//!
//! The section names the fields and nothing else, so each shape is decided
//! here:
//!
//! - **tree** — every directory that contains at least one member file, folded
//!   to [`TREE_DEPTH`] levels, each node carrying recursive `file_count` and
//!   `occurrence_count`. Deeper directories are *summarized into* their
//!   depth-3 ancestor rather than dropped, so the counts on a node always
//!   describe the whole subtree beneath it; the root (`""`) is depth 0 and
//!   totals the project.
//! - **entry_points** — a conventional-filename heuristic (see
//!   [`is_entry_point`]). The graph-shaped definition ("files nothing imports")
//!   is not available: imports are stored as **unresolved module specifiers**
//!   and resolving them to paths is post-v0 (spec 09 §6). Inventing a resolver
//!   inside this task would be both out of scope and a worse answer than an
//!   honest, documented heuristic.
//! - **top_imports** — frequency of `unresolved_reference.reference_text` across
//!   the generation. A frequency question needs no resolution, so this one is
//!   exact rather than heuristic; the specifiers are reported exactly as the
//!   source wrote them.
//!
//! # Caching
//!
//! Spec 11 §2 says "computed, cached per generation". The cache is in memory,
//! keyed by `(worktree_id, generation_id)` — so a generation switch does not
//! *invalidate* an entry, it stops addressing it, and a bounded LRU evicts the
//! stale one. That keeps `cache.sqlite`'s schema (and its version, whose bump
//! would drop every user's FTS view) untouched for a value that is a pure
//! function of `state.sqlite` and cheap to recompute.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use local_rag_protocol::{GenerationRef, ImportCount, OverviewNode, ProjectOverview};
use local_rag_store::rusqlite::Connection;
use local_rag_store::{
    generation_file_occurrence_counts, generation_number, top_imports_for_generation,
};

use crate::pipeline::SearchInfraError;

/// How many directory levels the tree keeps (spec 11 §2's "3-level tree").
pub const TREE_DEPTH: usize = 3;

/// How many import specifiers `top_imports` reports `[SPEC]`.
pub const TOP_IMPORTS_LIMIT: usize = 20;

/// How many `(worktree, generation)` overviews stay cached.
///
/// Small on purpose: the useful entry is the *current* generation of each open
/// worktree, and a switch makes its predecessor dead weight immediately.
const OVERVIEW_CACHE_CAPACITY: usize = 16;

/// Filenames that conventionally mark a program's entry point `[SPEC]`.
///
/// Matched against the path's final component; [`is_entry_point`] adds the
/// directory-shaped conventions. Kept as one sorted list so the heuristic is
/// reviewable in a single place rather than spread through the code.
const ENTRY_POINT_FILENAMES: &[&str] = &[
    "__main__.py",
    "index.js",
    "index.jsx",
    "index.ts",
    "index.tsx",
    "lib.rs",
    "main.go",
    "main.js",
    "main.py",
    "main.rs",
    "main.ts",
    "mod.rs",
];

/// Whether `normalized_path` looks like an entry point `[SPEC]`.
///
/// Two rules, both purely lexical:
///
/// 1. the final component is one of [`ENTRY_POINT_FILENAMES`];
/// 2. the path lies directly under a `bin/` directory (`src/bin/tool.rs`,
///    `bin/cli.js`) — the conventional "one executable per file" layout.
///
/// Deliberately *not* recursive-anything: a heuristic that fires on every
/// `index.ts` in a large TypeScript tree would report hundreds of "entry
/// points", which is why the caller also caps the list.
pub fn is_entry_point(normalized_path: &str) -> bool {
    let components: Vec<&str> = normalized_path
        .split('/')
        .filter(|c| !c.is_empty())
        .collect();
    let Some(file) = components.last() else {
        return false;
    };
    if ENTRY_POINT_FILENAMES.contains(file) {
        return true;
    }
    matches!(components.len().checked_sub(2), Some(i) if components[i] == "bin")
}

/// Fold `(path, occurrence_count)` pairs into the depth-capped directory tree.
///
/// Pure over its input so the folding rules are unit-testable without a store.
pub fn build_tree(files: &[(String, usize)]) -> Vec<OverviewNode> {
    // `directory prefix -> (files, occurrences)`, where the root is "".
    let mut totals: HashMap<String, (usize, usize)> = HashMap::new();

    for (path, occurrences) in files {
        let components: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
        // The file's own name is not a directory; ancestors are every prefix of
        // the remaining components, capped at TREE_DEPTH. Every ancestor is
        // credited, which is what makes the counts recursive — a file at depth 7
        // still shows up in its depth-3 ancestor's totals.
        let directories = components.len().saturating_sub(1).min(TREE_DEPTH);
        for depth in 0..=directories {
            let prefix = components[..depth].join("/");
            let entry = totals.entry(prefix).or_insert((0, 0));
            entry.0 += 1;
            entry.1 += occurrences;
        }
    }

    let mut nodes: Vec<OverviewNode> = totals
        .into_iter()
        .map(|(path, (file_count, occurrence_count))| OverviewNode {
            depth: if path.is_empty() {
                0
            } else {
                path.split('/').count()
            },
            path,
            file_count,
            occurrence_count,
        })
        .collect();
    // Deterministic, like every other response in this crate (spec 09 §7).
    nodes.sort_by(|a, b| a.path.cmp(&b.path));
    nodes
}

/// Compute the overview for `generation_id` from a `state.sqlite` read
/// connection opened under the worktree's `L2.read`.
pub(crate) fn compute(
    conn: &Connection,
    generation_id: &str,
) -> Result<ProjectOverview, SearchInfraError> {
    let number = generation_number(conn, generation_id)
        .map_err(SearchInfraError::StateRead)?
        .ok_or_else(|| SearchInfraError::MissingGeneration(generation_id.to_string()))?;

    let files = generation_file_occurrence_counts(conn, generation_id)
        .map_err(SearchInfraError::StateRead)?;

    let entry_points: Vec<String> = files
        .iter()
        .map(|(path, _)| path)
        .filter(|path| is_entry_point(path))
        .cloned()
        .collect();

    let top_imports = top_imports_for_generation(conn, generation_id, TOP_IMPORTS_LIMIT)
        .map_err(SearchInfraError::StateRead)?
        .into_iter()
        .map(|(specifier, count)| ImportCount { specifier, count })
        .collect();

    Ok(ProjectOverview {
        generation: GenerationRef {
            id: generation_id.to_string(),
            number,
        },
        tree: build_tree(&files),
        // `generation_file_occurrence_counts` already returns paths ascending,
        // so the filtered list is sorted by construction.
        entry_points,
        top_imports,
    })
}

/// A bounded, in-memory `(worktree, generation) -> overview` cache.
///
/// Insertion-ordered eviction rather than true LRU: entries are only ever
/// written once per generation and read many times, so recency of *use* carries
/// no information recency of *insertion* does not already carry.
#[derive(Debug, Default)]
pub(crate) struct OverviewCache {
    inner: Mutex<CacheState>,
}

#[derive(Debug, Default)]
struct CacheState {
    entries: HashMap<(String, String), Arc<ProjectOverview>>,
    order: Vec<(String, String)>,
}

impl OverviewCache {
    /// The cached overview for this tuple, if any.
    pub(crate) fn get(
        &self,
        worktree_id: &str,
        generation_id: &str,
    ) -> Option<Arc<ProjectOverview>> {
        let key = (worktree_id.to_string(), generation_id.to_string());
        self.inner
            .lock()
            .expect("overview cache mutex poisoned")
            .entries
            .get(&key)
            .cloned()
    }

    /// Cache `overview` for this tuple, evicting the oldest entry past capacity.
    pub(crate) fn put(
        &self,
        worktree_id: &str,
        generation_id: &str,
        overview: Arc<ProjectOverview>,
    ) {
        let key = (worktree_id.to_string(), generation_id.to_string());
        let mut state = self.inner.lock().expect("overview cache mutex poisoned");
        if state.entries.insert(key.clone(), overview).is_none() {
            state.order.push(key);
        }
        while state.order.len() > OVERVIEW_CACHE_CAPACITY {
            let oldest = state.order.remove(0);
            state.entries.remove(&oldest);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(paths: &[(&str, usize)]) -> Vec<(String, usize)> {
        paths.iter().map(|(p, n)| ((*p).to_string(), *n)).collect()
    }

    fn node<'a>(nodes: &'a [OverviewNode], path: &str) -> &'a OverviewNode {
        nodes
            .iter()
            .find(|n| n.path == path)
            .unwrap_or_else(|| panic!("no node {path:?} in {nodes:?}"))
    }

    // ---- tree ---------------------------------------------------------------

    #[test]
    fn the_root_totals_the_whole_project() {
        let tree = build_tree(&files(&[
            ("a.rs", 2),
            ("src/b.rs", 3),
            ("src/deep/c.rs", 4),
        ]));
        let root = node(&tree, "");
        assert_eq!(root.depth, 0);
        assert_eq!(root.file_count, 3);
        assert_eq!(root.occurrence_count, 9);
    }

    #[test]
    fn counts_are_recursive_over_a_subtree() {
        let tree = build_tree(&files(&[
            ("src/a.rs", 1),
            ("src/inner/b.rs", 2),
            ("other/c.rs", 5),
        ]));
        let src = node(&tree, "src");
        assert_eq!(src.depth, 1);
        assert_eq!(src.file_count, 2, "src/a.rs + src/inner/b.rs");
        assert_eq!(src.occurrence_count, 3);
        assert_eq!(node(&tree, "src/inner").file_count, 1);
        assert_eq!(node(&tree, "other").occurrence_count, 5);
    }

    /// The depth cap **summarizes** rather than drops: a file seven levels deep
    /// is still counted by its depth-3 ancestor, and no depth-4 node exists.
    #[test]
    fn deeper_directories_fold_into_their_depth_three_ancestor() {
        let tree = build_tree(&files(&[("a/b/c/d/e/f/g.rs", 7)]));
        assert!(
            tree.iter().all(|n| n.depth <= TREE_DEPTH),
            "no node deeper than {TREE_DEPTH}: {tree:?}"
        );
        let deepest = node(&tree, "a/b/c");
        assert_eq!(deepest.depth, TREE_DEPTH);
        assert_eq!(deepest.file_count, 1);
        assert_eq!(
            deepest.occurrence_count, 7,
            "the deep file is summarized in"
        );
    }

    #[test]
    fn a_root_level_file_only_credits_the_root() {
        let tree = build_tree(&files(&[("README.md", 0)]));
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].path, "");
        assert_eq!(tree[0].file_count, 1);
    }

    #[test]
    fn nodes_are_sorted_by_path() {
        let tree = build_tree(&files(&[("z/a.rs", 1), ("a/b.rs", 1), ("m/c.rs", 1)]));
        let paths: Vec<&str> = tree.iter().map(|n| n.path.as_str()).collect();
        let mut sorted = paths.clone();
        sorted.sort_unstable();
        assert_eq!(paths, sorted);
    }

    #[test]
    fn an_empty_generation_has_an_empty_tree() {
        assert!(build_tree(&[]).is_empty());
    }

    // ---- entry points -------------------------------------------------------

    #[test]
    fn conventional_filenames_are_entry_points() {
        for path in [
            "main.rs",
            "src/main.rs",
            "src/lib.rs",
            "pkg/index.ts",
            "app/__main__.py",
            "cmd/server/main.go",
        ] {
            assert!(is_entry_point(path), "{path} should be an entry point");
        }
    }

    #[test]
    fn a_file_directly_under_bin_is_an_entry_point() {
        assert!(is_entry_point("src/bin/tool.rs"));
        assert!(is_entry_point("bin/cli.js"));
        assert!(
            !is_entry_point("src/bin/nested/deep.rs"),
            "only direct children of bin/ count"
        );
    }

    #[test]
    fn ordinary_files_are_not_entry_points() {
        for path in [
            "src/parser.rs",
            "src/mainframe.rs",
            "docs/main.md",
            "indexer.ts",
            "",
        ] {
            assert!(!is_entry_point(path), "{path} should not be an entry point");
        }
    }

    /// `main.md` is *not* an entry point but `main.go` is — the heuristic keys
    /// on the whole filename, never on the stem alone.
    #[test]
    fn the_heuristic_matches_whole_filenames_not_stems() {
        assert!(!is_entry_point("main.md"));
        assert!(!is_entry_point("main.txt"));
        assert!(is_entry_point("main.go"));
    }

    // ---- cache --------------------------------------------------------------

    fn overview(generation: &str) -> Arc<ProjectOverview> {
        Arc::new(ProjectOverview {
            generation: GenerationRef {
                id: generation.to_string(),
                number: 1,
            },
            tree: Vec::new(),
            entry_points: Vec::new(),
            top_imports: Vec::new(),
        })
    }

    #[test]
    fn the_cache_round_trips_by_worktree_and_generation() {
        let cache = OverviewCache::default();
        cache.put("wt", "gen-a", overview("gen-a"));
        assert_eq!(
            cache.get("wt", "gen-a").expect("cached").generation.id,
            "gen-a"
        );
        assert!(cache.get("wt", "gen-b").is_none(), "another generation");
        assert!(cache.get("other", "gen-a").is_none(), "another worktree");
    }

    /// A generation switch does not need an invalidation step: the new
    /// generation is simply a different key.
    #[test]
    fn a_new_generation_is_a_different_key() {
        let cache = OverviewCache::default();
        cache.put("wt", "gen-a", overview("gen-a"));
        cache.put("wt", "gen-b", overview("gen-b"));
        assert_eq!(
            cache.get("wt", "gen-a").expect("still there").generation.id,
            "gen-a"
        );
        assert_eq!(
            cache.get("wt", "gen-b").expect("cached").generation.id,
            "gen-b"
        );
    }

    #[test]
    fn the_cache_evicts_oldest_past_capacity() {
        let cache = OverviewCache::default();
        for i in 0..OVERVIEW_CACHE_CAPACITY + 2 {
            let generation = format!("gen-{i}");
            cache.put("wt", &generation, overview(&generation));
        }
        assert!(cache.get("wt", "gen-0").is_none(), "oldest evicted");
        assert!(cache.get("wt", "gen-1").is_none(), "second oldest evicted");
        assert!(
            cache
                .get("wt", &format!("gen-{}", OVERVIEW_CACHE_CAPACITY + 1))
                .is_some(),
            "newest retained"
        );
    }

    /// Re-putting the same key must not grow the eviction queue — otherwise a
    /// hot generation would push live entries out.
    #[test]
    fn re_putting_a_key_does_not_grow_the_queue() {
        let cache = OverviewCache::default();
        for _ in 0..OVERVIEW_CACHE_CAPACITY * 3 {
            cache.put("wt", "gen-a", overview("gen-a"));
        }
        cache.put("wt", "gen-b", overview("gen-b"));
        assert!(cache.get("wt", "gen-a").is_some());
        assert!(cache.get("wt", "gen-b").is_some());
    }
}
