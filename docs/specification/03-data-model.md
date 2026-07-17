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

Deterministic IDs (`occurrence_id`, projection point IDs, memory-op keys) MUST be stable under
retry/reconcile and independent of row insertion order `[FIXED]`.

As-built note (T02-01, `[SPEC]`): `local_rag_core::identity::domain` implements the encoding
above (`encode`/`hash`, hex-encoded 64 chars) with a `Domain` enum covering all twelve domains
and the version-tagged string `local-rag/1/<slug>` (the two `subject/*` slugs keep their `/`).
Field payloads are raw bytes; the serialization conventions this codebase commits to are: text
→ UTF-8 bytes; an already-hex identity (a UUID or another domain hash) → its lowercase ASCII
bytes exactly as stored in its `TEXT` column; a fixed-width integer field → little-endian bytes
of the declared width. Only the two *lookup* fingerprints (`path_fingerprint`,
`remote_fingerprint`) have typed constructors so far; the deterministic-ID domains are hashed
through the generic `hash` entry point by their owning tasks (T03/T05/T08/T11/T14), which
assemble the field list in the order fixed by the table above.

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
  short_evidence_excerpt TEXT
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
```

## 3. `state.sqlite` write policy `[FIXED, numbers [SPEC]]`

Single **bounded global write queue** feeding one writer task (SQLite has one physical writer;
per-worktree writers converge into it). Batched `last_used_at`/`last_seen_at` updates
(flush ≤ every 5 s or 500 rows). WAL checkpoint: `PASSIVE` opportunistically;
`TRUNCATE` when WAL > 64 MiB and no readers. `VACUUM` by metrics (free-page ratio > 30 %),
never by schedule.

## 4. `cache.sqlite` — rebuildable, independently validated

```sql
PRAGMA journal_mode=WAL;
PRAGMA foreign_keys=OFF;          -- no FKs into another DB; internal integrity via heads
PRAGMA synchronous=NORMAL;        -- loss ⇒ rebuild, never data loss [SPEC]
PRAGMA busy_timeout=5000;
```

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

## 5. Migration boundaries `[FIXED]`

The identity model is migration-ready: deferred features (LLM descriptions, reranker, full
recall, ANN memory, several generators, cross-generation matching, LSP graph, multi-harness)
are **additive** — new tables/columns, no re-keying. What must never change without a
full-store migration: the hash schema version (§1.2), `occurrence_id` derivation,
`worktree_id` stability, `received_seq` semantics. Framework details: 13 §3.
