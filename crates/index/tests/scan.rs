//! T05-02 acceptance tests for the authoritative tree scan (spec 06 §1–2).
//!
//! These build **real** on-disk trees under an isolated [`TempHome`] (the first
//! code in the workspace to run `ignore::Walk` over a real tree) and assert the
//! canonical manifest, gitignore semantics, symlink/rename/delete handling, the
//! advisory stat cache + strict escalation, non-git parity, `.git`/prune-root
//! exclusion, and determinism.
//!
//! Determinism without wall-clock races: the manifest never carries mtime or
//! cache state, so it is a pure function of the tree bytes. The stat-cache tests
//! drive *controlled* [`StatKey`] tuples (a seeded cache) rather than sleeping to
//! force real mtime changes. Symlink cases are `#[cfg(unix)]`.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use local_rag_core::identity::path::CaseSensitivity;
use local_rag_index::scan::{ManifestEntry, ScanManifest, ScanMode, StatCache, StatKey, scan};
use local_rag_store::{WorktreeKind, content_hash};
use local_rag_test_support::TempHome;

/// A temporary worktree root on disk (a `wt/` subdir of an isolated home).
fn tree() -> (TempHome, PathBuf) {
    let home = TempHome::new().expect("temp home");
    let root = home.join("wt");
    fs::create_dir_all(&root).expect("create root");
    (home, root)
}

/// Write `contents` to `root/rel`, creating parent directories.
fn write(root: &Path, rel: &str, contents: &[u8]) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parents");
    }
    fs::write(path, contents).expect("write file");
}

/// Scan with the common test defaults (case-sensitive, 1 MiB cap, no prune roots,
/// a fresh cache) and return the manifest.
fn scan_default(root: &Path, kind: WorktreeKind, mode: ScanMode) -> ScanManifest {
    let mut cache = StatCache::new();
    scan(
        root,
        kind,
        CaseSensitivity::Sensitive,
        1 << 20,
        mode,
        &[],
        &mut cache,
    )
    .expect("scan")
    .0
}

/// The set of `normalized_path`s in a manifest.
fn paths(manifest: &ScanManifest) -> BTreeSet<String> {
    manifest
        .entries
        .iter()
        .map(|e| e.normalized_path.clone())
        .collect()
}

/// The single entry for `normalized_path`.
fn entry<'a>(manifest: &'a ScanManifest, normalized_path: &str) -> &'a ManifestEntry {
    manifest
        .entries
        .iter()
        .find(|e| e.normalized_path == normalized_path)
        .unwrap_or_else(|| panic!("no manifest entry for {normalized_path}"))
}

#[test]
fn nested_gitignore_and_negation() {
    let (_home, root) = tree();
    write(&root, ".gitignore", b"*.txt\n");
    write(&root, "docs/.gitignore", b"!keep.txt\n");
    write(&root, "notes.txt", b"n");
    write(&root, "docs/other.txt", b"o");
    write(&root, "docs/keep.txt", b"k");
    write(&root, "src/main.rs", b"fn main() {}\n");

    // Non-git mode still honors `.gitignore` (require_git(false)).
    let m = scan_default(&root, WorktreeKind::NonGit, ScanMode::Fast);
    let p = paths(&m);

    assert!(p.contains("src/main.rs"), "plain source indexed");
    assert!(
        p.contains("docs/keep.txt"),
        "deep negation re-includes keep.txt: {p:?}"
    );
    assert!(!p.contains("notes.txt"), "root *.txt ignored");
    assert!(!p.contains("docs/other.txt"), "nested *.txt ignored");
    // `.gitignore` files are themselves tracked text (git tracks them), so with
    // hidden(false) they appear as regular candidates.
    assert!(p.contains(".gitignore"));
    assert!(p.contains("docs/.gitignore"));
    // Manifest is canonically sorted.
    let sorted: Vec<_> = m.entries.iter().map(|e| &e.normalized_path).collect();
    let mut expected = sorted.clone();
    expected.sort();
    assert_eq!(sorted, expected, "entries are sorted by normalized_path");
}

