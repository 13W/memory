//! Git-probed worktree facts for MCP tool requests (spec 02 §3.3's as-built
//! note: "the daemon (T15) supplies already-canonicalized, git-probed
//! `WorktreeRootFacts`... because `local-rag-store` carries no git/network
//! dependency") — T15-03.
//!
//! [`request_root`] is total: any probe failure — no `worktree_root` in the
//! request context, a path that does not exist, the `git` binary missing, a
//! non-zero `git` exit — collapses to `worktree_root: None`, which
//! `local_rag_store::registry::resolve` turns into `Resolution::GlobalOnly`.
//! Spec 02 §3.3's own rule ("an unresolvable root resolves to `GlobalOnly` —
//! never an error") is realized structurally, not by discipline: [`probe`]
//! returns `Option`, so there is no error type to leak.
//!
//! Shells out to `git` (the same precedent `crates/xtask/src/bench/run.rs`'s
//! `git rev-parse --short HEAD` already sets) rather than adding a `git2`
//! dependency — this daemon needs three read-only facts, not a git
//! implementation.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use local_rag_core::identity::domain::path_fingerprint;
use local_rag_core::identity::path::{CaseSensitivity, canonicalize_absolute};
use local_rag_core::identity::remote::fingerprint as remote_url_fingerprint;
use local_rag_protocol::RequestContext;
use local_rag_store::{RequestRoot, RootResolver, WorktreeKind, WorktreeRootFacts};

/// Build a [`RequestRoot`] from an MCP request's context. Never fails: an
/// absent or unprobeable `worktree_root` simply yields `None`.
pub fn request_root(ctx: &RequestContext) -> RequestRoot {
    RequestRoot {
        worktree_root: ctx.worktree_root.as_deref().map(Path::new).and_then(probe),
        repo_hint: ctx.repo_hint.clone(),
    }
}

/// The spool importer's [`RootResolver`] (D-063): git-probes a frame's raw
/// `worktree_root` through [`probe`], the same way [`request_root`] does for a
/// live MCP request, memoizing by the raw string.
///
/// One instance per sweep, not one per daemon: a sweep touches every known
/// session, and the sessions of one repository all report the same `cwd`, so
/// the cache collapses N sessions into one `git` invocation — while a fresh
/// instance each sweep keeps a path that has since appeared, moved, or become
/// a git repository from being answered from a stale probe for the daemon's
/// whole lifetime.
///
/// A poisoned cache mutex is recovered from rather than propagated: the map is
/// a pure accelerator, and a panic mid-insert can leave it stale-but-valid at
/// worst (the probe itself is a pure function of the path).
#[derive(Default)]
pub struct ProbingRootResolver {
    probed: Mutex<HashMap<String, RequestRoot>>,
}

impl RootResolver for ProbingRootResolver {
    fn resolve_root(&self, raw_worktree_root: Option<&str>) -> RequestRoot {
        // No `cwd` in any of the batch's frames: nothing to probe, and spec
        // 07 §5's "an unknown root imports with NULL worktree" applies
        // literally.
        let Some(raw) = raw_worktree_root else {
            return RequestRoot::default();
        };
        let mut cache = self.probed.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(hit) = cache.get(raw) {
            return hit.clone();
        }
        // A spool frame carries no repo hint — the hook writes a `cwd` and
        // nothing else about identity (spec 07 §3's frame shape).
        let root = RequestRoot {
            worktree_root: probe(Path::new(raw)),
            repo_hint: None,
        };
        cache.insert(raw.to_string(), root.clone());
        root
    }
}

/// This platform's default filesystem case sensitivity — insensitive on
/// macOS/Windows, sensitive elsewhere. `crates/index/src/reconcile/driver.rs`
/// documents this exact ownership split: "the filesystem's case sensitivity
/// is not persisted in `state.sqlite`, so the daemon determines it (platform
/// default / probe) and passes it in." No live filesystem probe: creating a
/// temp file to test case-folding behavior would touch the caller's worktree
/// on every request, and a wrong guess only changes the *fold* applied to an
/// already-canonical string — bounded, not silent corruption. `pub` so
/// T15-07's indexer can call the same function and never drift from this
/// probe's own fold.
pub fn case_sensitivity() -> CaseSensitivity {
    if cfg!(any(target_os = "macos", target_os = "windows")) {
        CaseSensitivity::Insensitive
    } else {
        CaseSensitivity::Sensitive
    }
}

