# Группа 08 — FTS materialized view

Цель: lexical cache никогда не выглядит корректным без independently validated head. Ссылки:
spec 03 §4.3–4.4; 06 §3–4; 09 §2; 14 §2/§4.

## T08-01 — FTS schema, preprocessing и manifest

- **Результат:** cache migrations for fts_doc/FTS5/head; versioned code-aware tokenizer emits
  original+camel/snake/kebab/name/qualified/path/signature tokens; deterministic FTS manifest.
- **Тесты:** golden token fixtures (Unicode included), bm25 columns, rowid linkage, manifest
  order independence and worktree/generation domain separation.

## T08-02 — FTS build/delta и head-last commit

- **Результат:** generation materializer derives all unit kinds from state/source cache,
  mutates one cache tx and writes head last; evicted normalized text recomputed.
- **Тесты:** A→B add/rename/delete; all kinds present; fail before head rolls tx/no valid head;
  cache text eviction; repeated build exact same rows/head.

## T08-03 — Validation, degradation и rebuild

- **Результат:** cheap per-search head/count validation, strong open/post-rebuild manifest;
  <2s estimated synchronous rebuild else dense_only+diagnostic/background job.
- **Тесты:** missing/stale generation/schema/tokenizer/count/manifest cases; empty FTS invalid;
  diagnostics state exact reason; no dense leg produces INDEX_UNAVAILABLE instead of silence.

## T08-04 — FTS corruption/staleness tests

- **Результат:** integration suite deletes cache/head/rows and corrupts equal-count ID set.
- **Тесты:** every case either ready rebuilt or explicitly degraded; state.sqlite unchanged;
  delete whole cache→full restoration; concurrent validation/rebuild coalesces.

## G08 — Сверка FTS consistency

Перечитать spec 06 §3–4, 09 §2, 14. Проверить head-last, generation-scoped occurrences,
all unit kinds, version invalidation and no silently empty lexical leg. Run cache deletion and
rebuild acceptance case.
