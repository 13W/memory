//! Request-root resolution and worktree re-attach (spec 02 §3.3, 04 §7, 12 §7).
//!
//! This is the composition layer above the low-level `repository`/`worktree`
//! primitives. It turns a request's `worktree_root` into durable identity
//! (`{repo_id, worktree_id}`) — or into *global scope only* when the root does
//! not resolve — and re-binds an existing identity to a new path after a
//! directory move.
//!
//! Two rules from spec 01 §5 / 02 §3.3 shape the design:
//!
//! - **No ambient current project, and no identity from a path.** Resolution is
//!   an explicit registry lookup keyed on the request's `worktree_root`; there is
//!   no process-global "current" worktree. Auto-resolution matches strictly on a
//!   worktree's *current* observed canonical path
//!   ([`find_worktree_by_current_path`]). A `path_fingerprint`, the
//!   `git_remote_fingerprint` hint (spec 12 §7), and the daemon's common-dir /
//!   admin-dir fingerprint are all **advisory** — they can surface reattach
//!   *candidates*, but never mint a [`Resolution::Resolved`] on their own.
//! - **The store carries no git dependency** (architecture guardrail until T10).
//!   Git probing — classifying `kind` (`main`/`linked`/`non_git`), computing the
//!   common-dir fingerprint, normalizing the remote — is the daemon's job (T15);
//!   it hands finished, canonicalized [`WorktreeRootFacts`] here, mirroring how
//!   `repo_id`/`worktree_id` are minted by the caller and `CaseSensitivity` is
//!   supplied by the caller elsewhere in the registry.
//!
//! First-time registration (discovery) is deliberately **not** an operation
//! here: it is the caller minting a UUIDv7 and composing
//! [`create_repository`](super::create_repository) /
//! [`create_worktree`](super::create_worktree) / the `observe_*` primitives, so
//! identity minting stays out of the store (keeping entropy out of the write
//! path). This module adds only the resolver ([`resolve`]) and the re-attach
//! write ([`attach`]).

use std::collections::BTreeMap;

use rusqlite::{Connection, Transaction};

use super::repository::{find_repositories_by_remote, observe_repository_path};
use super::worktree::{
    IllegalWorktreeTransition, WorktreeKind, WorktreeState, WorktreeSummary,
    WorktreeTransitionError, find_worktree_by_current_path, find_worktrees_by_path_fingerprint,
    observe_worktree_path, transition_worktree_state, worktree_summary, worktrees_of_repo,
};

/// Canonicalized, git-probed facts about a request's `worktree_root`, supplied by
/// the daemon (spec 02 §3.3). The store crate never touches git/filesystem/network
/// (guardrail until T10): the caller canonicalizes the path (core
/// [`identity::path::Canonical`](local_rag_core::identity::path::Canonical)),
/// classifies `kind`, and computes the advisory fingerprints, then hands finished
/// facts here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeRootFacts {
    /// The canonical absolute path (the identity form,
    /// [`Canonical::canonical`](local_rag_core::identity::path::Canonical)).
    pub observed_canonical_path: String,
    /// The preserved original spelling of the path (identity never depends on it).
    pub display_path: String,
    /// `H(path_fingerprint, canonical_path)` — a lookup accelerator only, never
    /// identity (spec 01 §5). Computed by the caller via
    /// [`identity::domain::path_fingerprint`](local_rag_core::identity::domain::path_fingerprint).
    pub path_fingerprint: String,
    /// The worktree class from the daemon's git probe.
    pub kind: WorktreeKind,
    /// The daemon's common-dir / admin-dir fingerprint, if any. **Advisory only**:
    /// this module never stores or queries it — there is no column for it, which
    /// is the structural realization of "may serve as a hint, never as the sole
    /// ID" (spec 04 §7). Carried so the daemon can derive a `repo_hint`.
    pub common_dir_fingerprint: Option<String>,
    /// The repository's `H(remote_fingerprint)`, if it has a remote (spec 12 §7).
    /// An advisory hint used only to widen the reattach-candidate search.
    pub remote_fingerprint: Option<String>,
}

/// The registry-facing form of the request context `{session_id, worktree_root?,
/// repo_hint?}` (spec 02 §3.3), minus the routing-only `session_id`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RequestRoot {
    /// The probed root facts, or `None` for a request that carries no worktree.
    /// Either way an unresolvable root yields [`Resolution::GlobalOnly`] — never
    /// an error (spec 02 §3.3).
    pub worktree_root: Option<WorktreeRootFacts>,
    /// An optional `repo_id`, used **only** to break a tie between reattach
    /// candidates. It is never a lookup key into identity (spec 01 §5), and a
    /// repo-level hint cannot pick between two linked worktrees of one repository
    /// (that needs an explicit worktree-level [`attach`], spec 04 §7).
    pub repo_hint: Option<String>,
}