/// Probe `path`: git-classify it, canonicalize it, and compute the advisory
/// fingerprints. `None` only when the path itself cannot be canonicalized
/// (does not exist, inaccessible) — a path that canonicalizes but is not
/// inside a git repository still yields `Some` with
/// [`WorktreeKind::NonGit`].
pub fn probe(path: &Path) -> Option<WorktreeRootFacts> {
    let case = case_sensitivity();
    let (toplevel, kind, common_dir) = match git_facts(path) {
        Some((toplevel, kind, common_dir)) => (toplevel, kind, common_dir),
        None => (path.to_path_buf(), WorktreeKind::NonGit, None),
    };

    let canonical = canonicalize_absolute(&toplevel, case).ok()?;
    let common_dir_fingerprint = common_dir
        .and_then(|dir| canonicalize_absolute(&dir, case).ok())
        .map(|c| path_fingerprint(&c.canonical));
    let remote_fingerprint = match kind {
        WorktreeKind::NonGit => None,
        WorktreeKind::Main | WorktreeKind::Linked => {
            remote_origin_url(path).map(|url| remote_url_fingerprint(&url))
        }
    };

    Some(WorktreeRootFacts {
        observed_canonical_path: canonical.canonical.clone(),
        display_path: canonical.display,
        path_fingerprint: path_fingerprint(&canonical.canonical),
        kind,
        common_dir_fingerprint,
        remote_fingerprint,
    })
}

/// `(toplevel, kind, common_dir)` for a git worktree rooted at or above
/// `path`, or `None` if `path` is not inside a git repository at all (or
/// `git` itself could not be run).
///
/// `toplevel` is the snap target: `path` may be a *subdirectory* of the
/// worktree (an MCP session's `worktree_root` is the proxy's launch
/// `current_dir()`, which is often a package subdirectory, not the repo
/// root), and `local_rag_store::registry::resolve`'s only automatic path
/// matches against the *recorded worktree root* — without snapping, a
/// session started in `repo/packages/api/` would never resolve.
fn git_facts(path: &Path) -> Option<(PathBuf, WorktreeKind, Option<PathBuf>)> {
    // git >= 2.31: one call, everything already absolute.
    let (toplevel, git_dir, common_dir) = run_git(
        path,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-dir",
            "--git-common-dir",
        ],
    )
    .and_then(three_lines)
    .or_else(|| {
        // Fallback for older git: `--absolute-git-dir` (>= 2.13) is
        // absolute by name; `--git-common-dir` may still come back
        // relative to `path` here, resolved below before use.
        run_git(
            path,
            &[
                "rev-parse",
                "--show-toplevel",
                "--absolute-git-dir",
                "--git-common-dir",
            ],
        )
        .and_then(three_lines)
    })?;

    let git_dir = resolve_against(path, &git_dir);
    let common_dir = resolve_against(path, &common_dir);
    let kind = classify(&git_dir, &common_dir);
    Some((
        PathBuf::from(toplevel),
        kind,
        Some(PathBuf::from(common_dir)),
    ))
}

/// Split `git rev-parse`'s three-line stdout into its three fields, or
/// `None` if it did not produce exactly three non-empty lines.
fn three_lines(stdout: String) -> Option<(String, String, String)> {
    let mut lines = stdout.lines();
    let a = lines.next()?.to_string();
    let b = lines.next()?.to_string();
    let c = lines.next()?.to_string();
    if a.is_empty() || b.is_empty() || c.is_empty() {
        return None;
    }
    Some((a, b, c))
}

/// Resolve a possibly-relative git-reported path against `base` — the
/// fallback `--git-common-dir` form is not guaranteed absolute.
fn resolve_against(base: &Path, maybe_relative: &str) -> String {
    let candidate = Path::new(maybe_relative);
    if candidate.is_absolute() {
        maybe_relative.to_string()
    } else {
        base.join(candidate).to_string_lossy().into_owned()
    }
}

