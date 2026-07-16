# Группа 13 — Spool и observations

Цель: stable-identity event не теряется после durable append. Ссылки: spec 03 §2.5;
07; 11 §3–4; 12 §2/§6; 14 §3.

## T13-01 — Redaction/caps before disk

- **Результат:** reusable scanner из T03-02 применяется до spool serialization; добавлены deny
  paths/tools и payload/excerpt caps preserving hash/original_size; excluded data becomes
  envelope-only.
- **Тесты:** credential/high-entropy patterns, false-positive fixtures, 256 KiB boundary,
  excluded event; instrumentation proves raw secret never reaches spool builder/remote sink.

## T13-02 — Segment writer и hook fail-open

- **Результат:** LRSP header/frame CRC, per-session sequence/8 MiB rotation, flock/LockFileEx,
  single O_APPEND write+fdatasync, 0700/0600; hook always exits 0 within budget telemetry.
- **Тесты:** golden wire bytes, concurrent append no interleave, rotation, permissions,
  malformed input/disk error fail-open, frame >1 MiB rejected internally.

## T13-03 — Frame reader и identity semantics

- **Результат:** bounded streaming decoder stops at torn tail; source_event_id computed once
  for all event types and stable-vs-best-effort dedup classification.
- **Тесты:** each identity table row, identical prompts remain best-effort, CRC/len/version/
  UTF-8 errors, torn tail, newer format incompatibility diagnostic.

## T13-04 — Transactional importer/cursor

- **Результат:** per-session batch tx writes envelope/path/payload/received_seq and advances
  cursor together; exact unique and bounded-window dedup; unknown root allowed NULL.
- **Тесты:** stable duplicates across segments, window boundaries 10 min/512, rollback before
  commit, restart after commit, monotone seq, concurrency, bytes deleted only ≤ cursor.

## T13-05 — Payload TTL и spool GC

- **Результат:** sweeper removes payload only, retains envelope/evidence; fully committed absent
  sessions eligible after 14d; startup catch-up scheduling seam.
- **Тесты:** fake clock before/at/after TTL, evidence survives, uncommitted segment retained,
  repeated sweep, envelope metrics.

## T13-06 — S1–S8 kill matrix

- **Результат:** named deterministic subprocess/failpoint suite for every spec row.
- **Тесты:** exact outcomes S1–S8; especially fdatasync-before-exit imports once and any daemon
  kill point loses no durable stable event.

## G13 — Сверка no-loss ingestion

Перечитать spec 07, 11 §3–4, 12 §2/§6, 14 §3. Run S1–S8 and concurrency/permission suite.
Проверить hooks never call daemon for ingestion, durable moment definition, exact/best-effort
distinction and spool version handshake seam.
