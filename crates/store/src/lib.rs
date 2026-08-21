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
//! `write_projection_state` (read-then-write, no mutation on rejection).
//! Validate-on-open/rebuild is T07-04.
//!
//! T11-01 adds the **representation registry** ([`registry::representation`],
//! version-6 `SCHEMA_V6`): the `representation`/`model_space_representation`
//! tables (spec 03 §2.2), the canonical six-field [`RepresentationKey`], and the
//! `model_space` build-state machine ([`ModelSpaceState`], spec 04 §3) over the
//! `model_space` table `SCHEMA_V4` already created. [`Coverage`]/
//! [`recompute_coverage`] are the advisory-coverage data model and completeness
//! gate ([`transition_model_space`] requires full coverage before
//! `building → projection_ready`); real per-subject coverage counting against
//! occurrences/`embedding_cache` is T11-04. Wiring `crates/projection`'s
//! `expected::REQUIRED_REPRESENTATION_KINDS` placeholder to this registry is
//! T11-05, once a working multi-model-space switch needs it.
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
//! T13-04 adds the **transactional observation importer** ([`observation`],
//! spec 03 §2.5, 07 §5/§6): the version-7 migration creates the spool-derived
//! ledger — `observation_envelope`/`observation_path`/`observation_payload`/
//! `spool_import_cursor` — a deliberate subset of spec 03 §2.5's "Memory side"
//! block (the remaining `memory_entry`/`memory_evidence`/
//! `pending_memory_candidate`/`candidate_evidence`/`processing_cursor`/
//! `consolidation_run`/`audit_event` tables are [`memory`]'s, below).
//! [`observation::import_batch`] composes one per-session transaction: resolve
//! `worktree_root` once (an already-built [`registry::RequestRoot`] is
//! injected — the git probing that produces one is the daemon's job, not
//! started yet), exact dedup via a partial-unique-index `ON CONFLICT DO
//! NOTHING RETURNING`, best-effort dedup via a bounded per-session window
//! (10 min **or** last 512 envelopes, the same "most protective" union
//! `retention::mark_pins`'s K/T window already established), insert
//! envelope/path/payload rows, and advance `spool_import_cursor` — all in one
//! commit. [`observation::import_session_tail`] is the per-session driver:
//! reads real segment files, decodes via [`spool::decode_segment`]/
//! [`spool::decode_frames`] (T13-03, this module's first consumer), and
//! (best-effort, after the commit — spec 07 §7's S4 kill-matrix row) deletes
//! segment files the cursor has fully passed.
//!
//! T13-05 adds the **payload TTL sweep** ([`observation::run_payload_ttl_sweep`],
//! spec 12 §3): deletes `observation_payload` rows past their already-computed
//! `expires_at`, never touching `observation_envelope`/`observation_path` (a
//! payload row's absence *is* "no payload," whether it never had one or it
//! expired). And the **spool session GC sweep** ([`run_spool_session_sweep`],
//! spec 07 §6 `[SPEC: 14 days]`), a fourth sweep alongside
//! [`housekeeping`]'s three shard sweeps: removes a session's
//! `spool/<session_id>/` directory and its now-orphaned `spool_import_cursor`
//! row once the session has gone without a *new* import for the absence
//! budget ([`session_gc_due`], read from the cursor's own `updated_at` — no
//! filesystem mtime needed) **and** its spool data is fully committed
//! ([`is_fully_committed`] — no bytes left in the current segment beyond
//! `committed_offset`, and no further segment file yet). Both sweeps ship
//! without a scheduler, the same deferral every sweep in this crate carries
//! (group 15 triggers them); [`observation::known_spool_sessions`] is the thin
//! enumeration seam a future daemon-startup catch-up loop will drive
//! [`observation::import_session_tail`] over.
//!
//! T14-01 adds the **durable memory schema** ([`memory`], spec 03 §2.5 "Memory
//! side", the seven tables T13-04 left for this task): the version-9 migration
//! creates `memory_entry`/`memory_evidence`/`pending_memory_candidate`/
//! `candidate_evidence`/`processing_cursor`/`consolidation_run`/`audit_event`,
//! plus a pure, typed `check_transition`/`transition_*` guard per state
//! machine (spec 04 §4-6) — the same shape [`registry::GenerationState`]
//! established for the generation lifecycle. `memory_entry`'s machine is
//! *kind*-specific ([`memory::MemoryState::check_transition`] takes `kind` as
//! well as `to`): `task`/`question`, `hypothesis`, and
//! `fact`/`decision`/`convention`/`procedure` each have their own disjoint
//! legal transition set, and `memory_entry.state` carries no SQL `CHECK` (kind
//! conditions it), unlike `pending_memory_candidate.review_state`/
//! `consolidation_run.state`, which do. None of the three `transition_*`
//! primitives compose evidence linking, `audit_event` writing, the
//! `expected_version` optimistic-concurrency precondition, or
//! idempotency-key retry recognition — that atomic operation contract (spec 08
//! §3) is T14-02's transactional memory-op engine (below); T14-01 ships
//! exactly the schema and the transition legality it builds on, the same
//! division T05-01 drew relative to the generation builder/switch that
//! followed it.
//!
//! T14-02 adds the **transactional memory-op engine** ([`memory::op`], spec 08
//! §3): [`apply_create`]/[`apply_reinforce`] compose T14-01's primitives into
//! the atomic mutation+evidence+audit contract, checking
//! [`memory::find_by_idempotency_key`] **first** inside the same transaction
//! so a retried router op returns [`MemoryOpOutcome::Replayed`] — the
//! *original* result, not a freshly recomputed one — without touching
//! `memory_entry`/`memory_evidence` again. `apply_create` pre-checks
//! `canonical_key` scope-uniqueness with a typed [`MemoryOpError`] (spec 08
//! §3 asks for a typed error here, unlike `create_memory_entry`'s own raw
//! `UNIQUE`-violation bubble-up); `apply_reinforce` enforces
//! `expected_version` optimistic concurrency and structurally cannot edit
//! `text` — no such field exists on [`ReinforceMemoryOp`] — but always bumps
//! `entry_version` on a real mutation, since every apply gets a matching
//! `audit_event`. `apply_noop` writes **nothing**: the router's op envelope
//! (spec 08 §4) needs no target/kind/text/scope/confidence for "no action,"
//! and recording it as its own `audit_event` at the examined entry's
//! unchanged version would collide with `UNIQUE (entity_kind, entity_id,
//! entity_version)` the first time two independent consolidation runs both
//! reach that same "nothing to do" conclusion — a zero-write noop is
//! idempotent under retry by construction and sidesteps this entirely.
//! `resolve`/`supersede`/`retract`/`edit` (T14-03) and `merge_memories`
//! (T14-04) are later tasks composing the same primitives.
//!
//! T14-03 adds the **lifecycle/edit ops** in the same module:
//! [`apply_resolve`]/[`apply_retract`] — and [`apply_confirm`]/
//! [`apply_reject`], added by D-079 for the `hypothesis` machine — are thin
//! wrappers over a shared
//! private helper that reads `(kind, state, entry_version)` once and reuses
//! [`memory::MemoryState::check_transition`] directly (not
//! `transition_memory_entry`, which has no notion of `entry_version`).
//! [`apply_supersede`] is the promotion op: it creates the **new** entry
//! first, then retires the **old** one to `superseded` — both pre-validated
//! before either write, mirroring `local_rag_projection::switch::commit_switch`'s
//! "check both sides, then mutate" shape — and returns the **new** entry's
//! result only (the old entry's transition is a verified side effect, not a
//! second value threaded through the return type); only the new entry's
//! `audit_event` carries the caller's `idempotency_key`, so replay never
//! risks a second row colliding on the same key. [`apply_edit`] is the one op
//! allowed to change `text`, and adds [`MemoryOpError::EntryTerminal`] — an
//! as-built guard (spec 08 §3 leaves the exact "kind/state guards" this
//! task's card asks for unspecified) rejecting edits to a terminal entry.
//! This task also fixes **D-020**: spec 04 §5's prose narrates promotion
//! acting on an already-*confirmed* hypothesis, but T14-01 only allowed
//! `active → superseded`; `confirmed → superseded` is now legal too (see
//! `memory::MemoryState::check_transition`'s doc for the full rationale).
//!
//! T14-04 adds [`memory::apply_merge`]: a survivor absorbs evidence from N ≥
//! 1 losers, each transitioning to `superseded` with `supersedes_id` pointing
//! at the survivor (the first op setting that column on an already-existing
//! row, not just at `INSERT` time) — every precondition (both
//! `expected_version`s, scope compatibility via the new
//! [`MemoryOpError::IncompatibleScope`], each loser's kind/state guard)
//! checked before any write. A loser's evidence for an `observation_id` the
//! survivor already has is left attached to the (superseded) loser rather
//! than erroring or duplicating, computed with a plain `HashSet` rather than
//! a self-referential SQL subquery on `memory_evidence`. The response
//! describes the survivor only; only its `audit_event` carries the caller's
//! `idempotency_key` and a `serde_json`-encoded array of the merged loser
//! ids (`serde_json` already in `crates/store`'s graph via `registry::
//! representation::Coverage`, T11-01).
//!
//! T14-05 adds the **candidate review operations** ([`memory::review`], spec
//! 04 §6, 08 §3/§5/§8): [`memory::propose_candidate`]/[`memory::edit_candidate`]/
//! [`memory::reject_candidate`] are thin wrappers over T14-01's
//! [`CandidateState`] machine, and [`memory::approve_candidate`] is the task's
//! core — it deserializes the candidate's `proposed_operation` JSON (a
//! `#[serde(tag = "op")]` [`memory::ProposedOperation`] covering exactly the
//! five router ops that can be *materialized* — `create`/`reinforce`/
//! `resolve`/`retract`/`supersede`; `noop` writes nothing and `propose_candidate`
//! cannot nest itself, so neither is ever proposed, and `edit`/`merge` are
//! direct review-tool ops per spec 11 §2's table, never candidate-proposed) and
//! dispatches to the matching `memory::op::apply_*` with `actor=`[`Actor::User`]
//! — spec 04 §6's "same transactional memory-op path as the router" — inside
//! one transaction with the candidate's own `pending → approved` transition.
//! Materialization evidence is derived, not stored twice: `candidate_evidence`
//! carries only the FK to `observation_envelope` (no `evidence_kind`/
//! `session_id` columns of its own, unlike `memory_evidence`), so
//! `approve_candidate` reads each linked observation's own
//! `evidence_kind`/`session_id` to build the underlying op's evidence — the
//! DDL's own "FK provenance, not embedded snapshots" principle applied to
//! candidates. Double-approval is idempotent two ways: an already-`approved`
//! candidate short-circuits to [`memory::ApproveCandidateOutcome::AlreadyApproved`]
//! before any JSON parsing or op-engine call — the state machine's own
//! self-transition-is-legal convention is the primary guarantee — and, as
//! defense-in-depth for a crash mid-transaction, the dispatched op still
//! carries a deterministic `idempotency_key` (`candidate:<candidate_id>`)
//! that resolves through T14-02's replay mechanism if a retry reaches that
//! far. `pending_memory_candidate` has no `entry_version`/`updated_at` (spec
//! 11 §2's `edit_memory_candidate(id, patch)` signature, contrasted with
//! `edit_memory(id, patch, expected_version)`, confirms this is intentional),
//! so [`memory::edit_candidate`]'s conflict check is state-based: legal only
//! while `review_state == pending`, [`ReviewError::NotPending`] otherwise.
//! [`housekeeping::run_candidate_expiry_sweep`] is a fifth sweep alongside
//! T13-05's spool-session GC — the first sweep in this crate with no
//! filesystem component — batch-transitioning `pending` candidates past
//! [`housekeeping::CANDIDATE_EXPIRY_MS`] (spec 04 §6 `[SPEC: 30 days]`) to
//! `expired`, tolerating (as a retained, not failed, row) a candidate a
//! concurrent approve/reject already moved out of `pending` since the sweep's
//! read pass.
//!
//! T13-03 adds the **spool segment decoder** ([`spool`], spec 07 §2-§4): a
//! pure `&[u8]` → `DecodedObservation` transform with no database awareness —
//! [`spool::decode_segment`] validates the 16-byte header
//! (`local_rag_core::spool::decode_segment_header`) and rejects a
//! newer-than-supported format immediately, before attempting any frame;
//! [`spool::decode_frames`] then decodes as many whole frames as possible,
//! stopping cleanly at a torn tail (a legal `len` with insufficient trailing
//! bytes — never an error) and distinctly reporting corruption (CRC/length-
//! cap/UTF-8/shape/version mismatches). Each decoded frame is classified by
//! [`spool::DedupClass`] against spec 07 §4's stable/best-effort table, cross-
//! checked against the frame's actual `dedup_key` presence so an internally
//! inconsistent frame is caught here rather than poisoning T13-04's
//! transactional importer, which is this module's only consumer.
//!
//! T20-01 adds the **daemon-managed indexing registry**
//! ([`registry::managed_worktrees`] and friends, spec 03 §2.1, ADR-0009): the
//! version-10 migration creates `managed_worktree`, the persisted, explicit
//! opt-in list of the worktrees the daemon indexes in the background — keyed
//! by `worktree_id` with a foreign key into `worktree`, never by a path
//! (spec 01 §5). The table is the *truth*; a live daemon is only notified of
//! a change and re-reads it on a backstop poll, the same "notify is a hint"
//! discipline spec 06 §1 fixes for the reconcile watcher. It carries no
//! runtime columns — `running`/`last_error` are in-memory supervisor state —
//! and `enabled = 0` keeps a row enrolled but dormant, with
//! [`registry::managed_worktrees`] returning every row so the run/skip
//! decision lives in one place. The consumers (daemon supervisor, `local-rag
//! project` CLI, double-indexing advisory) are T20-06/T20-08/T20-09.
//!
//! `rusqlite` is re-exported so downstream crates share one SQLite vocabulary
//! (`local_rag_store::rusqlite`).

