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
//! T07-02 adds the **two-axis projection deployment state**
//! ([`registry::projection_state`]): the version-4 migration creates `model_space`
//! and `worktree_projection_state` (spec 03 §2.2) and seeds one default `active`
//! model space, and the module ships the guard layer — the [`ProjectionStatus`]
//! machine (`clean`/`updating`/`dirty`/`rebuilding`, spec 04 §2) with a pure
//! `check_transition`, the pure two-axis `check_invariants` truth table
//! (`clean ⇒ active == projected ∧ target NULL`; `updating ⇒ target ∧ op_id set`;
//! only one axis moves per switch, spec 04 §8/05 §5 `[FIXED]`), and the guarded
//! `write_projection_state` (read-then-write, no mutation on rejection). The
//! representation registry (`representation`/`model_space_representation`, the
//! canonical RepresentationKey, coverage, and the model-space build machine) is
//! T11-01; validate-on-open/rebuild is T07-04.
//!
//! T07-03 adds the one store-side reader the projection switch needs
//! ([`code::occurrence_ids_for_generation`]): every `occurrence_id` recorded for
//! a generation, ascending, served by the existing `occurrence_by_gen` index. The
//! switch orchestration itself — write-ahead → desired-set reconcile against a
//! `ProjectionStore` shard → commit, spec 05 §5 — lives in `local-rag-projection`
//! (`local_rag_projection::switch`), which depends on this crate for exactly this
//! reader plus [`registry::projection_state`], [`registry::generation`], and
//! [`registry::worktree`]'s `set_current_generation`.
//!
//! T08-01 adds the **FTS materialized-view schema and lexical preprocessing**
//! (spec 03 §4.3, 09 §2): the version-3 cache schema creates `fts_doc`/
//! `fts_occurrences` (FTS5)/`fts_projection_head`, and [`tokenize_identifier`]/
//! [`tokenize_qualified_name`]/[`tokenize_path`]/[`tokenize_signature`]
//! implement the versioned (`TOKENIZER_VERSION`) code-aware camelCase/
//! snake_case/kebab-case splitting required before insert (splitting runs on
//! original casing, each part folded via `casefold::simple_fold`; a
//! punctuation-free atom also gets a fused whole-token, since FTS5's
//! `unicode61` tokenizer has nothing left to split on for it). [`fts_manifest_hash`]
//! derives `fts_projection_head.manifest_hash` from the sorted-unique
//! occurrence-id set (spec 03 §1.2), generation-scoped only (no
//! `model_space_id` axis, unlike the dense projection's manifest).
//!
//! T08-02 adds the **generation materializer** ([`materialize_fts`], spec 06
//! §2/§4): the new store-side reader [`code::occurrences_for_fts`] joins
//! `generation_unit_occurrence ⋈ parsed_unit ⋈ content_blob` for a generation
//! (the first multi-table join in this crate), resolving each occurrence's
//! normalized body text via `normalized_text_cache` and recomputing it from
//! `source_blob` where evicted or absent (today's steady state for a cold
//! cache — this materializer is `normalized_text_cache`'s only production
//! writer). Because `occurrence_id` embeds `generation_id` (spec 03 §1.2), two
//! generations of one worktree never share an occurrence id, so every call
//! **fully replaces** the worktree's `fts_doc`/`fts_occurrences` rows with the
//! new generation's complete set and writes `fts_projection_head` **last**, all
//! inside one cache transaction (spec 06 §4 "single cache tx per generation
//! update"). [`read_fts_projection_head`] is a plain row accessor; per-search/
//! at-open validation and rebuild-on-divergence are T08-03.
//!
//! T09-01 adds the **lock hierarchy** ([`lock`], spec 02 §5): [`LockLevel`]
//! (`L0`…`L4b`, ranked so `L2Read`/`L2Write` and `L4a`/`L4b` share a rank —
//! siblings, not independently orderable) plus `debug`/`cargo test`-only
//! strict-order enforcement (a task may only acquire a strictly higher rank
//! than any it already holds) via `tokio::task_local!` — task-local rather
//! than thread-local because `L2.read` is meant to span an entire async
//! pipeline (spec 06 §3) across possible OS-thread migration on a
//! multi-threaded runtime. [`WorktreeLockRegistry`] realizes `L2` (a
//! per-worktree `RwLock`, entries permanent for now — eviction is `[OPEN]`).
//! `L1` (`migrate::run`) and `L4a`/`L4b` (`StateWriter`/`CacheWriter`) are
//! instrumented in place to actually participate (spec 02 §5 says "no
//! exceptions"); `L0`/`L3` ship as [`LockLevel`] variants only — no real
//! primitive exists yet (`store.lock` is T15's daemon lifecycle; the
//! shard-manager map is T09-02's). Because the write-queue job dispatch marks
//! itself as already holding the hierarchy's topmost rank, "L4 queues are
//! leaves" (spec 02 §5) is an enforced invariant: any lock acquisition
//! attempted from inside a queued job fails the order check. Adopting the
//! registry into the projection switch is later work (T11-05, group 11); the
//! reconcile driver's own adoption has no dedicated task yet in the current
//! plan. The read side is adopted by T09-03 (`local_rag_search::SearchEngine`,
//! `crates/search`) via [`lock::WorktreeLockRegistry::read_bounded`].
//!
//! `rusqlite` is re-exported so downstream crates share one SQLite vocabulary
//! (`local_rag_store::rusqlite`).

