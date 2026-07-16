# Группа 16 — Security, privacy и recovery operations

Цель: централизовать policy и дать пользователю проверяемый контроль над данными. Ссылки:
spec 02 §6; 12; 11 §6; 14 §6.

## T16-01 — Central remote policy enforcement

- **Результат:** all Embedder/Generator remote selections pass one effective-policy guard;
  metadata-only/redaction/full semantics explicit; blocked call typed and local fallback only
  where configured.
- **Тесты:** policy×provider matrix, repo cannot relax global, no bytes reach fake remote under
  local_only, redaction before remote, POLICY_BLOCKED_REMOTE diagnostic.

## T16-02 — Inspect/export/purge и audit tombstones

- **Результат:** scoped deterministic export; inspect observation/memory/generation; purge is
  only hard-delete path and rewrites audit refs to non-sensitive tombstones transactionally.
- **Тесты:** scope isolation, payload expired export, purge memory/session/all authorization UX,
  crash rollback, no orphan FK/private text in audit, retract remains non-delete.

## T16-03 — Doctor/rebuild/degraded diagnostics

- **Результат:** doctor validates lock/versions/heads/orphans/permissions; rebuild dense/FTS;
  diagnostics expose exact validation reason and actionable typed error.
- **Тесты:** seeded fault per diagnostic, dry/no mutation checks, dense/FTS/cache deletion
  recovery solely from state, both legs unavailable, repeated rebuild.

## T16-04 — Adversarial and ownership suite

- **Результат:** end-to-end corpus covers malicious repo/memory/transcript text, delimiter and
  control payloads, symlink/path tricks, wrong-owner store/endpoint permissions.
- **Тесты:** inert recall round-trip, caps always enforced, source secrets skipped/no blob,
  hook raw secret absent from disk, owner-only endpoint (platform gated).

## G16 — Сверка security/privacy

Перечитать spec 12 и related 02/11/14. Run adversarial corpus, remote exfiltration spies,
permissions and purge recovery. Trace each content flow through redact/cap/policy. Optional
encryption remains optional; отсутствие реализации не маскировать обещанием enabled.
