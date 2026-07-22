# 06 — Worktree Reconcile, FTS View, Retention/GC

**Principle `[FIXED]`: watcher = hint, reconcile = truth.** File-system events and `.git/HEAD`
changes only *schedule* work; correctness comes from authoritative reconcile.

## 1. Triggers & scheduling

- Watcher (`notify`) events, debounced `[SPEC: 500 ms quiet window]`.
- `.git/HEAD` / index change (checkout, rebase, commit).
- Watcher **overflow** → mandatory strict reconcile `[FIXED]` (never "resync from events").
- Startup of a known worktree; periodic strict reconcile `[SPEC: every 6 h while open]`.
- Manual: `local-rag reindex`.

Fast-path cache: `(mtime, size, file_id)` per path; any mismatch or doubt escalates the file to
content hashing. The fast-path cache is advisory only and lives in memory.

As-built note (T05-02, `[SPEC]`): steps 1–2 of the pipeline (§2 — the authoritative tree scan and
content-hash) are `local_rag_index::scan::scan`, which walks the worktree root with
`ignore::WalkBuilder` and returns a canonical, `(normalized_path, display_path)`-sorted
`ScanManifest` of indexable candidates. `ScanMode::Fast` consults the advisory `StatCache`
(`StatKey = (mtime, size, file_id)`); a reuse requires all three equal **and** a known `file_id`
(a missing inode is doubt → re-hash). `ScanMode::Strict` — the mandatory watcher-overflow / cold-
start / periodic mode — ignores the cache and hashes every candidate. The manifest never carries
mtime or cache state, so it is a deterministic function of the tree bytes; `content_hash` uses the
store's `H(file_content)` so it is directly comparable to `file_revision.content_hash`. Two skip
gates are applied here: `ignored` (native `ignore`-crate pruning — ignored files are absent from
the manifest, no `skipped_file` row) and the stat-only `huge` gate (`content_hash = None`, bytes
never read); the content-based reasons and the `skipped_file`/`file_revision`/generation writes are
the builder (T05-03). Determinism guards: `git_global(false)` + `parents(false)` (no `$HOME` /
above-root leakage), `follow_links(false)` + regular-files-only (symlinks/FIFOs/sockets excluded),
and the internal `.git` is pruned unconditionally. Non-git worktrees set `require_git(false)` so
`.gitignore` is still honored (§6 parity). An optional `prune_roots` excludes nested registered
worktrees (the daemon supplies them, T05-04). No git binary/crate is used (guardrail until T15).

As-built note (T05-04, `[SPEC]`): the scheduler is `local_rag_index::reconcile::{schedule, driver,
watcher}`. It is split so a **live** filesystem watcher never makes the timing untestable:

- **Engine (pure, `schedule`).** `Debouncer` is a pure state machine parameterized by an explicit
  monotonic `now_ms: i64` (mirroring `build_generation(.., now_ms)` and `uuidv7_from(now_ms, ..)`).
  It coalesces triggers into one pending request, escalates the scan mode (`Strict` wins), resets the
  quiet window on each debounced event, and self-injects the periodic backstop. The intervals are
  index-crate constants `DEBOUNCE_MS = 500` `[SPEC: 500 ms quiet window]` and
  `PERIODIC_MS = 6 h` `[SPEC: every 6 h while open]` (a `ScheduleConfig`), deliberately **not** added
  to `core::config::Config`, so spec 02 §3.1 and its pinned `default_matches_spec_toml` stay frozen.
