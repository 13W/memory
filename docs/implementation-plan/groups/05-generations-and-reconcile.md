# Группа 05 — Generations и strict reconcile

Цель: authoritative tree snapshot с structural sharing и deterministic occurrences. Ссылки:
spec 04 §1; 06 §1–3, §6; 14 §2/§4–5.

## T05-01 — Generation/occurrence schema и transitions

- **Результат:** generation repository allocates monotone number per worktree, legal states,
  deterministic occurrence IDs, source-blob structural guard.
- **Тесты:** concurrent allocation unique; illegal transitions rollback; cross-worktree current
  generation rejected; occurrence golden/retry/order independence; one active invariant.

## T05-02 — Authoritative tree scan и ignore semantics

- **Результат:** git-aware/non-git scanner returns canonical sorted manifest; stat cache only
  advisory and strict mode hashes all required files.
- **Тесты:** nested ignore/negation/symlink/rename/delete; stat collision escalates on doubt;
  watcher-overflow entry calls strict mode; non-git tree parity.

## T05-03 — Generation builder и structural sharing

- **Результат:** building N+1 reuses unchanged revisions/units, parses changed files, persists
  files/skips/occurrences atomically in bounded phases, reaches projection_ready only complete.
- **Тесты:** tree A→B fixture families; one-file edit does not duplicate untouched units;
  rename shares content but changes occurrences; deletion; parse failure yields failed/no active
  mutation; retry no duplicates.

## T05-04 — Reconcile scheduler/triggers

- **Результат:** per-worktree debounce (500 ms), startup/periodic/manual/git triggers coalesce;
  one writer per worktree; cancellation only at safe tx boundary.
- **Тесты:** fake-clock debounce/coalescing; overflow strict; concurrent triggers make one next
  generation; manual force; cancellation leaves active generation valid.

## T05-05 — Retry/failure handling

- **Результат:** typed failure records generation failed and projection last_error without
  routing it; backoff/counter observability.
- **Тесты:** failpoint each build phase; previous active remains; retry builds a new valid
  generation; failed/retiring never selected for routing.

## G05 — Сверка reconcile и generation invariants

Перечитать spec 04 §1, 06 §1–3/§6, 14. Run all A→B fixtures, retries and concurrent scans.
Проверить structural sharing, strict-overflow, immutable active snapshots и отсутствие routing
по retiring. Projection activation ещё не имитировать обходным update.
