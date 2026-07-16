# Группа 17 — Distribution и release gates

Цель: воспроизводимо доставить v0 на зафиксированные платформы и пройти все gates. Ссылки:
spec 01 §2; 13; 14; 15 §2/§4.

## T17-01 — npm launcher и platform packages

- **Результат:** `@13w/local-rag` selects optional platform package and launches native
  binary under npm/pnpm/yarn; forwards signals, cleans child, actionable missing-platform error.
- **Тесты:** mocked layout matrix, supported platform resolution, missing package, SIGINT/SIGTERM,
  spaces/symlinks, no orphan; package contents exclude weights and unrelated files.

## T17-02 — Claude Code plugin registration

- **Результат:** minimal plugin registers proxy and seven hook events automatically, no project
  init/rules files; MCP instructions deliver RECALL→SEARCH_CODE→THINK→ACT→REMEMBER.
- **Тесты:** manifest/schema, install/uninstall fixture, paths per platform, exact hooks list,
  no writes inside sample repository, hook cold-start measurement (<50ms target).

## T17-03 — cargo-dist/zigbuild platform CI

- **Результат:** signed/checksummed artifacts for darwin x64/arm64, linux x64/arm64, win32 x64;
  ORT/tree-sitter/SQLite/chosen backend smoke; win32-arm64 explicitly excluded.
- **Тесты:** binary version/health, SQLite migration, parser, embed, projection reopen, hook append,
  proxy handshake per target; artifact manifest verification.

## T17-04 — Upgrade/migration/offline install flows

- **Результат:** packaged old→new daemon drain/migrate/reconnect, every released schema fixture,
  spool backward compatibility and offline use after model init; O5 resolved before GA or
  explicitly documented MVP/GA block.
- **Тесты:** crash at migration checkpoints, restore backup, newer-store refusal, old spool
  versions import, incompatible new hook warning, offline search/recall.

## T17-05 — Full acceptance/resource/latency suite

- **Результат:** versioned release report for quality, memory-quality, latency, resources,
  reliability, consistency, sharing, idempotency and rebuild gates; O2/O6 remaining values
  resolved by evidence or release blocked.
- **Тесты:** all workspace/fixture/F1–F12/S1–S8/49-query/router/platform suites; idle RAM,
  bytes/symbol, cache budget, source/worktree ratio, p95 scenarios with raw artifacts.

## G17 — Финальная сверка v0

Перечитать `idea.md` rev 6 и **все** документы spec 01–15. Обновить TRACEABILITY фактическими
links `requirement → code → test → report`; проверить каждый `[FIXED]`, `[SPEC]`, `[OPEN]` и
deferred boundary. `DEVIATIONS.md` не должен иметь open/fixing; каждый предыдущий gate PASS.
Релиз разрешён только при зелёных обязательных gates либо явном product sign-off на числовой
`[BASELINE]`, но не на нарушение correctness/security invariants.
