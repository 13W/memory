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
//! This module (T02-02) owns the **repository side** of the registry: the
//! version-1 migration (`SCHEMA_V1`) and the low-level create/find/observe
//! operations (see [`create_repository`], [`observe_repository_path`], and the
//! `find`/`current`/`history` reads). The worktree/generation side (its circular
//! FKs) lands in a later migration (T02-03); `repo_settings` merge/data-policy
//! ordering is T02-05, so this module ships the `repo_settings` table but no
//! settings operations.

mod repository;

pub use repository::{
    PathObservation, create_repository, current_path, find_repositories_by_remote,
    find_repository_by_path, observe_repository_path, path_history,
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