pub use local_rag_core::VERSION;
pub use rusqlite;

mod cache;
mod clock;
pub mod code;
pub mod housekeeping;
pub mod lock;
pub mod migrate;
pub mod registry;
pub mod retention;
mod state;

pub use cache::{
    BatchingLastUsed, CACHE_SCHEMA_VERSION, CacheDb, CacheOpenError, CacheOpenOutcome,
    CacheWriteError, CacheWriter, FTS_SYNC_REBUILD_OCCURRENCE_THRESHOLD, FtsAvailability,
    FtsDivergence, FtsMaterializeError, FtsMaterializeOutcome, FtsOpenOutcome,
    FtsProjectionHeadRow, FtsRebuildError, LEXICAL_SCHEMA_VERSION, LastUsedSink, NormalizedTextRow,
    TOKENIZER_VERSION, ValidationDepth, delete_normalized_text, flush_last_used,
    fts_doc_occurrence_count, fts_doc_occurrence_ids, fts_manifest_hash, get_normalized_text,
    insert_normalized_text, materialize_fts, open_and_validate_fts, read_fts_projection_head,
    requires_index_unavailable, should_rebuild_synchronously, tokenize_identifier, tokenize_path,
    tokenize_qualified_name, tokenize_signature, validate_fts_cheap, validate_fts_strong,
    verify_cached_text,
};
pub use code::{
    ALGO_VERSION, BlobOutcome, DerivedContentBlob, EdgeResolution, FtsSourceRow,
    NORMALIZATION_VERSION, NewContentBlob, NewFileRevision, NewOccurrence, NewParsedUnit,
    NewResolvedEdge, NewUnresolvedReference, NewlineStyle, ParsedUnitOutcome, PreparedSource,
    RevisionOutcome, SOURCE_ENCODING_UTF8, SOURCE_ZSTD_LEVEL, SkipReason, SourceCompression,
    UnitKind, content_blob_exists, content_blob_id, content_hash, create_or_reuse_content_blob,
    create_or_reuse_file_revision, create_or_reuse_parsed_unit, decode_source,
    delete_unresolved_references_for_revision, derive_content_blob, detect_encoding,
    detect_newline_style, file_revision_id_by_content_key, insert_content_blob,
    insert_file_revision, insert_generation_file, insert_occurrence, insert_parsed_unit,
    insert_resolved_edge, insert_skipped_file, insert_unresolved_reference, member_file_revision,
    normalize, occurrence_count_for_generation, occurrence_id, occurrence_ids_for_generation,
    occurrences_for_fts, parsed_unit_id_by_natural_key, parsed_units_for_revision, prepare_source,
    skip_reason, source_bytes,
};
pub use housekeeping::{
    HousekeepingError, SHARD_DESTROY_GRACE_MS, ShardSweepReport, expired_shard_ids,
    run_expired_shard_sweep, run_orphan_shard_sweep, shard_destroy_due, sweep_expired_shard_dirs,
    sweep_orphan_shard_dirs,
};
pub use lock::{LockLevel, OrderViolation, WorktreeLockRegistry, check_order, held_level};
pub use migrate::{ALL, Migration, MigrationError, MigrationReport, MigrationStep, StepFn};
pub use registry::{
    AttachError, Candidate, DATA_POLICY_KEY, GenerationState, GenerationTransitionError,
    IllegalGenerationTransition, IllegalWorktreeTransition, PathObservation, RequestRoot,
    Resolution, WorktreeKind, WorktreePathObservation, WorktreeRootFacts, WorktreeState,
    WorktreeStateClock, WorktreeSummary, WorktreeTransitionError, active_generations,
    all_worktree_ids, allocate_generation, attach, create_repository, create_worktree,
    current_generation, current_path, current_worktree_path, effective_data_policy,
    find_repositories_by_remote, find_repository_by_path, find_worktree_by_current_path,
    find_worktrees_by_path_fingerprint, generation_state, get_repo_setting,
    observe_repository_path, observe_worktree_path, path_history, repo_data_policy, repo_settings,
    resolve, set_current_generation, set_repo_data_policy, set_repo_setting, transition_generation,
    transition_worktree_state, worktree_path_history, worktree_state, worktree_state_clocks,
    worktree_summary, worktrees_of_repo,
};
pub use registry::{
    DEFAULT_MODEL_SPACE_ID, DEFAULT_MODEL_SPACE_NAME, IllegalProjectionTransition,
    PROJECTION_SCHEMA_VERSION, ProjectionInvariantViolation, ProjectionStateChange,
    ProjectionStateError, ProjectionStateRow, ProjectionStatus, check_invariants,
    default_model_space_id, insert_projection_state, projection_state, write_projection_state,
};
pub use retention::{
    ExternalPins, GenerationMeta, JobLease, PinRoots, RetentionParams, SWEEP_BATCH_ROWS,
    SweepError, SweepPlan, SweepReport, generation_meta_for_worktree, mark_pins,
    pinned_generation_roots, plan_sweep, run_sweep, run_sweep_with_batch,
};
pub use state::{DEFAULT_WRITE_QUEUE_CAPACITY, OpenError, StateDb, StateWriter, WriteError};