- **Trigger taxonomy.** `Startup`/`Periodic`/`Manual`/`WatcherOverflow` bypass the debounce and select
  `ScanMode::Strict`; `Manual` is the "reindex" force. `FsChange`/`GitHead` are debounced and select
  `ScanMode::Fast`. `WatcherOverflow` is a mandatory immediate `Strict` (`[FIXED]` "never resync from
  events"). `GitHead` is dropped for non-git worktrees (§6).
- **Watcher = hint (`watcher`).** `watch_event_to_trigger(&WatchEvent, is_git)` is a pure, tested
  mapping (`Rescan → WatcherOverflow`; a `.git`-touching path → `GitHead` on git worktrees, else
  `FsChange`). The live `notify` wrapper (`spawn_watcher`) lowers `notify::Event → WatchEvent` and is
  intentionally excluded from CI (its event timing is not reproducible); reconcile — not the event
  stream — is the truth.
- **Driver (`driver`).** One `WorktreeReconciler` task per worktree owns the advisory `StatCache`, runs
  a `biased` `select!` (timer vs trigger), and on a due deadline runs one `reconcile_once` (`scan →
  build_generation`) **to completion** before re-arming — the "one writer per worktree" write side
  (spec 02 §5 L2), realized structurally by the single owning task (no explicit `RwLock`; the L2 read
  side and the projection switch are later groups). Triggers arriving during a build stay buffered and
  are coalesced into at most one follow-up, so concurrent triggers make exactly one next generation. On
  graceful shutdown (all trigger senders dropped) a scheduled reconcile is flushed. Cancellation is
  drop-safe at the state-writer tx boundary (spec 02 §4.3): each `db.writer().transaction().await` is
  atomic, so a dropped in-flight reconcile leaves only an abandoned `building`/`projection_ready`
  generation (a disjoint row set, never activated), and any already-active generation is untouched.
- **Registry composition.** `load_worktree_meta`/`nested_prune_roots` build `WorktreeMeta`
  (`worktree_id`, `root` via `current_worktree_path`, `kind`, `prune_roots`) from existing store
  readers. `prune_roots` are the **same-repo** nested worktrees under the root (no global cross-repo
  enumeration exists; a foreign nested checkout's own `.git` is still pruned). `CaseSensitivity` is
  **not persisted** — the daemon supplies it out-of-band. The scheduler builds on the T05-03 builder,
  whose `uuids` seam was tightened to `&(dyn UuidSource + Send + Sync)` so the reconcile future is
  `Send`-spawnable (a behavior-preserving bound tightening). It **stops at `projection_ready`** — no
  activation (group 07) and no typed failure/backoff bookkeeping (T05-05, which only reuses the
  builder's existing `building → failed`).

## 2. Reconcile pipeline `[FIXED]`

Under the per-worktree write lock (single writer per worktree; store-level lockfile at L0):

```
trigger → schedule → fast stat scan (gitignore-aware, `ignore` crate)
  → content-hash suspicious files
  → changed files:   parse → new file_revision(+source_blob, parser_fingerprint)
                     → parsed_units (+ content_blobs, unresolved_references)
  → unchanged files: reuse file_revision_id            (structural sharing)
  → build generation N+1:
       generation_file rows (+ display_path)
       skipped_file rows (reason ∈ binary|lfs|huge|secret|ignored|encoding)
       generation_unit_occurrences (deterministic occurrence_id)
       [post-v0] resolved_graph_edges
  → FTS delta for N+1 (§4)
  → write-ahead projection switch (05 §5)
  → delayed GC of N
```

Cost model (documented expectation, asserted by latency gates): checkout ≠ zero work — no
re-embedding of known content, but cost ∝ reading/verifying the changed tree + rebuilding
occurrences/graph. Rename is free **only** for content embeddings (occurrence context and FTS
rows change) `[FIXED]`.

As-built note (T05-03, `[SPEC]`): the "build generation N+1" body is
`local_rag_index::reconcile::build_generation`, consuming the T05-02 `ScanManifest`. It allocates a
`building` generation, then per manifest entry: a `huge` entry (no `content_hash`) becomes
`skipped_file(reason='huge')` unread; a **structural-sharing** pre-check
(`file_revision_id_by_content_key(content_hash, parser_fingerprint)` on a read connection) reuses an
existing `file_revision` and its `parsed_unit`s with **no read and no parse** (this is what makes
"editing one file does not duplicate units of unchanged files" hold, and a rename reuse content but
mint fresh path-scoped occurrences); otherwise the file is read once, `classify`d
(`lfs`/`binary`/`encoding`/`secret` → `skipped_file`), and — if indexed — `prepare_source` +
`parser_for(lang).parse` + `persist_parse_output` create the revision/units, followed by the
`generation_file` member and one deterministic occurrence per unit. Each file is one transaction
(the bounded phase); allocation and the final `building → projection_ready` transition are their own
transactions, so the generation reaches `projection_ready` **only** once every entry is persisted.
The IO/CPU (read, classify, prepare, parse) runs off the single writer thread; only the SQLite
writes are in the transaction closure. On any error the generation is transitioned to `failed`
(best-effort) and, because it is a distinct row set, no previously-built generation is mutated;
retry allocates a fresh generation and de-duplicates content via `create_or_reuse_*`, so replays add
no duplicate rows. **Deferral:** a file whose extension selects no v0 language
(`select_language` → `None`) is neither indexed nor recorded as a skip — the language-agnostic
`config_section | text_section | fallback_chunk` path (§2.1) is a later task, and there is no
`skipped_file` reason for "unsupported"; the builder counts these as `files_deferred`. T05-03
**stops at `projection_ready`**: activation, `worktree.current_generation_id`, and
`worktree_projection_state` are the projection switch (05 §5, a later group). `occurrence`
`qualified_name`/`context_hash` are left `NULL` (enrichment is search/§4, a later task).

### 2.1 Parsing rules

- tree-sitter; language chosen by extension/path (consequence: same bytes as `.c` vs `.cpp` are
  different file revisions) `[FIXED]`.
- Unit kinds: `symbol | file | config_section | text_section | fallback_chunk` — **all kinds
  are indexed** (v1 parity requirement) `[FIXED]`.
- Spans: byte offsets into the exact `source_blob`. Unsupported encodings → `skipped_file
  (reason='encoding')`; no transcoding without an offset mapping `[FIXED]`.
- Parser output MUST be deterministic for a given `(content_hash, parser_fingerprint)`;
  fixtures assert this (14 §5).
- Structural sharing acceptance: editing one file MUST NOT duplicate units of unchanged files
  `[FIXED gate]`.

### 2.2 Skip policy

`binary` (NUL heuristic + extension list), `lfs` (pointer file), `huge`
(> `max_file_size_kb`), `secret` (redaction scanner verdict, 12 §2), `ignored` (gitignore +
configured excludes), `encoding`. Skipped files get **no occurrences** and are absent from the
searchable generation `[FIXED §10 invariant, structural per 03 §2.4]`.

**Precedence and detector semantics (as-built, T03-02) `[SPEC]`.** `skipped_file`'s primary key
admits exactly one reason per path, so classification applies a deterministic precondition chain,
first match wins; a file matching none is indexed:

1. `ignored` — path only, gitignore + configured excludes; matched via the `ignore` crate's
   `gitignore` matcher with standard Git precedence (nearest `.gitignore` wins, `!` re-includes).
2. `huge` — `size_bytes` **strictly greater** than `max_file_size_kb · 1024` (a file exactly at the
   cap is kept). Uses stat only; content is not read.
3. `lfs` — a Git-LFS pointer file (first line the LFS v1 version line, with `oid sha256:` and a
   numeric `size`), detected by format so it is caught even at a binary extension.
4. `binary` — a NUL byte within the first 8 KiB, or a path with a built-in binary extension.
5. `encoding` — content is not valid UTF-8 (v0 supports only UTF-8; full `source_encoding` /
   `newline_style` detection for accepted files is a separate step, spec 03 §2.3).
6. `secret` — the shared redaction scanner (12 §2) flags the decoded UTF-8 text.

Each step is a precondition for the next (the secret scan runs only on valid decoded text). Because
every outcome is a skip, short-circuiting a cheaper reason never lets a secret-bearing file be
indexed. The binary-extension list is a built-in constant (not config); only the size cap is
configurable in v0.

## 3. Hybrid read consistency `[FIXED]`

The whole search pipeline holds the per-worktree READ lock:

```
L2.read → resolve active tuple → FTS5 (occurrences of active generation)
        → dense (shard) → RRF → graph/context enrichment → release
```

Otherwise the lexical leg could read generation N while dense reads an in-flight N+1.
The read lock prevents *mixing*; it does **not** detect an incomplete lexical projection —
that is the head's job (§4).

## 4. FTS as an independently validated materialized view `[FIXED]`

The FTS view lives in `cache.sqlite`, outside canonical transactions. Its validity proof is
`fts_projection_head` (03 §4.3), written as the **last** statement of an FTS delta/rebuild
(single cache tx per generation update `[SPEC]`).

Validation per search (cheap row read):

```
head missing for worktree                       → invalid
head.generation_id != active generation         → invalid
head.lexical_schema_version != binary constant  → invalid
head.tokenizer_version != binary constant       → invalid
occurrence_count / manifest_hash mismatch
  (manifest checked on open + after rebuilds,
   count checked per search)   [SPEC split]     → invalid
```

Invalid ⇒ **either** rebuild FTS to ready **or** serve an explicit degraded dense-only response
with a diagnostic flag `[FIXED]` (choice: rebuild synchronously if estimated < 2 s, else
degrade and rebuild in background `[SPEC]`). **An empty FTS is never silently treated as a
correct lexical result** `[FIXED]`.

FTS rebuild = delete worktree's `fts_doc` + `fts_occurrences` rows → re-derive from
`state.sqlite` occurrences + `normalized_text_cache` (recomputing normalized text from
`source_blob` where evicted) → write head.

As-built note (T08-02, `[SPEC]`): `local_rag_store::materialize_fts` (`crates/store/src/
cache/fts.rs`) implements this recipe. Because `occurrence_id` embeds `generation_id`
(03 §1.2), two generations of one worktree never share an occurrence id, so there is
nothing to incrementally diff against — every call is the full-replace recipe above, not
a row-level upsert/delete diff (unlike the dense projection's `switch()`, which diffs
specifically to skip recomputing expensive embeddings; FTS tokenization has no comparable
cost to amortize). This is also why T08-02 ("build/delta") and a future T08-03 rebuild
share one materialization core: both reduce to "derive the complete set for a generation,
replace the worktree's rows, write head last." The new store-side reader
`code::occurrences_for_fts` joins `generation_unit_occurrence ⋈ parsed_unit ⋈ content_blob`
(the first multi-table join in the crate) to get `unit_kind`/`local_name`/`blob_id`/
`file_revision_id`/`span_start`/`span_end`/`language` per occurrence; `qualified_name` is
read straight through (always `NULL` on real data today, §2's as-built note). The "single
cache tx per generation update" is realized literally as one `CacheWriter::transaction`
call: recompute-and-`insert_normalized_text` for any evicted/missing blob (this
materializer is `normalized_text_cache`'s only production writer, so on a worktree's first
FTS build essentially every blob takes this path — not a rare edge case) → delete the
worktree's stale `fts_doc`/`fts_occurrences` rows → insert the fresh set (rowids assigned
from a local `MAX(fts_rowid)+1` counter read once post-delete, not `last_insert_rowid()`,
which is connection-global and would be corrupted by the interleaved
`normalized_text_cache` inserts) → write `fts_projection_head` last. A stored
`normalized_text_cache` row found corrupt (fails `verify_cached_text`) is evicted via
`delete_normalized_text` before the recomputed text is re-inserted — `insert_normalized_text`'s
own `ON CONFLICT` only bumps `last_used_at` (T03-04's contract assumes a conflicting row is
already identical), so skipping the delete would silently leave corrupt text uncorrected.
`body` is the occurrence's raw `normalized_text` verbatim (09 §2's as-built note lists only
`name`/`qualified_name`/`path`/`signature` as app-side-tokenized). `signature` is always
empty (`tokenize_signature(&[])`) — plumbing real parameter/return-type text out of the
tree-sitter adapters is deferred past this task, matching 09 §2's own as-built scope note.

## 5. Retention & GC of canonical source `[FIXED]`

Pin roots (a `file_revision`/generation is unreferenced only if reachable from none):

```
pins: active + building/projection-target generations
      last K retired generations OR retention window T (rollback/debug)   [OPEN: K, T]
      memory evidence / audit / export references
      active rebuild/embedding job leases (temporary pins)
sweep: mark-and-sweep of unreferenced file_revisions, executed in batches
       through the global writer queue (bounded tx size [SPEC: ≤ 500 rows/tx])
```

Metrics that drive (not schedule) maintenance: `source_bytes / current worktree bytes`,
backup size, WAL size, free-page ratio → checkpoint/VACUUM policy by metrics `[FIXED]`.

Generation deletion order: occurrences/edges → generation_file/skipped_file → generation row →
then file_revision sweep. Shard/FTS rows for retired generations disappear naturally via
desired-set reconciliation (they are never part of an expected set).

As-built note (T06-01, `[SPEC]`): the **mark phase** — computing the pinned generation roots,
not the sweep — is `local_rag_store::retention` (`mark_pins` pure core + `pinned_generation_roots`
/ `generation_meta_for_worktree` DB readers; the batched, mutating sweep is T06-02). It is a pure
function of an explicit `now_ms: i64` returning sorted `BTreeSet`s, mirroring the codebase's other
time-as-a-parameter seams. As-built decisions that close gaps this section leaves implicit:
(1) the pin roots pinned unconditionally are `state ∈ {active, building, projection_ready}`; the
retention window applies **only** to `retiring` generations. (2) `failed` generations are **not**
pinned by retention — the pin list above names only "retired" generations, and 04 §1 marks both
`retiring` and `failed` as GC targets; a failed build's shared content still survives via the
`active` generation's references (structural sharing, §2), so only its genuinely orphaned rows are
swept. (`failed` can still be pinned transitively by an external reference or a lease.) (3) `K`
(last-K) and the window `T` are a **union** (the spec's "OR"), the most protective reading for the
"rollback/debug" intent. (4) the window `T` is measured against `generation.created_at` with an
inclusive lower edge (`created_at ≥ now − T`): the `generation` table has no `retired_at` column
(03 §2.1) and adding one is a numbered migration, out of scope for a pure mark phase; `K` needs no
timestamp and is the primary mechanism. (5) memory-evidence / audit / export references and active
job leases enter through an `ExternalPins` seam that defaults to empty — those subsystems are later
groups (14/16), so today the mark reduces to the generation-state and `K`/`T` roots. `K`/`T` remain
`[OPEN: O6]`, read from `[storage].retired_generations_keep` / `retired_generations_ttl_h`
(provisional defaults `2` / `168 h`, not normative).

As-built note (T06-02, `[SPEC]`): the **sweep phase** — the batched, mutating deletion — is
`local_rag_store::retention::{run_sweep, plan_sweep}` over the mark phase above. It walks the
delete order verbatim (occurrences/edges → generation_file/skipped_file → generation → then the
`file_revision` sweep); `resolved_graph_edge` is deleted before
`generation_unit_occurrence` because the edge foreign-keys the occurrence (`foreign_keys=ON`
enforces this order at runtime). As-built decisions that close gaps this section leaves implicit:
(1) **candidate = `state ∈ {retiring, failed}` AND not pinned.** The GC-eligible states are
exactly the two 04 §1 names; the state guard also means a concurrently built `building`/`active`
generation is never swept from a stale pin snapshot. The store-wide pin set is the union of every
worktree's [`pinned_generation_roots`], so a generation pinned in any worktree survives.
(2) **Reachability closure:** a `file_revision` is swept only when no *surviving* (non-candidate)
`generation_file` references it and it is not in `ExternalPins::referenced_file_revisions` — this
is the "shared revision retained until final ref" invariant (a rename/content-shared revision
outlives the retirement of any single generation). A `content_blob` is swept once no surviving
`parsed_unit` references it. (3) **Batch ceiling `[SPEC ≤ 500 rows/tx]`** is realized as
`DELETE … WHERE rowid IN (SELECT rowid … WHERE <pred> LIMIT n)` — portable, never depending on
`SQLITE_ENABLE_UPDATE_DELETE_LIMIT` — with `n = SWEEP_BATCH_ROWS` (`500`, tunable via
`run_sweep_with_batch`). The `parsed_unit` delete is **leaf-first** (each batch removes only rows
no not-yet-deleted orphan unit still names as `parent_unit_id`), so the self-referential foreign
key stays satisfied at every statement boundary even when a nested unit tree spans batches.
(4) **Resumable without a progress table:** each batch is its own committed transaction and the
sweepable sets are recomputed from the live database on every call, so an interruption between
batches — a returned error or a hard `SIGABRT` — is healed by simply re-running `run_sweep`;
already-deleted rows match nothing, deletions are monotone, and re-running converges. The
scratch sets live in connection-local `temp` tables (never part of `state.sqlite`).
(5) **Dry run (`plan_sweep`) mutates nothing:** it runs the same scratch-set setup and per-phase
counts inside one writer transaction that touches only the `temp` schema (read-only connections
are `query_only` and cannot create temp tables), so no canonical row and no main-database WAL
frame is written. Shard/FTS rows for swept generations are **not** touched here — they disappear
via desired-set reconciliation (05 §8), never as part of a sweep.

## 6. Non-git roots

`kind='non_git'` worktrees reconcile identically minus git triggers (watcher + periodic only).
Remote URL is never the sole repository ID; non-git repositories simply have a NULL remote
fingerprint `[FIXED]`.
