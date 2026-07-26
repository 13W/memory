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

As-built note (T12-05, `[SPEC]`): the `quality` row's `X`/`Y` are now set —
`mrr_regression_budget = 0.03`, `min_recall_at_5 = 0.80` — and live in
`fixtures/search/baseline/thresholds.json`, not in code, so a retune is a reviewable diff with a
stated justification ("tuning changes are versioned"). They are derived from the **agreed v1
baseline**, deliberately not from the first v2 run: that run regressed, and deriving a threshold
from a regressed measurement would encode the regression as acceptable (O2). The gate consequently
**fails on today's v2** — MRR 0.5646 vs the 0.6963 baseline, Recall@5 0.7755 — which is registered
as `D-016` (`blocked`, product decision) rather than papered over. The `latency`/`resources`/
`memory-quality` rows remain `[BASELINE]`-pending (T14-07, T17-05).

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

As-built note (T12-05, `[SPEC]`): the 49-query benchmark runner is `cargo xtask bench`
(`crates/xtask/src/bench/`), split so everything *scored* is an ordinary offline test and only the
end-to-end run needs weights, a corpus checkout and a `libonnxruntime`: `corpus` loads and refuses
anything that is not the corpus the baseline was measured against, `score` holds the matching
semantics and metric math, `report` shapes the output and the v1 diff, `gate` turns a report plus
versioned thresholds into a verdict, and `run` is the only piece that indexes anything.

**Matching semantics** are taken verbatim from the corpus's own description, because they define
what the recorded numbers mean: `file` = substring of the result path, `symbol` = substring of the
result name, `symbol: null` = file-level (any symbol of that file). **`Recall@5 == hit@5`** on this
corpus by construction — it is single-relevant, so "how many relevant documents were retrieved"
is one or zero; both names are reported rather than one silently standing in for the other.

**Comparability** is enforced by mirroring the baseline's own corpus definition: `node_modules`,
`dist` and `.git` are pruned, and the report records file/occurrence counts so a reader can check
the corpora matched (v2 indexed 101 files / 581 occurrences against v1's 96 / 544 — the residual
difference is v2's parser-derived units versus v1's own chunker).

**Per-query diff vs v1 is metric-level, not rank-level (D-015)**: the imported v1 artifact holds
aggregates only, and v1's runner folds ranks into counters inside its scoring loop without ever
emitting a per-query rank. Recovering them would mean editing v1's source and re-running it, which
T00-01 explicitly declined. The report therefore carries full per-query detail for v2 and reserves
`v1_rank` for the day v1 is re-run with per-query output.

The first recorded run and its per-leg diagnostics live in `fixtures/search/baseline/`; the
regression they expose is `D-016`.

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

As-built note (T10-04, `[SPEC]`): `recall_at_k` is likewise a genuine measurement for
the Qdrant Edge candidate (`reports_recall() -> true`, same opt-in machinery). No score
transform is applied (`Distance::Dot`'s `postprocess_score` is a verified identity — see
spec 05 §1's as-built note), the simplest scoring story of the three real candidates.
`filtered_hnsw_available` is `true` — arguably the most "native" of the three, since
payload filtering is a first-order parameter on every search/scroll/count call rather
than a separate method. Measured recall on the seeded `small` matrix dataset (544
points, seed 42) is a stable `1.0`, comfortably clearing the calibrated 0.9 lower bound
(`spike/qdrant-edge/tests/qdrant_edge.rs::search_recall_clears_a_reasonable_lower_bound`);
**`representative` (50,000 points) also measured a stable `1.0`**.

**Important caveat, load-bearing for T10-05 — "plain until optimized"**: unlike usearch
(a live HNSW graph from the first insert), a freshly created Qdrant Edge segment is
"plain" (exact, unindexed, full-scan) until `EdgeShard::optimize()` runs **and** the
segment already exceeds the crate's own default 10,000 KB indexing threshold. Neither
this spike's `measure_metrics` nor a real product `switch()`/rebuild calls `optimize()`
automatically (spec 05 §9 `[FIXED]`: "triggered by metrics only... never after every
reconcile") — so the `1.0` recall measured at both `small` and `representative` reflects
**exact search**, not this backend's approximate/HNSW behavior, and is not directly
comparable to usearch's own (genuinely approximate, at every scale) recall numbers
without this context. This is not a spike-harness artifact — it is exactly how this
candidate would behave in the real product architecture, and arguably a better fit for
spec 05 §9's own principle than usearch's always-live-graph model. `optimize()` itself
is wired to the real `EdgeShard::optimize()` (found during T10-04 implementation, not a
no-op as originally planned) and verified safe on a segment large enough to actually
cross the indexing threshold
(`spike/qdrant-edge/src/lib.rs::optimize_handles_a_segment_above_the_indexing_threshold`).

**Important caveat, load-bearing for T10-05**: the shared conformance suite's generic
on-disk corruption case does not produce a detected divergence for this candidate at
`TINY` scale — its vector/payload/WAL storage uses fixed-capacity preallocated files
immune to truncation-based corruption (see spec 05 §1/§10's as-built notes) — and a
candidate-specific test that targets the real structural identity-tracking file instead
found a genuine, separate robustness gap in the vendored `qdrant-edge` 0.7.2 crate: an
uncaught panic on corrupted state, not a clean, catchable error. `ram_bytes_per_shard`/
`lru` remain `None` for the same unchanged reasons as the other two candidates.

As-built note (T10-05, `[SPEC]`, closes O1): the comparison is decided —
[ADR-0003](../adr/0003-dense-backend-selection.md), brute-force. A same-machine,
`--release`, matching-seed re-run of all three candidates at `small`/`representative`
(`spike/artifacts/<adapter>-<dataset>.json`) filled the one gap left by T10-02 (brute-force's
own metric matrix was never captured): `open_ms`/`close_ms`/`registry_startup_ms` stay
sub-millisecond at both sizes (e.g. `open_ms=0.0088` @544, `0.0092` @50,000 — flat, since a
fresh in-memory `Vec<f32>` has no size-dependent open cost), `warm_search_p95_ms` grows with
corpus size as expected for linear scan (`0.353` @544 → `18.1` @50,000, still comfortably
interactive), and `recall_at_k` stays `None` (exact by construction, spec 05 §1). `large`
(500,000 points) was not re-run: T10-02/T10-04 already completed it and T10-03 already
documented a 35+ minute non-completion, all with reproducing commands recorded in
`PROGRESS.md` — repeating either result would not add information. The explicit weighted
comparison (favoring correctness/robustness/simplicity over raw latency, per the "simplest
candidate passing quality/platform/correctness gates" card requirement) and the full
dependency/license audit are in the ADR, not duplicated here.