pub use local_rag_core::VERSION;
pub use rusqlite;

mod cache;
mod checkpoint;
mod clock;
pub mod code;
pub mod eviction;
pub mod housekeeping;
pub mod lock;
pub mod memory;
pub mod migrate;
pub mod observation;
pub mod privacy;
pub mod registry;
pub mod retention;
pub mod spool;
mod state;
pub mod subjects;

pub use cache::{
    BM25_DEFAULT_WEIGHTS, BatchingLastUsed, BatchingLastUsedEmbeddings, CACHE_SCHEMA_VERSION,
    CacheDb, CacheDiagnosis, CacheOpenError, CacheOpenOutcome, CacheWriteError, CacheWriter,
    EMBEDDING_SUBJECT_CHUNK, EmbeddingCacheEntry, EmbeddingCacheMeta, EmbeddingCacheRow,
    EmbeddingDivergence, EmbeddingKey, FTS_SYNC_REBUILD_OCCURRENCE_THRESHOLD, FtsAvailability,
    FtsCheckOutcome, FtsDivergence, FtsMaterializeError, FtsMaterializeOutcome, FtsOpenOutcome,
    FtsProjectionHeadRow, FtsRebuildError, LEXICAL_SCHEMA_VERSION, LastUsedSink,
    LastUsedSinkEmbedding, LexicalHit, LexicalQuery, MIN_CANDIDATE_DEPTH, NormalizedTextRow,
    SubjectKind, TOKENIZER_VERSION, ValidationDepth, VectorLengthError, all_embedding_meta,
    candidate_depth, check_fts, decode_vector_le, delete_all_memory_embeddings, delete_embedding,
    delete_embeddings_for_subject, delete_normalized_text, document_frequencies,
    embeddings_for_subjects, encode_vector_le, flush_last_used, flush_last_used_embeddings,
    fts_doc_occurrence_count, fts_doc_occurrence_ids, fts_manifest_hash, fts_match_expression,
    fts_match_expression_from_terms, get_embedding, get_normalized_text, indexed_document_count,
    insert_embedding, insert_normalized_text, lexical_leg, materialize_fts, open_and_validate_fts,
    query_fts, read_fts_projection_head, requires_index_unavailable, selective_terms,
    should_rebuild_synchronously, tokenize_identifier, tokenize_path, tokenize_qualified_name,
    tokenize_signature, validate_fts_cheap, validate_fts_strong, verify_cached_embedding,
    verify_cached_text,
};
pub use code::{
    ALGO_VERSION, BlobOutcome, CONTEXT_VERSION, ContextInput, ContextSubject, DerivedContentBlob,
    EdgeResolution, FtsSourceRow, NORMALIZATION_VERSION, NewContentBlob, NewFileRevision,
    NewOccurrence, NewParsedUnit, NewResolvedEdge, NewUnresolvedReference, NewlineStyle,
    OccurrenceMetadata, ParsedUnitOutcome, PreparedSource, RevisionOutcome, SOURCE_ENCODING_UTF8,
    SOURCE_ZSTD_LEVEL, SkipReason, SourceCompression, UnitKind, content_blob_exists,
    content_blob_id, content_blob_ids_for_generation, content_blob_ids_for_generations,
    content_hash, context_subjects_for_generation, create_or_reuse_content_blob,
    create_or_reuse_file_revision, create_or_reuse_parsed_unit, decode_source,
    delete_unresolved_references_for_revision, derive_content_blob, detect_encoding,
    detect_newline_style, file_revision_id_by_content_key, generation_file_occurrence_counts,
    insert_content_blob, insert_file_revision, insert_generation_file, insert_occurrence,
    insert_parsed_unit, insert_resolved_edge, insert_skipped_file, insert_unresolved_reference,
    member_file_revision, normalize, occurrence_count_for_generation, occurrence_id,
    occurrence_ids_for_generation, occurrences_by_id, occurrences_for_fts,
    occurrences_for_generations, occurrences_for_path, parsed_unit_id_by_natural_key,
    parsed_units_for_revision, prepare_source, serialize_context, skip_reason, source_bytes,
    top_imports_for_generation,
};
pub use eviction::{
    EVICTION_BATCH_ROWS, EvictionError, EvictionParams, EvictionReport, rows_to_evict,
    run_embedding_cache_eviction, store_wide_embedding_pins,
};
pub use housekeeping::{
    CANDIDATE_EXPIRY_MS, CandidateExpirySweepReport, HousekeepingError, SHARD_DESTROY_GRACE_MS,
    SPOOL_SESSION_ABSENCE_MS, ShardSweepReport, SpoolSessionSweepReport, candidate_expiry_due,
    expired_shard_ids, is_fully_committed, run_candidate_expiry_sweep, run_expired_shard_sweep,
    run_orphan_shard_sweep, run_spool_session_sweep, run_unreferenced_space_sweep, session_gc_due,
    shard_destroy_due, store_has_pending_spool_bytes, sweep_expired_shard_dirs,
    sweep_orphan_shard_dirs, sweep_unreferenced_space_dirs,
};
pub use lock::{LockLevel, OrderViolation, WorktreeLockRegistry, check_order, held_level};
pub use memory::{
    Actor, ApplyReport, ApproveCandidateOutcome, AuditEventRow, CURRENT_NORMALIZER_VERSION,
    CandidateCountRow, CandidateRow, CandidateState, CandidateTransitionError, ClassifiedFailure,
    ConfirmMemoryOp, ConsolidationWindow, CreateMemoryEntryError, CreateMemoryOp,
    DeadLetteredNormalization, EditMemoryOp, EvidenceInput, FailureKind, GLOBAL_SCOPE_OWNER_ID,
    GeneratedOp, IllegalCandidateTransition, IllegalMemoryTransition, IllegalRunTransition,
    LEASE_DURATION_MS, LEASE_RENEW_INTERVAL_MS, MAX_NORMALIZATION_ATTEMPTS, MemoryCountRow,
    MemoryEntryRow, MemoryEntrySummary, MemoryKind, MemoryOpError, MemoryOpOutcome, MemoryOpResult,
    MemoryState, MemoryTransitionError, MergeLoser, MergeMemoryOp, NewAuditEvent, NewCandidate,
    NewConsolidationRun, NewMemoryEntry, NewMemoryEvidence, NormalizationBacklog,
    NormalizationCountRow, NormalizationRow, NormalizationStatus, NormalizationWrite,
    PendingNormalization, ProposedOperation, RecallCandidate, ReinforceMemoryOp, RejectMemoryOp,
    RenewError, ResolveMemoryOp, RetractMemoryOp, ReviewError, RunCountRow, RunOutcome,
    RunOutcomeError, RunState, RunTransitionError, RunWindow, RunnerApplyError, RunnerError,
    STUCK_RUN_ATTEMPT_THRESHOLD, STUCK_RUN_REASON_MAX_CHARS, ScopeKind, SnapshotOutcome, StaleRun,
    StuckRunRow, SupersedeMemoryOp, TRANSIENT_BACKOFF_BASE_MS, TRANSIENT_BACKOFF_CAP_MS,
    UnconsolidatableSession, UpsertOutcome, WindowObservation, acquire_lease,
    active_entries_for_scope, active_entry_with_text, all_memory_entries_with_text, apply_confirm,
    apply_create, apply_edit, apply_merge, apply_noop, apply_reinforce, apply_reject,
    apply_resolve, apply_retract, apply_supersede, approve_candidate, candidate_evidence_for,
    candidate_state, canonical_key_owner, commit_apply_run, consolidation_run_counts,
    consolidation_run_state, create_candidate, create_consolidation_run, create_memory_entry,
    dead_lettered_normalizations, delete_normalization, edit_candidate,
    entries_needing_normalization, find_by_idempotency_key, has_unconsolidated_checkpoint,
    insert_audit_event, insert_candidate_evidence, insert_memory_evidence, lease_expired,
    list_candidates, list_memory_entries_for_scope, memory_entry_by_id, memory_entry_counts,
    memory_entry_state, memory_entry_summary, memory_evidence_for, normalization_backlog,
    normalization_counts, normalization_for, observation_evidence_source,
    observations_applied_since, oldest_open_run_created_at, open_next_run, pending_backlog,
    pending_candidate_ages, pending_candidate_counts, processing_cursor, propose_candidate,
    read_audit_events_for_entity, recall_candidate_by_id, recall_candidates_for_scope,
    record_run_failure, reject_candidate, renew_lease, retry_run, run_once, session_idle_since,
    sessions_with_pending_backlog, stale_runs, stuck_consolidation_runs, total_pending_backlog,
    transient_backoff_delay_ms, transition_candidate, transition_memory_entry, transition_run,
    unconsolidatable_sessions, upsert_normalization, upsert_processing_cursor,
};
pub use migrate::{
    ALL, Migration, MigrationError, MigrationReport, MigrationStep, StepFn, VersionDiagnosis,
    VersionReport,
};
pub use observation::{
    EvidenceKind, ImportBatchReport, ImportError, ImportOutcome, NewObservationEnvelope,
    PayloadStatus, PayloadSweepError, PayloadSweepReport, RootResolver, TrustLevel,
    diagnose_spool_tail, import_batch, import_session_tail, insert_envelope, known_spool_sessions,
    observation_envelope_count, run_payload_ttl_sweep,
};
pub use privacy::{
    EvidenceSummary, MemoryInspection, ObservationInspection, PurgeAllPreview, PurgeAllReport,
    PurgeMemoryError, PurgeMemoryPreview, PurgeMemoryReport, PurgeSessionPreview,
    PurgeSessionReport, export_scope, inspect_generation, inspect_memory, inspect_observation,
    preview_purge_all, preview_purge_memory, preview_purge_session, purge_all, purge_memory,
    purge_session,
};
pub use registry::{
    AttachError, Candidate, DATA_POLICY_KEY, GenerationRow, GenerationState,
    GenerationTransitionError, IllegalGenerationTransition, IllegalWorktreeTransition,
    PathObservation, RequestRoot, Resolution, SUPERSEDED_BATCH, WorktreeKind,
    WorktreePathObservation, WorktreeRootFacts, WorktreeState, WorktreeStateClock, WorktreeSummary,
    WorktreeTransitionError, active_generations, all_repository_ids, all_worktree_ids,
    allocate_generation, attach, create_repository, create_worktree, current_generation,
    current_path, current_worktree_path, effective_data_policy, ensure_store_instance_uuid,
    fail_superseded_generations, find_repositories_by_remote, find_repository_by_path,
    find_worktree_by_current_path, find_worktrees_by_path_fingerprint, generation_number,
    generation_row, generation_state, get_repo_setting, observe_repository_path,
    observe_worktree_path, path_history, repo_data_policy, repo_settings, resolve,
    set_current_generation, set_repo_data_policy, set_repo_setting, store_instance_uuid,
    transition_generation, transition_worktree_state, worktree_path_history, worktree_state,
    worktree_state_clocks, worktree_summary, worktrees_of_repo,
};
pub use registry::{
    Coverage, CoverageEntry, DistanceMetric, IllegalModelSpaceTransition, ModelSpaceState,
    ModelSpaceTransitionError, RepresentationKey, RepresentationKind, create_model_space,
    eligible_as_target, model_space_ids_in_states, model_space_required_kinds,
    model_space_required_representation_ids, model_space_state, recompute_coverage,
    register_representation, representation_key, set_model_space_representation,
    transition_model_space, write_model_space_coverage,
};
pub use registry::{
    DEFAULT_MODEL_SPACE_ID, DEFAULT_MODEL_SPACE_NAME, DefaultModelSpaceError,
    IllegalProjectionTransition, PROJECTION_SCHEMA_VERSION, ProjectionInvariantViolation,
    ProjectionStateChange, ProjectionStateError, ProjectionStateRow, ProjectionStatus,
    check_invariants, default_model_space_id, insert_projection_state, projection_state,
    referenced_generation_ids, referenced_model_space_ids, set_default_model_space_id,
    write_projection_state,
};
pub use registry::{
    IndexingOutcome, ManagedWorktree, WorktreeIndexingStatus, indexing_status, indexing_statuses,
    is_managed, managed_worktrees, register_managed_worktree, set_managed_enabled,
    unregister_managed_worktree, write_indexing_status,
};
pub use retention::{
    ExternalPins, GenerationMeta, JobLease, PinRoots, RetentionParams, SWEEP_BATCH_ROWS,
    SweepError, SweepPlan, SweepReport, generation_meta_for_worktree, mark_pins,
    pinned_generation_roots, plan_sweep, run_sweep, run_sweep_with_batch,
};
pub use spool::{
    ClassificationError, DecodedObservation, DedupClass, FrameDecodeError, SegmentTailDecode,
    StopReason, decode_frames, decode_segment,
};
pub use state::{
    CheckpointMode, CheckpointStats, DEFAULT_WRITE_QUEUE_CAPACITY, OpenError, StateDb, StateWriter,
    WriteError,
};
pub use subjects::{
    SubjectSet, expected_subject_keys, memory_entry_subject_keys, memory_subject_hash,
    pinned_generations, protected_model_space_ids, protected_subject_keys,
};