#[cfg(unix)]
#[test]
fn symlinks_are_excluded_and_not_followed() {
    use std::os::unix::fs::symlink;

    let (_home, root) = tree();
    write(&root, "real.rs", b"fn real() {}\n");
    write(&root, "sub/inner.rs", b"fn inner() {}\n");
    // A symlink to an in-tree file, a symlink to an in-tree dir, and a dangling one.
    symlink(root.join("real.rs"), root.join("link.rs")).expect("file symlink");
    symlink(root.join("sub"), root.join("sub_link")).expect("dir symlink");
    symlink(root.join("nowhere"), root.join("dangling.rs")).expect("dangling symlink");

    let p = paths(&scan_default(&root, WorktreeKind::NonGit, ScanMode::Fast));

    // Only the real regular files; every symlink (file/dir/dangling) is excluded,
    // and the dir symlink is not followed (no duplicate `sub_link/inner.rs`).
    assert_eq!(
        p,
        BTreeSet::from(["real.rs".to_string(), "sub/inner.rs".to_string()]),
        "symlinks excluded, not followed: {p:?}",
    );
}

#[test]
fn delete_absent_and_rename_moves_content_hash() {
    let (_home, root) = tree();
    write(&root, "a.rs", b"content A\n");
    write(&root, "b.rs", b"content B\n");

    let first = scan_default(&root, WorktreeKind::NonGit, ScanMode::Fast);
    let a_hash = entry(&first, "a.rs").content_hash.clone();
    assert!(a_hash.is_some());
    assert!(paths(&first).contains("b.rs"));

    // Delete b.rs; rename a.rs -> c.rs (same bytes at a new path).
    fs::remove_file(root.join("b.rs")).expect("delete b");
    fs::rename(root.join("a.rs"), root.join("c.rs")).expect("rename a->c");

    let second = scan_default(&root, WorktreeKind::NonGit, ScanMode::Fast);
    let p = paths(&second);

    assert!(!p.contains("b.rs"), "deleted file is absent");
    assert!(!p.contains("a.rs"), "renamed-away path is absent");
    assert!(p.contains("c.rs"), "renamed-to path is present");
    assert_eq!(
        entry(&second, "c.rs").content_hash,
        a_hash,
        "rename preserves content_hash (structural sharing signal for T05-03)",
    );
}

#[test]
fn stat_mismatch_escalates_to_hashing() {
    let (_home, root) = tree();
    write(&root, "f.rs", b"hello\n");

    // Seed the cache with a stat tuple that does NOT match the file, paired with a
    // bogus hash. A Fast scan must detect the mismatch and re-hash.
    let mut cache = StatCache::new();
    cache_seed(&mut cache, "f.rs", stat_key(999, Some(0)), "deadbeef");

    let manifest = scan(
        &root,
        WorktreeKind::NonGit,
        CaseSensitivity::Sensitive,
        1 << 20,
        ScanMode::Fast,
        &[],
        &mut cache,
    )
    .expect("scan")
    .0;

    assert_eq!(
        entry(&manifest, "f.rs").content_hash.as_deref(),
        Some(content_hash(b"hello\n").as_str()),
        "stat mismatch re-hashes the real content, not the stale cache",
    );
}

#[test]
fn fast_trusts_full_match_but_strict_rehashes_all() {
    let (_home, root) = tree();
    write(&root, "f.rs", b"hello\n");

    // Seed the cache with the file's *true* stat tuple but a WRONG hash.
    let true_key = StatKey::from_metadata(&fs::metadata(root.join("f.rs")).expect("stat"));
    let mut cache = StatCache::new();
    cache_seed(&mut cache, "f.rs", true_key, "wronghash");

    // Fast trusts a full stat match: it returns the (wrong) cached hash without
    // reading. This documents the advisory risk that periodic strict reconcile
    // (spec 06 §1) exists to correct — the cache is a hint, not truth.
    let fast = scan(
        &root,
        WorktreeKind::NonGit,
        CaseSensitivity::Sensitive,
        1 << 20,
        ScanMode::Fast,
        &[],
        &mut cache,
    )
    .expect("fast scan")
    .0;
    assert_eq!(
        entry(&fast, "f.rs").content_hash.as_deref(),
        Some("wronghash"),
        "Fast trusts a full stat match (advisory)",
    );

    // Strict ignores the cache and re-hashes every candidate → the real hash.
    let strict = scan(
        &root,
        WorktreeKind::NonGit,
        CaseSensitivity::Sensitive,
        1 << 20,
        ScanMode::Strict,
        &[],
        &mut cache,
    )
    .expect("strict scan")
    .0;
    assert_eq!(
        entry(&strict, "f.rs").content_hash.as_deref(),
        Some(content_hash(b"hello\n").as_str()),
        "Strict re-hashes all, ignoring the advisory cache",
    );
}

