# Группа 12 — Hybrid code search

Цель: deterministic lexical+dense retrieval из одной active tuple. Ссылки: spec 06 §3–4;
09; 11 §2; 14 §2/§7.

## T12-01 — Lexical leg и фильтры

- **Результат:** active-generation FTS query with specified BM25 defaults, code-aware query
  preprocessing, name_pattern prefix filter and candidate depth.
- **Тесты:** ranking goldens, identifiers/path/signature, all unit kinds, filter edge cases,
  stale head never queried as valid.

## T12-02 — Production dense leg

- **Результат:** selected T10 backend adapter integrated; query embedding from active model
  representation/distance; code_raw v0 and code_context only if ADR benchmark selects it.
- **Тесты:** active representation selection, distance metrics, missing/corrupt shard explicit
  lexical_only, no tenant/generation filter dependence within per-worktree shard.

## T12-03 — RRF и deterministic response

- **Результат:** RRF k=60, depth max(limit*4,50), occurrence merge, tie by occurrence_id;
  canonical response generation/degraded/diagnostics.
- **Тесты:** hand-calculated fusion, duplicate across legs, ties, limits, single-leg degradation,
  neither leg error; repeated output byte-stable where serialization promises it.

## T12-04 — Source_blob snippets/context/overview

- **Результат:** size-capped span-bound snippets from stored bytes; get_file_context and cached
  per-generation 3-level overview without live disk reads.
- **Тесты:** mutate/delete live file after generation and get original snippet; UTF-8 byte span,
  8 KiB cap/truncation metadata; unknown path; overview invalidates on generation switch.

## T12-05 — 49-query baseline/gates

- **Результат:** runner produces MRR/Recall@5, latency and per-query diff vs v1; O2 search
  thresholds filled only from agreed baseline; tuning changes are versioned.
- **Тесты:** metric math goldens, corpus integrity, deterministic runs/tolerance, regression
  gate intentionally fails on degraded fixture.

## G12 — Сверка search v0

Перечитать spec 09, 11 code tools, 14. Run 49 queries, generation-mixing load test, cache/shard
degradation and live-file mutation cases. Semantic description/reranker and graph tools remain
unsupported/deferred unless separately approved after baseline.
