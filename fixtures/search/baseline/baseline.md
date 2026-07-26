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

## v2 gate thresholds `[SPEC]`

Set in `thresholds.json` (versioned, machine-read by `cargo xtask bench`):

| Threshold | Value | Where it comes from |
| --- | --- | --- |
| MRR regression budget `X` | **0.03** | ~4% relative to the v1 MRR; one query moving between rank 1 and 2 shifts MRR by ~0.010 on a 49-query corpus, so this absorbs jitter without absorbing a real regression |
| `Recall@5 ≥ Y` | **0.80** | just under v1's 0.8367 — two queries may drop out of the top 5 |
| warm-search p95 latency | still TBD | measured (see below) but T17-05 owns the latency gate |

Derived from **this v1 baseline**, deliberately not from the first v2 run: that
run regressed, and deriving a threshold from a regressed measurement would encode
the regression as acceptable (O2: collect metrics, never invent thresholds).

## v2 measurement — 2026-07-26 `[BASELINE]`

Recorded by `cargo xtask bench` (T12-05) against the **same corpus checkout the
v1 baseline used** (`/opt/soft/local-rag` @ `31dfba2`) and the same model family
(`embeddinggemma-300m`, local ONNX). Raw artifacts: `run-v2-2026-07-26.json` /
`.report.md`, plus per-leg diagnostics `…-code-only.json` and
`…-lexical-only.json`.

| Run | Hit@1 | Hit@3 | Hit@5 / Recall@5 | MRR |
| --- | --- | --- | --- | --- |
| **v1 baseline** (dense) | 0.5918 | 0.7959 | 0.8367 | 0.6963 |
| **v2 hybrid** | 0.4286 | 0.6939 | 0.7755 | **0.5646** |
| v2 dense only (`--mode code`) | 0.3265 | 0.6939 | 0.7551 | 0.4939 |
| v2 lexical only (`--mode lexical`) | 0.3061 | 0.5306 | 0.6735 | 0.4374 |

Corpus as indexed by v2: 101 files, 581 occurrences (v1: 96 files, 544 chunks).
Latency: index 1.7 s, embed 61.4 s (569 subjects), warm search p50 122.6 ms /
p95 126.1 ms — search time is dominated by per-query ONNX inference, which runs
inside the held `L2.read` (09 §3's as-built note).

**The gate fails on this run**: MRR regressed 0.1316 against a 0.03 budget, and
Recall@5 is 0.7755 against a 0.80 floor. That is the gate working, not a gate
misconfiguration — see `DEVIATIONS.md` D-016, which is `blocked` on a product
decision.

### Сопоставимость: что было неравным и что это стоило (D-016)

Первое измерение сравнивало не совсем одинаковые вещи. Чтение исходников v1
(`/opt/soft/local-rag` @ `31dfba2`) дало четыре расхождения, три из них устранимы.
Каждая ступень измерена отдельно, чтобы вклад был виден, а не смешался:

| Ступень | Корпус | Окно | Файлы / occ | Hit@1 | Hit@3 | Hit@5 | MRR |
| --- | --- | --- | --- | --- | --- | --- | --- |
| исходный прогон | весь репозиторий | 256 | 101 / 581 | 0.4286 | 0.6939 | 0.7755 | 0.5646 |
| **A** — корпус `src/` | `src/` | 256 | 93 / 545 | 0.4490 | 0.6939 | 0.7755 | **0.5680** |
| **A + B** — окно 1024 | `src/` | 1024 | 93 / 545 | 0.4490 | 0.6735 | 0.7959 | **0.5782** |
| v1 бейзлайн | `src/` | 3000 симв. | 96 / 544 | 0.5918 | 0.7959 | 0.8367 | **0.6963** |

**A — корпус.** v1 индексировал строго `<root>/src/` (`benchmark.ts::collectSrcFiles`),
92 `.ts` + 4 `.json` = ровно записанные 96 файлов. Первый прогон v2 взял весь
репозиторий, включая `scripts/benchmark.ts` — файл, где все 49 запросов лежат
дословными строковыми литералами и который для BM25 является почти идеальным
совпадением с заведомо неверным ответом. Ожидалось, что его удаление заметно
поднимет лексическую ногу; **это не подтвердилось** — +0.0034 MRR. Ценность
ступени в том, что корпуса теперь сошлись (93/545 против 96/544), а не в приросте.

**B — окно входа.** v1 усекал текст перед эмбеддингом на 3000 символах
(`MODEL_CONFIGS`), v2 — на 256 токенах, то есть примерно втрое агрессивнее.
Поднятие до 1024 дало +0.0102 MRR и +0.0204 Hit@5.

Вместе A и B отыграли **0.0136 из 0.1317** разрыва — около десятой части. Основное
расхождение остаётся за **C**.

**C — что именно эмбеддится.** v1 никогда не эмбеддил голый код: он строил
размеченный контекстный конверт (`benchmark.ts::buildEmbedCtx`) из пути, типа,
имени, докблока, сигнатуры **и** тела. v2's `code_raw` — это
`normalize(source_blob[span])`, без единого из этих полей. Отдельный
`description_vector` под LLM-описания у v1 тоже был, но в бейзлайне он **выключен**,
так что к разрыву отношения не имеет.

**D — веса.** v1 через сборку Ollama, v2 — `model_quantized.onnx` (q8). Уравнять
нечем; остаточное расхождение.

Остаточные различия, которые не устраняются и записаны явно: 4 `.json`-файла, которые
v1 индексировал, а v2 не берёт (v0-языки ts/js/rust, ADR-0001), и квантование весов.

### What the per-leg split rules in and out

- **Fusion is not the problem.** Hybrid (0.5646) beats both of its own legs
  (dense 0.4939, lexical 0.4374), so RRF is adding what it is supposed to add.
- **The gap is the dense leg.** v2's dense-only MRR (0.4939) sits 0.2024 below
  v1's dense-only baseline (0.6963), which is essentially the whole regression.
  Same model family, same corpus, same queries, and neither v1 nor v2 applies
  EmbeddingGemma's task prefixes — so the difference is in *what text gets
  embedded*, not in how it is searched.
- **Leading candidate: v1 embedded documentation, v2 does not.** v1's chunker
  attached each symbol's preceding JSDoc/`//` block to the embedded text
  (`src/indexer/parser.ts::extractDoc`/`extractJsDoc`/`extractLineComments`);
  v2's tree-sitter adapters carry no comment handling at all, so a unit's
  embedded text is bare code. For a corpus whose queries are natural-language
  descriptions ("retry embedding request on failure with backoff"), the doc
  comment is precisely the matching text.
- **Secondary candidates, unmeasured**: the installed weights are
  `model_quantized.onnx` (v1 went through Ollama's own build), and
  `MAX_SEQUENCE_TOKENS = 256` truncates long units.

None of these is fixed here: changing what a unit's text contains alters
`content_blob` derivation and invalidates every cache, which is well outside
T12-05's card.

## Notes / gaps

- Single-relevant corpus: one ground-truth target per query, no graded relevance judgments
  (14 §1 says "queries + relevance judgments") — registered as GAP-03.
- Only one embedding model was run. The v1 benchmark can sweep others
  (`qwen3-embedding:0.6b/4b/8b`, `mxbai-embed-large`); those are not required for the baseline
  shape and are not pulled here. Additional models can be appended as more `run-*.json` files.
- These numbers describe v1 behavior on the v1 codebase; they are a reference point for the v2
  gate, not a v2 target.