/// `Main` iff this tree's own admin dir *is* the shared common dir (`git
/// worktree add` gives a linked tree a `--git-dir` of
/// `<common>/worktrees/<name>` while `--git-common-dir` stays `<common>`) —
/// the exact discriminator, not a heuristic. Pure string comparison so this
/// is unit-testable without a real git repository.
fn classify(git_dir: &str, common_dir: &str) -> WorktreeKind {
    if git_dir == common_dir {
        WorktreeKind::Main
    } else {
        WorktreeKind::Linked
    }
}

/// `git config --get remote.origin.url`, or `None` if the key is absent
/// (exit 1 — normal, not a failure) or the value is empty.
fn remote_origin_url(path: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["config", "--get", "remote.origin.url"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let url = String::from_utf8(output.stdout).ok()?;
    let trimmed = url.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Run `git -C <path> <args>`, returning stdout only on a zero exit — any
/// failure (not a git repo, `git` missing, I/O error) is `None`, never an
/// `Err` a caller could mistake for something worth reporting.
fn run_git(path: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use local_rag_test_support::TempHome;

    #[test]
    fn classify_equal_dirs_as_main() {
        assert_eq!(classify("/repo/.git", "/repo/.git"), WorktreeKind::Main);
    }

    #[test]
    fn classify_differing_dirs_as_linked() {
        assert_eq!(
            classify("/repo/.git/worktrees/feature", "/repo/.git"),
            WorktreeKind::Linked
        );
    }

    #[test]
    fn request_root_with_no_worktree_root_is_none() {
        let ctx = RequestContext {
            session_id: "sess-1".to_string(),
            worktree_root: None,
            repo_hint: Some("repo-1".to_string()),
        };
        let root = request_root(&ctx);
        assert_eq!(root.worktree_root, None);
        assert_eq!(root.repo_hint, Some("repo-1".to_string()));
    }

    #[test]
    fn a_nonexistent_path_probes_to_none() {
        assert!(probe(Path::new("/definitely/does/not/exist/xyz-123")).is_none());
    }

    #[test]
    fn a_plain_existing_directory_probes_as_non_git() {
        let home = TempHome::new().expect("temp home");
        let dir = home.join("plain-dir");
        std::fs::create_dir_all(&dir).expect("create dir");
        let facts = probe(&dir).expect("an existing, non-git directory still probes");
        assert_eq!(facts.kind, WorktreeKind::NonGit);
        assert_eq!(facts.common_dir_fingerprint, None);
        assert_eq!(facts.remote_fingerprint, None);
    }

    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "local-rag-test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "local-rag-test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed");
    }

    #[test]
    fn a_real_git_repo_probes_as_main() {
        if !git_available() {
            eprintln!("skip: git not on PATH");
            return;
        }
        let home = TempHome::new().expect("temp home");
        let repo = home.join("repo");
        std::fs::create_dir_all(&repo).expect("create repo dir");
        git(&repo, &["init", "-q"]);

        let facts = probe(&repo).expect("probe succeeds for a real git repo");
        assert_eq!(facts.kind, WorktreeKind::Main);
        assert_eq!(facts.remote_fingerprint, None);
    }

    #[test]
    fn probing_from_a_subdirectory_snaps_to_the_toplevel() {
        if !git_available() {
            eprintln!("skip: git not on PATH");
            return;
        }
        let home = TempHome::new().expect("temp home");
        let repo = home.join("repo");
        let sub = repo.join("packages").join("api");
        std::fs::create_dir_all(&sub).expect("create subdirectory");
        git(&repo, &["init", "-q"]);

        let facts = probe(&sub).expect("probe succeeds from a subdirectory");
        let expected = canonicalize_absolute(&repo, case_sensitivity()).expect("canonicalize repo");
        assert_eq!(facts.observed_canonical_path, expected.canonical);
    }

    #[test]
    fn a_remote_url_is_fingerprinted() {
        if !git_available() {
            eprintln!("skip: git not on PATH");
            return;
        }
        let home = TempHome::new().expect("temp home");
        let repo = home.join("repo");
        std::fs::create_dir_all(&repo).expect("create repo dir");
        git(&repo, &["init", "-q"]);
        git(
            &repo,
            &[
                "remote",
                "add",
                "origin",
                "https://example.invalid/org/repo.git",
            ],
        );

        let facts = probe(&repo).expect("probe succeeds");
        assert_eq!(
            facts.remote_fingerprint,
            Some(remote_url_fingerprint(
                "https://example.invalid/org/repo.git"
            ))
        );
    }

    /// D-063: a batch whose frames carry no `cwd` costs no probe at all and
    /// answers exactly what the importer treated as universal before — an
    /// unresolvable root.
    #[test]
    fn the_probing_resolver_answers_an_absent_root_without_probing() {
        let resolver = ProbingRootResolver::default();
        let root = resolver.resolve_root(None);
        assert_eq!(root.worktree_root, None);
        assert_eq!(root.repo_hint, None);
        assert!(resolver.probed.lock().expect("cache").is_empty());
    }

    /// A path that no longer exists (a repository deleted after its session
    /// was spooled) degrades to the same unresolvable root rather than
    /// failing the import.
    #[test]
    fn the_probing_resolver_answers_a_vanished_root_as_unresolvable() {
        let resolver = ProbingRootResolver::default();
        let root = resolver.resolve_root(Some("/definitely/does/not/exist/xyz-123"));
        assert_eq!(root.worktree_root, None);
    }

    /// The raw string is the cache key and the probe runs once for it: a sweep
    /// over many sessions of one repository must not spawn one `git` per
    /// session. Two *different* raw strings that snap to the same toplevel are
    /// still two entries — the key is the frame's own `cwd`, deliberately not
    /// the canonicalized answer (computing that is the probe).
    #[test]
    fn the_probing_resolver_probes_once_per_distinct_raw_root() {
        if !git_available() {
            eprintln!("skip: git not on PATH");
            return;
        }
        let home = TempHome::new().expect("temp home");
        let repo = home.join("repo");
        let sub = repo.join("packages").join("api");
        std::fs::create_dir_all(&sub).expect("create subdirectory");
        git(&repo, &["init", "-q"]);

        let resolver = ProbingRootResolver::default();
        let repo_raw = repo.to_str().expect("utf-8 path");
        let sub_raw = sub.to_str().expect("utf-8 path");

        let first = resolver.resolve_root(Some(repo_raw));
        let again = resolver.resolve_root(Some(repo_raw));
        let from_sub = resolver.resolve_root(Some(sub_raw));

        let facts = first.worktree_root.as_ref().expect("a real repo probes");
        assert_eq!(facts.kind, WorktreeKind::Main);
        assert_eq!(again.worktree_root, first.worktree_root);
        // Both raw paths snap to the same worktree toplevel (probe's own
        // behavior, `probing_from_a_subdirectory_snaps_to_the_toplevel`).
        assert_eq!(
            from_sub
                .worktree_root
                .as_ref()
                .expect("a subdirectory probes")
                .observed_canonical_path,
            facts.observed_canonical_path
        );
        assert_eq!(resolver.probed.lock().expect("cache").len(), 2);
    }

    #[test]
    fn a_linked_worktree_probes_as_linked() {
        if !git_available() {
            eprintln!("skip: git not on PATH");
            return;
        }
        let home = TempHome::new().expect("temp home");
        let main = home.join("main");
        std::fs::create_dir_all(&main).expect("create main dir");
        git(&main, &["init", "-q"]);
        // A worktree needs at least one commit to attach a branch to.
        std::fs::write(main.join("f.txt"), "x").expect("seed file");
        git(&main, &["add", "f.txt"]);
        git(&main, &["commit", "-q", "-m", "seed"]);
        let linked = home.join("linked");
        git(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                linked.to_str().expect("utf-8 path"),
                "-b",
                "feature",
            ],
        );

        let facts = probe(&linked).expect("probe succeeds for a linked worktree");
        assert_eq!(facts.kind, WorktreeKind::Linked);
    }
}
