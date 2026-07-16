# Группа 01 — Миграции и SQLite foundation

Цель: безопасно открыть и эволюционировать два независимых хранилища. Ссылки: spec 02 §2,
§4–5; 03 §1.4, §2.1, §3–5; 13 §3–4.

## T01-01 — Store/config paths и permissions

- **Результат:** platform abstraction разрешает `LOCAL_RAG_HOME`, нормативный XDG/POSIX
  fallback и Windows dirs/endpoints; создаёт dirs 0700/files 0600 и проверяет owner UID.
- **Тесты:** precedence table, Unicode/space paths, wrong owner refusal (platform-gated),
  idempotent creation, endpoint path/pipe naming fixture.
- **Не в scope:** daemon lock/lifecycle (T15-01).

## T01-02 — State DB open policy и bounded writer

- **Результат:** state connections применяют WAL/FK/FULL/busy timeout; единственная bounded
  async write queue исполняет короткие транзакции и backpressure.
- **Тесты:** pragma assertions; FK rejection; rollback on closure error; queue saturation
  waits/cancels cleanly; concurrent producers serialize; direct production writer API absent.

## T01-03 — Forward-only migration runner

- **Результат:** numbered/checksummed migrations, compatibility check и migration lock;
  bootstrap schema_migrations/store_settings.
- **Тесты:** empty→latest, older→latest, checksum drift rejection, newer-store rejection,
  concurrent migrator exclusion, repeated open no-op.

## T01-04 — Resumable/destructive migrations

- **Результат:** progress checkpoints for optional Rust steps; destructive marker forces
  `VACUUM INTO` backup before mutation; documented restore seam.
- **Тесты:** crash after each checkpoint resumes exactly; backup opens and has pre-change
  schema/data; failed step leaves version unapplied; retry succeeds.

## T01-05 — Cache DB binding и recreation

- **Результат:** independent cache open applies NORMAL/no cross-DB FK, binds store UUID/schema,
  recreates incompatible cache atomically; separate bounded writer.
- **Тесты:** matching reopen preserves rows; UUID/schema mismatch rebuilds; state untouched;
  corrupt cache yields clean cache; source scan/lint rejects writable ATTACH path.

## G01 — Сверка migration/storage foundation

Перечитать spec 02 §2, §4–5; 03 §1.4, §3–5; 13 §3. Проверить pragmas, lock ordering seams,
cross-DB prohibition, compatibility errors, resumability и backups. Запустить migration matrix,
concurrency tests и schema lint. Зафиксировать PASS до G02.
