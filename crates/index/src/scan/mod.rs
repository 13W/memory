//! Authoritative worktree tree scan (spec 06 §1–2, steps 1–2 of the reconcile
//! pipeline) — T05-02.
//!
//! [`scan`] walks a worktree root with the `ignore` crate, honoring gitignore
//! semantics, and returns a **canonical sorted manifest** of the indexable file
//! candidates — each with its worktree-relative `normalized_path`/`display_path`,
//! size, and (unless it is a `huge` stat-only skip) its exact-content
//! `content_hash`. This realizes spec 06 §2 steps "fast stat scan (gitignore-aware,
//! `ignore` crate)" and "content-hash suspicious files".
//!
//! **Principle (spec 06, `[FIXED]`): watcher = hint, reconcile = truth.** The scan
//! is the authoritative truth: it reads the real tree, never an event stream. The
//! [`StatCache`] fast path is advisory only (spec 06 §1); [`ScanMode::Strict`]
//! bypasses it and content-hashes every candidate (the watcher-overflow / cold-
//! start / periodic entry, spec 06 §1 `[FIXED]`).
//!
//! # Scope (T05-02)
//!
//! This module produces the candidate manifest only. It does **not**:
//! - run the content-based skip classification (`lfs`/`binary`/`encoding`/`secret`
//!   via [`classify`](crate::classify)) — that reads bytes on a manifest miss in
//!   the generation builder (T05-03);
//! - write `skipped_file`/`file_revision` rows or build a generation (T05-03);
//! - diff tree A→B or share structure across generations (T05-03);
//! - own the watcher/debounce/overflow triggers (T05-04) — it only exposes
//!   [`ScanMode`] as the seam those triggers select.
//!
//! `ignored` files are pruned by the walk and are therefore simply **absent** from
//! the manifest (spec 06 §2.2 / §10: skipped files are absent from the searchable
//! generation); no `skipped_file(reason='ignored')` row exists in v0. The `huge`
//! gate is applied here because it is stat-only (spec 06 §2.2 reason 2, "content is
//! not read"): a candidate larger than the cap keeps its manifest entry with
//! `content_hash = None` and its bytes are never read.
//!
//! No git binary is invoked and no git crate is linked (guardrail until T15):
//! git-awareness is purely `WorktreeKind` → `ignore` walk configuration.

pub mod stat_cache;

use std::io;
use std::path::Path;

use ignore::WalkBuilder;
use local_rag_core::identity::path::{CaseSensitivity, normalize_relative};
use local_rag_store::WorktreeKind;
use local_rag_store::content_hash;

pub use stat_cache::{StatCache, StatKey};

/// How aggressively [`scan`] resolves content identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanMode {
    /// Trust the advisory [`StatCache`] fast path: a candidate whose
    /// `(mtime, size, file_id)` matches its cached tuple reuses the cached
    /// `content_hash` without re-reading (spec 06 §1). Any mismatch or doubt
    /// escalates to hashing.
    Fast,
    /// Ignore the [`StatCache`] and content-hash every candidate. The mandatory
    /// mode for watcher overflow, cold start, and periodic reconcile (spec 06 §1
    /// `[FIXED]`: "never resync from events").
    Strict,
}

/// One indexable file candidate in a [`ScanManifest`] (spec 06 §2).
///
/// `normalized_path` is a manifest attribute and the sort key — never a durable id
/// (spec 01 §5.1: no durable ID is derived from a filesystem path; identity flows
/// from `content_hash`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestEntry {
    /// Worktree-relative canonical path (`/`-separated, NFC, optionally
    /// case-folded).
    pub normalized_path: String,
    /// The path's preserved original spelling.
    pub display_path: String,
    /// File size in bytes (from stat).
    pub size: u64,
    /// Exact-content `H(file_content)` (spec 03 §1.2), directly comparable to
    /// `file_revision.content_hash`. `None` iff the file exceeds the size cap
    /// (`huge`, stat-only): its bytes are never read here.
    pub content_hash: Option<String>,
}

