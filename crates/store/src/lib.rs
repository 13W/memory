//! `local-rag` durable storage layer.
//!
//! This crate owns the SQLite databases described in spec 03: the canonical
//! `state.sqlite` (source of truth) and the rebuildable `cache.sqlite`.
//!
//! T01-02 introduces the **state open policy** and the **bounded writer**:
//!
//! - every connection to `state.sqlite` applies the normative pragmas
//!   (`journal_mode=WAL`, `foreign_keys=ON`, `synchronous=FULL`,
//!   `busy_timeout=5000`; spec 03 §2);
//! - all writes flow through a single bounded async queue feeding one writer
//!   task, so SQLite's single physical writer is respected and producers see
//!   backpressure (spec 02 §5 L4a, 03 §3);
//! - no writable [`rusqlite::Connection`] ever leaves this crate — the only way
//!   to mutate `state.sqlite` is [`StateWriter::transaction`]. Reads use a
//!   read-only connection ([`StateDb::open_read`]) that cannot write.
//!
//! T01-03 adds the **forward-only migration runner** ([`migrate`]): every
//! `StateDb::open` bootstraps `schema_migrations`/`store_settings`, checks
//! compatibility, and applies pending migrations under the migration lock (L1)
//! before the writer spawns. T01-04 extends it with **resumable/destructive
//! mechanics**: complex migrations apply as per-unit checkpoints
//! (`migration_progress`) so a crash resumes exactly, and a `destructive`
//! migration takes a `VACUUM INTO` backup into `<root>/backups/` before any
//! mutation.
//!
//! T01-05 adds the **cache open policy** ([`CacheDb`]): `cache.sqlite` is opened
//! with its own pragmas (`foreign_keys=OFF`, `synchronous=NORMAL`; spec 03 §4),
//! bound to a store via `cache_meta`, and dropped & rebuilt when incompatible or
//! corrupt (03 §4.4). It is never migrated (13 §3). Its writes flow through a
//! **separate** bounded queue ([`CacheWriter`], 02 §5 L4b), physically distinct
//! from state's, so state and cache can never share one transaction — the
//! writable cross-database `ATTACH` prohibition (03 §1.4) is structural.
//!
//! T02-02 adds the **repository registry** ([`registry`]): the version-1
//! migration creates the repository-side tables (`repository`,
//! `repository_path`, `repo_settings`; spec 03 §2.1), and the module's
//! operations create/find repositories and observe their current path while
//! keeping exactly one current path per repository. `repository.repo_id` is a
//! random UUIDv7, never path-derived (spec 01 §5); the git remote fingerprint is
//! a nullable, non-unique hint (spec 12 §7).
//!
//! T02-03 adds the **worktree registry** (version-2 migration): the
//! `worktree`/`worktree_path`/`generation` tables — whose circular composite FK
//! proves the current generation belongs to its worktree — plus worktree
//! create/observe/transition operations and the explicit `active`/`detached`/
//! `removing` state machine (spec 04 §7). `worktree.worktree_id` is a random,
//! caller-minted UUIDv7 (never path-derived); `worktree_path.path_fingerprint`
//! is a lookup accelerator only, never identity (spec 01 §5).
//!
//! T02-04 adds the **resolution layer** on top of those primitives:
//! [`resolve`] turns a request's `worktree_root` into `{repo_id, worktree_id}` —
//! or *global scope only* when it does not resolve — with no ambient current
//! project (spec 02 §3.3), and [`attach`] re-binds an existing identity to a new
//! path after a directory move (spec 04 §7). Auto-resolution matches strictly on
//! a worktree's current observed path; the path/remote/common-dir fingerprints are
//! advisory hints, never identity (spec 12 §7), so a recreated path never steals a
//! moved worktree's identity and ambiguous linked worktrees require an explicit
//! attach.
//!
//! `rusqlite` is re-exported so downstream crates share one SQLite vocabulary
//! (`local_rag_store::rusqlite`).

pub use local_rag_core::VERSION;
pub use rusqlite;

mod cache;
mod clock;
pub mod migrate;
pub mod registry;
mod state;

pub use cache::{
    CACHE_SCHEMA_VERSION, CacheDb, CacheOpenError, CacheOpenOutcome, CacheWriteError, CacheWriter,
};
pub use migrate::{ALL, Migration, MigrationError, MigrationReport, MigrationStep, StepFn};
pub use registry::{
    AttachError, Candidate, IllegalWorktreeTransition, PathObservation, RequestRoot, Resolution,
    WorktreeKind, WorktreePathObservation, WorktreeRootFacts, WorktreeState, WorktreeSummary,
    WorktreeTransitionError, attach, create_repository, create_worktree, current_generation,
    current_path, current_worktree_path, find_repositories_by_remote, find_repository_by_path,
    find_worktree_by_current_path, find_worktrees_by_path_fingerprint, observe_repository_path,
    observe_worktree_path, path_history, resolve, set_current_generation,
    transition_worktree_state, worktree_path_history, worktree_state, worktree_summary,
    worktrees_of_repo,
};
pub use state::{DEFAULT_WRITE_QUEUE_CAPACITY, OpenError, StateDb, StateWriter, WriteError};
