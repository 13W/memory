# ADR-0003: Dense backend selection for the code-search projection (v0)

## Status

Accepted — 2026-07-24.

Closes open question **O1 "Dense backend (Qdrant Edge / usearch / brute-force)"**
([spec 15 §4](../specification/15-roadmap.md)), resolved "by step-11 spike" per the
`[FIXED]` roadmap order. Delivered by task **T10-05**
([group 10](../implementation-plan/groups/10-dense-backend-spike.md)), which
compares the three candidate adapters built by T10-02 (brute-force), T10-03
(`usearch`), and T10-04 (Qdrant Edge) inside the isolated `spike/` workspace.
Follows the ADR convention established by ADR-0001/ADR-0002.

## Context

Spec 05 §1 names three candidates for the `[FIXED]`-shaped `ProjectionStore`
trait, backend chosen "at roadmap step 11 `[OPEN]`" — this task. Spec 15 §2 fixes
the MVP dense leg as "the simplest backend that passes the benchmark (possibly
brute-force — step 11 decides)"; spec 05 §1's own core principle is that the dense
projection is **always an untrusted cache** — correctness rests on
detectability-of-divergence at every open, not on backend durability. T10-01
built a shared, backend-neutral conformance + benchmark harness (seeded
`small`/`representative`/`large` datasets, `dims=768` anchored on the measured v1
baseline of 544 chunks) and the fixed 14 §7 metric matrix; T10-02/03/04 each
built and measured one real candidate, isolated in the `spike/` workspace so no
dense-vector SDK ever entered the product `Cargo.lock` before this decision
(`CONTRIBUTING.md` § Dependency policy).

### Raw results

T10-02/03/04 already measured `small` (544 pts) and — for usearch/qdrant-edge —
`representative` (50,000 pts) and `large` (500,000 pts); T10-02's own evidence
never captured brute-force's per-operation metrics (only pass/fail). T10-05 fills
that gap and re-runs all three candidates for `small`/`representative` on one
machine, in `--release`, same seeds, for a fair side-by-side (artifacts committed
under `spike/artifacts/`: `<adapter>-<dataset>.json`); `large` is **not**
re-run — T10-02/T10-04 already completed it and T10-03 already documented
non-completion after 35+ minutes in `PROGRESS.md`, all with exact reproducing
commands, so repeating either an expensive success or an expensive documented
failure adds no new information.

**`small` (544 points, dim 768, 49 queries):**

| Metric | brute-force | usearch | Qdrant Edge |
| --- | --- | --- | --- |
| `open_ms` | 0.0088 | 0.0175 | 75.3 |
| `close_ms` | 0.018 | 0.066 | 21.2 |
| `registry_startup_ms` | 0.528 | 0.382 | 624.3 |
| `warm_search_p95_ms` | 0.353 | 0.101 | 0.095 |
| `recall_at_k` | exact (n/a) | 0.980 | 1.0 |
| conformance `all_passed` | true | true | **false** |
| `filtered_hnsw_available` | false | true | true |
| new dependency | none | Apache-2.0 | Apache-2.0 |

**`representative` (50,000 points):**

| Metric | brute-force | usearch | Qdrant Edge |
| --- | --- | --- | --- |
| `open_ms` | 0.0092 | 0.0172 | 68.0 |
| `close_ms` | 0.978 | 3.58 | 183.1 |
| `registry_startup_ms` | 0.390 | 0.454 | 573.5 |
| `warm_search_p95_ms` | 18.1 | 0.836 | 2.79 |
| `recall_at_k` | exact (n/a) | **0.092** | 1.0 (not genuine ANN, see below) |
| conformance `all_passed` | true | true | **false** |

**`large` (500,000 points, historical T10-02/03/04 evidence, not re-run):**
brute-force completed its full conformance+build run in ~4m34s (`all_passed`,
corruption correctly detected); Qdrant Edge completed in 9m01s
(`warm_search_p95_ms=2755`, `close_ms=1103`, `recall_at_k=1.0`); usearch's
`--release` attempt was manually stopped after 35+ minutes without finishing (not
a code defect — a full HNSW graph build over 500k×768 vectors plus an exact-oracle
recall pass, impractical in this sandbox).

