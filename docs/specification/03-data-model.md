# 03 — Data Model (DDL)

Authoritative schema for `state.sqlite` (§2) and `cache.sqlite` (§4). Types follow SQLite
affinity; conventions in §1 are normative for all tables.

## 1. Conventions, IDs, hashes

### 1.1 Column conventions `[SPEC]`

- Timestamps: `INTEGER` Unix **milliseconds**, UTC, suffix `_at`.
- Booleans: `INTEGER` 0/1 with `CHECK (x IN (0,1))`.
- UUIDs: `TEXT`, lowercase canonical form. New random IDs are **UUIDv7** (time-ordered → good
  B-tree locality). Deterministic IDs are hash-derived (§1.2), stored as lowercase hex.
- JSON payloads: `TEXT` with documented shape; never load-bearing for constraints.
- Vectors: `BLOB`, little-endian f32 array.

### 1.2 Hashing rules `[FIXED principle, encoding [SPEC]]`

All content/manifest/subject hashes are **domain-separated and version-tagged**.

- Algorithm: **BLAKE3**, 32-byte digest, hex-encoded (64 chars).
- Canonical input encoding:
  `H(domain, fields…) = blake3( utf8(domain) ‖ 0x00 ‖ concat( le_u32(len(fᵢ)) ‖ fᵢ ) )`
  where `domain` is e.g. `"local-rag/1/occurrence_id"` (the `1` is the hash-schema version).
  Length-prefixing makes field boundaries unambiguous; changing field order or count requires
  a new domain version.
- Defined domains:

| Domain | Fields (in order) | Used as |
| --- | --- | --- |
| `…/file_content` | raw file bytes | `file_revision.content_hash` |
| `…/content_blob` | algo_version, language, normalization_version, normalized_text | `content_blob.blob_id` |
| `…/signature_fingerprint` | canonical signature descriptor (one field) | `parsed_unit.syntax_locator` `sig` (§2.4, ADR-0002) |
| `…/occurrence_id` | generation_id, normalized_path, unit_id | `generation_unit_occurrence.occurrence_id` |
| `…/projection_point` | worktree_id, occurrence_id, model_space_id, representation_kind | dense point ID (05 §3) |
| `…/projection_manifest` | tuple fields ‖ point IDs sorted ascending bytewise | `ProjectionHead.manifest_hash` |
| `…/fts_manifest` | worktree_id, generation_id, occurrence IDs sorted | `fts_projection_head.manifest_hash` |
| `…/subject/content_blob` | blob_id | `embedding_cache.subject_hash` |
| `…/subject/occurrence_context` | context_version, occurrence context serialization | `embedding_cache.subject_hash` |
| `…/subject/memory_entry` | memory_id, H(text) | `embedding_cache.subject_hash` |
| `…/path_fingerprint` | canonical path | `worktree_path.path_fingerprint` (lookup only) |
| `…/remote_fingerprint` | normalized remote URL (credentials stripped) | `repository.git_remote_fingerprint` |
| `…/memory_op` | run_id, op_index | consolidation idempotency key |
| `…/truncated_excerpt` | full excerpt bytes | `{hash, original_size}` truncation metadata (12 §2) |

Deterministic IDs (`occurrence_id`, projection point IDs, memory-op keys) MUST be stable under
retry/reconcile and independent of row insertion order `[FIXED]`.

As-built note (T02-01, updated T04-03, `[SPEC]`): `local_rag_core::identity::domain` implements
the encoding above (`encode`/`hash`, hex-encoded 64 chars) with a `Domain` enum covering all
fourteen domains (`truncated_excerpt` added by T12-04) and the version-tagged string `local-rag/1/<slug>` (the two `subject/*` slugs
keep their `/`). Field payloads are raw bytes; the serialization conventions this codebase
commits to are: text → UTF-8 bytes; an already-hex identity (a UUID or another domain hash) →
its lowercase ASCII bytes exactly as stored in its `TEXT` column; a fixed-width integer field →
little-endian bytes of the declared width. The single-field fingerprints (`path_fingerprint`,
`remote_fingerprint`, and — added in T04-03 — `signature_fingerprint`) have typed constructors;
the multi-field deterministic-ID domains are hashed through the generic `hash` entry point by
their owning tasks (T03/T05/T08/T11/T14), which assemble the field list in the order fixed by the
table above. `signature_fingerprint` hashes exactly one field — an opaque canonical descriptor
the parser assembles (ADR-0002) — so its internal structure may evolve within a `queries=`/
`grammar=` rebuild event without a `HASH_SCHEMA_VERSION` bump.

As-built note (T03-04, `[SPEC]`): the `content_blob` domain is the first to encode an *integer*
field, so it fixes the previously-unstated "declared width" for this codebase: `algo_version` and
`normalization_version` are hashed as **little-endian `u32`** (4 bytes), matching the `le_u32`
length framing of `encode` and `HASH_SCHEMA_VERSION: u32`. `content_blob_id`
(`local_rag_store::code::normalize`) assembles the fields in exact table order — `algo_version`
(u32-LE), `language` (UTF-8), `normalization_version` (u32-LE), `normalized_text` (UTF-8) — and a
golden test pins the resulting digest and the width choice. The `content_blob` DB columns remain
`INTEGER` (i64, SQLite affinity); only the hash pre-image narrows to `u32`.

As-built note (T11-02, `[SPEC]`): the three `embedding_cache.subject_hash` domains now have typed
constructors too (`local_rag_core::identity::domain::{subject_content_blob, subject_occurrence_context,
subject_memory_entry}`), joining the single-field-fingerprint precedent above rather than being left
to ad-hoc `hash()` call sites. `subject_content_blob` follows the existing single-field shape;
`subject_occurrence_context`/`subject_memory_entry` are this codebase's first *typed* multi-field
constructors (previously only the generic `hash` entry point handled multi-field domains). See spec
03 §4.2's own as-built note for the field-shape and scope-boundary details (the real
`occurrence_context` serialization was still `[OPEN]` then; D-016 fixed it — see §4.2's own
as-built note).

### 1.3 Path canonicalization `[FIXED principle, details [SPEC]]`

`normalized_path` (worktree-relative): `/` separators, no leading `./`, Unicode **NFC**; on
case-insensitive filesystems additionally simple case-fold. The original spelling is preserved
in `display_path`. Absolute worktree paths use the same rules plus: symlink resolution,
Windows drive-letter upcasing, UNC normalization. Identity never depends on the display form.

