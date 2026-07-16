# Группа 07 — Fake projection и протокол переключения

Цель: доказать crash correctness до выбора backend. Ссылки: spec 03 §2.2; 04 §1–3/§8;
05 целиком; 14 §3–5.

## T07-01 — ProjectionStore contract и fake backend

- **Результат:** backend-neutral traits/types, deterministic point/head/manifest functions;
  persistent fake supports inspect/corrupt/failpoint controls.
- **Тесты:** point/manifest golden; sorted-set independence; idempotent upsert/delete; head is
  last operation; fake reopen preserves configurable state.

## T07-02 — Two-axis projection state guards

- **Результат:** DDL/repository for worktree_projection_state and minimal model registry seed;
  clean/updating/dirty/rebuilding invariants and one-axis-per-operation preconditions.
- **Тесты:** truth table of valid/invalid rows; target required updating; active==projected clean;
  simultaneous generation+model target rejected; typed illegal transition.

## T07-03 — Desired-set write-ahead switch

- **Результат:** prepare→SQLite updating→desired-set backend reconcile→head→single final tx;
  generation transition in same final tx; no command-log replay.
- **Тесты:** add/change/delete point sets; unchanged vectors not recomputed; retry from arbitrary
  partial fake set; backend error leaves detectable updating; final tuple exact.

## T07-04 — Validate-on-open и rebuild

- **Результат:** every shard open validates status/tuples/op/head/count/manifest; divergence
  marks dirty then full fresh/quarantined rebuild of active tuple; missing vectors callback.
- **Тесты:** each predicate independently; equal-count/different-ID; unopenable shard;
  crash/retry rebuild; never clean with partial expected set; second rebuild no-op-equivalent.

## T07-05 — F1–F12 fault matrix

- **Результат:** named test per spec 05 §10 with exact expected detection signal and reusable
  artifact output.
- **Тесты:** all F1–F12, including post-clean WAL loss simulation, swallowed backend errors,
  crash during rebuild; each asserts detection **and** correct idempotent rebuild.

## G07 — Сверка projection correctness

Перечитать spec 04 projection/generation, 05 и 14 §3. Проверить matrix 12/12, two-axis
serialization, no durable-barrier assumption, validate on every fake reopen and recovery only
from active canonical state. Backend-specific crate dependency запрещена до PASS G10.