#[test]
fn strict_reused_stats_reports_reuse_zero() {
    let (_home, root) = tree();
    write(&root, "a.rs", b"a\n");
    write(&root, "b.rs", b"b\n");

    // Warm the cache with a Fast scan.
    let mut cache = StatCache::new();
    let (_m1, s1) = scan(
        &root,
        WorktreeKind::NonGit,
        CaseSensitivity::Sensitive,
        1 << 20,
        ScanMode::Fast,
        &[],
        &mut cache,
    )
    .expect("warm scan");
    assert_eq!((s1.hashed, s1.reused), (2, 0), "cold scan hashes all");

    // A second Fast scan reuses both from the warm cache.
    let (_m2, s2) = scan(
        &root,
        WorktreeKind::NonGit,
        CaseSensitivity::Sensitive,
        1 << 20,
        ScanMode::Fast,
        &[],
        &mut cache,
    )
    .expect("warm scan 2");
    assert_eq!((s2.hashed, s2.reused), (0, 2), "warm Fast scan reuses all");

    // Strict never reuses, even with a warm cache.
    let (_m3, s3) = scan(
        &root,
        WorktreeKind::NonGit,
        CaseSensitivity::Sensitive,
        1 << 20,
        ScanMode::Strict,
        &[],
        &mut cache,
    )
    .expect("strict scan");
    assert_eq!((s3.hashed, s3.reused), (2, 0), "Strict hashes all");
}

#[test]
fn non_git_and_git_modes_produce_the_same_manifest() {
    let (_home, root) = tree();
    write(&root, ".gitignore", b"*.log\n");
    write(&root, "a.rs", b"fn a() {}\n");
    write(&root, "b.log", b"noise\n");

    // Non-git worktree (no `.git`): `.gitignore` still honored.
    let non_git = scan_default(&root, WorktreeKind::NonGit, ScanMode::Strict);

    // Make it a git repo and scan git-aware.
    write(&root, ".git/HEAD", b"ref: refs/heads/main\n");
    write(&root, ".git/config", b"[core]\n");
    let git = scan_default(&root, WorktreeKind::Main, ScanMode::Strict);

    assert_eq!(
        non_git, git,
        "non-git and git modes agree (spec 06 §6 parity)"
    );
    let p = paths(&git);
    assert!(p.contains("a.rs") && p.contains(".gitignore"));
    assert!(!p.contains("b.log"), "gitignored in both modes");
    assert!(
        !p.iter().any(|s| s.starts_with(".git/")),
        ".git internals never indexed: {p:?}",
    );
}

#[test]
fn git_directory_is_never_indexed() {
    let (_home, root) = tree();
    write(&root, "a.rs", b"fn a() {}\n");
    write(&root, ".git/HEAD", b"ref: refs/heads/main\n");
    write(&root, ".git/objects/pack/x", b"binary-ish\n");

    let p = paths(&scan_default(&root, WorktreeKind::Main, ScanMode::Fast));
    assert!(p.contains("a.rs"));
    assert!(
        !p.iter().any(|s| s == ".git" || s.starts_with(".git/")),
        ".git pruned even with hidden(false): {p:?}",
    );
}

#[test]
fn prune_roots_excludes_nested_worktree_subtree() {
    let (_home, root) = tree();
    write(&root, "a.rs", b"fn a() {}\n");
    write(&root, ".worktrees/wt1/inner.rs", b"fn inner() {}\n");

    // Without pruning, the nested subtree is walked.
    let unpruned = paths(&scan_default(&root, WorktreeKind::NonGit, ScanMode::Fast));
    assert!(unpruned.contains(".worktrees/wt1/inner.rs"));

    // With the nested worktree root pruned, its subtree is absent.
    let mut cache = StatCache::new();
    let pruned = scan(
        &root,
        WorktreeKind::NonGit,
        CaseSensitivity::Sensitive,
        1 << 20,
        ScanMode::Fast,
        &[".worktrees/wt1".to_string()],
        &mut cache,
    )
    .expect("scan")
    .0;
    let p = paths(&pruned);
    assert!(p.contains("a.rs"));
    assert!(
        !p.contains(".worktrees/wt1/inner.rs"),
        "pruned subtree is absent: {p:?}",
    );
}