As-built note (T02-01, `[SPEC]`): `local_rag_core::identity::path` returns a `Canonical
{ canonical, display }` so the identity form and the preserved spelling travel together.
Simple case folding uses `casefold::simple_fold` (a Unicode simple-fold, 1:1, distinct from
full folding) applied *after* NFC. Filesystem case-sensitivity is not probed here: the caller
passes a `CaseSensitivity` (the registry knows each worktree's filesystem), keeping the
primitive deterministic. Symlink/`.`/`..` resolution is delegated to `std::fs::canonicalize`
(the only filesystem-touching step); the Windows `\\?\` verbatim and `\\?\UNC\` prefixes are
stripped and drive letters upcased as part of the string-level rules.

### 1.4 Cross-database rule `[FIXED]`

`state.sqlite` and `cache.sqlite` are **never** written in one transaction via `ATTACH` —
cross-DB atomicity under WAL is not crash-guaranteed. Read-only `ATTACH` for ad-hoc queries is
permitted. The cache is an independently validated materialized view with its own heads.

## 2. `state.sqlite` — source of truth

```sql
PRAGMA journal_mode=WAL;
PRAGMA foreign_keys=ON;
PRAGMA synchronous=FULL;          -- memory durability is the budget priority [SPEC]
PRAGMA busy_timeout=5000;
```

As-built note (`D-092`, `[SPEC]`): `busy_timeout` binds only if the transaction asks for its write
lock at `BEGIN`. The single writer queue therefore uses `BEGIN IMMEDIATE`, not SQLite's `DEFERRED`
default — see spec 02 §5's `D-092` note for the mechanism and the measurement. The same holds for
`cache.sqlite` (§4). `D-094` bounds that: a pass that never writes `main` uses the queue's read-only
entry point and opens `DEFERRED`, because holding the write lock across a long read starves every
other process's writer — measured, four failed daemon writes per 28-second `gc --dry-run`.

### 2.1 Registry & settings

```sql
CREATE TABLE schema_migrations (
  version     INTEGER PRIMARY KEY,
  name        TEXT NOT NULL,
  checksum    TEXT NOT NULL,
  applied_at  INTEGER NOT NULL
);

CREATE TABLE store_settings (        -- store_instance_uuid, default_model_space_id, …
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
-- As-built (T11-05, [SPEC]): `default_model_space_id` has exactly one writer,
-- `registry::set_default_model_space_id`, which refuses a space that is not
-- `active` — so 04 §3's "the default space MUST be active" is enforced where the
-- value is established rather than assumed by its readers (05 §8's dormant
-- migration, `subjects::protected_model_space_ids`). Migration 4 seeds it.
-- As-built (T15-01, [SPEC]): `store_instance_uuid` has exactly one producer,
-- `registry::ensure_store_instance_uuid` (first-writer-wins atomic upsert, no
-- migration seed — the value is inherently random). The daemon calls it once,
-- inside 02 §4.1 step 2, strictly before step 3 opens `cache.sqlite` (§4.4
-- below consumes it).

CREATE TABLE migration_progress (    -- resumable-migration checkpoints (13 §3); rows exist
  version  INTEGER NOT NULL,         -- only for the in-flight migration, cleared on finalize
  seq      INTEGER NOT NULL,         -- unit index within the migration (backup/sql/step…)
  label    TEXT NOT NULL,
  done_at  INTEGER NOT NULL,
  PRIMARY KEY (version, seq)
);

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

CREATE TABLE worktree (
  worktree_id            TEXT PRIMARY KEY,            -- stable UUIDv7, NEVER path-derived [FIXED]
  repo_id                TEXT NOT NULL REFERENCES repository(repo_id),
  kind                   TEXT NOT NULL CHECK (kind IN ('main','linked','non_git')),
  current_generation_id  TEXT,
  state                  TEXT NOT NULL CHECK (state IN ('active','detached','removing')),
  created_at             INTEGER NOT NULL,
  last_seen_at           INTEGER NOT NULL,
  state_changed_at       INTEGER NOT NULL,            -- migration 5; 05 §8 grace clock [SPEC]
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

CREATE TABLE managed_worktree (                        -- migration 10; ADR-0009 opt-in list [SPEC]
  worktree_id    TEXT PRIMARY KEY REFERENCES worktree(worktree_id),
  enabled        INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0,1)),
  registered_at  INTEGER NOT NULL,
  updated_at     INTEGER NOT NULL
);
-- Keyed by the stable worktree UUID, never a path [FIXED]. No runtime columns
-- (running/last_error): supervisor state is in-memory, surfaced by admin/projects_list.

CREATE TABLE worktree_indexing_status (                -- migration 13; X-006 durable indexing outcome
  worktree_id           TEXT PRIMARY KEY REFERENCES worktree(worktree_id),
  last_attempt_at       INTEGER,                       -- epoch ms; when the last cycle started
  last_success_at       INTEGER,                       -- epoch ms; when the last cycle succeeded
  last_generation_id    TEXT,                          -- advisory, no FK: GC may retire it
  consecutive_failures  INTEGER NOT NULL DEFAULT 0,
  last_error            TEXT,
  updated_at            INTEGER NOT NULL
);
-- Enrollment lives in managed_worktree; this table is the outcome axis only.
-- 'in progress' is NOT persisted: only a live daemon can answer that truthfully.
```

`retiring` exists for GC/audit only, never for routing `[FIXED]`: the per-worktree lock
guarantees no readers of the old generation exist at final commit.

As-built note (T02-02, `[SPEC]`): the repository-side tables (`repository`,
`repository_path` + its `repository_path_current` partial unique index, `repo_settings`) are
created by the first numbered migration — `schema_migrations` version `1`, name `registry`
(`local_rag_store::registry::SCHEMA_V1`, reproduced byte-for-byte from this section so its
checksum is stable). The worktree/generation tables above are the second migration (see the
T02-03 note below); `repo_settings` reads/writes and the data-policy merge are T02-05 (see that
note below). The
`create`/`observe`/`find`
operations (`local_rag_store::registry`) mint `repo_id` as a caller-supplied UUIDv7 (never
path-derived, 01 §5) and treat `git_remote_fingerprint` as a nullable, non-unique hint (12 §7).
`repository_path` keeps at most one `is_current = 1` row per repo: because SQLite has no
deferred UNIQUE constraints, observing a path clears the current flag and re-sets it as two
separate statements (never a single multi-row swap), so the partial unique index is never
transiently violated; path history is retained (a moved path keeps its row with `is_current = 0`
and its original `first_seen_at`).

As-built note (T02-03, `[SPEC]`): the worktree-side tables (`worktree`, `worktree_path` + its
`worktree_path_current` partial unique index and `worktree_path_fp` lookup index, `generation`)
are the second numbered migration — `schema_migrations` version `2`, name `worktree`
(`local_rag_store::registry::SCHEMA_V2`, reproduced byte-for-byte from this section). All three
tables ship in one migration because their foreign keys are circular
(`worktree`→`generation`→`worktree`); SQLite resolves FK parents lazily, and the composite FK
target is valid because `generation` declares `UNIQUE (generation_id, worktree_id)`. The
worktree operations (`local_rag_store::registry::worktree`) mint `worktree_id` as a
caller-supplied UUIDv7 (never path-derived, 01 §5); `worktree_path.path_fingerprint` is a lookup
accelerator only (never identity, never an FK target), stored alongside the preserved
`display_path`, and `worktree_path` keeps a single current path via the same clear-then-set
upsert as `repository_path` (history retained). The `active`/`detached`/`removing` machine
(04 §7) is enforced by `transition_worktree_state`, which returns a typed
`WorktreeTransitionError` (`UnknownWorktree` or an `IllegalWorktreeTransition { from, to }`) and
mutates nothing on rejection; self-transitions (`X → X`) are idempotent no-ops, which keeps a
crash/retry that re-requests the current state safe (never a coercion, 04 preamble). The
`generation` table ships here only as the worktree composite-FK seam — its builder, occurrence
schema (§2.4), and state machine (04 §1) are group 05 — and `set_current_generation` is the
worktree-side write that the FK guards against pointing at another worktree's generation.

As-built note (D-007, `[SPEC]`): `worktree.state_changed_at` is **migration 5**
(`schema_migrations` version `5`, name `worktree_state_clock`,
`local_rag_store::registry::SCHEMA_V5`) — an `ALTER TABLE … ADD COLUMN` plus a backfill, not an
edit to the frozen version-2 text above. It exists for exactly one normative requirement:
05 §8's "remove/detach: grace period `[SPEC: 7 days]`, then destroy" needs a clock to measure
from, and neither existing timestamp is one (`created_at` predates every transition;
`last_seen_at` tracks *path observation*, not lifecycle). Semantics: the epoch-ms time of the
row's most recent **effective** lifecycle transition (04 §7). `create_worktree` stamps
`created_at`; `transition_worktree_state` restamps only when `from != to`, deliberately
preserving its "self-transition is an idempotent no-op" contract — a crash/retry that
re-requests the state a worktree is already in must not push the destruction deadline forward,
or a retry loop could keep a doomed shard alive indefinitely. A `detached → active` reattach
(`repo attach`) therefore *resets* the budget, which is the intended behavior: a worktree that
comes back before the deadline keeps its shard. Pre-existing rows are backfilled from
`last_seen_at` (the closest available lower bound on "last known in use"), never left at the
`ALTER`'s `0` default, which would have made every existing shard instantly eligible for
destruction. The store-wide reader is `worktree_state_clocks`; the sweep that consumes it is
`local_rag_store::housekeeping::run_expired_shard_sweep` (see 05 §8's own D-007 note).

As-built note (T02-04, `[SPEC]`): the request-root resolver and `repo attach` (spec 02 §3.3,
04 §7) are a composition layer (`local_rag_store::registry::resolve`) over the T02-02/03
primitives — no new tables or migration. It adds three registry reads:
`find_worktree_by_current_path` (the single worktree whose `is_current = 1` observed canonical
path equals the query — symmetric to `find_repository_by_path`), `worktree_summary`
(`worktree_id`/`repo_id`/`kind`/`state` in one row), and `worktrees_of_repo`. Because
`worktree_path_current`/`repository_path_current` are *per-row* partial unique indexes, a
canonical path is not globally unique across worktrees/repos; `find_worktree_by_current_path`
returns the deterministic first (`ORDER BY worktree_id LIMIT 1`) and the daemon maintains one
current occupant per path. `resolve` auto-resolves **only** on an exact current-path match;
`path_fingerprint` (current or historical), `git_remote_fingerprint`, and the daemon's
common-dir/admin-dir fingerprint are advisory and never produce a resolved identity on their own
— reattach candidates are restricted to `state = 'detached'` worktrees of the requested `kind`,
and binding one requires an explicit id via `attach` (an explicit `repo_hint` that selects a
single candidate also resolves; a repo-level hint cannot pick between two linked worktrees of
one repo). A recreated path therefore never steals a moved (still-`active`) worktree's identity,
and an unknown root yields *global scope only* (not an error). `attach` composes
`transition_worktree_state`(→`active`) + `observe_worktree_path` (+ `observe_repository_path`
only when the **stored** `kind` is not `linked`, so a linked reattach never moves the main
checkout's path); it returns a typed `AttachError` (`UnknownWorktree`, `RepoMismatch`,
`NotReattachable`) and mutates nothing on rejection. The daemon's common-dir fingerprint is
carried on the request facts but **not stored** (no column) — advisory by construction. Git
probing (`kind`, common-dir, remote URL) is the daemon's (T15); `local-rag-store` takes no git
dependency (architecture guardrail until T10).

As-built note (T02-05, `[SPEC]`): `repo_settings` reads/writes and the effective-`data_policy`
merge (spec 02 §3.2, 12 §1) are `local_rag_store::registry::settings` — no new table or migration
(the generic `(repo_id, key, value)` table from `SCHEMA_V1` needs no schema change; a policy is
just a key). Writes compose in a `StateWriter::transaction` (`set_repo_setting` upserts via
`ON CONFLICT(repo_id, key) DO UPDATE`; an unknown `repo_id` is rejected by the FK and rolls back)
and reads run on a read-only connection (`get_repo_setting`, `repo_settings` ordered by key). The
typed accessors `set_repo_data_policy`/`repo_data_policy` store/parse the canonical string under
the mirrored key `data_policy` (`DATA_POLICY_KEY`); a stored value outside the four canonical
names is corruption and surfaces as `rusqlite::Error::FromSqlConversionFailure` (the same idiom as
`worktree.state`), never a silent default. `effective_data_policy(global, conn, repo_ids)` folds
`DataPolicy::most_restrictive` over the global value and every involved repository's stored policy;
because that operation is commutative/associative the fold is order-independent (deterministic
merged snapshot) and a repository can only *tighten*, never relax, the global policy. The central
remote-policy guard that consumes the effective value (provider pool, spec 10 §1, 12 §1) is a later
group (T11/T16); T02-05 supplies only the stored values and the merge. The global config side is
`local_rag_core::config` (see 02 §3.1).

As-built note (T20-01, `[SPEC]`): `managed_worktree` is **migration 10** (`schema_migrations`
version `10`, name `managed_worktree`, `local_rag_store::registry::SCHEMA_V10`, reproduced
byte-for-byte from this section so its checksum is stable) — the persisted, explicit opt-in list
of the worktrees the daemon indexes in the background, decided by ADR-0009 (see 11 §8). Keyed by
`worktree_id` with a foreign key into `worktree`: the key is the stable UUID, never a path (01
§5), and an unknown id is rejected by that FK and rolls the transaction back rather than leaving a
dangling enrollment. Three alternatives were rejected for normative reasons, recorded here so they
are not silently revisited: a `repo_settings` key (wrong granularity — repository, not worktree —
and 02 §3.2 defines that table as the mirror of the global `[models]`/`[index]` config sections,
not as a work queue); reusing `worktree.state` (04 §7's `active|detached|removing` machine answers
"does this path still resolve", an orthogonal axis — conflating them would make "the user paused
indexing" indistinguishable from "the path vanished" and would require editing `[SPEC]`
transitions); and a JSON blob in `store_settings` (bootstrap framework storage for singletons — no
foreign key, no per-row query, and one toggle would rewrite the whole value). The table carries
**no runtime columns** (`running`, `last_error`): those are in-memory supervisor state surfaced by
`admin/projects_list` (11 §8) — and, since X-006, additionally mirrored into a table of their own
(`worktree_indexing_status`, see the note below), never into this one. `enabled = 0` keeps a row
enrolled but **dormant**;
`managed_worktrees` returns every row and the supervisor filters, the same "return everything,
decide in one pure place" discipline `worktree_state_clocks` already uses. `enabled` follows
§1.1's boolean convention (`INTEGER` 0/1 with `CHECK`), like `worktree_path.is_current` and
`model_space_representation.required`. The operations are `local_rag_store::registry::managed`:
`register_managed_worktree` (idempotent upsert — a repeat bumps `updated_at` only, never
re-enabling a deliberately disabled row nor resetting `registered_at`, because enabling is its own
verb), `unregister_managed_worktree` and `set_managed_enabled` (both report whether a row matched;
`set_managed_enabled` is an `UPDATE`, never an upsert, because registration is explicit per
ADR-0009 and a toggle must not implicitly enroll), plus the reads `managed_worktrees` (all rows,
`ORDER BY worktree_id`) and `is_managed` (enrolled at all, regardless of `enabled` — the question
11 §6's double-indexing advisory asks). Writes compose in a `StateWriter::transaction`, so
enrolling a brand-new path is *one* transaction alongside `create_repository`/`create_worktree`.
The consumers — the daemon supervisor, the `local-rag project` CLI, and the advisory warning — are
`T20-06`/`T20-08`/`T20-09`; T20-01 ships exactly the table and its typed accessors, the same
division T02-05 drew relative to the policy guard that followed it.

As-built note (X-006, `[SPEC]`): `worktree_indexing_status` is **migration 13**
(`registry::SCHEMA_V13`, module `local_rag_store::registry::indexing_status`) — the durable
**outcome** of background indexing, one row per worktree that has completed at least one cycle.

It exists because the outcome previously lived only in the supervisor's memory
(`WorktreeTaskStatus`, T20-05), so every idle shutdown — 15 minutes of quiet by default, 02
§4.3 — erased the entire answer to "did background indexing ever run, and when?". That is the
observability gap X-006 was filed for: neither `local-rag project list` nor `project status`
could say anything at all about a worktree once its daemon had gone to sleep.

**Why a second table rather than columns on `managed_worktree`.** T20-01's note above rules out
runtime columns there, and its reasoning — subscription and runtime are orthogonal axes, and
conflating them makes both unreadable — is unchanged by this task; the owner chose the separate
table explicitly when this was raised. The pattern is the one §2.2's `worktree_projection_state`
already established: durable per-worktree runtime state lives beside the registry, keyed by the
same stable worktree id, never inside it. `SCHEMA_V10` is untouched (frozen once shipped).

`last_generation_id` carries **no foreign key** on purpose: it names the generation the last
successful cycle projected, but retention/GC (06 §5) may retire that generation later, and a
status row must never be the reason a sweep fails — the same choice
`worktree_projection_state.projection_op_id` already makes for an id kept for diagnosis rather
than for referential integrity. `in_progress_since` is deliberately **not** persisted: after a
crash such a row is a lie nothing would ever clear, and "a cycle is running right now" is a
question only a live daemon can answer truthfully (`admin/projects_list`, 11 §8).

The single writer is `write_indexing_status` — a full-row upsert of values the caller already
computed, never a read-modify-write. Because `consecutive_failures` arrives as a number rather
than an `x = x + 1` increment, replaying one cycle's outcome leaves the row identical instead of
inflating the counter. On success `last_success_at`/`last_generation_id` advance and `last_error`
clears; on failure both success fields keep their previous values through `COALESCE`, so
"last known good" survives an arbitrarily long failure streak — precisely what a stale-index
warning must read. Reads are `indexing_status` (one worktree) and `indexing_statuses` (all rows,
`ORDER BY worktree_id`, the deterministic order a CLI join needs). The caller is
`daemon::indexing::worktree_task::project_one`, in one short `StateWriter::transaction` taken
**outside** `write_locked` so it never lengthens L2 write-lock hold; a failure to persist is
logged and dropped, never fatal — the generation is projected either way.

### 2.2 Projection state & model registry

```sql
CREATE TABLE representation (
  representation_id       TEXT PRIMARY KEY,           -- UUIDv7
  kind                    TEXT NOT NULL CHECK
    (kind IN ('code_raw','code_context','structural_description','memory')),
  representation_version  INTEGER NOT NULL,
  normalization_version   INTEGER NOT NULL,
  model_id                TEXT NOT NULL,
  dimensions              INTEGER NOT NULL,
  distance_metric         TEXT NOT NULL CHECK (distance_metric IN ('cosine','dot','l2')),
  created_at              INTEGER NOT NULL,
  UNIQUE (kind, representation_version, normalization_version,
          model_id, dimensions, distance_metric)      -- canonical RepresentationKey
);

CREATE TABLE model_space (
  model_space_id  TEXT PRIMARY KEY,                   -- UUIDv7
  display_name    TEXT NOT NULL UNIQUE,
  state           TEXT NOT NULL CHECK
    (state IN ('building','projection_ready','active','retiring','failed')),
  coverage        TEXT,        -- advisory JSON {kind:{expected,ready,failed}}; recomputable
  benchmark_result TEXT,       -- JSON
  created_at      INTEGER NOT NULL,
  updated_at      INTEGER NOT NULL
);

CREATE TABLE model_space_representation (             -- [SPEC] normalization of rev6 shape
  model_space_id       TEXT NOT NULL REFERENCES model_space(model_space_id),
  representation_kind  TEXT NOT NULL,
  representation_id    TEXT NOT NULL REFERENCES representation(representation_id),
  required             INTEGER NOT NULL DEFAULT 1 CHECK (required IN (0,1)),
  PRIMARY KEY (model_space_id, representation_kind)
);

CREATE TABLE worktree_projection_state (              -- two-axis: generation × model space [FIXED]
  worktree_id                 TEXT PRIMARY KEY REFERENCES worktree(worktree_id),
  active_generation_id        TEXT REFERENCES generation(generation_id),
  active_model_space_id       TEXT REFERENCES model_space(model_space_id),
  projected_generation_id     TEXT REFERENCES generation(generation_id),
  projected_model_space_id    TEXT REFERENCES model_space(model_space_id),
  target_generation_id        TEXT REFERENCES generation(generation_id),
  target_model_space_id       TEXT REFERENCES model_space(model_space_id),
  projection_op_id            TEXT,                   -- UUID of in-flight/last op
  projection_schema_version   INTEGER NOT NULL,
  status                      TEXT NOT NULL CHECK
    (status IN ('clean','updating','dirty','rebuilding')),
  last_error                  TEXT,
  updated_at                  INTEGER NOT NULL
);
```

Deployment state (which model space is active where) lives **only** here; `model_space.state`
is build/registry state `[FIXED]`. `ProjectionVersion = (worktree_id, generation_id,
model_space_id, projection_schema_version)`.

### 2.3 Code side (path-independent, shared by content)

```sql
CREATE TABLE file_revision (
  file_revision_id    TEXT PRIMARY KEY,               -- UUIDv7
  content_hash        TEXT NOT NULL,                  -- H(file_content)
  parser_fingerprint  TEXT NOT NULL,                  -- canonical string, §2.3.1
  source_blob         BLOB NOT NULL,                  -- exact original bytes [FIXED: strict invariant]
  source_compression  TEXT NOT NULL CHECK (source_compression IN ('none','zstd')),
  source_encoding     TEXT NOT NULL,                  -- e.g. 'utf-8'
  newline_style       TEXT NOT NULL CHECK (newline_style IN ('lf','crlf','mixed')),
  source_size         INTEGER NOT NULL,               -- uncompressed bytes
  created_at          INTEGER NOT NULL,
  UNIQUE (content_hash, parser_fingerprint)
);

CREATE TABLE content_blob (                           -- identity + metadata; text lives in cache
  blob_id                TEXT PRIMARY KEY,            -- H(content_blob …)
  language               TEXT NOT NULL,
  algo_version           INTEGER NOT NULL,
  normalization_version  INTEGER NOT NULL,
  created_at             INTEGER NOT NULL
);

CREATE TABLE parsed_unit (
  unit_id           TEXT PRIMARY KEY,                 -- UUIDv7
  file_revision_id  TEXT NOT NULL REFERENCES file_revision(file_revision_id),
  unit_kind         TEXT NOT NULL CHECK
    (unit_kind IN ('symbol','file','config_section','text_section','fallback_chunk')),
  syntax_locator    TEXT NOT NULL,                    -- canonical serialization, NO path
  blob_id           TEXT NOT NULL REFERENCES content_blob(blob_id),
  span_start        INTEGER NOT NULL,                 -- byte offsets into exact source_blob
  span_end          INTEGER NOT NULL CHECK (span_end >= span_start),
  local_name        TEXT,
  kind              TEXT,                             -- language-level kind (fn/class/…)
  parent_unit_id    TEXT REFERENCES parsed_unit(unit_id),
  UNIQUE (file_revision_id, unit_kind, syntax_locator, span_start, span_end)
);
```

**2.3.1 `parser_fingerprint`** `[FIXED semantics, format [SPEC]]` — canonical sorted
`key=value;` string covering everything that affects unit boundaries:
`chunk=<policy_ver>;grammar=<name>@<ver>;lang=<language_id>;norm=<boundary_norm_ver>;queries=<ts_query_ver>`.
Consequence (explicit): byte-identical source under `.c` vs `.cpp` yields **different** file
revisions — language is selected by extension/path. Spans are always byte offsets into the
exact `source_blob`; unsupported encoding ⇒ the file is **skipped** (no transcoding without an
offset mapping) `[FIXED]`.

As-built format `[SPEC]` (realized by T04-02 in `crates/index/src/parse/`):

- Keys are sorted in **ascending ASCII byte order** (`chunk` < `grammar` < `lang` < `norm` <
  `queries`) and joined with `;`; `;` is a **separator with no trailing terminator** (the
  `key=value;` above denotes the pair grammar, not a trailing `;` — matching the concrete
  example). Sorting makes the value order-independent by construction.
- `lang` is the canonical `LanguageId` string (= the `index.languages` token). The
  language-by-path selector (`parse::select_language`) realizes ADR-0001's deferred
  "precise selector is T04-02": extension-only, case-insensitive
  (`.ts/.tsx/.mts/.cts`→typescript, `.js/.jsx/.mjs/.cjs`→javascript, `.rs`→rust).
- Version realization: `chunk=CHUNK_POLICY_VERSION`, `norm=BOUNDARY_NORM_VERSION`,
  `grammar=<grammar_name>@<grammar_version>`, `queries=<query_version>`; all `1` in v0.
  `BOUNDARY_NORM_VERSION` is **distinct** from `content_blob`'s `normalization_version`
  (§4.2) — that versions text identity, this versions boundary-affecting normalization.
- `grammar_version`/`query_version` are **our** boundary-version counters (not the upstream
  crate semver). T04-03 links the first real grammar (TypeScript, `tsx` variant) and
  **reconciles them to `@1`/`1`** against the pinned crates `tree-sitter 0.24` /
  `tree-sitter-typescript 0.23`; T04-04 links the second (JavaScript) the same way against
  `tree-sitter-javascript 0.23`, and T04-05 the third (Rust) against `tree-sitter-rust 0.23` —
  each pinned at 0.23 (ABI 14), not the 0.24+/0.25 lines (ABI 15, which the 0.24 core rejects)
  (recorded in `parse::fingerprint::descriptor`; ADR-0002). No
  units are persisted before T04-06, so the `fingerprint.rs` goldens stay green — this is the
  deliberate, documented reconciliation the earlier rev anticipated, never a silent bump. A
  later grammar/query change that shifts unit boundaries is a deliberate version bump (a rebuild
  event), guarded by the `version_constants_and_descriptors_are_pinned` tripwire.
  `BOUNDARY_NORM_VERSION = 1` means "no boundary-shifting normalization (raw bytes are parsed)";
  `CHUNK_POLICY_VERSION = 1` means "fallback chunks only for outermost ERROR/MISSING spans, no
  size-based splitting". The FIXED key set, the sort, extension-based language selection, and the
  `.c`/`.cpp` consequence are unchanged.

### 2.4 Generation membership (path-dependent)

```sql
CREATE TABLE generation_file (
  generation_id     TEXT NOT NULL REFERENCES generation(generation_id),
  normalized_path   TEXT NOT NULL,
  display_path      TEXT NOT NULL,
  file_revision_id  TEXT NOT NULL REFERENCES file_revision(file_revision_id),
  PRIMARY KEY (generation_id, normalized_path)
);

CREATE TABLE skipped_file (
  generation_id    TEXT NOT NULL REFERENCES generation(generation_id),
  normalized_path  TEXT NOT NULL,
  reason           TEXT NOT NULL CHECK
    (reason IN ('binary','lfs','huge','secret','ignored','encoding')),
  content_hash     TEXT,
  PRIMARY KEY (generation_id, normalized_path)
);
-- skipped files NEVER get occurrences [FIXED §10 invariant]

CREATE TABLE generation_unit_occurrence (
  occurrence_id    TEXT PRIMARY KEY,                  -- H(occurrence_id, …) deterministic
  generation_id    TEXT NOT NULL,
  normalized_path  TEXT NOT NULL,
  unit_id          TEXT NOT NULL REFERENCES parsed_unit(unit_id),
  qualified_name   TEXT,
  context_hash     TEXT,
  UNIQUE (generation_id, normalized_path, unit_id),
  -- occurrence only on a member file ⇒ (with file_revision.source_blob NOT NULL)
  -- the source-blob invariant is structural:
  FOREIGN KEY (generation_id, normalized_path)
    REFERENCES generation_file(generation_id, normalized_path)
);
CREATE INDEX occurrence_by_gen ON generation_unit_occurrence(generation_id);
CREATE INDEX occurrence_by_unit ON generation_unit_occurrence(unit_id);

CREATE TABLE unresolved_reference (                   -- parse-local, per file revision
  file_revision_id  TEXT NOT NULL REFERENCES file_revision(file_revision_id),
  source_unit_id    TEXT NOT NULL REFERENCES parsed_unit(unit_id),
  reference_text    TEXT NOT NULL,
  reference_kind    TEXT NOT NULL
);
CREATE INDEX unresolved_by_rev ON unresolved_reference(file_revision_id);

CREATE TABLE resolved_graph_edge (                    -- per generation, on occurrence IDs
  generation_id      TEXT NOT NULL REFERENCES generation(generation_id),
  src_occurrence_id  TEXT NOT NULL REFERENCES generation_unit_occurrence(occurrence_id),
  dst_occurrence_id  TEXT NOT NULL REFERENCES generation_unit_occurrence(occurrence_id),
  edge_kind          TEXT NOT NULL,                   -- import | call_heuristic | … [OPEN: final graph semantics]
  resolution         TEXT NOT NULL CHECK (resolution IN ('heuristic','syntax','lsp')),
  UNIQUE (generation_id, src_occurrence_id, dst_occurrence_id, edge_kind)
);
```

Locators `[FIXED]`:
`SyntaxLocator = {language, syntax_path | local_ordinal, signature_fingerprint, blob_id}` — no path.
`OccurrenceLocator = {normalized_path, qualified_name, SyntaxLocator}`.

As-built `SyntaxLocator` serialization `[SPEC]` (realized by T04-02 in
`crates/index/src/parse/locator.rs`): a canonical, path-free string — sorted `key=value`
over the allow-list `{anchor, blob, lang, sig}` joined with `;` (via the same `canonical_kv`
as the fingerprint), where `anchor` is tagged `p:<syntax_path>` or `o:<local_ordinal>`.
Path-freedom is enforced in two layers: the `SyntaxLocator` type structurally cannot hold a
filesystem path, and the parser rejects any path-like key (`path`, `normalized_path`,
`display_path`, …) or non-allow-listed key (spec 01 §5.1).

As-built `SyntaxLocator` **derivation** `[SPEC]` (realized by T04-03 in
`crates/index/src/parse/`, fixed by **ADR-0002** — resolves the `SyntaxLocator` half of O7):
`anchor` is a named route `<lang_kind>:<name>/…` from the enclosing declaration ancestors when
the unit and all ancestors have safe (identifier) names (the whole-file unit uses `p:file`),
else `o:<local_ordinal>` (position among the parent's direct child units in canonical order);
`signature_fingerprint` is the domain-separated hash (`…/signature_fingerprint`, §1.2) of a
canonical descriptor of the unit's signature built from the parse subtree only (path-free,
offset-free). The parse output is a pure, DB-free function `bytes -> {units, unresolved_refs}`
(spans are byte offsets into the exact `source_blob`; a unit's `parent` is an in-file index;
`unit_id`/`blob_id` are minted at persistence, T04-06). Units are emitted in a canonical order
(`span.start` asc, `span.end` desc, `unit_kind`, `lang_kind`, `local_name`, `sig`). The graph
half of O7 (`resolved_graph_edge.edge_kind`, `find_usages`/`get_dependencies`) remains `[OPEN]`.

As-built parse-output **persistence** `[SPEC]` (realized by T04-06 in
`crates/index/src/parse/persist.rs::persist_parse_output`, over new `create_or_reuse_parsed_unit`
/ `delete_unresolved_references_for_revision` in `crates/store/src/code`): the content side of one
file's parse (`content_blob`, `parsed_unit`, `unresolved_reference`) is written under a single
atomic transaction, so any error rolls the whole file's graph back (no partial graph, 06 §2.1).
It is idempotent by create-or-reuse: a `content_blob` reuses by its content-derived PK; a
`parsed_unit` reuses by its natural key `UNIQUE (file_revision_id, unit_kind, syntax_locator,
span_start, span_end)` — not by `unit_id`, which is a fresh UUIDv7 the caller mints and which is
consumed only when a row is created. Because `unresolved_reference` has no natural key (a file may
legitimately repeat a specifier), reference idempotence is a per-revision clear-then-reinsert, not
per-row reuse. Re-persisting an unchanged revision therefore adds no duplicate rows and returns the
*same* unit ids (the stability the deterministic `occurrence_id` of group 05 builds on). Units are
inserted in the canonical order above, so a parent's row always precedes its children's
`parent_unit_id` self-reference. Persisting generation membership (`generation_unit_occurrence`) is
group 05, not this function.

As-built note (T05-01, `[SPEC]`): the deterministic `occurrence_id` derivation is
`local_rag_store::code::occurrence_id` — the free function `H(occurrence_id: generation_id,
normalized_path, unit_id)` through the generic `identity::domain::hash` entry point, assembling the
fields in the exact §1.2 table order (all three are text / already-hex identities → their exact
bytes). Because each id depends only on its own tuple, it is stable under retry/reconcile and
independent of row insertion order (§1.2 `[FIXED]`) — a store-layer golden pins the digest and asserts
the function only forwards its fields. `insert_occurrence` still stores the id verbatim (T03-01); the
generation *builder* that mints occurrences with this derivation and writes them is T05-03.

### 2.5 Memory side

```sql
CREATE TABLE observation_envelope (
  received_seq      INTEGER PRIMARY KEY AUTOINCREMENT, -- transactional monotone → cursor basis [FIXED]
  observation_id    TEXT NOT NULL UNIQUE,              -- UUIDv7 assigned at import
  source_event_id   TEXT NOT NULL,                     -- event-specific identity (07 §4)
  dedup_key         TEXT,                              -- ONLY for stable-identity events
  payload_hash      TEXT NOT NULL,
  event_type        TEXT NOT NULL,                     -- SessionStart | UserPromptSubmit | …
  evidence_kind     TEXT NOT NULL CHECK
    (evidence_kind IN ('user_statement','tool_result','test_result','code_state','model_claim')),
  trust             TEXT NOT NULL CHECK (trust IN ('low','normal','high')),
  source_timestamp  INTEGER,
  repo_id           TEXT REFERENCES repository(repo_id),
  worktree_id       TEXT REFERENCES worktree(worktree_id),
  session_id        TEXT NOT NULL,
  agent_id          TEXT,
  turn_id           TEXT,
  batch_id          TEXT,
  commit_hash       TEXT,
  short_evidence_excerpt TEXT,
  redaction_version INTEGER                            -- migration 8 (D-019); NULL if never scanned
);
CREATE UNIQUE INDEX envelope_dedup
  ON observation_envelope(dedup_key) WHERE dedup_key IS NOT NULL;  -- [FIXED]
CREATE INDEX envelope_session ON observation_envelope(session_id, received_seq);

CREATE TABLE observation_path (
  observation_id   TEXT NOT NULL REFERENCES observation_envelope(observation_id) ON DELETE CASCADE,
  normalized_path  TEXT NOT NULL,
  PRIMARY KEY (observation_id, normalized_path)
);

CREATE TABLE observation_payload (                    -- short TTL; envelope survives it [FIXED]
  observation_id   TEXT PRIMARY KEY REFERENCES observation_envelope(observation_id) ON DELETE CASCADE,
  redacted_payload BLOB NOT NULL,
  byte_size        INTEGER NOT NULL,
  expires_at       INTEGER NOT NULL
);

CREATE TABLE memory_entry (
  memory_id        TEXT PRIMARY KEY,                  -- UUIDv7
  kind             TEXT NOT NULL CHECK
    (kind IN ('fact','decision','convention','procedure','task','question','hypothesis')),
  state            TEXT NOT NULL,                     -- kind-specific machine, doc 04 §5
  text             TEXT NOT NULL,
  canonical_key    TEXT,
  scope_kind       TEXT NOT NULL CHECK (scope_kind IN ('global','repository','worktree')),
  scope_owner_id   TEXT NOT NULL,                     -- NOT NULL closes the NULL-unique hole [FIXED]
                                                      -- global → fixed singleton UUID
  confidence       REAL NOT NULL CHECK (confidence BETWEEN 0.0 AND 1.0),
  importance       REAL NOT NULL CHECK (importance BETWEEN 0.0 AND 1.0),
  valid_from_tree  TEXT,
  last_verified_tree TEXT,
  supersedes_id    TEXT REFERENCES memory_entry(memory_id),
  entry_version    INTEGER NOT NULL DEFAULT 1,
  created_at       INTEGER NOT NULL,
  updated_at       INTEGER NOT NULL
);
CREATE UNIQUE INDEX memory_canonical
  ON memory_entry(scope_kind, scope_owner_id, canonical_key)
  WHERE canonical_key IS NOT NULL;
-- kind is origin and IMMUTABLE [FIXED]; promotion only via supersede.
-- No scope_repo_id for worktree scope: repo is derived through worktree [FIXED].
-- Global singleton scope_owner_id [SPEC]: 00000000-0000-7000-8000-000000000001.

CREATE TABLE memory_evidence (                        -- survives payload TTL [FIXED]
  memory_id       TEXT NOT NULL REFERENCES memory_entry(memory_id),
  observation_id  TEXT NOT NULL REFERENCES observation_envelope(observation_id),
  evidence_kind   TEXT NOT NULL CHECK
    (evidence_kind IN ('user_statement','tool_result','test_result','code_state','model_claim')),
  session_id      TEXT NOT NULL,
  agent_id        TEXT,
  commit_hash     TEXT,
  PRIMARY KEY (memory_id, observation_id)
);

CREATE TABLE pending_memory_candidate (
  candidate_id        TEXT PRIMARY KEY,               -- UUIDv7
  proposed_operation  TEXT NOT NULL,                  -- JSON: op + target + text + …
  conflicts           TEXT,                           -- JSON array of memory_ids
  review_state        TEXT NOT NULL CHECK
    (review_state IN ('pending','approved','rejected','expired')),
  created_at          INTEGER NOT NULL
);

CREATE TABLE candidate_evidence (                     -- FK provenance, not embedded snapshots [FIXED]
  candidate_id    TEXT NOT NULL REFERENCES pending_memory_candidate(candidate_id) ON DELETE CASCADE,
  observation_id  TEXT NOT NULL REFERENCES observation_envelope(observation_id),
  PRIMARY KEY (candidate_id, observation_id)
);

CREATE TABLE processing_cursor (
  session_id                       TEXT PRIMARY KEY,
  last_consolidated_received_seq   INTEGER NOT NULL
);

CREATE TABLE consolidation_run (
  run_id             TEXT PRIMARY KEY,                -- UUIDv7
  session_id         TEXT NOT NULL,
  from_received_seq  INTEGER NOT NULL,
  to_received_seq    INTEGER NOT NULL,
  router_version     TEXT NOT NULL,
  state              TEXT NOT NULL CHECK (state IN ('pending','running','applied','failed')),
  lease_until        INTEGER,
  created_at         INTEGER NOT NULL,
  updated_at         INTEGER NOT NULL
);
CREATE INDEX consolidation_by_session ON consolidation_run(session_id, state);

CREATE TABLE audit_event (
  audit_id         INTEGER PRIMARY KEY AUTOINCREMENT,
  entity_kind      TEXT NOT NULL,                     -- memory_entry | candidate | …
  entity_id        TEXT NOT NULL,
  entity_version   INTEGER NOT NULL,
  op               TEXT NOT NULL,                     -- create|reinforce|resolve|supersede|retract|edit|merge|noop
  actor            TEXT NOT NULL CHECK (actor IN ('user','router','system')),
  idempotency_key  TEXT UNIQUE,                       -- H(memory_op, run_id, op_index) for router ops
  payload          TEXT,                              -- JSON diff/details
  created_at       INTEGER NOT NULL,
  UNIQUE (entity_kind, entity_id, entity_version)
);

CREATE TABLE spool_import_cursor (                    -- durable import progress (07 §6)
  session_id        TEXT PRIMARY KEY,
  segment_seq       INTEGER NOT NULL,
  committed_offset  INTEGER NOT NULL,
  updated_at        INTEGER NOT NULL
);

CREATE TABLE memory_text_normalization (              -- migration 15; T21-13 (ADR-0011)
  memory_id           TEXT PRIMARY KEY REFERENCES memory_entry(memory_id) ON DELETE CASCADE,
  status              TEXT NOT NULL CHECK (status IN ('translated','english','failed')),
  canon_text_sha256   TEXT NOT NULL,                  -- memory_entry.text as this row saw it
  source_text         TEXT,                           -- the author's words; NULL unless 'translated'
  source_language     TEXT,                           -- detector's answer, advisory
  normalizer_model_id TEXT,                           -- provenance: which model produced the canon
  prompt_version      INTEGER,                        -- provenance: which prompt
  normalizer_version  INTEGER NOT NULL,               -- bump re-examines every row
  attempt_count       INTEGER NOT NULL DEFAULT 0,
  last_error          TEXT,
  next_attempt_at     INTEGER,                        -- transient backoff gate, epoch ms
  created_at          INTEGER NOT NULL,
  updated_at          INTEGER NOT NULL,
  CHECK ((status = 'translated') = (source_text IS NOT NULL))
);
CREATE INDEX memory_normalization_queue
  ON memory_text_normalization(status, next_attempt_at);
```

As-built note (T13-04, `[SPEC]`): this section's block covers two independently-owned table
clusters, shipped by two different tasks/groups. **T13-04** (group 13) ships exactly the
observation ledger illustrated above — `observation_envelope`, `observation_path`,
`observation_payload`, `spool_import_cursor` — as migration version 7
(`local_rag_store::observation::SCHEMA_V7`). **`memory_entry`, `memory_evidence`,
`pending_memory_candidate`, `candidate_evidence`, `processing_cursor`, `consolidation_run`, and
`audit_event`** remain unbuilt here; they are group 14's T14-01 ("Memory DDL and legal
transitions", `groups/14-memory.md`), a later, separate migration. This mirrors the precedent
D-013 already established for splitting a single spec block's ownership across groups when part
of it depends on infrastructure a later group provides. `local_rag_store::observation::
import_batch` is this ledger's only writer: it resolves `worktree_root` once per batch (see 07 §5's
as-built note on why resolution is an injected `RequestRoot`, not computed here), applies exact
dedup via `observation_envelope`'s partial `envelope_dedup` index (`INSERT ... ON CONFLICT(dedup_key)
WHERE dedup_key IS NOT NULL DO NOTHING RETURNING received_seq`), applies the bounded best-effort
window (07 §5 as-built note), inserts `observation_path`/`observation_payload` rows, and advances
`spool_import_cursor` — all in one transaction. `observation_payload.expires_at` is computed here
from the already-existing `StorageConfig::payload_ttl_hours` (default 72h, from the storage
section's `[OPEN]` provisional-defaults note above); removing expired rows is T13-05's sweeper, not
this task's. `payload_hash` is a plain `local_rag_core::hash::sha256_hex` over the frame's
(already redacted) payload text, or over an empty byte slice for an envelope-only event —
deliberately not a domain-separated `identity::domain` hash, the same reasoning 07 §4's as-built
note gives for the best-effort fingerprints (never an identity/UNIQUE/FK column).

As-built note (D-019, `[SPEC]`, found at gate G13): migration **8**, `observation_redaction_version`
(`local_rag_store::observation::SCHEMA_V8`), adds `observation_envelope.redaction_version` —
`ALTER TABLE observation_envelope ADD COLUMN redaction_version INTEGER`, **no backfill**. Unlike
D-007's `state_changed_at` (where a `0` default would have been actively wrong), `NULL` is the
correct value both for a row written before this migration and for an envelope-only (denied)
event, whose payload was never scanned in the first place — there is no fabricated version to
backfill to. Written by `local_rag_store::observation::import::import_batch` from the decoded
frame's `redaction_version` field (07 §3's own D-019 as-built note); read by nothing yet (a future
audit/inspect consumer, 11 §6's `local-rag inspect observation <id>`, is the natural owner).
Closes the gap spec 12 §2's "versioned `redaction_version` recorded in envelopes" described but
T13-01…T13-04 never wired end to end.

As-built note (T14-01, `[SPEC]`): migration **9**, `memory` (`local_rag_store::memory::SCHEMA_V9`),
ships the seven tables this section's block left unbuilt above — `memory_entry`,
`memory_evidence`, `pending_memory_candidate`, `candidate_evidence`, `processing_cursor`,
`consolidation_run`, `audit_event` — byte-exact apart from stripping the block's own prose
comments (the same convention `SCHEMA_V7` already established). Two details the DDL alone does
not make explicit: `memory_entry.scope_owner_id`'s "global → fixed singleton UUID" rule is a
comment, not a `CHECK`, so `local_rag_store::memory::create_memory_entry` enforces it in Rust
(`memory::GLOBAL_SCOPE_OWNER_ID`, `00000000-0000-7000-8000-000000000001` — the same literal
value `registry::DEFAULT_MODEL_SPACE_ID` happens to use for an unrelated table, coincidental, not
shared); and `memory_entry.state` carries no `CHECK` at all (unlike
`pending_memory_candidate.review_state`/`consolidation_run.state`, which do), because its legal
domain is conditional on `kind` — see 04 §5's as-built note for the guard shape. This task ships
schema plus the pure transition-legality guard per machine only; the atomic
mutation+evidence+audit+idempotency operation contract (08 §3) — including the `entry_version`
increment 04 §5 couples to a matching `audit_event` — is T14-02's transactional memory-op engine,
not this migration's concern.


As-built note (T21-13, `[SPEC]`, [ADR-0011](../adr/0011-english-canon-for-durable-memory.md)):
migration **15** inverts the table the next note describes, which is kept as history. The block
above shows v15 — brought to as-built at gate `G21`, which found it still carrying v14's columns
while every other migration in this section (8, 14) had updated it; the v14 shape is preserved in
the T21-01 note below, which is where the history belongs. English is
the canon (08 §3), so `memory_entry.text` holds the English text and this table holds the author's
own words: `normalized_text` → `source_text`, `source_text_sha256` → `canon_text_sha256` (the hash
of `memory_entry.text` as the row last saw it), `ready`/`skipped`/`failed` →
`translated`/`english`/`failed`. A table rebuild rather than `ALTER`s, because SQLite cannot alter a
`CHECK`; deliberately **not** flagged destructive, because the one table it rebuilds is entirely
derived — model output plus retry bookkeeping — and the `VACUUM INTO` copy that flag buys would
protect nothing canonical (spec 13 §3). Data carried across by meaning: `skipped` rows become
`english` (the entry is English and unchanged), `failed` rows keep their retry bookkeeping, and
`ready` rows are **dropped** — their English text was never installed as canon and v15 has no state
for a translation waiting to be installed. `english` is not tidiness: the queue predicate is SQL
with a `LIMIT` and cannot run the detector, so without a stored marker every English entry would be
re-offered on every tick and starve the entries that need work.

As-built note (T21-01, `[SPEC]`, ADR-0010): migration **14**, `memory_text_normalization`
(`local_rag_store::memory::normalization`), adds the English-normalization axis of durable
memory — at most one row per `memory_entry`, holding the variant fed to the embedder, the hash of
the text it was derived from, and the provenance and retry bookkeeping of the attempt that
produced it.

**Why a table and not columns on `memory_entry`.** 08 §3 `[FIXED]` lets only `edit` change a
memory's text, and only with a new `entry_version` in the audit ledger, while `reinforce` may not
touch it at all — a background translator writing into that column would violate both, and would
show the user a machine translation of their own note. The variant is a derived axis, and the
precedent for a derived axis is a table of its own (§2.1's `worktree_indexing_status`, X-006), not
columns on a migration frozen by checksum. The separate table also makes T21-07's purge a single
`DELETE` through `ON DELETE CASCADE`, and gives T21-08 a countable `GROUP BY status`.

**Staleness is `source_text_sha256`, never `entry_version`.** `apply_reinforce` bumps the version
without touching the text, so a version comparison would both re-translate unchanged entries and
let an unrelated bump make a stale variant look current. Readers therefore compare the stored hash
against the entry's text as it stands; `upsert_normalization` refuses a write whose
`source_text_sha256` no longer matches, which is what stops an `edit` landing mid-translation from
committing a translation of text that is no longer there.

**Why not `cache.sqlite`.** ADR-0010 rejected it: a translation is not locally recomputable —
restoring it needs an LLM that may be unavailable — so it cannot live in a store whose defining
invariant is that it is fully rebuildable. This is the opposite of `normalized_text_cache`'s case
(§4.2), where normalization is a pure function of the blob; the schema lint that forbids a
`normalized_text` column elsewhere in `state.sqlite` is scoped to those code rows and names this
table as the one exemption, with that reasoning.

**Inert on upgrade.** The migration creates an empty table, and an empty table is
indistinguishable from the pre-T21-01 store: every effective text is still the original, every
subject hash is unchanged, every existing vector stays valid. T21-01 ships the storage and its
guards only — the reader that decides which text is embedded is T21-02, the detector T21-03, the
translator T21-04, the daemon worker T21-06. Nothing consumes this table yet.

## 3. `state.sqlite` write policy `[FIXED, numbers [SPEC]]`

Single **bounded global write queue** feeding one writer task (SQLite has one physical writer;
per-worktree writers converge into it). Batched `last_used_at`/`last_seen_at` updates
(flush ≤ every 5 s or 500 rows). WAL checkpoint: `PASSIVE` opportunistically;
`TRUNCATE` when WAL > 64 MiB and no readers. `VACUUM` by metrics (free-page ratio > 30 %),
never by schedule.

As-built note (T03-04, `[SPEC]`): the batched-`last_used_at` seam for the cache side is
`LastUsedSink`/`BatchingLastUsed` + `flush_last_used` (§4.2 note); the flush-cadence driver is a
later task. The `last_seen_at` registry updates remain immediate single-row writes for now.

As-built note (D-083, `[SPEC]`): this section's own checkpoint policy — "`PASSIVE`
opportunistically; `TRUNCATE` when WAL > 64 MiB and no readers" — had **no implementation at all**
outside shutdown. `local_rag_store::StateWriter::checkpoint` existed and was correct, but the only
production caller was `daemon::shutdown` (02 §4.3's step). While the daemon ran, nothing
checkpointed, and SQLite's own automatic checkpoint cannot compensate: it can never transfer a
frame that a reader still needs, and an indexing cycle reads the store essentially without a gap
(`build_generation`'s per-file `file_revision_id_by_content_key` accounted for ~96 % of one
sampled cycle) while writing gigabytes of `generation_file`/`occurrence` frames. The result was
unbounded growth, measured on the owner's store: `state.sqlite-wal` at **324 GB** against a 41 GB
database, 336 GB for the store as a whole, and a full disk. `PRAGMA wal_checkpoint(PASSIVE)`
reported `busy = 0` with `checkpointed_frames` frozen at exactly 206 516 for tens of minutes while
`log_frames` climbed from 6.3 to 10 million — the checkpointer was not blocked, it simply had
nothing it was allowed to move. That signature is now pinned by a deterministic test
(`crates/store/tests/checkpoint.rs`).

The checkpoint is now taken at the end of every indexing cycle
(`daemon::indexing::worktree_task`), after the durable status write, where the cycle's own readers
are gone. It is unconditionally `TRUNCATE` rather than this section's 64 MiB threshold: the
threshold exists so a blocking truncate is not paid too often, and once per cycle is already rare,
while `PASSIVE` alone would leave the file at its high-water mark — transferring the frames but
never returning the disk, which is the half that actually mattered here. The numbers in this
section are `[SPEC]`, so this is a documented refinement of the cadence, not a change to the
policy's shape. Consolidation is deliberately left out: its per-run write volume is a handful of
rows, and nothing measured points at it.

As-built note (D-086, `[SPEC]`, amends the note above on its last two sentences): the policy now has
a **second** driver, and the `TRUNCATE` clause is implemented as this section literally writes it —
`> 64 MiB and no readers`, checked on the consolidation trigger's tick
(`daemon::consolidation_trigger`, 15 s). Two of D-083's closing claims are retracted by measurement.

"The checkpoint is now taken at the end of every indexing cycle" was the whole implementation, and
D-089 has since stopped a reconcile from producing a cycle when the tree is unchanged — so a
repository nobody is editing reaches that boundary **never**. Nothing else returned the file:
`journal_size_limit` is unset workspace-wide, so SQLite never shrinks the `-wal` on its own, and
`PASSIVE` transfers frames without giving the disk back.

"Consolidation is deliberately left out: its per-run write volume is a handful of rows" is
falsified: it is the largest writer outside indexing, spool import runs inside the same tick, and
that tick is the one boundary that arrives whether or not the repository changed. It is now where
the threshold is tested.

"No readers" is **approximated** by "no `JobKind::Reconcile` running", and the approximation's edge
is stated here rather than glossed. It covers the reader D-083 measured as the blocker — the
embedding backfill's, held across `blob_index`/`context_index`/`write_coverage` — and every
short-lived reader (search, the trigger's own queries) opens and drops within a call. It does not
cover `local_rag_index::reconcile::build`'s own read connection, which is open for the whole build
and sits *before* the job guard `project_one` takes. The gap is a cost and not a hazard: a
`TRUNCATE` under a live reader transfers what it may, leaves the file at its high-water mark and
returns, so the worst case is one wasted `PRAGMA` on a tick during a build — bounded further by the
threshold. Closing it properly means giving the build phase a job guard, which belongs to the
indexing task rather than to this policy.

`PASSIVE` gains no explicit caller: SQLite's own `wal_autocheckpoint` (1000 pages, unset here and
therefore at its default) **is** the opportunistic half, and on a quiet store it demonstrably keeps
up — measured 2026-08-21, `state.sqlite-wal` flat at 9 747 952 bytes across four consecutive
one-minute samples while the consolidation trigger wrote on every 15 s tick.

Live acceptance, deferred to `D-090` at the time and carried out with it (with two writers on the
store, any measurement of this driver measures something else): a daemon brought up on a store
whose indexing was disabled, with a 321 096 352-byte `-wal` left behind by a `kill -9`'d cycle,
took the file to **0** thirty-eight seconds after startup — no indexing cycle, no restart, nothing
else that could have done it. Before this note's change there was no driver that could: the only
one was the cycle boundary, and there were no cycles.

What this does **not** claim: that the threshold would have prevented D-083's 324 GB. It would not.
That growth happened *under* the indexing cycle's own reader, where this clause correctly declines
to truncate — measured again on 2026-08-21, one 2.8-minute cycle (`duration_ms=169732`) took the
`-wal` from 9.7 MB to **2.5 GB**, about 0.9 GB/min, and its own end-of-cycle truncate returned all
of it. The mechanism behind that number was the cycle failing to *end*, which is D-088's subject,
not this section's. What the threshold covers is the other case: a large write from outside a cycle
— spool import, the startup generation sweep, a consolidation burst — on a store whose indexing
then goes quiet.

## 4. `cache.sqlite` — rebuildable, independently validated

```sql
PRAGMA journal_mode=WAL;
PRAGMA foreign_keys=OFF;          -- no FKs into another DB; internal integrity via heads
PRAGMA synchronous=NORMAL;        -- loss ⇒ rebuild, never data loss [SPEC]
PRAGMA busy_timeout=5000;
```

As-built note (T15-01, `[SPEC]`): unlike §3's `state.sqlite` write policy, this section fixes no
WAL-checkpoint policy of its own. `local_rag_store::cache::CacheWriter::checkpoint` adopts §3's
literal `PASSIVE` opportunistically / `TRUNCATE` policy for `cache.sqlite` too, called on shutdown
(02 §4.3's "flush WAL checkpoint" step) alongside `state.sqlite`'s own checkpoint. This is a
deliberately more aggressive policy than a rebuildable cache strictly needs, but it is strictly
*safe* here — this section's own `synchronous=NORMAL` rationale ("loss ⇒ rebuild, never data
loss") already accepts a cache-side loss as the worst case, and adopting one proven policy is
simpler than inventing a second, unneeded one.

### 4.1 Meta / binding

```sql
CREATE TABLE cache_meta (          -- validated at open (§4.4)
  key   TEXT PRIMARY KEY,          -- store_instance_uuid | cache_schema_version | created_at
  value TEXT NOT NULL
);
```

### 4.2 Embedding & text caches

```sql
CREATE TABLE embedding_cache (
  subject_kind       TEXT NOT NULL CHECK
    (subject_kind IN ('content_blob','occurrence_context','memory_entry')),
  subject_hash       TEXT NOT NULL,                   -- domain-separated, §1.2
  representation_id  TEXT NOT NULL,                   -- logical FK → state.representation
  dimensions         INTEGER NOT NULL,
  vector_f32         BLOB NOT NULL,
  byte_size          INTEGER NOT NULL,
  checksum           TEXT NOT NULL,                   -- H over vector bytes
  created_at         INTEGER NOT NULL,
  last_used_at       INTEGER NOT NULL,
  PRIMARY KEY (subject_kind, subject_hash, representation_id)
) WITHOUT ROWID;

CREATE TABLE normalized_text_cache (                  -- derived from source_blob
  blob_id          TEXT PRIMARY KEY,
  normalized_text  TEXT NOT NULL,
  byte_size        INTEGER NOT NULL,
  created_at       INTEGER NOT NULL,
  last_used_at     INTEGER NOT NULL
);
```

`content_blob` embeddings are shared across paths; `occurrence_context` embeddings are
per-occurrence (context is path-dependent by definition) `[FIXED]`. Eviction: LRU by
`last_used_at` toward `embedding_cache_budget_mb`; rows pinned while referenced by an active
projection tuple or a running rebuild are exempt `[SPEC]`.

As-built note (T03-04, `[SPEC]`): `normalized_text_cache` is created (alongside `cache_meta`) by
the cache seed transaction (`local_rag_store::cache::open`), and `CACHE_SCHEMA_VERSION` is bumped
to `2` — an older `cache_meta`-only cache is auto-dropped-and-rebuilt on open (§4.4 step 2). The
`normalized_text` is derived from the exact `source_blob` by a **versioned normalization**
(`normalization_version = 1`, `local_rag_store::code::normalize`): strip a leading UTF-8 BOM →
`CRLF`/lone-`CR` → `LF` → Unicode **NFC** → trim trailing whitespace per line (deterministic,
idempotent). `byte_size` is the UTF-8 length of `normalized_text`. Because the row's `blob_id`
*is* `H(content_blob …)` over that text, there is no separate checksum: `verify_cached_text`
recomputes the identity and a mismatch means the row is corrupt → delete + regenerate from
`source_blob` (spec 06 §4). Normalized text is stored **only** here — never in the canonical
`content_blob` row (asserted by the schema audit) — so the cache stays fully rebuildable and the
content-shared `state.sqlite` rows stay path-/text-free (spec 01 §5.1).

Batching seam (T03-04, `[SPEC]`, spec §3): the `last_used_at` updates required to be batched are
serviced through a seam — `LastUsedSink`/`BatchingLastUsed` (dedup-to-latest in-memory buffer) +
`flush_last_used` (one batched `UPDATE` transaction). The flush *cadence* (≤ 5 s / 500 rows) is a
later reconcile/search task; T03-04 ships only the interface + accumulator + flush helper.

As-built note (T11-02, `[SPEC]`): `embedding_cache` is created (byte-exact reproduction of the
block above, `WITHOUT ROWID` preserved) by `local_rag_store::cache::open`'s seed transaction, and
`CACHE_SCHEMA_VERSION` is bumped to `4` (`3`=+FTS, T08-01). `subject_hash` is produced by three
typed constructors in `local_rag_core::identity::domain` — `subject_content_blob(blob_id)` (one
field, §1.2), `subject_occurrence_context(context_version, serialization)` (two fields, version
LE `u32`), `subject_memory_entry(memory_id, text)` (two fields, the table's own "`H(text)`" computed
via `hash::sha256_hex`, not a domain-separated hash — no domain exists for raw memory text and the
memory tables this would back do not exist before group 14). **`checksum` ("H over vector bytes")
is a plain, non-domain-separated `hash::sha256_hex` digest, not a spec 03 §1.2 identity hash** — an
explicit as-built decision: §1.2's domain table is for hashes things are looked up/deduped by
(`subject_hash`, manifest hashes); the vector-bytes checksum has no identity role, only corruption
detection, the same family `Migration::checksum` already uses. `vector_f32` is little-endian `f32`
(`local_rag_store::cache::embedding::{encode_vector_le, decode_vector_le}`); `verify_cached_embedding`
checks `dimensions`/`byte_size` against the decoded length before recomputing the checksum (cheap
before expensive). The batching seam (`BatchingLastUsedEmbeddings`/`flush_last_used_embeddings`) is
a second, composite-keyed type mirroring `normalized_text_cache`'s, since this table's key is the
three-part `(subject_kind, subject_hash, representation_id)`, not a single `blob_id`.

Eviction (T11-02, `[SPEC]`): `local_rag_store::eviction::run_embedding_cache_eviction` — LRU by
`last_used_at` toward `embedding_cache_budget_mb` (`local_rag_core::config::StorageConfig`, already
`[storage]`-configured, default 2048 MiB), batched (`EVICTION_BATCH_ROWS = 500`, mirroring the
retention sweep's own `[SPEC]` batch ceiling), dry-run capable. Pins: a
`(generation_id, model_space_id)` tuple is pinned from **both** the `active_*` and `target_*`
columns of every worktree's `worktree_projection_state` row — `active_*` covers "an active
projection tuple" and "a running rebuild" (rebuild always retargets the active tuple, never
`target`, spec 05 §7); `target_*` is a deliberate, conservative superset (an in-flight `switch()`
reads `embedding_cache` for the target tuple's missing points before committing, spec 05 §5 step 1).
Only `code_raw` subjects were resolved to real pinned keys at T11-02 (via the occurrence →
`parsed_unit.blob_id` join and `subject_content_blob`); `code_context`'s subject format was still
`[OPEN]` (09 §3) and `memory`'s backing tables do not exist before group 14 — both were skipped as
a safe no-op, since no such `embedding_cache` row could exist yet either. D-016 added the
`code_context` resolution (see the as-built note below); `memory` remains the only skipped kind.

As-built note (D-016, `[SPEC]`): the `occurrence_context` serialization is no longer `[OPEN]` at
this level — `local_rag_store::code::context` (`serialize`, `CONTEXT_VERSION = 1`) renders the
labelled envelope, and `context_subjects_for_generation` derives one subject per occurrence of a
generation from `state.sqlite` alone. The envelope is line-oriented and fixed-order:

```text
File: {normalized_path}
Type: {unit_kind}/{language kind}
Name: {local_name}
Doc: {doc block, blank lines dropped}
Sig: {first line of the unit, capped at SIGNATURE_CAP_CHARS = 200 chars}
Code:
{normalized unit text}
```

Absent fields are **omitted entirely**, never emitted empty, so two units differing only in
whether they carry a docblock never hash alike through a shared blank label. `Code:` is always
last and always present. The doc block is recovered by walking backwards from the unit's span over
blank lines and then over either a `/** … */` block or a run of `//` lines, stopping at the
previous unit's end — the parsers do not attach comments to units, so this is a *reader-side*
reconstruction and does not touch `parsed_unit` spans or `content_blob` identity. That separation
is the point: the envelope is a search **representation**, so it lives in the representation
layer, while the content-shared rows stay path- and context-free (01 §5.1). Per-occurrence, not
per-blob: the path is inside the pre-image, so the "context is path-dependent by definition"
`[FIXED]` above is now structural rather than aspirational, and two occurrences of one
`content_blob` produce two distinct subjects (`crates/store/tests/subjects.rs::
context_subjects_do_not_share_across_occurrences`).

Pin rule, revised (T11-04, `[SPEC]`): the pinned set is now **pin-root generations × protected model
spaces** (`local_rag_store::subjects::protected_subject_keys`), a widening of the tuple-only rule
above in two places. *Generations* come from the retention pin roots (06 §5) rather than the
`active_*`/`target_*` columns alone — those columns are a subset of the roots, so every guarantee
recorded above still holds, and `retiring` generations inside the `K`/`T` window are covered too,
matching what the backfill worker (10 §4 step 2) is required to embed. *Model spaces* additionally
include every space in `building`/`projection_ready`, plus the default space
(`store_settings.default_model_space_id`). Without the first addition a space being backfilled — which
no worktree references yet, since a new space enters `worktree_projection_state` only at switch time
(10 §4 step 4) — would have its freshly written rows evicted as the LRU's first victims, i.e. the
worker and the evictor would fight indefinitely; the second covers a dormant worktree that has not
opened yet but will migrate to the default space when it does (05 §8 `[FIXED]`). A `retiring` space
no worktree references stays unprotected, which is exactly when "its cache rows become evictable"
(10 §4 step 6). `EvictionParams` therefore carries the retention `K`/`T` as well as the byte budget,
and `run_embedding_cache_eviction` takes `now_ms` — both come from the one `[storage]` section.

As-built note (T14-08, `[SPEC]`, closes D-013): `memory` had no subject function before this task —
`local_rag_store::subjects::expected_subject_keys` reported it in `SubjectSet::unsupported`, and
the backfill worker refused to run rather than report a silent zero-coverage lie (spec 04 §3, 02
§6). `RepresentationKind::Memory` now resolves through a new
`local_rag_store::subjects::memory_entry_subject_keys(conn, representation_id)`, called from
`expected_subject_keys`'s own `Memory` arm — **ignoring** that function's `generations` parameter
entirely, since `memory_entry` rows have no relationship to a code generation at all (unlike
`code_raw`/`code_context`). Every row (terminal states included — subject computation answers
"what should be embedded", a backfill-coverage question, independent of spec 08 §6's own,
separate recall-eligibility filter) gets one `EmbeddingKey` via the existing `subject_memory_entry`
identity constructor. `local_rag_embed::backfill`'s text-resolution step gained a matching
`SubjectKind::MemoryEntry` arm (a new `memory_index` reading `memory_entry.text` directly — no
normalization step, unlike `code_raw`'s `content_blob`-derived text). What remains **out** of this
task's scope, matching how `code_raw`'s own registration is production-wired only by a later
task (T15-07's `init`): no production code calls `set_model_space_representation(..,
RepresentationKind::Memory, required=true, ..)` yet — every existing caller of that function is a
test or `xtask bench`. This task only makes the subject function exist so that registration,
whenever it happens, does not hit `BackfillError::UnsupportedRequiredKind`.

### 4.3 FTS materialized view

```sql
CREATE TABLE fts_doc (
  fts_rowid      INTEGER PRIMARY KEY,
  occurrence_id  TEXT NOT NULL UNIQUE,                -- occurrence is generation-scoped already
  worktree_id    TEXT NOT NULL,
  generation_id  TEXT NOT NULL
);
CREATE INDEX fts_doc_by_wt ON fts_doc(worktree_id, generation_id);

CREATE VIRTUAL TABLE fts_occurrences USING fts5(
  name, qualified_name, path, signature, body,
  tokenize = 'unicode61 remove_diacritics 2'
);
-- rowid of fts_occurrences == fts_doc.fts_rowid.
-- Code-aware splitting (camelCase/snake_case, path components) is done app-side
-- BEFORE insert (09 §2); the tokenizer only finishes the job.

CREATE TABLE fts_projection_head (                    -- validity proof for the FTS view [FIXED]
  worktree_id            TEXT PRIMARY KEY,
  generation_id          TEXT NOT NULL,
  lexical_schema_version INTEGER NOT NULL,
  tokenizer_version      INTEGER NOT NULL,
  occurrence_count       INTEGER NOT NULL,
  manifest_hash          TEXT NOT NULL,               -- H(fts_manifest, …)
  updated_at             INTEGER NOT NULL
);
```

As-built note (T08-01, `[SPEC]`): `fts_doc`/`fts_occurrences`/`fts_projection_head`
are created byte-for-byte as above by the cache seed transaction
(`local_rag_store::cache::open`), and `CACHE_SCHEMA_VERSION` is bumped to `3` —
an older cache lacking these tables is auto-dropped-and-rebuilt on open (§4.4
step 2). No new Cargo feature is required: `libsqlite3-sys`'s bundled build
already compiles SQLite with `SQLITE_ENABLE_FTS5` unconditionally under the
existing `bundled` rusqlite feature (verified in `libsqlite3-sys`'s `build.rs`;
see CONTRIBUTING.md). The binary constants read at validation (06 §4) are
`local_rag_store::LEXICAL_SCHEMA_VERSION`/`TOKENIZER_VERSION`, both `1` at this
task. `local_rag_store::fts_manifest_hash(worktree_id, generation_id,
occurrence_ids)` hashes `worktree_id, generation_id ‖ occurrence IDs sorted
ascending bytewise and de-duplicated` through `Domain::FtsManifest` (already
defined ahead of this task), mirroring
`local_rag_projection::identity::manifest_hash`'s sorted-unique convention but
with **no** `model_space_id` axis — the FTS view is generation-scoped only.
Row insertion (the generation materializer) and per-search/at-open validation
are T08-02/T08-03; this task ships schema plus pure identity/tokenizer
functions only (09 §2 as-built note has the tokenizer detail).

### 4.4 Cache open-validation `[FIXED principle]`

At daemon startup / cache open:

1. `cache_meta.store_instance_uuid` ≠ state's → **drop and recreate cache**.
2. `cache_meta.cache_schema_version` unsupported → drop and recreate.
3. Per-worktree FTS validity is *not* checked here; it is checked lazily per search via
   `fts_projection_head` (06 §4).
4. `embedding_cache` rows are trusted per-row via `checksum` on read; mismatch → delete row,
   recompute lazily.

Loss of `cache.sqlite` or `projection/` loses nothing: both are reconstructible from
`state.sqlite` `[FIXED]` — this is asserted by the `rebuild` acceptance gate (14).

As-built note (T01-05, `[SPEC]`): steps 1–2 (plus a corrupt/unreadable file, "rebuild on
doubt") are implemented by the cache open path; steps 3–4 stay lazy and their tables land in
later tasks. `cache_schema_version` is currently `1`. Recreate is delete-and-recreate: the
stale `cache.sqlite`/`-wal`/`-shm` are removed and `cache_meta` (schema + binding rows) is
written in one transaction, so a crash mid-recreate leaves a missing/unbound cache that the
next open rebuilds — idempotent convergence, `state.sqlite` never touched. The authoritative
`store_instance_uuid` is supplied by the caller (the daemon reads it from `store_settings` at
startup, §4.1 / 02 §4.1); seeding that value into state is deferred to the UUIDv7 generator
(step 2) and daemon wiring (step 15).

As-built note (T15-01, `[SPEC]`): the deferred seeding above is now wired. §2.1's
`store_settings` table carries `store_instance_uuid` as an ordinary `(key, value)` row, produced
by `local_rag_store::registry::ensure_store_instance_uuid(tx, candidate)` — a first-writer-wins
atomic upsert (`INSERT ... ON CONFLICT(key) DO UPDATE SET value = store_settings.value
RETURNING value`, the same idiom `register_representation` already uses for its own
converge-on-first-registered-id upsert). `local_rag::daemon::lifecycle::DaemonHandle::start`
calls it once, inside one short transaction, immediately after 02 §4.1 step 2's migrations
succeed and strictly before step 3 opens the cache — `candidate` is a fresh UUIDv7 minted by the
caller before the call (entropy stays out of the write path, mirroring
`create_repository`'s own caller-mints-the-id discipline), discarded on every open after the
first. This value is the store's own durable identity across restarts, distinct from a running
daemon's own per-process `instance_uuid` (02 §2/§4.1) — the latter is fresh every start, the
former must survive them for `cache.sqlite`'s binding to mean anything.

Close semantics (D-009, `[SPEC]`): dropping a `CacheDb` closes the write queue but does **not**
wait for the writer thread to finish closing its connection — asynchronous teardown is the
deliberate design (02 §4.3 leaves graceful drain to the daemon, T15). `CacheDb::close` is the
*waiting* variant, and it is required before anything unlinks or recreates the same path:
SQLite opens `-wal`/`-shm` **by name**, so a still-closing connection can checkpoint/unlink the
sidecars of a *newly created* database at that path, leaving it empty and its readers seeing
`SQLITE_IOERR_SHORT_READ`. This matters for the recreate path above (and for tests that model an
interrupted rebuild); the daemon, which holds one instance per path for its lifetime, is
unaffected.

## 5. Migration boundaries `[FIXED]`

The identity model is migration-ready: deferred features (LLM descriptions, reranker, full
recall, ANN memory, several generators, cross-generation matching, LSP graph, multi-harness)
are **additive** — new tables/columns, no re-keying. What must never change without a
full-store migration: the hash schema version (§1.2), `occurrence_id` derivation,
`worktree_id` stability, `received_seq` semantics. Framework details: 13 §3.
