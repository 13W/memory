# 49-query search benchmark — v1 baseline

The 49-query code-search benchmark is `[FIXED]`: baseline on v1, gate on v2 (spec 14 §7). This
file records the **v1 baseline numbers** captured by a live run. Per O2 ("collect metrics, do
not invent thresholds"), the numbers below are `[BASELINE]` data; the v2 **gate thresholds**
(allowed MRR regression `X`, `Recall@5 ≥ Y`) remain **TBD** and are decided later (T12-05 / T17-05).

## Run provenance

| Field | Value |
| --- | --- |
| v1 repo / commit | `local-rag` @ `31dfba2` |
| Date | 2026-07-16 (report timestamp `2026-07-16T13:12:13.607Z`) |
| Embedding model | `embeddinggemma:300m` (dim 768), via Ollama |
| Description leg | **disabled** (code-only; descriptions are deferred post-v0, 15 §3) |
| Corpus indexed | project source only (`node_modules`/`.git`/`dist` excluded) — 96 files, 544 chunks |
| Search | hybrid RRF fusion over `code_vector` + `description_vector`, limit 5 (description leg empty) |
| Scoring | per query: single ground-truth target; file = substring of file path, symbol = substring of name |
| Infra | Qdrant 1.18.2 @ localhost:6333; Ollama @ localhost:11434 |
| Host | darwin (arm64) |
| Runner | v1 `scripts/benchmark.ts` (compiled). Two build-artifact edits used for this run and reverted afterwards: forced code-only, and excluded vendored `node_modules` from the corpus walk. v1 source was not modified. |

Raw evidence: `run-embeddinggemma-300m-2026-07-16.json` and `.report.md` in this directory.

## Metrics `[BASELINE]`

| Metric | Value |
| --- | --- |
| Hit@1 | 0.5918 (29/49) |
| Hit@3 | 0.7959 (39/49) |
| Hit@5 | 0.8367 (41/49) |
| MRR | 0.6963 |
| Index time | 13006 ms (code embed 12562 ms) |
| Query embed | 4008 ms (49 queries) |
| Search | 229 ms (49 queries) |

## v2 gate thresholds — TBD

| Threshold | Status |
| --- | --- |
| MRR regression budget `X` (v2 not worse than v1 by more than X) | TBD |
| `Recall@5 ≥ Y` | TBD |
| warm-search p95 latency | TBD |

## Notes / gaps

- Single-relevant corpus: one ground-truth target per query, no graded relevance judgments
  (14 §1 says "queries + relevance judgments") — registered as GAP-03.
- Only one embedding model was run. The v1 benchmark can sweep others
  (`qwen3-embedding:0.6b/4b/8b`, `mxbai-embed-large`); those are not required for the baseline
  shape and are not pulled here. Additional models can be appended as more `run-*.json` files.
- These numbers describe v1 behavior on the v1 codebase; they are a reference point for the v2
  gate, not a v2 target.