/// The canonical sorted manifest of a worktree scan (spec 06 §2).
///
/// `entries` are sorted by `(normalized_path, display_path)`, so the manifest is a
/// deterministic function of the tree's bytes — independent of walk order, mtimes,
/// and the advisory cache's state.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ScanManifest {
    /// The indexable candidates, canonically sorted.
    pub entries: Vec<ManifestEntry>,
}

/// Fast-path telemetry for one scan (not part of the manifest, so it never affects
/// determinism): how many candidate bytes were hashed vs reused from the cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ScanStats {
    /// Candidates whose bytes were read and hashed.
    pub hashed: usize,
    /// Candidates whose `content_hash` was reused from the advisory cache.
    pub reused: usize,
}

/// Scan the worktree rooted at `root`, returning the canonical sorted manifest and
/// fast-path telemetry (spec 06 §1–2).
///
/// - `root`: the worktree's canonical absolute path (from the registry).
/// - `kind`: git-aware ([`Main`](WorktreeKind::Main)/[`Linked`](WorktreeKind::Linked))
///   vs non-git ([`NonGit`](WorktreeKind::NonGit)). Non-git still honors
///   `.gitignore` files present (spec 06 §6: "reconcile identically minus git
///   triggers").
/// - `case`: the worktree filesystem's case sensitivity (from the registry).
/// - `max_file_size_bytes`: the `huge` cap; a candidate strictly larger is kept
///   with `content_hash = None` and never read (spec 06 §2.2 reason 2).
/// - `mode`: [`ScanMode::Fast`] consults `cache`; [`ScanMode::Strict`] hashes all.
/// - `prune_roots`: worktree-relative paths whose subtrees are excluded (nested
///   registered worktrees — a linked worktree's `.git` is a file the `ignore`
///   crate does not prune; the daemon (T05-04) supplies these from the registry).
/// - `cache`: the advisory fast-path cache, updated in place (spec 06 §1).
///
/// # Errors
///
/// Returns an [`io::Error`] if `root` does not exist or is not a directory, or if
/// the walk / a stat / a read fails.
pub fn scan(
    root: &Path,
    kind: WorktreeKind,
    case: CaseSensitivity,
    max_file_size_bytes: u64,
    mode: ScanMode,
    prune_roots: &[String],
    cache: &mut StatCache,
) -> io::Result<(ScanManifest, ScanStats)> {
    // Fail fast on a non-existent or non-directory root (rather than yielding an
    // empty manifest that masks a bad request).
    let root_meta = std::fs::metadata(root)?;
    if !root_meta.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("worktree root is not a directory: {}", root.display()),
        ));
    }

    let git_aware = matches!(kind, WorktreeKind::Main | WorktreeKind::Linked);

    // Prune the internal `.git` (dir in a main tree, file in a linked worktree) and
    // any caller-supplied nested-worktree roots. `.git` must never be indexed;
    // `hidden(false)` (below) would otherwise descend into it.
    let prunes = PruneSet::new(root, case, prune_roots);

    let mut builder = WalkBuilder::new(root);
    builder
        // Determinism: never read the user's global gitignore ($HOME /
        // core.excludesFile) or ignore files in parent directories above the
        // root — both would leak host state into the manifest (spec 06 §1 truth).
        .git_global(false)
        .parents(false)
        // Do not follow symbolic links (avoids cycles and out-of-tree escapes);
        // symlinks are excluded below by the regular-file filter.
        .follow_links(false)
        // Index dotfiles (e.g. `.github/`); gitignore still applies, and `.git`
        // itself is pruned explicitly by `prunes`.
        .hidden(false)
        // Honor `.gitignore` / `.git/info/exclude` / `.ignore`.
        .git_ignore(true)
        .git_exclude(true)
        .ignore(true)
        // Non-git worktrees still honor `.gitignore` even without a `.git`
        // (spec 06 §6 parity); git worktrees require it as usual.
        .require_git(git_aware)
        .filter_entry(move |entry| prunes.keep(entry));

    let mut entries = Vec::new();
    let mut stats = ScanStats::default();

    for result in builder.build() {
        let entry = result.map_err(io::Error::other)?;
        // Skip the root itself.
        if entry.depth() == 0 {
            continue;
        }
        // Regular files only: this is false for directories, symlinks, FIFOs,
        // sockets, and devices, giving a single deterministic exclusion.
        match entry.file_type() {
            Some(ft) if ft.is_file() => {}
            _ => continue,
        }

        let path = entry.path();
        let rel = path.strip_prefix(root).map_err(io::Error::other)?;
        let canonical = normalize_relative(&rel.to_string_lossy(), case);

        let metadata = entry.metadata().map_err(io::Error::other)?;
        let size = metadata.len();

        let content_hash = if size > max_file_size_bytes {
            // `huge`: stat-only, bytes never read (spec 06 §2.2 reason 2).
            None
        } else {
            Some(resolve_content_hash(
                path,
                &canonical.canonical,
                &metadata,
                mode,
                cache,
                &mut stats,
            )?)
        };

        entries.push(ManifestEntry {
            normalized_path: canonical.canonical,
            display_path: canonical.display,
            size,
            content_hash,
        });
    }

    entries.sort_by(|a, b| {
        (a.normalized_path.as_str(), a.display_path.as_str())
            .cmp(&(b.normalized_path.as_str(), b.display_path.as_str()))
    });
    debug_assert!(
        entries
            .windows(2)
            .all(|w| w[0].normalized_path != w[1].normalized_path),
        "two candidates share a normalized_path (wrong CaseSensitivity?)",
    );

    Ok((ScanManifest { entries }, stats))
}

