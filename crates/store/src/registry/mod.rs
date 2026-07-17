//! The repository/worktree registry in `state.sqlite` (spec 03 §2.1, 02 §3).
//!
//! The registry is where a request context (`{session_id, worktree_root?,
//! repo_hint?}`, spec 02 §3.3) is resolved to durable identity. Two rules from
//! spec 01 §5 shape it:
//!
//! - **No durable ID is derived from a filesystem path.** `repository.repo_id`
//!   is a random UUIDv7; the path a repo is observed at lives only in
//!   `repository_path`, never as an identity. There is deliberately no
//!   `canonical_path` column on `repository` — `repository_path` is the single
//!   source of the current path.
//! - **The git remote is a hint, not identity** (spec 12 §7):
//!   `repository.git_remote_fingerprint` is nullable and NOT unique, so the same
//!   remote may map to more than one repository.
//!
//! This module owns two migrations of the registry:
//!
//! - **repository side** (T02-02, version-1 `SCHEMA_V1`): the low-level
//!   create/find/observe operations (see [`create_repository`],
//!   [`observe_repository_path`], and the `find`/`current`/`history` reads).
//! - **worktree side** (T02-03, version-2 `SCHEMA_V2`): the `worktree`,
//!   `worktree_path`, and `generation` tables — whose circular composite FK
//!   proves the current generation belongs to its worktree — plus the worktree
//!   operations and the explicit `active`/`detached`/`removing` state machine
//!   (see [`worktree`]).
//!
//! On top of those primitives sits the **resolution layer** (T02-04, module
//! `resolve`): [`resolve()`] turns a request's `worktree_root` into
//! `{repo_id, worktree_id}` (or *global scope only*) via the registry, and
//! [`attach()`] re-binds an existing identity to a new path after a directory
//! move (spec 02 §3.3, 04 §7). Auto-resolution is by *current* path only; the
//! path/remote/common-dir fingerprints are advisory (spec 12 §7).
//!
//! Per-repository **settings** (T02-05, module `settings`) sit alongside the
//! resolution layer: generic `repo_settings` reads/writes (spec 03 §2.1) plus the
//! typed `data_policy` accessor and the effective-policy merge — the *most
//! restrictive* of the global and every involved repository's policy (spec 02
//! §3.2, 12 §1). The `generation` builder, occurrence schema (spec 03 §2.4), and
//! generation state machine (spec 04 §1) are group 05 — T02-03 ships only the
//! `generation` table and the worktree-side seam that writes
//! `worktree.current_generation_id`.

mod repository;
mod resolve;
mod settings;
pub mod worktree;

pub use repository::{
    PathObservation, create_repository, current_path, find_repositories_by_remote,
    find_repository_by_path, observe_repository_path, path_history,
};
pub use resolve::{
    AttachError, Candidate, RequestRoot, Resolution, WorktreeRootFacts, attach, resolve,
};
pub use settings::{
    DATA_POLICY_KEY, effective_data_policy, get_repo_setting, repo_data_policy, repo_settings,
    set_repo_data_policy, set_repo_setting,
};
pub use worktree::{
    IllegalWorktreeTransition, WorktreeKind, WorktreePathObservation, WorktreeState,
    WorktreeSummary, WorktreeTransitionError, create_worktree, current_generation,
    current_worktree_path, find_worktree_by_current_path, find_worktrees_by_path_fingerprint,
    observe_worktree_path, set_current_generation, transition_worktree_state,
    worktree_path_history, worktree_state, worktree_summary, worktrees_of_repo,
};

/// Version-1 migration DDL: the repository-side registry (spec 03 §2.1).
///
/// Byte-exact reproduction of the three `state.sqlite` §2.1 blocks this task
/// owns — `repository`, `repository_path` (+ the `repository_path_current`
/// partial unique index), and `repo_settings`. Referenced by
/// [`crate::migrate::ALL`] as migration version 1.
///
/// **Frozen once shipped.** The migration checksum is the SHA-256 of this text
/// (see [`crate::migrate::Migration::checksum`]); any edit — even whitespace or
/// a comment — changes the checksum and trips
/// [`ChecksumDrift`](crate::migrate::MigrationError::ChecksumDrift) on an
/// existing store. Future schema changes are new numbered migrations, never an
/// edit here.
pub(crate) const SCHEMA_V1: &str = "\
CREATE TABLE repository (
  repo_id                 TEXT PRIMARY KEY,           -- UUIDv7
  git_remote_fingerprint  TEXT,                       -- H(remote_fingerprint), nullable; NOT unique
  created_at              INTEGER NOT NULL,
  last_seen_at            INTEGER NOT NULL
);
-- No canonical_path column: repository_path is the single source of current path [FIXED].

