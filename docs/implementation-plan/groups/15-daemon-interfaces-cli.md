# Группа 15 — Daemon, protocol, MCP и CLI

Цель: собрать реализованные ядра в co-located продукт без indexing на MCP path. Ссылки:
spec 02 §1/§4/§6; 11; 13 §4.

## T15-01 — Store lock и daemon lifecycle

- **Результат:** one daemon/store lock JSON with instance UUID handshake, stale-owner recovery,
  startup order/workers and drain shutdown; idle shutdown checks sessions/spool/jobs.
- **Тесты:** live conflict, PID reuse mismatch, stale socket/lock, migration-only health mode,
  pending spool prevents idle exit, SIGTERM at safe points, WAL checkpoint/release.

## T15-02 — Versioned proxy protocol/handshake

- **Результат:** framed UDS/named-pipe HELLO/WELCOME/INCOMPATIBLE with proto/MCP/spool versions;
  thin stdio pass-through adds explicit request context; spawn retry and graceful upgrade request.
- **Тесты:** compatible/incompatible tables, daemon absent backoff with fake clock, 20s cap,
  30s upgrade timeout, context on every request, signal forwarding, proxy holds no project state.

## T15-03 — MCP code query tools

- **Результат:** search_code/get_file_context/project_overview adapt domain APIs to MCP content,
  degraded flags and canonical errors; server instructions describe search protocol.
- **Тесты:** JSON contract snapshots, explicit context routing, isError mapping, no synchronous
  indexing call, unknown worktree behavior.

## T15-04 — MCP status и memory read tools

- **Результат:** stats/health/recall/list_memory/list_candidates/inspect_evidence adapters with
  pagination/filter contracts and global-only behavior where applicable.
- **Тесты:** contract snapshots, scope/filter/pagination, degraded status, unknown worktree with
  global memory, read calls produce no writes.

## T15-05 — MCP memory/write tools

- **Результат:** remember/review/edit/retract/merge/give_feedback with preconditions/idempotency;
  feedback creates daemon-side observation identity.
- **Тесты:** each tool happy/error/retry, expected_version conflicts, actor/trust semantics,
  feedback duplicate request; v1 forget/consolidate names not exposed as destructive behavior.

## T15-06 — Hook recall RPC и additionalContext

- **Результат:** after unconditional spool append, SessionStart/UserPrompt hook optionally calls
  read-only recall ≤300ms, never starts daemon, outputs required JSON or nothing and exit 0.
- **Тесты:** ordering append-before-RPC, reachable/unreachable/timeout/error, zero bytes empty,
  read path performs no writes, byte-deterministic adversarial block.

## T15-07 — Service/index/registry CLI

- **Результат:** serve/status/stop/restart/init, index/reindex/watch, repo/worktree and rebuild
  command surface delegates typed APIs.
- **Тесты:** CLI parse snapshots, service lifecycle, attach ambiguity, reindex/rebuild errors,
  JSON-friendly output where documented.
- **Добавлено D-013 (G11):** `init` обязан **зарегистрировать `code_raw`-представление дефолтного
  model space** (`local_rag_embed::register_embedder_representation` + `set_model_space_representation`,
  required = true) для установленной ADR-0004 модели — spec 10 §3 «a model space bundles … at
  minimum `code_raw` + `memory` in v0». Сегодня seeded space (`SCHEMA_V4`) `active`, но пуст, и
  свежий стор отказывает `NoCodeRepresentation`/`NoShardParams`. Миграцией это делать нельзя:
  `model_id` — решение ADR-0004, зашивать его в DDL значит захардкодить то, ради миграции чего
  существует 10 §4, и заставить каждый стор заявлять модель, чьи веса могли не скачиваться.
  `memory`-половина — группа 14.
- **Тесты (добавлено D-013):** после `init` дефолтный space требует `code_raw`, его
  `RepresentationKey` совпадает с `key()` установленного провайдера, а `params_for_model_space`
  отдаёт `dimensions` этой модели; повторный `init` идемпотентен (тот же `representation_id`).
- **Добавлено ADR-0009 (группа 20):** `index`/`reindex`/`watch` остаются ровно тем, что эта
  карточка построила — standalone-процессы, безопасные рядом с живым `serve`. Группа 20 не
  реверсирует это решение: она даёт демону **дополнительный**, персистентно-регистрируемый
  путь (`local-rag project …`) для тех worktree, которые пользователь явно поручил демону, а
  `watch` остаётся daemon-independent сиблингом для ручного/CI использования — тот же прецедент,
  что уже фиксирует spec 11 §6's as-built заметка к этой карточке.

## T15-08 — Memory/privacy/diagnostic CLI

- **Результат:** memory review/mutation, inspect/export/purge, gc/doctor/stats command adapters.
- **Тесты:** parse snapshots and critical subprocess flows; expected_version surfaced;
  destructive purge requires explicit selector/confirmation; dry-run GC and doctor exit codes.

## G15 — Сверка interface contracts

Перечитать spec 02 lifecycle/errors, 11, 13 §4. Execute proxy-daemon-hook end-to-end with daemon
down/restart/upgrade, all v0 MCP contract tests and CLI inventory. Проверить no project files,
no ambient routing, ingestion never via RPC and indexing isolated from MCP path.