/// A detached worktree that *might* be the request's root, surfaced by an advisory
/// hint (path fingerprint or remote fingerprint) when no current path matched.
/// Binding one is always explicit (via [`attach`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// The candidate's repository.
    pub repo_id: String,
    /// The candidate worktree.
    pub worktree_id: String,
    /// Its kind (always equal to the requested `kind`).
    pub kind: WorktreeKind,
}

/// The outcome of resolving a request's `worktree_root` (spec 02 §3.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// The root resolved unambiguously to durable identity.
    Resolved {
        /// The resolved repository.
        repo_id: String,
        /// The resolved worktree.
        worktree_id: String,
    },
    /// No worktree resolved; the request operates in global scope only (spec
    /// 02 §3.3 / §6 `WORKTREE_NOT_INDEXED`). Not an error.
    GlobalOnly,
    /// Advisory hints surfaced reattach candidates but none could be chosen
    /// automatically; the caller must re-bind one explicitly via [`attach`]
    /// (spec 04 §7). This is a normal typed outcome, not an error.
    Ambiguous {
        /// The candidate worktrees, ascending by `worktree_id`.
        candidates: Vec<Candidate>,
    },
}

/// Why an [`attach`] request was rejected at the domain level (as opposed to an
/// infrastructure/SQLite failure, which surfaces as the outer [`rusqlite::Error`]
/// and rolls the transaction back).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachError {
    /// No `worktree` row has the given id.
    UnknownWorktree,
    /// The worktree exists but belongs to a different repository than claimed.
    RepoMismatch {
        /// The `repo_id` the caller claimed.
        expected_repo: String,
        /// The `repo_id` the worktree actually belongs to.
        actual_repo: String,
    },
    /// The worktree is `removing` (terminal) and cannot be reattached (spec 04 §7).
    NotReattachable(IllegalWorktreeTransition),
}

impl std::fmt::Display for AttachError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AttachError::UnknownWorktree => write!(f, "unknown worktree"),
            AttachError::RepoMismatch {
                expected_repo,
                actual_repo,
            } => write!(
                f,
                "worktree belongs to repository {actual_repo}, not {expected_repo}"
            ),
            AttachError::NotReattachable(ill) => write!(f, "worktree is not reattachable: {ill}"),
        }
    }
}

impl std::error::Error for AttachError {}

/// Resolve a request's `worktree_root` to durable identity (spec 02 §3.3).
///
/// The outer [`rusqlite::Result`] carries only SQLite failures; [`Resolution`] —
/// including [`Resolution::Ambiguous`] — is a normal read outcome, because
/// resolution is a pure read.
///
/// The algorithm:
///
/// 1. No `worktree_root` → [`Resolution::GlobalOnly`].
/// 2. An **exact current** canonical-path match → [`Resolution::Resolved`]. This
///    is the *only* automatic resolution: a returning move, a recreated path, or
///    a remote match never resolves on its own.
/// 3. Otherwise gather the advisory detached candidates (see below). Empty →
///    [`Resolution::GlobalOnly`].
/// 4. With candidates: a `repo_hint` that selects exactly one →
///    [`Resolution::Resolved`]; anything else → [`Resolution::Ambiguous`].
///
/// Why this yields the required behaviors: because auto-resolution is
/// current-path-only and candidates are restricted to `detached` worktrees of the
/// requested `kind`, a worktree that merely moved away (and is still `active` at
/// its new home) is not a candidate — so a **recreated** path resolves to
/// `GlobalOnly` (or to a freshly registered worktree), never stealing the moved
/// worktree's identity; an **unknown** root with no hint match is `GlobalOnly`;
/// and two detached linked worktrees of one repository are `Ambiguous`, since a
/// repo-level hint cannot choose between them (spec 04 §7).
pub fn resolve(conn: &Connection, request: &RequestRoot) -> rusqlite::Result<Resolution> {
    let Some(facts) = request.worktree_root.as_ref() else {
        return Ok(Resolution::GlobalOnly);
    };

    // (2) The only automatic resolution: an exact current-path match.
    if let Some(worktree_id) = find_worktree_by_current_path(conn, &facts.observed_canonical_path)?
        && let Some(summary) = worktree_summary(conn, &worktree_id)?
    {
        return Ok(Resolution::Resolved {
            repo_id: summary.repo_id,
            worktree_id: summary.worktree_id,
        });
    }

    // (3) Advisory detached candidates from the path/remote fingerprints.
    let candidates = detached_candidates(conn, facts)?;
    if candidates.is_empty() {
        return Ok(Resolution::GlobalOnly);
    }

    // (4) Only an explicit hint selecting exactly one candidate auto-resolves.
    if let Some(hint) = request.repo_hint.as_deref() {
        let mut matching = candidates.iter().filter(|c| c.repo_id == hint);
        if let (Some(only), None) = (matching.next(), matching.next()) {
            return Ok(Resolution::Resolved {
                repo_id: only.repo_id.clone(),
                worktree_id: only.worktree_id.clone(),
            });
        }
    }
    Ok(Resolution::Ambiguous { candidates })
}

