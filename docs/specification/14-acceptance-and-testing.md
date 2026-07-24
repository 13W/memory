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

As-built note (T10-02, `[SPEC]`): for the brute-force candidate, warm search p95 /
open / close / registry-startup are measured generically by
`spike/harness/src/lib.rs::measure_metrics` (adapter-agnostic — the fake and future
T10-03/04 candidates get `close_ms` populated the same way, for free). RAM/shard and
LRU stay unmeasured (`None`) for the reasons T10-01 already documented: no approved
portable RSS probe, and LRU needs `ShardManager` wiring that does not exist before
groups 12/15. Durability is the existing shared conformance corruption case
(`durability_summary`). `recall_at_k` stays `None` for brute-force itself — it is
definitionally exact, so a constant `1.0` would not be a measurement — but T10-02
exposes a reusable exact-neighbor reference (`spike/harness/src/oracle.rs::exact_top_k`)
for T10-03/04's own `recall_at_k` against it.

As-built note (T10-03, `[SPEC]`): `recall_at_k` is now a genuine measurement for the
`usearch` candidate, opt-in via a new `SpikeAdapter::reports_recall()` trait method
(default `false`, so fake/brute-force are unaffected and still report `None` — this
*extends*, not reverses, the T10-02 note above). `measure_metrics` computes it by
reusing the warm-search loop's own results against `oracle::exact_top_k`, gated on the
flag so exact-by-construction backends never pay for an oracle pass they didn't ask
for. `filtered_hnsw_available` is `true` for `usearch` — the first candidate to report
real filtered-HNSW support. Measured recall on the seeded `small` matrix dataset (544
points, seed 42) is a stable **0.98** with default HNSW tuning (`connectivity`/
`expansion_*` left at the library's own internal defaults, spec 05 §1). **Important
caveat, load-bearing for T10-05**: recall degrades substantially with corpus size on
this spike's *synthetic i.i.d. random* vectors — measured at dims=768 with default
tuning: ~0.94 at 1,000 points, ~0.68 at 3,000, ~0.49 at 5,000, ~0.28 at 10,000, ~0.16 at
20,000, ~0.09 at the `representative` matrix size (50,000). This is a smooth,
monotonic curve (verified: `usearch::Index::size()` matches the adapter's own point
count exactly at every scale tested — not a dropped-insert defect) consistent with a
well-known property of graph-based ANN search: greedy traversal has no exploitable
locality structure to follow in *unstructured* random vectors, unlike real code/text
embeddings, which cluster semantically. This is a genuine measurement of this spike's
synthetic corpus, not necessarily predictive of recall on real `embedding_cache`
vectors once T11 exists — flagged explicitly so T10-05 does not read the `large`/
`representative` recall numbers as a verdict on `usearch` itself without this context.
