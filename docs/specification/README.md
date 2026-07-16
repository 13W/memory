# local-rag v2 — Specification

Executable-level specification for the rewrite of `local-rag` (design baseline: **idea.md rev 6**).
This document set replaces the v1 `specification.md`, which is renamed to `specification-v1.md`
and kept as **behavioral archaeology only** (non-normative).

## Status legend

Every normative statement carries one of three markers where provenance matters:

| Marker | Meaning |
| --- | --- |
| `[FIXED]` | Decided in design rev 5/6. Changing it requires a new design revision. |
| `[SPEC]` | Concretization introduced by this specification (the design left it at principle level). Review before implementation; changing it requires only a spec edit. |
| `[OPEN]` | Explicitly open in rev 6 (§18). Resolved by spike/benchmark, not by this document. Implementation MUST NOT hard-code assumptions that pre-empt the resolution. |

Conformance keywords **MUST / MUST NOT / SHOULD / MAY** follow RFC 2119 semantics.

## Document map

| Doc | Contents |
| --- | --- |
| [01-overview.md](01-overview.md) | Scope, non-goals, correctness budget, identity ladders, glossary |
| [02-architecture.md](02-architecture.md) | Process topology, store & config layout, daemon lifecycle, lock order, degraded modes, error taxonomy |
| [03-data-model.md](03-data-model.md) | Full DDL for `state.sqlite` and `cache.sqlite`, ID/hash rules, SQLite policy, migration boundaries |
| [04-state-machines.md](04-state-machines.md) | State machines: generation, projection, model space, memory entry, candidate, consolidation run |
| [05-projection.md](05-projection.md) | `ProjectionStore` trait contract, write-ahead + validate-on-open, switch algorithm, fault-detection matrix, shard lifecycle |
| [06-reconcile-and-fts.md](06-reconcile-and-fts.md) | Worktree reconcile pipeline, parsing rules, FTS materialized view, retention/GC |
| [07-observations-spool.md](07-observations-spool.md) | Spool wire format, atomic append semantics, source identity per event type, import protocol, kill matrix |
| [08-memory.md](08-memory.md) | Memory model, consolidation router, transactional memory ops, recall v0, review tools, quality benchmark |
| [09-search.md](09-search.md) | Hybrid search pipeline, FTS preprocessing, RRF, symbol graph, degraded responses |
| [10-models-and-embeddings.md](10-models-and-embeddings.md) | Representations registry, model spaces, per-worktree activation, double-buffer migration, provider pool |
| [11-interfaces.md](11-interfaces.md) | MCP tool contracts, hook contracts, proxy↔daemon protocol, CLI surface, `additionalContext` format |
| [12-security-privacy.md](12-security-privacy.md) | Redaction, data policy, TTL, source-blob policy, untrusted-recall encoding, filesystem permissions |
| [13-distribution-and-migrations.md](13-distribution-and-migrations.md) | npm packaging, platform matrix, model asset delivery, migration framework |
| [14-acceptance-and-testing.md](14-acceptance-and-testing.md) | Acceptance gates, fault-injection suites, fixture strategy, benchmarks |
| [15-roadmap.md](15-roadmap.md) | Implementation order, MVP (v0) scope, deferred features, open-questions register |

## Reading order

For implementation start (steps 1–7 of the roadmap): 01 → 02 → 03 → 04 → 06 → 14.
Projection work (steps 8–11): 05 → 10. Memory work (steps 14–15): 07 → 08 → 11.

## Relationship to the design document

`idea.md` (rev 6) remains the **design rationale**: it explains *why*. This spec states *what*,
in testable form. On conflict, rev 6 decisions win until a rev 7 exists; report conflicts as
spec bugs. Sections of this spec covering roadmap steps not yet started are expected to gain
detail as those steps begin (per rev 6 §18 the spec is grown incrementally, not frozen).