CREATE TABLE repository_path (
  repo_id        TEXT NOT NULL REFERENCES repository(repo_id),
  observed_path  TEXT NOT NULL,                       -- canonical absolute form
  is_current     INTEGER NOT NULL CHECK (is_current IN (0,1)),
  first_seen_at  INTEGER NOT NULL,
  last_seen_at   INTEGER NOT NULL,
  PRIMARY KEY (repo_id, observed_path)
);
CREATE UNIQUE INDEX repository_path_current
  ON repository_path(repo_id) WHERE is_current = 1;

CREATE TABLE repo_settings (
  repo_id TEXT NOT NULL REFERENCES repository(repo_id),
  key     TEXT NOT NULL,
  value   TEXT NOT NULL,
  PRIMARY KEY (repo_id, key)
);
";

/// Version-2 migration DDL: the worktree-side registry (spec 03 §2.1).
///
/// Byte-exact reproduction of the three `state.sqlite` §2.1 blocks this task
/// owns — `worktree` (with its composite FK into `generation`), `worktree_path`
/// (+ the `worktree_path_current` partial unique index and the `worktree_path_fp`
/// lookup index), and `generation`. Referenced by [`crate::migrate::ALL`] as
/// migration version 2.
///
/// The three tables form a **circular** foreign-key graph — `worktree` references
/// `generation(generation_id, worktree_id)` and `generation` references
/// `worktree(worktree_id)` — so they MUST be created in one migration. SQLite
/// resolves foreign-key parents lazily (only at row write time), so the forward
/// reference to `generation` in the `worktree` definition is fine; the composite
/// FK target is valid because `generation` declares `UNIQUE (generation_id,
/// worktree_id)`.
///
/// **Frozen once shipped.** Like [`SCHEMA_V1`], the checksum is the SHA-256 of
/// this text (see [`crate::migrate::Migration::checksum`]); any edit changes the
/// checksum and trips [`ChecksumDrift`](crate::migrate::MigrationError::ChecksumDrift)
/// on an existing store. Future schema changes are new numbered migrations.
pub(crate) const SCHEMA_V2: &str = "\
CREATE TABLE worktree (
  worktree_id            TEXT PRIMARY KEY,            -- stable UUIDv7, NEVER path-derived [FIXED]
  repo_id                TEXT NOT NULL REFERENCES repository(repo_id),
  kind                   TEXT NOT NULL CHECK (kind IN ('main','linked','non_git')),
  current_generation_id  TEXT,
  state                  TEXT NOT NULL CHECK (state IN ('active','detached','removing')),
  created_at             INTEGER NOT NULL,
  last_seen_at           INTEGER NOT NULL,
  -- composite FK proves the current generation belongs to THIS worktree [SPEC]:
  FOREIGN KEY (current_generation_id, worktree_id)
    REFERENCES generation(generation_id, worktree_id)
);
-- App invariant (asserted in tests): the referenced generation is in state 'active'.

CREATE TABLE worktree_path (
  worktree_id              TEXT NOT NULL REFERENCES worktree(worktree_id),
  observed_canonical_path  TEXT NOT NULL,
  display_path             TEXT NOT NULL,
  path_fingerprint         TEXT NOT NULL,             -- lookup accelerator ONLY, not identity
  is_current               INTEGER NOT NULL CHECK (is_current IN (0,1)),
  first_seen_at            INTEGER NOT NULL,
  last_seen_at             INTEGER NOT NULL,
  PRIMARY KEY (worktree_id, observed_canonical_path)
);
CREATE UNIQUE INDEX worktree_path_current
  ON worktree_path(worktree_id) WHERE is_current = 1;
CREATE INDEX worktree_path_fp ON worktree_path(path_fingerprint);

CREATE TABLE generation (
  generation_id      TEXT PRIMARY KEY,                -- UUIDv7
  worktree_id        TEXT NOT NULL REFERENCES worktree(worktree_id),
  generation_number  INTEGER NOT NULL,
  state              TEXT NOT NULL CHECK
    (state IN ('building','projection_ready','active','retiring','failed')),
  created_at         INTEGER NOT NULL,
  UNIQUE (worktree_id, generation_number),
  UNIQUE (generation_id, worktree_id)                 -- target for composite FKs
);
";