/// The advisory detached-worktree candidates for `facts`, deduplicated by
/// `worktree_id` and ordered ascending (a [`BTreeMap`] keeps it deterministic).
///
/// A candidate must be `detached` **and** of the requested `kind`. Candidates come
/// from two advisory sources:
///
/// 1. worktrees carrying this exact `path_fingerprint` (current or historical) — a
///    return to a previously-seen path;
/// 2. if a remote-fingerprint hint is present, every worktree of every repository
///    sharing it — the store-side stand-in for the daemon's common-dir hint.
fn detached_candidates(
    conn: &Connection,
    facts: &WorktreeRootFacts,
) -> rusqlite::Result<Vec<Candidate>> {
    let mut by_id: BTreeMap<String, Candidate> = BTreeMap::new();

    for worktree_id in find_worktrees_by_path_fingerprint(conn, &facts.path_fingerprint)? {
        if let Some(summary) = worktree_summary(conn, &worktree_id)? {
            insert_candidate(&mut by_id, summary, facts);
        }
    }

    if let Some(remote_fp) = facts.remote_fingerprint.as_deref() {
        for repo_id in find_repositories_by_remote(conn, remote_fp)? {
            for summary in worktrees_of_repo(conn, &repo_id)? {
                insert_candidate(&mut by_id, summary, facts);
            }
        }
    }

    Ok(by_id.into_values().collect())
}

/// Record `summary` as a candidate iff it is `detached` and of the requested
/// `kind`; a no-op otherwise. Idempotent per `worktree_id`.
fn insert_candidate(
    by_id: &mut BTreeMap<String, Candidate>,
    summary: WorktreeSummary,
    facts: &WorktreeRootFacts,
) {
    if summary.state == WorktreeState::Detached && summary.kind == facts.kind {
        by_id
            .entry(summary.worktree_id.clone())
            .or_insert_with(|| Candidate {
                repo_id: summary.repo_id.clone(),
                worktree_id: summary.worktree_id.clone(),
                kind: summary.kind,
            });
    }
}

/// Re-bind an existing identity (`repo_id` + `worktree_id`) to the observed root
/// (spec 04 §7) — the `local-rag repo attach` operation.
///
/// The outer [`rusqlite::Result`] carries only infrastructure failures (which roll
/// the whole transaction back, including the state flip). The inner [`Result`]
/// carries the domain outcome; every domain rejection is detected **before** any
/// write, so the enclosing transaction commits a no-op:
///
/// - no such worktree → `Ok(Err(AttachError::UnknownWorktree))`;
/// - the worktree belongs to another repository →
///   `Ok(Err(AttachError::RepoMismatch { .. }))`;
/// - the worktree is `removing` (terminal) →
///   `Ok(Err(AttachError::NotReattachable(..)))`.
///
/// On success it composes the existing primitives in one transaction: drive the
/// worktree back to `active` (a `detached → active` reattach, or an idempotent
/// `active → active` no-op), observe the new current path on the worktree (history
/// retained), and — only when the worktree is the repository's root tree
/// (`main`/`non_git`, decided by the **stored** `kind`, not the caller-supplied
/// one) — sync `repository_path` too. Linked worktrees never move the main
/// checkout's path (spec 04 §7). Same ids, new current path, old paths retained:
/// a directory move preserves identity.
pub fn attach(
    tx: &Transaction<'_>,
    repo_id: &str,
    worktree_id: &str,
    facts: &WorktreeRootFacts,
    now_ms: i64,
) -> rusqlite::Result<Result<(), AttachError>> {
    // 1) The worktree must exist.
    let Some(summary) = worktree_summary(tx, worktree_id)? else {
        return Ok(Err(AttachError::UnknownWorktree));
    };
    // 2) …and belong to the claimed repository.
    if summary.repo_id != repo_id {
        return Ok(Err(AttachError::RepoMismatch {
            expected_repo: repo_id.to_string(),
            actual_repo: summary.repo_id,
        }));
    }
    // 3) Drive it back to `active`. A `removing` worktree is terminal and not
    //    reattachable; the illegality is detected before any write.
    match transition_worktree_state(tx, worktree_id, WorktreeState::Active, now_ms)? {
        Ok(()) => {}
        Err(WorktreeTransitionError::Illegal(ill)) => {
            return Ok(Err(AttachError::NotReattachable(ill)));
        }
        // Unreachable after step 1 inside one transaction, handled for totality.
        Err(WorktreeTransitionError::UnknownWorktree) => {
            return Ok(Err(AttachError::UnknownWorktree));
        }
    }
    // 4) Observe the new current path on the worktree (history retained).
    observe_worktree_path(
        tx,
        worktree_id,
        &facts.observed_canonical_path,
        &facts.display_path,
        &facts.path_fingerprint,
        now_ms,
    )?;
    // 5) For the repository's root tree (main/non_git), keep `repository_path` in
    //    sync. The stored kind — not the caller's — governs this, so a caller
    //    cannot misdirect the repository path.
    if summary.kind != WorktreeKind::Linked {
        observe_repository_path(tx, repo_id, &facts.observed_canonical_path, now_ms)?;
    }
    Ok(Ok(()))
}
