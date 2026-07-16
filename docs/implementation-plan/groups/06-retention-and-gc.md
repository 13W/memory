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

## G06 — Сверка rebuildability и retention

Перечитать spec 05 §8, 06 §5, 07 §6. Построить pin-root truth table и crash sweep tests.
Проверить transaction batch ceiling и metrics-driven checkpoint/VACUUM policy seams. O6 не
считать закрытым без данных.