/// Resolve a candidate's `content_hash`, using the advisory cache in
/// [`ScanMode::Fast`] and always reading in [`ScanMode::Strict`] (spec 06 §1).
fn resolve_content_hash(
    path: &Path,
    normalized_path: &str,
    metadata: &std::fs::Metadata,
    mode: ScanMode,
    cache: &mut StatCache,
    stats: &mut ScanStats,
) -> io::Result<String> {
    let key = StatKey::from_metadata(metadata);

    // Fast consults the advisory cache; Strict always reads (spec 06 §1).
    let cached = match mode {
        ScanMode::Fast => cache.reuse(normalized_path, &key),
        ScanMode::Strict => None,
    };
    if let Some(hash) = cached {
        stats.reused += 1;
        return Ok(hash.to_string());
    }

    // Miss / doubt / strict → read the exact bytes and hash them with the store's
    // `H(file_content)` so the manifest hash is comparable to
    // `file_revision.content_hash`.
    let bytes = std::fs::read(path)?;
    let hash = content_hash(&bytes);
    cache.record(normalized_path.to_string(), key, hash.clone());
    stats.hashed += 1;
    Ok(hash)
}

/// The set of subtree roots the walk must prune: the internal `.git` plus any
/// caller-supplied nested-worktree roots, matched against worktree-relative
/// canonical paths.
struct PruneSet {
    root: std::path::PathBuf,
    case: CaseSensitivity,
    nested: Vec<String>,
}

impl PruneSet {
    fn new(root: &Path, case: CaseSensitivity, prune_roots: &[String]) -> PruneSet {
        PruneSet {
            root: root.to_path_buf(),
            case,
            nested: prune_roots
                .iter()
                .map(|p| normalize_relative(p, case).canonical)
                .collect(),
        }
    }

    /// Whether the walk should keep (and descend into) `entry`.
    fn keep(&self, entry: &ignore::DirEntry) -> bool {
        // Always keep the root.
        if entry.depth() == 0 {
            return true;
        }
        let Ok(rel) = entry.path().strip_prefix(&self.root) else {
            return true;
        };
        let norm = normalize_relative(&rel.to_string_lossy(), self.case).canonical;

        // Prune the internal `.git` at any depth (git never tracks it), and any
        // path under a caller-supplied nested-worktree root.
        if norm.split('/').any(|c| c == ".git") {
            return false;
        }
        !self
            .nested
            .iter()
            .any(|r| norm == *r || norm.starts_with(&format!("{r}/")))
    }
}