#[test]
fn huge_files_keep_an_entry_with_no_hash() {
    let (_home, root) = tree();
    write(&root, "small.rs", b"tiny\n");
    write(&root, "big.rs", &[b'x'; 100]);

    let mut cache = StatCache::new();
    let manifest = scan(
        &root,
        WorktreeKind::NonGit,
        CaseSensitivity::Sensitive,
        16, // cap: 16 bytes
        ScanMode::Fast,
        &[],
        &mut cache,
    )
    .expect("scan")
    .0;

    let small = entry(&manifest, "small.rs");
    assert_eq!(
        small.content_hash.as_deref(),
        Some(content_hash(b"tiny\n").as_str()),
        "under-cap file is hashed",
    );
    let big = entry(&manifest, "big.rs");
    assert_eq!(big.size, 100);
    assert_eq!(big.content_hash, None, "huge file is stat-only, not read");
}

#[test]
fn empty_tree_yields_empty_manifest() {
    let (_home, root) = tree();
    let manifest = scan_default(&root, WorktreeKind::NonGit, ScanMode::Fast);
    assert!(manifest.entries.is_empty());
}

#[test]
fn nonexistent_root_is_an_error() {
    let (_home, root) = tree();
    let missing = root.join("does-not-exist");
    let mut cache = StatCache::new();
    let result = scan(
        &missing,
        WorktreeKind::NonGit,
        CaseSensitivity::Sensitive,
        1 << 20,
        ScanMode::Fast,
        &[],
        &mut cache,
    );
    assert!(result.is_err(), "missing root is an io error");
}

#[test]
fn a_file_root_is_an_error() {
    let (_home, root) = tree();
    write(&root, "f.rs", b"x\n");
    let mut cache = StatCache::new();
    let result = scan(
        &root.join("f.rs"),
        WorktreeKind::NonGit,
        CaseSensitivity::Sensitive,
        1 << 20,
        ScanMode::Fast,
        &[],
        &mut cache,
    );
    assert!(result.is_err(), "a non-directory root is rejected");
}

#[test]
fn manifest_is_deterministic_across_scans() {
    let (_home, root) = tree();
    write(&root, "z/last.rs", b"z\n");
    write(&root, "a/first.rs", b"a\n");
    write(&root, "mid.rs", b"m\n");

    let first = scan_default(&root, WorktreeKind::NonGit, ScanMode::Strict);
    let second = scan_default(&root, WorktreeKind::NonGit, ScanMode::Strict);
    assert_eq!(first, second, "two scans of an identical tree are equal");

    // And a warm-cache Fast scan produces the identical manifest (reuse yields the
    // same hash re-reading would).
    let mut cache = StatCache::new();
    let warm1 = scan(
        &root,
        WorktreeKind::NonGit,
        CaseSensitivity::Sensitive,
        1 << 20,
        ScanMode::Fast,
        &[],
        &mut cache,
    )
    .expect("warm 1")
    .0;
    let warm2 = scan(
        &root,
        WorktreeKind::NonGit,
        CaseSensitivity::Sensitive,
        1 << 20,
        ScanMode::Fast,
        &[],
        &mut cache,
    )
    .expect("warm 2")
    .0;
    assert_eq!(first, warm1);
    assert_eq!(warm1, warm2, "Fast reuse does not change the manifest");
}

// --- small helpers that need module-private constructors via the public API ---

/// Build a [`StatKey`] with a controlled size/file id and a fixed epoch mtime (the
/// tests drive tuples explicitly; real mtimes are never depended on).
fn stat_key(size: u64, file_id: Option<u64>) -> StatKey {
    StatKey {
        mtime: std::time::SystemTime::UNIX_EPOCH,
        size,
        file_id,
    }
}

/// Seed a cache entry through the public API used by the scanner
/// ([`StatCache::record`]).
fn cache_seed(cache: &mut StatCache, normalized_path: &str, key: StatKey, hash: &str) {
    cache.record(normalized_path.to_string(), key, hash.to_string());
}
