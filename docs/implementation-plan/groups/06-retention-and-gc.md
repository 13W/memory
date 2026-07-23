# Группа 06 — Retention и GC

Цель: bounded storage без удаления pin roots. Ссылки: spec 03 §3; 05 §8; 06 §5; 07 §6;
15 O6.

## T06-01 — Pin-root calculation

- **Результат:** pure mark phase covers active/building/target, configurable last K or TTL,
  memory/audit/export refs and leased jobs; O6 values stay configurable and documented TBD.
- **Тесты:** table-driven root combinations, expired/non-expired lease, K/T boundaries,
  worktree isolation; mark is deterministic.

## T06-02 — Batched generation/source sweep

- **Результат:** delete order edges/occurrences→membership/skips→generation→unreferenced
  revisions/units/blobs in ≤500-row state transactions; dry-run report.
- **Тесты:** pinned rows survive; orphan graph fully removed; interruption between batches
  resumes; shared revision retained until final ref; dry-run mutates nothing.

## T06-03 — Orphan/quarantine/spool housekeeping

- **Результат:** startup/periodic sweep handles orphan shard dirs, max two quarantine cycles,
  removing grace and fully committed old session spools; never deletes uncommitted bytes.
- **Тесты:** fake-clock grace/14-day cases; unknown shard; quarantine rotation; partial cursor
  retention; repeated sweep idempotence.

**As-built (T06-03, split per D-004):** реализована только **orphan shard-dir sweep** (spec 05 §8),
единственная часть с готовым фундаментом (layout `projection/<worktree_id>` + таблица `worktree`
существуют) — `local_rag_store::housekeeping` (`sweep_orphan_shard_dirs` + `run_orphan_shard_sweep`);
покрыты «unknown shard» и «repeated sweep idempotence». Остальные цели/тест-кейсы отложены в их
owning-карточки, т.к. их подсистемы вводятся позже по `[FIXED]` roadmap (spec 15 §1): **quarantine
rotation** → T07-04 (выполнено); **grace-destroy `removing`/`detached` shard** (нужна миграция
`removed_at`) → группа 07/09 shard lifecycle — **закрыто в D-007**: гейт G09 обнаружил, что оба
названных владельца прошли без этой цели, и требование реализовано там (миграция 5
`worktree.state_changed_at` + `run_expired_shard_sweep`); **spool-GC (14-day / committed cursor /
uncommitted-retained)** → T13-05 (дубль spec 07 §6). См. `DEVIATIONS.md` D-004 и D-007;
G06 подтверждает деферал в owning-карточки.

## G06 — Сверка rebuildability и retention

Перечитать spec 05 §8, 06 §5, 07 §6. Построить pin-root truth table и crash sweep tests.
Проверить transaction batch ceiling и metrics-driven checkpoint/VACUUM policy seams. O6 не
считать закрытым без данных.
