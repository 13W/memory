# Группа 02 — Реестр repository/worktree

Цель: durable identity не зависит от пути и переживает move/reattach. Ссылки: spec 01 §5;
02 §3; 03 §1.1–1.3, §2.1; 04 §7; 12 §7.

## T02-01 — Canonical path и hash/ID primitives

- **Результат:** UUIDv7 generator; BLAKE3 domain encoding; relative/absolute canonicalization,
  display preservation; remote normalization strips credentials.
- **Тесты:** golden hashes for every domain; field-boundary collision regression; NFC,
  symlink, case-insensitive, drive/UNC platform fixtures; remote SSH/HTTPS equivalence;
  hash-derived IDs deterministic under order/retry.

## T02-02 — Registry schema и repository operations

- **Результат:** migrations for repository/path/settings; create/find/observe path operations
  update single current path transactionally; remote hash is hint, not unique identity.
- **Тесты:** one-current constraint; same remote can map to two repos; path history retained;
  no `canonical_path` duplicate source; retry idempotence.

## T02-03 — Stable worktree operations и path history

- **Результат:** worktree/worktree_path/generation FK seam; random durable worktree UUID;
  explicit state transitions active/detached/removing.
- **Тесты:** path hash never equals/defines ID; current generation cross-worktree FK rejected;
  one-current path; detach/reattach retains ID; illegal transition typed error.

## T02-04 — Attach/move/non-git resolution

- **Результат:** request root resolver and attach operation for main/linked/non_git, explicit
  ambiguity error, common-dir fingerprint only advisory.
- **Тесты:** directory move preserves repo/worktree IDs; recreated path does not steal identity;
  linked ambiguity requires ID; unknown root resolves global-only; non-git happy path.

## T02-05 — Config merge и data-policy ordering

- **Результат:** versioned TOML config plus repo settings; validated defaults; most-restrictive
  effective data policy.
- **Тесты:** every policy pair ordering; invalid enum/version; repo cannot relax global;
  no repo-local config file lookup; deterministic merged snapshot.

## G02 — Сверка identity и registry

Перечитать spec 01 §5, 02 §3, 03 §1–2.1, 04 §7, 12 §7. Провести schema audit: ни один
durable FK не указывает на path-derived value; нет ambient current project и второго current
path. Запустить move/attach/normalization tests на доступных OS runners.
