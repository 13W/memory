# Группа 14 — Durable memory

Цель: транзакционно строгая, аудируемая память и измеряемое качество router. Ссылки: spec 03
§2.5; 04 §4–6; 08; 11 §2/§5; 12 §3–4; 14.

## T14-01 — Memory DDL и legal transitions

- **Результат:** memory/evidence/candidate/cursor/run/audit schema and typed kind-specific
  state guards; global singleton owner; immutable kind.
- **Тесты:** every legal/illegal transition, scope/canonical uniqueness incl global, terminal
  recall exclusion, hypothesis confirm vs fact supersede, constraints/FKs.

## T14-02 — Базовый transactional memory-op engine

- **Результат:** shared transaction/idempotency framework плюс create/reinforce/noop; mutation,
  evidence и audit атомарны; reinforce never edits text.
- **Тесты:** три operation contracts, optimistic conflict, same key returns original result,
  rollback failpoints, audit versions contiguous.

## T14-03 — Lifecycle/edit memory operations

- **Результат:** resolve/supersede/retract/edit поверх общего engine с expected_version и
  kind/state guards.
- **Тесты:** contract каждого op, illegal transition rollback, promotion creates fact via
  supersede, retract not delete, user/router actor audit.

## T14-04 — Merge memory operation

- **Результат:** one-tx merge: survivor absorbs evidence, losers become superseded and audit
  records exact merge set.
- **Тесты:** 2+ entries, duplicate evidence, incompatible scope/error, failpoint rollback,
  optimistic conflict and retry idempotence.

## T14-05 — Candidates и review operations

- **Результат:** propose/edit/approve/reject/expire; approval routes through same op engine with
  actor user and FK evidence.
- **Тесты:** state machine, 30-day fake-clock expiry, double approval idempotence, conflicting
  edit/version, rejected never materializes, list exposes version/provenance.

## T14-06 — Consolidation lease/cursor runner

- **Результат:** bounded snapshot, 120s lease/30s renew, generator outside tx, ordered op apply+
  cursor+run applied in one short tx; startup expired-lease retry/checkpoint triggers.
- **Тесты:** crash each step, lease expiry/renewal, never past to_seq, generator observes no DB
  tx, op retry no duplicates, cursor cannot advance on partial apply.

## T14-07 — Local router и quality fixture set

- **Результат:** ADR closes generator part O3; local_only default router distinguishes durable
  decision/hypothesis/question/negation/model claim and emits allowed ops/candidates; RU/EN set.
- **Тесты:** labeled create/reinforce/supersede/noop fixtures, adversarial/mixed-language cases,
  precision/recall report; O2 P/R threshold established from approved baseline, not invented.

## T14-08 — Recall relevance и safe formatting

- **Результат:** scope union, eligible filter, FTS+bounded brute cosine behind trait, RRF/budget,
  deterministic order; additionalContext sanitation/length/caps/delimiter escape and empty output.
- **Тесты:** scope isolation/union, ≤20k guard, terminal exclusion, tie order, 1500-token budget,
  control/injection/`</memory`/1KiB cases, exact byte len, empty emits zero bytes.

## G14 — Сверка memory correctness/quality

Перечитать spec 04 §4–6, 08, 11 §5, 12 §3–4, 14. Run state/transaction crash suite,
router benchmark and adversarial recall corpus. Проверить cursor atomicity, model claims never
auto-fact, evidence survives TTL and quality thresholds recorded from real baseline.
