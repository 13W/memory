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

## 6. Non-git roots

`kind='non_git'` worktrees reconcile identically minus git triggers (watcher + periodic only).
Remote URL is never the sole repository ID; non-git repositories simply have a NULL remote
fingerprint `[FIXED]`.
