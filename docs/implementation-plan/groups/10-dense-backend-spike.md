# Группа 10 — Выбор dense backend

Цель: закрыть O1 измерениями, не предпочтением исполнителя. Ссылки: spec 05 §1/§9;
10 §1; 14 §7; 15 step 11/O1.

## T10-01 — Воспроизводимый spike harness и corpus

- **Результат:** одинаковый ProjectionStore conformance/benchmark runner для кандидатов;
  datasets small/representative/large и фиксированная matrix metrics.
- **Тесты:** seeded dataset repeatability; metric schema validation; adapter conformance
  includes reopen/corruption/head/manifest; unsupported platform reported, not skipped silently.

## T10-02 — Brute-force adapter и измерения

- **Результат:** isolated experimental adapter over embedding cache; report warm p95, RAM,
  open/close, registry startup, LRU, rebuild/durability and platform support.
- **Тесты:** shared conformance + exact-neighbor oracle + crash/reopen cases.

## T10-03 — usearch adapter и измерения

- **Результат:** isolated adapter and same raw/report artifacts; build/platform friction and
  ID width mapping recorded.
- **Тесты:** identical shared conformance, recall vs oracle, F1–F12 applicable scenarios;
  win32 build smoke or explicit evidence of failure.

## T10-04 — Qdrant Edge adapter и измерения

- **Результат:** isolated adapter and same artifacts; embedded/no-external-daemon validation;
  filtered-HNSW availability recorded though not critical path.
- **Тесты:** same conformance/quality/crash/platform suite with no external service.

## T10-05 — Сравнить, выбрать backend и оформить ADR

- **Результат:** normalized comparison table, reproducible raw results, explicit weights and
  chosen simplest candidate passing quality/platform/correctness gates; optimize thresholds.
- **Тесты:** report regeneration check; selected adapter passes ProjectionStore suite on CI
  targets available; dependency/license audit.
- **Не в scope:** embedding pipeline or production search integration.

## G10 — Сверка backend neutrality

Перечитать spec 05, 10 §1, 14 §7, 15 O1. Проверить одинаковость experiments, все обязательные
metrics и отсутствие предрешённого coupling в T00–T09. Обновить O1 в spec/ADR; удалить только
production coupling к проигравшим кандидатам, сохранив reproducible spike artifacts.
