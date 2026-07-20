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
//! T02-05 adds **per-repository settings and the `data_policy` merge**
//! ([`registry::set_repo_setting`]/[`registry::get_repo_setting`] and the typed
//! [`registry::repo_data_policy`]/[`registry::set_repo_data_policy`]): the generic
//! `repo_settings` table (spec 03 §2.1) whose keys mirror the global
//! `[models]`/`[index]` config, plus [`registry::effective_data_policy`], which
//! returns the *most restrictive* of the global and every involved repository's
//! policy (spec 02 §3.2, 12 §1). The global config itself is parsed by
//! [`local_rag_core::config`]; the central remote-policy guard is a later group.
//!
//! T03-01 adds the **code-storage side** ([`code`]): the version-3 migration
//! creates the exact-source tables — the content-shared, path-independent
//! `file_revision`/`content_blob`/`parsed_unit` (spec 03 §2.3) and the
//! path-dependent generation-membership tables `generation_file`/`skipped_file`/
//! `generation_unit_occurrence`/`unresolved_reference`/`resolved_graph_edge`
//! (spec 03 §2.4) — plus typed insert/read repositories and the CHECK-mirroring
//! enums. The strict source-blob invariant (spec 12 §5) is structural: an
//! occurrence exists only on a `generation_file` member (composite FK), so a
//! `skipped_file` never gets one. The deterministic `occurrence_id` derivation and
//! generation builder are group 05.
//!
//! T03-03 adds the **exact-source ingestion** layer ([`code::prepare_source`] and
//! [`code::create_or_reuse_file_revision`]): raw file bytes become a
//! `file_revision` via `H(file_content)` hashing (spec 03 §1.2), `newline_style`/
//! `source_encoding` detection, and a keep-if-smaller optional-zstd policy whose
//! [`code::source_bytes`] read-back reproduces the exact original bytes (spec 03
//! §2.3 `[FIXED]`, 12 §5). Revisions are created-or-reused by
//! `(content_hash, parser_fingerprint)` (structural sharing, spec 06 §2); the pure
//! preparation runs off the writer thread, and the transaction step is a single
//! lookup + insert. Building the `parser_fingerprint` from a real parser is T04-02
//! and the normalized-text cache derived from these bytes is T03-04.
//!
//! `rusqlite` is re-exported so downstream crates share one SQLite vocabulary
//! (`local_rag_store::rusqlite`).

pub use local_rag_core::VERSION;
pub use rusqlite;

mod cache;
mod clock;
pub mod code;
pub mod migrate;
pub mod registry;
mod state;

pub use cache::{
    BatchingLastUsed, CACHE_SCHEMA_VERSION, CacheDb, CacheOpenError, CacheOpenOutcome,
    CacheWriteError, CacheWriter, LastUsedSink, NormalizedTextRow, delete_normalized_text,
    flush_last_used, get_normalized_text, insert_normalized_text, verify_cached_text,
};
pub use code::{
    ALGO_VERSION, BlobOutcome, DerivedContentBlob, EdgeResolution, NORMALIZATION_VERSION,
    NewContentBlob, NewFileRevision, NewOccurrence, NewParsedUnit, NewResolvedEdge,
    NewUnresolvedReference, NewlineStyle, PreparedSource, RevisionOutcome, SOURCE_ENCODING_UTF8,
    SOURCE_ZSTD_LEVEL, SkipReason, SourceCompression, UnitKind, content_blob_exists,
    content_blob_id, content_hash, create_or_reuse_content_blob, create_or_reuse_file_revision,
    decode_source, derive_content_blob, detect_encoding, detect_newline_style,
    file_revision_id_by_content_key, insert_content_blob, insert_file_revision,
    insert_generation_file, insert_occurrence, insert_parsed_unit, insert_resolved_edge,
    insert_skipped_file, insert_unresolved_reference, member_file_revision, normalize,
    prepare_source, skip_reason, source_bytes,
};
pub use migrate::{ALL, Migration, MigrationError, MigrationReport, MigrationStep, StepFn};
pub use registry::{
    AttachError, Candidate, DATA_POLICY_KEY, IllegalWorktreeTransition, PathObservation,
    RequestRoot, Resolution, WorktreeKind, WorktreePathObservation, WorktreeRootFacts,
    WorktreeState, WorktreeSummary, WorktreeTransitionError, attach, create_repository,
    create_worktree, current_generation, current_path, current_worktree_path,
    effective_data_policy, find_repositories_by_remote, find_repository_by_path,
    find_worktree_by_current_path, find_worktrees_by_path_fingerprint, get_repo_setting,
    observe_repository_path, observe_worktree_path, path_history, repo_data_policy, repo_settings,
    resolve, set_current_generation, set_repo_data_policy, set_repo_setting,
    transition_worktree_state, worktree_path_history, worktree_state, worktree_summary,
    worktrees_of_repo,
};
pub use state::{DEFAULT_WRITE_QUEUE_CAPACITY, OpenError, StateDb, StateWriter, WriteError};
