# Группа 03 — Exact source и skip policy

Цель: searchable content всегда восстанавливается только из state.sqlite. Ссылки: spec 03
§2.3–2.4, §4.2; 06 §2; 12 §2, §5.

## T03-01 — Code-storage DDL и repositories

- **Результат:** migrations и typed repositories для file_revision/content_blob/parsed_unit,
  generation membership/skips с точными constraints (пока без parser logic).
- **Тесты:** full DDL constraint suite: unique revision key, span check, skip reason enum,
  occurrence requires generation_file, FK behavior; schema audit forbidden path columns.

## T03-02 — File classification и skip reasons

- **Результат:** deterministic classifier for ignored/binary/LFS/huge/secret/encoding with
  configured cap and precedence documented; shared versioned secret scanner reusable by
  spool/remote flows; skipped metadata never stores source_blob.
- **Тесты:** fixture per reason, nested gitignore/negation, NUL, LFS pointer, exact size edge,
  invalid encoding, secret scanner verdict; skipped files yield no occurrences.

## T03-03 — Exact source_blob/file_revision reuse

- **Результат:** content hash, encoding/newline detection, optional zstd, exact byte round-trip,
  create-or-reuse by `(content_hash, parser_fingerprint)`.
- **Тесты:** LF/CRLF/mixed and non-ASCII bytes round-trip; compression; same key reuses row;
  same bytes/different parser fingerprint separates; live-file mutation does not affect stored.

## T03-04 — Normalized text cache regeneration

- **Результат:** versioned normalization derives cache text/blob identity from exact source;
  per-row cache regeneration and last-used batching seam.
- **Тесты:** deterministic normalization golden cases; cache delete→same reconstruction;
  invalid row/checksum path; no normalized text stored in path-bearing/canonical code rows.

## G03 — Сверка source-blob invariant

Перечитать spec 03 §2.3–2.4, 06 §2, 12 §2/§5. Property test:
`occurrence ⇒ generation_file ⇒ file_revision.source_blob`; для каждого skip reason обратное
отсутствует. Удалить cache и доказать восстановление normalized text. Schema audit обязателен.