### Findings load-bearing for this decision

- **usearch recall degrades sharply with corpus size on this spike's synthetic,
  unstructured i.i.d. vectors** (0.98 @544 → 0.092 @50,000; a diagnostic sweep in
  T10-03 showed a smooth monotonic curve: ~0.94@1k, ~0.68@3k, ~0.49@5k, ~0.28@10k,
  ~0.16@20k). Spec 14 §7's own as-built note flags this as **not necessarily
  predictive** of real (semantically clustered) code/text embeddings — but no
  real-embedding evidence exists until T11, so this remains an unquantified risk
  today, not a settled non-issue.
- **Qdrant Edge's `recall_at_k = 1.0` at every scale reflects "plain until
  optimized" exact search**, not genuine HNSW recall: a fresh segment stays
  unindexed below the crate's own 10,000 KB default indexing threshold, and
  neither this harness nor a real product `switch()`/rebuild calls `optimize()`
  automatically (spec 05 §9 `[FIXED]`: "triggered by metrics only… never after
  every reconcile"). The number is real, but it is not comparable to usearch's
  own genuinely-approximate recall.
- **Qdrant Edge's `open_ms`/`registry_startup_ms` are two to four orders of
  magnitude higher than the other two candidates at every scale tested**
  (75.3 ms vs. 0.009–0.018 ms `open_ms` at `small`) — in direct tension with spec
  05 §1 `[FIXED]`: "`open` … MUST be cheap enough to run validate-on-open on
  **every** open." The per-worktree shard model (spec 05 §2) opens/closes shards
  routinely under LRU eviction; this cost compounds across many worktrees.
- **A genuine robustness gap in vendored `qdrant-edge` 0.7.2**: corrupting the
  structural id-tracker file causes an **uncaught panic**, not a catchable
  `Result::Err` (T10-04, `corrupting_the_id_tracker_panics_instead_of_erroring_cleanly`).
  This is in direct tension with spec 05's core untrusted-cache principle and
  F12's contract ("corruption making shard unopenable: open error → quarantine →
  rebuild") — an uncaught panic aborts the process instead of surfacing a
  catchable error the `ShardManager` can quarantine and rebuild from. The shared
  conformance suite's own generic on-disk corruption case does not even detect
  divergence for this candidate at `small` scale (fixed-capacity preallocated
  storage files, immune to truncation) — `conformance.all_passed = false` for
  Qdrant Edge in both tables above is this same, already-documented, expected gap.
- **Qdrant Edge carries the real Qdrant server's WAL/segment storage engine**
  (~80 transitive dependencies: `tokio`, `tonic`, `rayon`, BM25 tokenization,
  geo, Linux `io-uring`/`cgroups-rs`/`procfs`) — a fundamentally different risk
  class from a compact library, in tension with "no mandatory external daemon"
  in spirit even though this candidate is embedded, and with the MVP "simplest
  backend" guidance.
- **win32 build-smoke failed in-sandbox for both real ANN candidates**: usearch's
  `numkong` SIMD dependency and Qdrant Edge's `ring`/`link-cplusplus` both fail to
  cross-compile without an MSVC/mingw toolchain (inconclusive — a sandbox
  limitation, not necessarily a crate defect, per T10-03/04's own evidence).
  Brute-force has **no native compiler dependency at all** (pure `std`), so this
  entire risk class does not apply to it.

## Decision

**The v0 dense backend is brute-force linear scan over `embedding_cache`.**

### Explicit weights

| Dimension | Weight | brute-force | usearch | Qdrant Edge |
| --- | --- | --- | --- | --- |
| Correctness/robustness (recall trust, corruption detection, no panics) | 30 | 5 | 3 | 2 |
| Open-cost cheapness (spec 05 §1 "cheap enough for every open") | 20 | 5 | 5 | 1 |
| Simplicity / dependency & license footprint / no premature coupling | 20 | 5 | 3 | 1 |
| Platform support (incl. win32 evidence, native-toolchain risk) | 15 | 5 | 2 | 2 |
| Raw latency at realistic per-worktree scale | 15 | 4 | 5 | 4 |
| **Weighted total (/500)** | | **485 (97%)** | **355 (71%)** | **190 (38%)** |

Weights favor simplicity/robustness/architectural fit over raw latency because
(a) the guardrail explicitly forbids coupling to a real dense backend before this
spike and asks for the *simplest* candidate that clears quality/platform/
correctness gates, not the fastest one; (b) a shard is per-worktree, not global
(spec 05 §2) — the only real baseline measurement (v1's 544-chunk corpus) is
`small`, not the `500,000`-point `large` stress case; and (c) brute-force's own
18 ms p95 at 50,000 points is comfortably within interactive-search latency for
an MCP tool, while its recall is exact by construction at any scale — it has no
tuning-vs-recall tradeoff to make at all.

Brute-force wins on every dimension except raw latency, where usearch is
measurably faster but carries the biggest unresolved risk (recall) and the
narrowest platform story. Qdrant Edge is decisively last: its dependency weight,
open-cost, and the uncaught-panic robustness gap all cut against the "simplest
candidate passing correctness gates" bar the card sets, regardless of its
competitive search latency and its exact-until-optimized recall.

### `optimize()` thresholds

Spec 05 §9 `[FIXED]` calls threshold values "backend-specific outputs of the
step-11 spike `[OPEN]`". Brute-force's `optimize()` is a documented no-op — "a
wholesale-rewritten flat array has nothing to compact" (`brute_force.rs`) — there
is no segment/graph structure whose fragmentation could accrue, so **no
threshold exists to set**. This is recorded explicitly, not left silently
unresolved (O2: never invent a threshold that isn't needed).

### Dependency / license audit

The winning candidate adds **zero** new dependencies to the product workspace —
brute-force is pure `std` (already true today; verified by the unchanged T10
guardrail grep for dense-backend SDK names across every `Cargo.toml` and the root
`Cargo.lock`). The two rejected candidates, for the record: `usearch` 2.26
(Apache-2.0) + its default SIMD dependency `numkong` (Apache-2.0), ~20 crates
total; `qdrant-edge` 0.7 (Apache-2.0) over ~80 transitive crates, not
individually license-audited since rejected — a real future cost had it been
chosen.

## Consequences

- Spec 05 §1's trait doc comment ("Backend chosen at roadmap step 11 `[OPEN]`")
  and spec 15 §4's O1 row are amended to record the fixed choice, citing this
  ADR — the same as-built pattern ADR-0001/0002 used for O4/O7.
- T12-02 ("Интегрировать выбранный dense backend") copies the brute-force spike
  candidate's design (contiguous row-major vectors, streamed fixed-record
  on-disk format) into the product workspace; it does **not** copy
  `usearch`/`qdrant-edge` code paths. G10 (next task, not this one) removes any
  production coupling to the losing candidates — today there is none, since every
  candidate lives isolated in `spike/`.
- Known, accepted v0 limitation, recorded deliberately: linear-scan search cost
  scales linearly with per-worktree corpus size. This is acceptable at MVP scale
  (the only measured real corpus is 544 chunks; `18 ms` p95 at 50,000 synthetic
  points is still comfortably interactive) and mirrors the exact pattern spec
  10 §6 already fixed for memory relevance: "v0: FTS + brute-force cosine …
  switch to ANN only on cardinality/latency metrics." A future switch to an ANN
  backend for code search, if usage metrics ever justify it, is additive and
  does not require revisiting this ADR's reasoning — only new evidence of actual
  production cardinality/latency pressure.
- `usearch`'s recall-vs-corpus-size risk and Qdrant Edge's open-cost/panic
  findings remain valuable, reproducible spike artifacts (`spike/`,
  `spike/artifacts/`) for any future backend reconsideration — they are not
  deleted, only not chosen.
- No `[FIXED]` text is changed by this ADR; only `[OPEN]` → resolved `[SPEC]`
  amendments.
