# 01 — Overview

## 1. What the system is

`local-rag` v2 is a **local, co-located MCP service for Claude Code** providing three pillars:

1. **Persistent memory** — durable, auditable knowledge entries scoped to repository / worktree / global.
2. **Semantic code search** — hybrid lexical + dense retrieval over a parsed, generation-versioned code index.
3. **Observations** — durable capture of agent-session events feeding memory consolidation.

Implementation language is **Rust** `[FIXED]`. Distribution is **npm** with per-platform native
binaries `[FIXED]`. There are **no mandatory external daemons** (no Qdrant, no Ollama as
prerequisites) `[FIXED]` — this is the headline break from v1.

## 2. Scope

- **Claude Code is the only supported harness** `[FIXED]`. Multi-harness (Gemini CLI etc.) is deferred.
- One **co-located daemon per OS user**, a **thin stdio MCP proxy per session**, and
  **spool-only hook ingestion** `[FIXED]`: hooks never talk to the daemon for ingestion.
- Platform targets: `darwin-x64`, `darwin-arm64`, `linux-x64`, `linux-arm64`, `win32-x64`
  `[FIXED]`. `win32-arm64` and FreeBSD deferred `[FIXED]`.
- Dense vector backend is abstracted behind a `ProjectionStore` trait; the concrete backend
  (Qdrant Edge / usearch / brute-force over `embedding_cache`) is chosen by the comparative
  spike at roadmap step 11 `[OPEN]`. Nothing before step 11 may depend on a specific backend.

## 3. Non-goals

- Not a general RAG server for arbitrary documents.
- Not a remote/multi-user service; single OS user, local trust domain.
- No canonical data in the vector store (v1 anti-pattern). The vector projection is a cache, always.
- No mutable process-global "current project / current branch" `[FIXED]`.
- No in-place re-embedding or in-place collection migration `[FIXED]`.
- v0 does not index files without a stored exact source (`non_rebuildable` tier rejected) `[FIXED]`.

## 4. Correctness budget `[FIXED]`

The single non-recoverable asset is **memory** (observation envelopes, memory entries, audit).
For memory: full transactional rigor, no-loss, idempotent operations.

The **code index is rebuildable by construction**. Its regime is *detect on open → rebuild on
doubt* — the system never attempts to prove durability of a third-party dense engine. All
protocols in docs 05–07 follow from this split:

| Asset | Guarantee | Mechanism |
| --- | --- | --- |
| Memory & observations | Durable, exactly-once effect | SQLite tx on `state.sqlite`; spool append = durable moment; idempotent consolidation |
| Code index (dense + FTS) | Detectably-consistent cache | Write-ahead marker + `ProjectionHead` / `fts_projection_head` validate-on-open; full rebuild is the recovery default |

## 5. The two identity ladders `[FIXED]`

**Code:** `content blob ≠ parsed unit ≠ generation occurrence ≠ vector representation ≠ generation`.
**Memory:** `raw observation ≠ evidence ≠ durable memory ≠ recalled context`.

### 5.1 Systemic audit rule (extended in rev 6)

No row that is **shared by content** (`content_blob`, `file_revision`, `parsed_unit`,
content-subject rows of `embedding_cache`) may carry **any** context- or path-specific field.
Everything path/generation-dependent lives only in `generation_unit_occurrence`,
`resolved_graph_edge`, `generation_file`, and the FTS projection of occurrences.

**Additionally: no durable ID may be derived from a filesystem path.** A path-derived hash is
permitted only as a *lookup key* (`worktree_path.path_fingerprint`), never as an FK target for
durable state. This closes the recurring bug class:
`chunk_id→blob`, `occurrence→generation`, `path in parsed_unit`, `FTS over parsed_unit`,
`worktree_id←path` — all are the same violation at different levels.

The audit rule is enforced three ways: schema shape (03), a schema lint test that greps the DDL
for forbidden column placements (14), and code review checklist.

## 6. Glossary

| Term | Definition |
| --- | --- |
| **store** | The per-OS-user data root: `state.sqlite`, `cache.sqlite`, `projection/`, `spool/`, `models/`. |
| **repository** | Durable registry entry (UUID) for a code project; survives directory moves. |
| **worktree** | A checkout (main / linked / non-git root) with a **stable UUID** independent of its path. |
| **generation** | An immutable snapshot of a worktree's indexed file set; identified by `(worktree_id, generation_number)`. |
| **file revision** | Content-addressed `(content_hash, parser_fingerprint)` pair with the exact `source_blob`. |
| **parsed unit** | Path-independent parse product of a file revision (symbol / file / config section / text section / fallback chunk). |
| **occurrence** | A parsed unit *at a path within a generation*; the only path-bearing code identity. |
| **representation** | A registered embedding key: `(kind, versions, model, dimensions, metric)`. |
| **model space** | A coherent set of representations that can be active together; unit of embedding-model migration. |
| **projection** | Per-worktree dense shard + FTS view materialized from `state.sqlite` for one `(generation, model space)` tuple. |
| **spool** | Append-only per-session segment files; the only hook ingestion path. |
| **envelope** | Durable observation row (identity + metadata); payload is TTL-bound. |
| **consolidation** | Cursor-driven, leased, idempotent routing of observations into memory operations. |
| **recall** | Scoped, budgeted retrieval of memory entries injected as `additionalContext`. |

## 7. Behavioral contract inherited from v1 `[FIXED]`

Preserved (see 11-interfaces and 14-acceptance for the testable form):
hook fail-open; recall via `additionalContext`; empty recall emits no text; deterministic
recall formatting; embedding provider primary/fallback + retry; async description backfill
(post-v0); parser/resolver test coverage; gitignore semantics; parent/child chunks *iff* the
benchmark justifies them; the 49-query code-search benchmark; indexing isolated from the MCP path.

Explicitly **not** carried over as design: branch tags/manifests inside the vector store; the
vector store as canonical store; mutable process-level current branch/project; in-place
re-embed/migration of collections; deriving headless mode from `stop_hook_active`; splitting
agent memory by physical collections.

v1 tests are converted to **implementation-neutral fixtures** *before* the rewrite: input tree /
event stream / query → expected behavior, never expected internal payload schema of a vector store.
