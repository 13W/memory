# Прогресс имплементации

Статусы: `[ ]` не начато, `[~]` в работе, `[x]` завершено, `[!]` заблокировано. Одновременно
может быть `[~]` только одна задача, если карточка явно не разрешает независимую работу.
Gate следующей группы нельзя начинать до `PASS` предыдущего.

## 00 — Контракт разработки и baseline

- [ ] T00-01 Импортировать v1 behavioral fixtures и зафиксировать baseline inventory
- [ ] T00-02 Создать Rust workspace, quality commands и CI smoke
- [ ] T00-03 Создать общий fixture/failpoint test harness
- [ ] G00 Сверка foundations и testing contract

## 01 — Миграции и SQLite foundation

- [ ] T01-01 Реализовать разрешение store/config путей и permissions
- [ ] T01-02 Реализовать state DB open policy и bounded writer
- [ ] T01-03 Реализовать forward-only migration runner
- [ ] T01-04 Добавить resumable/destructive migration mechanics
- [ ] T01-05 Реализовать cache DB binding и recreation
- [ ] G01 Сверка migration/storage foundation

## 02 — Реестр repository/worktree

- [ ] T02-01 Реализовать canonical path и hash/ID primitives
- [ ] T02-02 Добавить registry schema и repository operations
- [ ] T02-03 Добавить stable worktree operations и path history
- [ ] T02-04 Реализовать attach/move/non-git resolution
- [ ] T02-05 Реализовать config merge и data-policy ordering
- [ ] G02 Сверка identity и registry

## 03 — Exact source и skip policy

- [ ] T03-01 Добавить code-storage DDL и repositories
- [ ] T03-02 Реализовать file classification и skip reasons
- [ ] T03-03 Реализовать exact source_blob/file_revision reuse
- [ ] T03-04 Реализовать normalized text cache regeneration
- [ ] G03 Сверка source-blob invariant

## 04 — Парсинг и path-independent units

- [ ] T04-01 Закрыть O4 и оформить ADR первого набора языков
- [ ] T04-02 Реализовать parser fingerprint и parser abstraction
- [ ] T04-03 Реализовать первый язык и parser fixtures
- [ ] T04-04 Реализовать второй язык и parser fixtures
- [ ] T04-05 Реализовать третий язык, если выбран ADR
- [ ] T04-06 Реализовать deterministic parsed-unit persistence
- [ ] G04 Сверка parsing identity

## 05 — Generations и strict reconcile

- [ ] T05-01 Реализовать generation/occurrence schema и state transitions
- [ ] T05-02 Реализовать authoritative tree scan и gitignore semantics
- [ ] T05-03 Реализовать generation builder и structural sharing
- [ ] T05-04 Реализовать reconcile scheduler/triggers
- [ ] T05-05 Реализовать retry/failure handling
- [ ] G05 Сверка reconcile и generation invariants

## 06 — Retention и GC

- [ ] T06-01 Реализовать pin-root calculation
- [ ] T06-02 Реализовать batched generation/source sweep
- [ ] T06-03 Реализовать orphan/quarantine/spool housekeeping
- [ ] G06 Сверка rebuildability и retention

## 07 — Fake projection и протокол переключения

- [ ] T07-01 Определить ProjectionStore contract и fake backend
- [ ] T07-02 Добавить two-axis projection state guards
- [ ] T07-03 Реализовать desired-set write-ahead switch
- [ ] T07-04 Реализовать validate-on-open и rebuild
- [ ] T07-05 Реализовать F1–F12 fault matrix
- [ ] G07 Сверка projection correctness

## 08 — FTS materialized view

- [ ] T08-01 Добавить FTS schema, preprocessing и manifest
- [ ] T08-02 Реализовать FTS build/delta и head-last commit
- [ ] T08-03 Реализовать validation, degradation и rebuild
- [ ] T08-04 Добавить FTS corruption/staleness tests
- [ ] G08 Сверка FTS consistency

## 09 — Locking и shard lifecycle

- [ ] T09-01 Реализовать lock hierarchy и per-worktree coordination
- [ ] T09-02 Реализовать ref-counted shard LRU
- [ ] T09-03 Реализовать snapshot/read-lock search skeleton
- [ ] T09-04 Проверить concurrent switches и generation mixing
- [ ] G09 Сверка concurrency model

## 10 — Выбор dense backend

