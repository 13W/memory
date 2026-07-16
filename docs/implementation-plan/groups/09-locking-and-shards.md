# Группа 09 — Locking и shard lifecycle

Цель: поиск видит одну projection version, а open shards bounded. Ссылки: spec 02 §5–6;
04 §8; 05 §2/§8; 06 §3; 14 §4.

## T09-01 — Lock hierarchy и per-worktree coordination

- **Результат:** typed lock wrappers/documented levels L0–L4; per-worktree RwLock registry;
  writer queues are leaves; debug/test order assertions.
- **Тесты:** allowed order succeeds; reverse acquisition fails fast in test; separate worktrees
  progress concurrently; same worktree writers serialize; queue callback cannot acquire locks.

## T09-02 — Ref-counted shard LRU

- **Результат:** max_open_shards manager holds mutex only for lookup/eviction, returns ref-counted
  handles, validates every actual open/reopen, never evicts live handle.
- **Тесты:** LRU order/cap; concurrent same-key open once; in-use eviction deferred; corrupt
  cold reopen triggers rebuild; remove cancels background rebuild safely.

## T09-03 — Snapshot/read-lock search skeleton

- **Результат:** orchestration resolves explicit context, takes L2.read across active tuple,
  FTS and fake dense calls and enrichment, then releases; canonical degraded/error envelope.
- **Тесты:** instrumentation proves lock held in every leg; unknown root; dense-only,
  lexical-only, neither, bounded writer wait/BUSY_RETRY.

## T09-04 — Concurrent switches и generation mixing

- **Результат:** load/failpoint tests alternate generation/model switch against searches.
- **Тесты:** every response contains occurrences from exactly one active generation/model tuple;
  axes serialize and final tuple deterministic; no L3 held during backend query; no deadlock.

## G09 — Сверка concurrency model

Перечитать spec 02 §5–6, 04 §8, 05 §8, 06 §3. Проверить code paths against lock table,
threaded/loom-style tests where useful, full generation-mixing case and explicit degradation.
Нельзя переходить к backend spike с обходом trait или read lock.
