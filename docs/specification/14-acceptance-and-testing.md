# 14 — Acceptance Gates & Testing

Numbers marked `[BASELINE]` are fixed only after the v1 baseline run `[OPEN]`; the *existence*
and *shape* of each gate is `[FIXED]`.

## 1. Fixture strategy `[FIXED]`

v1 tests are converted to **implementation-neutral fixtures before any rewrite code**:
`input tree / event stream / query → expected behavior` — never expected internal payload
schemas of a vector store. Fixture families:

1. Parser fixtures: source file → expected units (kind, span, locator) per language.
2. Reconcile fixtures: tree A → tree B → expected generation diff (files, occurrences, skips).
3. Search fixtures: the 49-query benchmark corpus (queries + relevance judgments).
4. Memory-quality fixtures: labeled observation streams → expected ops
   (`create|reinforce|supersede|noop`), covering decision vs hypothesis vs negation, RU/EN
   mixed transcripts `[FIXED, new in rev 6]`.
5. Adversarial recall fixtures (12 §4).
6. Fault-injection scripts (05 §10, 07 §7).

## 2. Acceptance gates `[FIXED set]`

| Gate | Criterion |
| --- | --- |
| quality | MRR not worse than v1 baseline by more than X; Recall@5 ≥ Y `[BASELINE]` |
| memory-quality | router precision/recall on fixture set ≥ P/R `[BASELINE]` |
| latency | warm search p95; one-file reconcile p95; branch-checkout reconcile `[BASELINE]` |
| resources | idle RAM; index bytes/symbol; embedding cache budget adherence; source bytes / worktree bytes `[BASELINE]` |
| reliability | crash/restart: ANY projection divergence is detected at open → rebuild without manual clear; watcher overflow caught by strict reconcile; no stable-identity event lost after spool append (daemon killed at any import point) |
| consistency | validate-on-open matrix (05 §10) fully green; hybrid never mixes generations; empty/partial/stale FTS detected via `fts_projection_head` |
| sharing | changing one file does not duplicate units of unchanged files |
| idempotency | replayed spool event / retried reconcile ⇒ no duplicate memory op / duplicate rows (deterministic IDs) |
| rebuild | deleted dense projection / cache fully restored from `state.sqlite` |

## 3. Fault-injection suite `[FIXED]`

Proves exactly **two properties** of the projection protocol — (a) any divergence detected at
open, (b) rebuild correct and idempotent — plus corruption cases as *detection* tests, plus
the spool kill matrix (07 §7). Harness `[SPEC]`: deterministic kill points via a
`fail_point!`-style crate compiled in test builds; each matrix row is a named test asserting
the *specific* detection signal (05 §10 column 3), not just "eventually rebuilt".

## 4. Consistency tests `[SPEC mechanics]`

- Generation-mixing: concurrent search vs switch under load; assert every result set's
  occurrences belong to exactly one generation.
- FTS staleness: drop cache after a switch; assert flagged degraded or rebuilt — never an
  empty lexical leg treated as valid.
- Two-axis interleaving: alternate generation and model-space switches; assert serialization
  and correct final tuple.

## 5. Determinism tests

- Parser determinism: same `(content, parser_fingerprint)` ⇒ byte-identical unit sets.
- Deterministic IDs stable under retry reconcile and repository move `[FIXED]`.
- `additionalContext` byte-determinism (11 §5).
- Schema audit lint: no path/context column on content-shared tables; no durable FK onto a
  path-derived value (01 §5.1).

## 6. Adversarial tests `[FIXED]`

Prompt-injection payloads stored as memories / present in indexed code round-trip as inert,
correctly escaped text; recall block never exceeds caps; delimiter collisions escaped.

## 7. Benchmarks

- 49-query code-search benchmark: baseline on v1, gate on v2 `[FIXED]`.
- Memory-quality benchmark (08 §7).
- Step-11 dense-backend spike matrix `[FIXED]`: warm search p95; RAM/shard; open/close cost;
  startup with a large registry; LRU behavior; durability/validate-on-open semantics;
  platform support (win32); filtered-HNSW available. **Backend choice is fixed here, not
  earlier.**