- [ ] T10-01 Подготовить воспроизводимый spike harness и corpus
- [ ] T10-02 Адаптер и измерения brute-force backend
- [ ] T10-03 Адаптер и измерения usearch backend
- [ ] T10-04 Адаптер и измерения Qdrant Edge backend
- [ ] T10-05 Сравнить, выбрать backend и оформить ADR
- [ ] G10 Сверка отсутствия преждевременной backend coupling

## 11 — Embeddings и model spaces

- [ ] T11-01 Реализовать representation/model-space registry
- [ ] T11-02 Реализовать embedding cache integrity и eviction
- [ ] T11-03 Реализовать local embedder provider pool
- [ ] T11-04 Реализовать resumable coverage backfill
- [ ] T11-05 Реализовать per-worktree model switch
- [ ] T11-06 Реализовать model asset installer
- [ ] G11 Сверка model migration

## 12 — Hybrid code search

- [ ] T12-01 Реализовать lexical leg и фильтры
- [ ] T12-02 Интегрировать выбранный dense backend
- [ ] T12-03 Реализовать RRF и deterministic response
- [ ] T12-04 Реализовать source_blob snippets/context/overview
- [ ] T12-05 Запустить 49-query baseline/gates
- [ ] G12 Сверка search v0

## 13 — Spool и observations

- [ ] T13-01 Реализовать redaction/caps перед записью
- [ ] T13-02 Реализовать segment writer и hook fail-open
- [ ] T13-03 Реализовать frame reader и identity semantics
- [ ] T13-04 Реализовать transactional importer/cursor
- [ ] T13-05 Реализовать payload TTL и spool GC
- [ ] T13-06 Реализовать S1–S8 kill matrix
- [ ] G13 Сверка no-loss ingestion

## 14 — Durable memory

- [ ] T14-01 Добавить memory DDL и legal state transitions
- [ ] T14-02 Реализовать базовый transactional memory-op engine
- [ ] T14-03 Реализовать lifecycle/edit memory operations
- [ ] T14-04 Реализовать merge memory operation
- [ ] T14-05 Реализовать candidates и review operations
- [ ] T14-06 Реализовать consolidation lease/cursor runner
- [ ] T14-07 Реализовать local router и quality fixture set
- [ ] T14-08 Реализовать recall relevance и safe formatting
- [ ] G14 Сверка memory correctness/quality

## 15 — Daemon, protocol, MCP и CLI

- [ ] T15-01 Реализовать store lock и daemon lifecycle
- [ ] T15-02 Реализовать versioned proxy protocol/handshake
- [ ] T15-03 Реализовать MCP code query tools
- [ ] T15-04 Реализовать MCP status и memory read tools
- [ ] T15-05 Реализовать MCP memory/write tools
- [ ] T15-06 Реализовать hook recall RPC и additionalContext
- [ ] T15-07 Реализовать service/index/registry CLI
- [ ] T15-08 Реализовать memory/privacy/diagnostic CLI
- [ ] G15 Сверка interface contracts

## 16 — Security, privacy и recovery operations

- [ ] T16-01 Централизовать remote policy enforcement
- [ ] T16-02 Реализовать inspect/export/purge и audit tombstones
- [ ] T16-03 Реализовать doctor/rebuild/degraded diagnostics
- [ ] T16-04 Добавить adversarial and ownership suite
- [ ] G16 Сверка security/privacy

## 17 — Distribution и release gates

- [ ] T17-01 Создать npm launcher и platform packages
- [ ] T17-02 Создать Claude Code plugin registration
- [ ] T17-03 Настроить cargo-dist/zigbuild platform CI
- [ ] T17-04 Проверить upgrade/migration/offline install flows
- [ ] T17-05 Запустить полный acceptance/resource/latency suite
- [ ] G17 Финальная сверка v0

## Evidence

### Gate results

Заполняется только после выполнения gate: `PASS`, `PASS after D-NNN` или `BLOCKED`.

| Gate | Результат | Ссылка на evidence/report |
| --- | --- | --- |
| G00 | — | — |
| G01 | — | — |
| G02 | — | — |
| G03 | — | — |
| G04 | — | — |
| G05 | — | — |
| G06 | — | — |
| G07 | — | — |
| G08 | — | — |
| G09 | — | — |
| G10 | — | — |
| G11 | — | — |
| G12 | — | — |
| G13 | — | — |
| G14 | — | — |
| G15 | — | — |
| G16 | — | — |
| G17 | — | — |

### Task evidence

Добавляйте строки без удаления истории:

| ID | Commit/PR | Проверки | Результат/артефакт | Исполнитель/дата |
| --- | --- | --- | --- | --- |
| — | — | — | — | — |
