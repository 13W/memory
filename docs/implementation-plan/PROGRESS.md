# Прогресс имплементации

Статусы: `[ ]` не начато, `[~]` в работе, `[x]` завершено, `[!]` заблокировано. Одновременно
может быть `[~]` только одна задача, если карточка явно не разрешает независимую работу.
Gate следующей группы нельзя начинать до `PASS` предыдущего.

## 00 — Контракт разработки и baseline

- [x] T00-01 Импортировать v1 behavioral fixtures и зафиксировать baseline inventory
- [x] T00-02 Создать Rust workspace, quality commands и CI smoke
- [x] T00-03 Создать общий fixture/failpoint test harness
- [x] G00 Сверка foundations и testing contract

## 01 — Миграции и SQLite foundation

- [x] T01-01 Реализовать разрешение store/config путей и permissions
- [x] T01-02 Реализовать state DB open policy и bounded writer
- [x] T01-03 Реализовать forward-only migration runner
- [x] T01-04 Добавить resumable/destructive migration mechanics
- [x] T01-05 Реализовать cache DB binding и recreation
- [x] D-001 Устранить недетерминизм concurrent-migrator теста (WAL-init race)
- [x] G01 Сверка migration/storage foundation

## 02 — Реестр repository/worktree

- [x] T02-01 Реализовать canonical path и hash/ID primitives
- [x] T02-02 Добавить registry schema и repository operations
- [x] T02-03 Добавить stable worktree operations и path history
- [x] T02-04 Реализовать attach/move/non-git resolution
- [x] T02-05 Реализовать config merge и data-policy ordering
- [x] D-002 Сузить source-lint no_writable_cross_db_attach до SQL-ключевого слова
- [ ] G02 Сверка identity и registry

## 03 — Exact source и skip policy

- [ ] T03-01 Добавить code-storage DDL и repositories
- [ ] T03-02 Реализовать file classification и skip reasons
- [ ] T03-03 Реализовать exact source_blob/file_revision reuse
- [ ] T03-04 Реализовать normalized text cache regeneration
- [ ] G03 Сверка source-blob invariant

## 04 — Парсинг и path-independent units

- [ ] T04-01 Закрыть O4 и оформить ADR первого набора языков
- [ ] T04-02 Реализовать parser fingerprint и parser abstraction
- [ ] T04-03 Реализовать первый язык и parser fixtures
- [ ] T04-04 Реализовать второй язык и parser fixtures
- [ ] T04-05 Реализовать третий язык, если выбран ADR
- [ ] T04-06 Реализовать deterministic parsed-unit persistence
- [ ] G04 Сверка parsing identity

## 05 — Generations и strict reconcile

- [ ] T05-01 Реализовать generation/occurrence schema и state transitions
- [ ] T05-02 Реализовать authoritative tree scan и gitignore semantics
- [ ] T05-03 Реализовать generation builder и structural sharing
- [ ] T05-04 Реализовать reconcile scheduler/triggers
- [ ] T05-05 Реализовать retry/failure handling
- [ ] G05 Сверка reconcile и generation invariants

## 06 — Retention и GC

- [ ] T06-01 Реализовать pin-root calculation
- [ ] T06-02 Реализовать batched generation/source sweep
- [ ] T06-03 Реализовать orphan/quarantine/spool housekeeping
- [ ] G06 Сверка rebuildability и retention

## 07 — Fake projection и протокол переключения

- [ ] T07-01 Определить ProjectionStore contract и fake backend
- [ ] T07-02 Добавить two-axis projection state guards
- [ ] T07-03 Реализовать desired-set write-ahead switch
- [ ] T07-04 Реализовать validate-on-open и rebuild
- [ ] T07-05 Реализовать F1–F12 fault matrix
- [ ] G07 Сверка projection correctness

## 08 — FTS materialized view

- [ ] T08-01 Добавить FTS schema, preprocessing и manifest
- [ ] T08-02 Реализовать FTS build/delta и head-last commit
- [ ] T08-03 Реализовать validation, degradation и rebuild
- [ ] T08-04 Добавить FTS corruption/staleness tests
- [ ] G08 Сверка FTS consistency

## 09 — Locking и shard lifecycle

- [ ] T09-01 Реализовать lock hierarchy и per-worktree coordination
- [ ] T09-02 Реализовать ref-counted shard LRU
- [ ] T09-03 Реализовать snapshot/read-lock search skeleton
- [ ] T09-04 Проверить concurrent switches и generation mixing
- [ ] G09 Сверка concurrency model

## 10 — Выбор dense backend

- [ ] T10-01 Подготовить воспроизводимый spike harness и corpus
- [ ] T10-02 Адаптер и измерения brute-force backend
- [ ] T10-03 Адаптер и измерения usearch backend
- [ ] T10-04 Адаптер и измерения Qdrant Edge backend
- [ ] T10-05 Сравнить, выбрать backend и оформить ADR
- [ ] G10 Сверка отсутствия преждевременной backend coupling

## 11 — Embeddings и model spaces

- [ ] T11-01 Реализовать representation/model-space registry
- [ ] T11-02 Реализовать embedding cache integrity и eviction
- [ ] T11-03 Реализовать local embedder provider pool
- [ ] T11-04 Реализовать resumable coverage backfill
- [ ] T11-05 Реализовать per-worktree model switch
- [ ] T11-06 Реализовать model asset installer
- [ ] G11 Сверка model migration

## 12 — Hybrid code search

- [ ] T12-01 Реализовать lexical leg и фильтры
- [ ] T12-02 Интегрировать выбранный dense backend
- [ ] T12-03 Реализовать RRF и deterministic response
- [ ] T12-04 Реализовать source_blob snippets/context/overview
- [ ] T12-05 Запустить 49-query baseline/gates
- [ ] G12 Сверка search v0

## 13 — Spool и observations

- [ ] T13-01 Реализовать redaction/caps перед записью
- [ ] T13-02 Реализовать segment writer и hook fail-open
- [ ] T13-03 Реализовать frame reader и identity semantics
- [ ] T13-04 Реализовать transactional importer/cursor
- [ ] T13-05 Реализовать payload TTL и spool GC
- [ ] T13-06 Реализовать S1–S8 kill matrix
- [ ] G13 Сверка no-loss ingestion

## 14 — Durable memory

- [ ] T14-01 Добавить memory DDL и legal state transitions
- [ ] T14-02 Реализовать базовый transactional memory-op engine
- [ ] T14-03 Реализовать lifecycle/edit memory operations
- [ ] T14-04 Реализовать merge memory operation
- [ ] T14-05 Реализовать candidates и review operations
- [ ] T14-06 Реализовать consolidation lease/cursor runner
- [ ] T14-07 Реализовать local router и quality fixture set
- [ ] T14-08 Реализовать recall relevance и safe formatting
- [ ] G14 Сверка memory correctness/quality

## 15 — Daemon, protocol, MCP и CLI

- [ ] T15-01 Реализовать store lock и daemon lifecycle
- [ ] T15-02 Реализовать versioned proxy protocol/handshake
- [ ] T15-03 Реализовать MCP code query tools
- [ ] T15-04 Реализовать MCP status и memory read tools
- [ ] T15-05 Реализовать MCP memory/write tools
- [ ] T15-06 Реализовать hook recall RPC и additionalContext
- [ ] T15-07 Реализовать service/index/registry CLI
- [ ] T15-08 Реализовать memory/privacy/diagnostic CLI
- [ ] G15 Сверка interface contracts

## 16 — Security, privacy и recovery operations

- [ ] T16-01 Централизовать remote policy enforcement
- [ ] T16-02 Реализовать inspect/export/purge и audit tombstones
- [ ] T16-03 Реализовать doctor/rebuild/degraded diagnostics
- [ ] T16-04 Добавить adversarial and ownership suite
- [ ] G16 Сверка security/privacy

## 17 — Distribution и release gates

- [ ] T17-01 Создать npm launcher и platform packages
- [ ] T17-02 Создать Claude Code plugin registration
- [ ] T17-03 Настроить cargo-dist/zigbuild platform CI
- [ ] T17-04 Проверить upgrade/migration/offline install flows
- [ ] T17-05 Запустить полный acceptance/resource/latency suite
- [ ] G17 Финальная сверка v0

## Evidence

### Gate results

Заполняется только после выполнения gate: `PASS`, `PASS after D-NNN` или `BLOCKED`.

| Gate | Результат | Ссылка на evidence/report |
| --- | --- | --- |
| G00 | PASS | строка G00 в «Task evidence» + трейс «G00 — трейс требование → artifact/test» ниже |
| G01 | PASS | строка G01 в «Task evidence» + трейс «G01 — трейс требование → artifact/test» ниже |
| G02 | — | — |
| G03 | — | — |
| G04 | — | — |
| G05 | — | — |
| G06 | — | — |
| G07 | — | — |
| G08 | — | — |
| G09 | — | — |
| G10 | — | — |
| G11 | — | — |
| G12 | — | — |
| G13 | — | — |
| G14 | — | — |
| G15 | — | — |
| G16 | — | — |
| G17 | — | — |

### Task evidence

Добавляйте строки без удаления истории:

| ID | Commit/PR | Проверки | Результат/артефакт | Исполнитель/дата |
| --- | --- | --- | --- | --- |
| T02-05 | коммит `T02-05: config merge + data-policy ordering` (строка evidence в том же коммите) | `cargo test -p local-rag-core --lib config` OK (13: `data_policy_string_round_trips_and_rejects_bogus` — round-trip всех 4 вариантов + `bogus`/`LOCAL_ONLY`/`""` → None; `data_policy_default_is_local_only`; `most_restrictive_covers_every_pair` — все 16 пар = strictest-of-pair + коммутативность + идемпотентность на диагонали + spot-check spec-порядка; `default_matches_spec_toml` — `parse_toml(SPEC §3.1 verbatim) == Config::default()` (страж дрейфа дефолтов); `empty_toml_is_all_defaults`; `partial_toml_defaults_missing_keys_and_sections`; `unknown_keys_are_ignored` (ленивость — незнакомые ключ/секция игнорируются); `unsupported_schema_version_is_rejected` → `UnsupportedSchemaVersion{2,1}`; `invalid_data_policy_is_rejected_not_defaulted` → `InvalidDataPolicy{"yolo_remote"}` (без тихого дефолта); `malformed_toml_is_rejected` → `Toml`; `config_toml_path_joins_filename`); `cargo test -p local-rag-core --test config` OK (5: missing-file→defaults, missing-dir→defaults, present→parsed, present-invalid→typed `InvalidDataPolicy`, **`config_is_not_read_from_inside_a_repository`** — `config.toml`/`.local-rag.toml` подброшены в repo-каталог, `load(config_dir)` вернул defaults `local_only`, доказывая единственный вход `config_dir`); `cargo test -p local-rag-store --test settings` OK (11: generic round-trip; upsert-идемпотентность (2× запись ключа → 1 строка, latest wins); FK-отказ на unknown repo → `WriteError::Sqlite`, 0 строк; typed data_policy round-trip всех 4 + хранение под mirrored `data_policy`; unset→None; corrupt stored → `FromSqlConversionFailure(0,_,_)`; **every-pair** effective = `most_restrictive` (16 комбо через реальный StateDb); `repo_cannot_relax_global` (global=metadata_only_remote + repo=allow_remote_full → metadata_only_remote — end-to-end поведенческая проверка); repo-без-настройки→global; multi-repo tightening order-independent (forward==reverse); `repo_settings` listing ordered-by-key); регресс `--test registry` 11, `--test worktree` 12, `--test resolve` 11, `--test migrate` 8, `--test cache` 10, `--test state` 6, `--test migrate_resumable` 6 — все OK (SCHEMA_V1/V2 checksum не тронут); `cargo xtask ci` = all checks passed (fmt --check → clippy `-D warnings` → test --workspace → doc --no-deps → clippy+test `--features failpoints`); offline `CARGO_NET_OFFLINE=true cargo xtask ci` = passed; `RUSTDOCFLAGS="-D warnings" cargo doc -p local-rag-core -p local-rag-store --no-deps` clean; `Cargo.lock` источников 45→57 (+12: toml 0.8.23/toml_edit/toml_datetime/toml_write/serde_spanned/winnow/memchr/indexmap/equivalent + serde/serde_core/serde_derive 1.0.228; proc-macro2/quote/syn/unicode-ident/hashbrown переиспользованы из rusqlite-subtree, не новые); grep всех `Cargo.toml` на qdrant/usearch/ollama/onnx/candle/tree-sitter/reqwest/hyper/tonic = NONE (T10 держится) | Новый модуль `crates/core/src/config/mod.rs`: `DataPolicy` (4-вариантный enum без serde: `as_str`/`from_str_value`/`restrictiveness_rank`/`most_restrictive` (strictest-wins, коммут./ассоц.)/`Default=LocalOnly`); типизированный `Config` (+`DaemonConfig`/`StorageConfig`/`ModelsConfig`/`IndexConfig`, `Default` = дословные defaults 02 §3.1) с `parse_toml`/`load(<config_dir>)`/`config_toml_path`; приватные `RawConfig`/`RawModels` (serde `Deserialize` + `#[serde(default)]`, **без** `deny_unknown_fields` → ленивость) → валидация `from_raw` (schema_version==1 иначе `UnsupportedSchemaVersion`; parse data_policy иначе `InvalidDataPolicy`); typed `ConfigError`(`Io`/`Toml`/`UnsupportedSchemaVersion`/`InvalidDataPolicy`)+Display/Error. **«No repo-local lookup»** структурно: единственный вход — `config_dir` (нет API с worktree/repo root). Новый модуль `crates/store/src/registry/settings.rs` (зеркалит `repository.rs`: запись `&Transaction`, чтение `&Connection`): generic `set_repo_setting`(upsert `ON CONFLICT(repo_id,key) DO UPDATE`, FK-enforced)/`get_repo_setting`/`repo_settings`(ordered); typed `set_repo_data_policy`/`repo_data_policy` (ключ `DATA_POLICY_KEY="data_policy"`, corrupt → `FromSqlConversionFailure`); `effective_data_policy(global,conn,repo_ids)` — fold `most_restrictive` (детерминирован, repo только ужесточает). Ре-экспорты `core/lib.rs` (`Config`/`ConfigError`/`DataPolicy`), `registry/mod.rs`+`store/lib.rs`. Без новой миграции (generic `repo_settings` из SCHEMA_V1). Deps (утв. пользователем): `toml`+`serde(derive)` в `core/Cargo.toml`, 2 allowlist-строки в `CONTRIBUTING.md` (обоснование+транзитив+license MIT/Apache-2.0). `[SPEC]`-правки (тот же коммит): 02 §3.1 (validation policy: missing→defaults, version MUST=1, invalid enum→error, unknown keys ignored, `[OPEN]` числа provisional, single config path), 02 §3.2 (effective merge as-built, guard отложен T11/T16), 03 §2.1 (T02-05 as-built note + снята пометка «T02-05»). Guard в provider pool — вне scope (T11/T16). Инвариант 02 §3.2 «repo не ослабляет global» и 02 §6 «nothing degrades silently» соблюдены. Отклонений не обнаружено; `DEVIATIONS.md` без изменений | Claude Opus 4.8 / 2026-07-17 |
| T02-04 | коммит `T02-04: attach/move/non-git resolution` (строка evidence в том же коммите) | `cargo test -p local-rag-store --test resolve` OK (11: directory_move_preserves_repo_and_worktree_ids — те же repo/worktree id после detach→attach@/new, resolve@/new=Resolved, resolve@/old=GlobalOnly, 1 worktree/1 repo, state Active, /old в истории `is_current=0` обеих сторон; recreated_path_does_not_steal_identity — worktree переезжает /p→/p2 (Active), resolve@/p=GlobalOnly (fp-match отсеян т.к. Active), свежие R2/W2@/p → resolve@/p=Resolved{R2,W2}≠{R,W}, старый цел; linked_ambiguity_requires_id — 2 detached linked одного repo → Ambiguous[WL1,WL2] (sorted), repo-hint тоже Ambiguous, attach(WL1)→resolve@/moved=Resolved{R,WL1}; unknown_root_resolves_global_only — /never/seen и RequestRoot::default() → GlobalOnly; non_git_happy_path — NonGit resolve + move синкает repository_path; attach_unknown_worktree→Ok(Err(UnknownWorktree)) 0 строк; attach_repo_mismatch→Ok(Err(RepoMismatch)) без мутации; attach_removing_is_not_reattachable→Ok(Err(NotReattachable{Removing→Active})) state цел, путь не наблюдён; attach_is_idempotent_under_retry→1 current-row, first_seen цел/last_seen bumped, Active; repo_hint_selects_single_detached_main→hint=Some→Resolved, None→Ambiguous[W]; common_dir_fingerprint_alone_never_resolves→GlobalOnly); юнит-тесты `--lib` 5 (+2: summary_from_row_rejects_corrupt_enum — corrupt kind/state → `FromSqlConversionFailure(2/3,Text)`, summary_from_row_parses_valid_row); регресс `--test registry` 11 OK, `--test worktree` 12 OK, `--test cache` 10 OK (было 9 + D-002 regression), `--test migrate` 8 OK, `--test migrate_resumable` 6 OK, `--test state` 6 OK; `cargo xtask ci` = all checks passed (fmt --check → clippy `-D warnings` → test --workspace → doc --no-deps → clippy+test `--features failpoints`); offline `CARGO_NET_OFFLINE=true cargo xtask ci` = passed; `RUSTDOCFLAGS="-D warnings" cargo doc -p local-rag-store --no-deps` clean (исправлена redundant-explicit-link в `registry/mod.rs` → `[resolve()]`/`[attach()]`); `git diff --stat Cargo.lock` пуст (0 новых зависимостей); grep всех `Cargo.toml` на qdrant/usearch/ollama/onnx/candle/tree-sitter/reqwest/hyper/tonic = NONE (T10 держится) | Новый композиционный модуль `crates/store/src/registry/resolve.rs` над примитивами T02-02/03 (без новых таблиц/миграций): `WorktreeRootFacts` (canonical/display/path_fingerprint/kind + advisory `common_dir_fingerprint`/`remote_fingerprint` — факты подаёт демон, store без git/net), `RequestRoot{worktree_root?,repo_hint?}` (маппинг 02 §3.3, `session_id` routing-only), `Resolution{Resolved,GlobalOnly,Ambiguous{candidates}}`, `Candidate`, `AttachError{UnknownWorktree,RepoMismatch,NotReattachable}` (+Display/Error). `resolve(&Connection,&RequestRoot)` — авто-резолв ТОЛЬКО по current exact-path (`find_worktree_by_current_path`); иначе advisory detached-кандидаты (`detached_candidates`: fp-lookup + remote-hint, фильтр `state=Detached ∧ kind=facts.kind`, dedup `BTreeMap`), `repo_hint` выбирает ровно одного → Resolved, иначе Ambiguous; пусто → GlobalOnly. `attach(&Transaction,...)` — перепривязка existing identity: summary→(UnknownWorktree|RepoMismatch)→transition(→Active, Removing→NotReattachable до записи)→observe_worktree_path→(если stored kind≠Linked) observe_repository_path; `Result<Result<(),AttachError>>` (внешний Err=SQLite/rollback, внутренний=отказ без мутации). Новые read'ы в `worktree.rs`: `WorktreeSummary`, `find_worktree_by_current_path` (is_current=1, `ORDER BY worktree_id LIMIT 1`), `worktree_summary`, `worktrees_of_repo` (parse kind/state с `FromSqlConversionFailure`-fallback). Ре-экспорты `registry/mod.rs`+`lib.rs`. `[SPEC]`-ноты (тот же коммит): 03 §2.1 (resolver/attach композиция, 3 read'а, per-row uniqueness→детерминированный первый, auto-resolve только exact current, fingerprint/remote/common-dir advisory, кандидаты detached+kind, common-dir не хранится, git-пробинг у демона) и 02 §3.3 (маппинг RequestRoot, session_id routing-only, GlobalOnly не ошибка, repo_hint только tie-break, факты от демона/store без git). Найдено отклонение D-002 (source-lint false-positive на имени `attach`), исправлено до T02-04 done. Инвариант 01 §5 соблюдён (identity не выводится из пути; fingerprint advisory). Guardrail T10 держится (0 новых deps) | Claude Opus 4.8 / 2026-07-17 |
| D-002 | коммит `T02-04: attach/move/non-git resolution` (та же правка) | `cargo test -p local-rag-store --test cache` OK (10; было 9 + `source_lint_targets_sql_attach_not_the_rust_identifier`: positive `"ATTACH DATABASE …"`/`ATTACH 'file' AS aux`; negative `pub fn attach(`, `AttachError::…`, `NotReattachable(…)`); `no_writable_cross_db_attach` OK); `cargo xtask ci` = all checks passed | Source-lint `tests/cache.rs::no_writable_cross_db_attach` (T01-05, охрана 03 §1.4 «нет writable cross-DB ATTACH») матчил `line.to_ascii_lowercase().contains("attach")` → ложно срабатывал на введённую T02-04 spec-именованную операцию `repo attach` (04 §7): `pub fn attach`/`AttachError`/`NotReattachable`. Фикс: хелпер `contains_sql_attach(line)=line.contains("ATTACH")` (SQL в крейте — заглавными по строгой конвенции, ловит и многострочный `execute_batch`), Rust-идентификатор больше не матчится. Инвариант НЕ ослаблен (любой uppercase SQL `ATTACH` без маркера `// cross-db: read-only` по-прежнему flagged); добавлен regression-тест дискриминации. `DEVIATIONS.md` → resolved | Claude Opus 4.8 / 2026-07-17 |
| T02-03 | коммит `T02-03: stable worktree operations + path history` (строка evidence в том же коммите) | `cargo test -p local-rag-store --test worktree` OK (12: worktree_id≠fp и не выводится из fp (fp хранится только в `worktree_path.path_fingerprint`, `find_worktrees_by_path_fingerprint` возвращает UUID-lookup, display сохранён), `worktree` без path-производной колонки (ровно 7 §2.1-колонок, нет canonical_path/path/path_fingerprint), composite FK отвергает генерацию чужого worktree (`(g2,w1)` ∉ generation → `WriteError::Sqlite`, откат, `current_generation(w1)==g1`), observe single-current A→B→A, partial-unique index отвергает форс-2nd-current (A остаётся), path history retained across move (`is_current=0`, `first_seen_at` цел), observe идемпотентен под retry (без дубля, first_seen цел, last_seen/display bumped), detach→reattach retains id (Active→Detached→Active, id неизменен, обе истории пути живы), illegal transition Removing→Active → `Ok(Err(Illegal{Removing,Active}))` без мутации, unknown worktree → `Ok(Err(UnknownWorktree))`, create_worktree unknown repo → FK-отказ (0 строк), миграция v2 = ровно worktree-схема (3 таблицы + partial unique `worktree_path_current WHERE is_current = 1` + `worktree_path_fp` + 2-колоночный composite FK на `generation`)); юнит-тесты `--lib` 3 (kind/state round-trip, полная матрица `check_transition`: 4 legal + 3 self-noop + 2 illegal); регресс `--test registry` 11 OK (обновлён `migration_produces_exact_registry_schema`: набор `[(1,registry),(2,worktree)]` — усиление), `--test migrate` 8 OK (обновлён `state_db_open_bootstraps...`: applied 1→2, +проверка worktree-таблиц и строки `(2,worktree)` — усиление), `--test state` 6 OK, `--test cache` 9 OK, `--test migrate_resumable` 6 OK; `cargo xtask ci` = all checks passed (fmt --check → clippy `-D warnings` → test --workspace → doc --no-deps → clippy+test `--features failpoints`); offline `CARGO_NET_OFFLINE=true cargo xtask ci` = passed; `RUSTDOCFLAGS="-D warnings" cargo doc -p local-rag-store --no-deps` clean; `git diff Cargo.lock` пуст (0 новых зависимостей — только std + уже-разрешённые rusqlite/tokio); grep всех `Cargo.toml` на qdrant/usearch/ollama/onnx/candle/tree-sitter/reqwest/hyper/tonic = NONE (T10 держится) | Миграция v2 `worktree` (`registry::SCHEMA_V2`, byte-exact §2.1: `worktree`+composite FK / `worktree_path`+partial-unique `worktree_path_current`+`worktree_path_fp` / `generation`), добавлена в `migrate::ALL`. Циклический FK worktree↔generation в одной миграции (SQLite резолвит FK-родителей лениво; composite-цель валидна через `UNIQUE (generation_id, worktree_id)`). Новый модуль `crates/store/src/registry/worktree.rs`: `WorktreeKind{Main,Linked,NonGit}`, `WorktreeState{Active,Detached,Removing}` + чистая `check_transition` (04 §7; self-переходы — идемпотентный no-op), типы ошибок `IllegalWorktreeTransition{from,to}`/`WorktreeTransitionError{UnknownWorktree,Illegal(..)}` (Display+Error). Операции (write через `&Transaction`, read через `&Connection`, как T02-02): `create_worktree` (state='active', current_gen=NULL, FK на repo), `observe_worktree_path` (clear-then-set single-current в 3 statements + path_fingerprint/display, FK на worktree), `set_current_generation` (worktree-side seam; composite FK), `transition_worktree_state -> rusqlite::Result<Result<(),WorktreeTransitionError>>` (внешний Err = сбой SQLite/rollback, внутренний = доменный отказ без мутации; corrupt state → `FromSqlConversionFailure`); reads `worktree_state`/`current_worktree_path`/`current_generation`/`worktree_path_history`/`find_worktrees_by_path_fingerprint`. `registry/mod.rs`+`lib.rs` ре-экспорты. `worktree_id` caller-minted UUIDv7 (не path-derived, 01 §5); `path_fingerprint` — lookup-only (01 §5, 03 §2.1). `generation`-builder/occurrence/state-machine — group 05 (scope-guard). `[SPEC]`-нота 03 §2.1 (тот же коммит): as-built миграция v2, идемпотентные self-переходы, generation как FK seam. Инвариант 01 §5 соблюдён (нет path-производной identity/FK-цели). Отклонений не обнаружено; `DEVIATIONS.md` без изменений | Claude Opus 4.8 / 2026-07-17 |
| T02-02 | коммит `T02-02: registry schema + repository operations` (строка evidence в том же коммите) | `cargo test -p local-rag-store --test registry` OK (11: one-current across switches, partial-unique index rejects 2nd current (A survives), same-remote→2 repos (hint не unique), path history retained (`first_seen_at` intact, old row `is_current=0`), observe idempotent under retry (`first_seen_at` preserved/`last_seen_at` bumped), FK rejects observe on unknown repo (0 rows), stored fingerprint = 64-hex BLAKE3 (не raw/normalized URL), NULL fingerprint allowed, migration schema exact (3 таблицы + UNIQUE partial index `WHERE is_current = 1` + ровно 1 row `(1,"registry")`), `repository` без `canonical_path` (ровно 4 §2.1-колонки), `find_by_path` матчит только current (после move старый путь→None, но в history остаётся)); регресс `--test migrate` 8 OK (обновлён `state_db_open_bootstraps_and_is_idempotent`: `applied` 0→1, +проверка registry-таблиц и row `(1,"registry")` — усиление, не ослабление), `--test migrate_resumable` 6 OK, `--test state` 6 OK, `--test cache` 9 OK; `cargo xtask ci` = all checks passed (fmt --check → clippy `-D warnings` → test --workspace → doc --no-deps → clippy+test `--features failpoints`); offline `CARGO_NET_OFFLINE=true cargo xtask ci` = passed; `RUSTDOCFLAGS="-D warnings" cargo doc -p local-rag-store -p local-rag-core --no-deps` clean; аудит `Cargo.lock` = 45 внешних источников, `git diff Cargo.lock` пуст (0 новых зависимостей vs T02-01 baseline); grep всех `Cargo.toml` на qdrant/usearch/ollama/onnx/candle/tree-sitter/reqwest/hyper/tonic = NONE (T10 держится) | Новый модуль `crates/store/src/registry/{mod,repository.rs}`. `mod.rs`: `pub(crate) const SCHEMA_V1` — byte-exact DDL §2.1 (`repository`/`repository_path`+partial unique index `repository_path_current`/`repo_settings`), ре-экспорт операций. `repository.rs`: свободные функции — записи через `&Transaction` (композируются в одной `StateWriter::transaction`), чтения через `&Connection`, все возвращают `rusqlite::Result`. `create_repository(tx, repo_id, git_remote_fingerprint: Option, now)`; `observe_repository_path(tx, repo_id, path, now)` — clear-then-set в 3 statements (SQLite без deferred UNIQUE), upsert по PK `(repo_id, observed_path)` (new → `first_seen=last_seen=now`; existing → `first_seen` сохранён, `last_seen` bumped), unknown repo отвергается FK; `find_repository_by_path` (только current), `find_repositories_by_remote` (МНОГО — hint), `current_path`, `path_history` (+ тип `PathObservation`). `migrate/mod.rs`: `ALL = &[Migration::sql(1, "registry", registry::SCHEMA_V1)]` (+doc). `lib.rs`: `pub mod registry` + ре-экспорты. repo_id минтится вызывающей стороной (UUIDv7 вне транзакции; closure `Send+'static`); типизированный `RegistryError` не вводился (семантика resolution/ambiguity — T02-04, `WriteError::Sqlite(ConstraintViolation)` покрывает unknown-repo). `[SPEC]`-уточнение (тот же коммит): 03 §2.1 as-built-нота (миграция v1 `registry`; single-current-path через clear-then-set; repo_id caller-minted/не path-derived; fingerprint — nullable non-unique hint; history retained). Инвариант 01 §5 соблюдён (`repository` без `canonical_path`; identity не выводится из пути). Отклонений не обнаружено; `DEVIATIONS.md` без изменений | Claude Opus 4.8 / 2026-07-17 |
| T02-01 | коммит `T02-01: canonical path + hash/ID primitives (identity module)` (строка evidence в том же коммите) | `cargo test -p local-rag-core` OK (53 unit + 4 doctests; +31 identity: blake3 reference KAT len 0/1/3/1024 (multi-chunk tree), `encode` byte-exact, 12-domain goldens + distinctness, field-boundary collision (`["ab","c"]≠["a","bc"]`), retry-determinism, typed fingerprints; uuidv7 golden layout / version=7+variant=0b10 stamp / 48-бит ts round-trip+truncate / ordering / shape / system source distinct; path relative dot+sep+NFC+fold+display-preserve, absolute drive-upcase/UNC/verbatim-strip/NFC/fold-keeps-drive, symlink resolve via `TempHome`; remote SSH/HTTPS equivalence + creds-strip + host-lowercase + nested + distinct); `cargo xtask ci` = all checks passed (fmt --check → clippy `-D warnings` → test --workspace → doc --no-deps → clippy+test `--features failpoints`); offline `CARGO_NET_OFFLINE=true cargo xtask ci` = passed; `RUSTDOCFLAGS="-D warnings" cargo doc -p local-rag-core --no-deps` clean; аудит `Cargo.lock` = 45 внешних источников (+9 vs 36: native-link blake3+arrayref+arrayvec+constant_time_eq, unicode-normalization+tinyvec+tinyvec_macros, casefold; `cpufeatures` — phantom, gated out `pure`, не в `cargo tree -e normal` core); grep всех `Cargo.toml` на qdrant/usearch/ollama/onnx/candle/tree-sitter/reqwest/net = NONE (T10 держится) | Новый модуль `crates/core/src/identity/{mod,domain,uuidv7,path,remote}.rs`. `domain`: `HASH_SCHEMA_VERSION=1`, `Domain` (12 доменов §1.2, subject-слуги с `/`), `encode` (`utf8(domain)‖0x00‖Σ(le_u32(len)‖f)`) + `hash` (BLAKE3 64-hex), типизированные `path_fingerprint`/`remote_fingerprint`. `uuidv7`: чистый `uuidv7_from(now_ms,[u8;10])→Uuid` (RFC 9562: 48-бит BE ts, version 7, variant 0b10), `Display` 8-4-4-4-12, трейт `UuidSource` + `SystemUuidV7` (unix: `SystemTime` + `/dev/urandom`). `path`: `CaseSensitivity`, `Canonical{canonical,display}`, `normalize_relative`, чистая `normalize_absolute_str` (verbatim/UNC/drive-upcase/NFC/fold), FS-`canonicalize_absolute` (symlink). `remote`: `normalize_remote_url` (каноник `host/path`; SSH/HTTPS/git/https эквивалентность; срез scheme/credentials/port/`.git`/слэшей; host lowercase, path-case сохранён) + `fingerprint`. Deps (утв. пользователем): `blake3`(pure), `unicode-normalization`, `casefold`(github/rust-gems) — 3 строки allowlist в `CONTRIBUTING.md` (обоснование+license+транзитив). `[SPEC]`-уточнения (as-built, тот же коммит): §1.2 field-encoding-конвенции (text→utf8, id→lowercase-hex-ascii, int→le fixed-width) и типизированы лишь 2 lookup-домена (остальные — через generic `hash` в задачах-владельцах, без premature coupling); §1.3 simple case-fold через `casefold::simple_fold` (порядок NFC→fold), FS-чувствительность подаёт вызывающий. Инвариант 01 §5 соблюдён: `worktree_id` НЕ выводится из пути (identity = UUIDv7; path — только `path_fingerprint` lookup). Отклонений не обнаружено; `DEVIATIONS.md` без изменений | Claude Opus 4.8 / 2026-07-17 |
| T00-01 | не коммичено | `python3 fixtures/tooling/validate.py` exit 0 (stdlib); тот же скрипт под venv `jsonschema==4.23.0` exit 0; негативный self-test PASS (denylist/schema/49-count/dup-id); live v1 бенчмарк без ошибок | `fixtures/` — 6 семейств, 114 уникальных id; `search/corpus.json` = 49 запросов; baseline снят (embeddinggemma:300m, code-only): MRR 0.696, Hit@1 0.592, Hit@3 0.796, Hit@5 0.837, 544 чанка; gap-register GAP-01..06; пороги = TBD | Claude Opus 4.8 / 2026-07-16 |
| T00-02 | коммит `T00-02: scaffold Rust workspace + xtask quality gate + CI` (хэш в git-логе; строка edet в том же коммите) | `cargo build --workspace` OK; каждый бинарник `version` → `<имя> 0.0.0`, неизвестная команда exit 2; `cargo xtask ci` (fmt --check → clippy `-D warnings` → test --workspace → doc --no-deps) = all checks passed; offline `CARGO_NET_OFFLINE=true cargo xtask ci` = passed; 12 значимых тестов (3×2 CLI-smoke, 2 core unit, 2 core doctests, 2 xtask ci_config) | Workspace: 6 либ (`core/store/index/projection/memory/protocol`) + 3 продуктовых бинарника (`local-rag`, `local-rag-proxy`, `local-rag-hook`) + dev-only `xtask`; toolchain pin 1.96.1 (`rust-toolchain.toml`), edition 2024, MSRV 1.96; `CONTRIBUTING.md` (единая команда `cargo xtask ci` + dependency policy); `.github/workflows/ci.yml` (ubuntu-latest); `Cargo.lock` без внешних зависимостей (0 registry sources) | Claude Opus 4.8 / 2026-07-16 |
| T00-03 | коммит `T00-03: shared fixture/failpoint test harness` (хэш в git-логе; строка evidence в том же коммите) | `cargo test -p local-rag-test-support` OK (8 integration + 5 unit + 6 doctests); `cargo test -p local-rag --test harness_smoke` OK; `cargo xtask ci` = all checks passed; offline `CARGO_NET_OFFLINE=true cargo xtask ci` = passed; `cargo doc` без warnings; ручная проверка bundle: `status.txt=signal: 6 (SIGABRT)` + command/stdout/stderr сохранены вне temp home, temp homes подчищены на Drop | Dev-only crate `crates/test-support` (`local-rag-test-support`, в `members`, не в `default-members`): `TempHome` (temp `LOCAL_RAG_HOME` под `env::temp_dir()`, RAII-cleanup, `.command()` ставит env только в дочерний процесс), `Clock`/`FixedClock`/`ManualClock`, `IdSource`/`SeqUuids`, `subprocess::run_capturing` + artifact bundle, `Failpoints` + `fail_point!` (registry-strict: неизвестное имя → `FailpointError::Unknown`; `Action::Abort` = crash-точка F/S-матриц), `fixtures` (root/read, std-only). Приёмка: smoke в `local-rag` (dev-dependency); `$HOME` не читается (страж-тест). `Cargo.lock` по-прежнему 0 внешних источников. GAP-06 механизм закрыт; строки F1–F12/S1–S8 остаются за T07-05/T13-06 | Claude Opus 4.8 / 2026-07-16 |
| T01-02 | коммит `T01-02: state.sqlite open policy + bounded writer` (строка evidence в том же коммите) | `cargo test -p local-rag-store` OK (6 integration: pragma-assertions journal_mode=wal/foreign_keys=1/synchronous=2(FULL)/busy_timeout=5000, FK rejection+rollback, closure-error rollback + idempotent retry, 64 concurrent producers serialize, queue-saturation waits+cancel-clean, read-only conn rejects write); `cargo clippy -p local-rag-store --all-targets -- -D warnings` clean; `RUSTDOCFLAGS="-D warnings" cargo doc -p local-rag-store --no-deps` clean; `cargo xtask ci` = all checks passed; offline `CARGO_NET_OFFLINE=true cargo xtask ci` = passed; аудит `Cargo.lock` = 36 внешних источников (rusqlite 0.40.1 + libsqlite3-sys 0.38.1 + cc + tokio 1.52.4 + транзитивные; wasm-*/sqlite-wasm-rs target-gated); grep всех `Cargo.toml` на dense/model/tree-sitter/net SDK = NONE (T10-инвариант держится) | Крейт `local-rag-store`: `crates/store/src/state/{mod,open,writer}.rs`. Open policy (`open_state_rw`/`open_state_read_only`) применяет 03 §2 pragmas (WAL set-and-verify; FK/synchronous=FULL/busy_timeout=5000 per-connection). `StateDb::open[_with_capacity]` спавнит единственный writer-поток, владеющий одним write-`Connection` (`blocking_recv`); публично — только `StateWriter::transaction` (bounded `tokio::mpsc`→`oneshot`, backpressure + чистая отмена, неявная BEGIN/COMMIT с rollback на Err/drop) и read-only `open_read`; writable `Connection` наружу не выходит (спец 02 §5). Метрики очереди `queue_capacity`/`available_slots` (02 §5). Deps добавлены в `CONTRIBUTING.md` allowlist (rusqlite+bundled, tokio lib-фича только `sync`). `[FIXED]`/`[SPEC]` 02 §5, 03 §2/§3 соблюдены; отклонений нет, `DEVIATIONS.md` без изменений | Claude Opus 4.8 / 2026-07-16 |
| T01-01 | коммит `T01-01: store/config path resolution + permissions` (строка evidence в том же коммите) | `cargo test -p local-rag-core` OK (21 unit + 4 doctests: sha256 NIST-KAT, precedence table data_dir/config_dir, unicode/space + non-UTF-8 paths, wrong-owner refusal `/`, idempotent 0700-tree + 0600-file, pipe-name fixture, endpoint socket); `cargo xtask ci` = all checks passed; offline `CARGO_NET_OFFLINE=true cargo xtask ci` = passed; аудит `Cargo.lock` = ровно 1 внешний источник (`libc 0.2.186`, unix-target); grep Cargo.toml по dense/model/tree-sitter/sqlite/net SDK = NONE | Модуль `crates/core/src/paths/{mod,perms,hash}.rs`: `Env`/`SystemEnv` (инъекция env), `data_dir`/`config_dir` (02 §2.1 precedence, XDG empty=unset/relative-ignored), `StoreLayout` (все пути layout 02 §2 + идемпотентный `ensure()` 0700/owner-verify), `Endpoint`/`socket_path`/`pipe_name` (vendored std-only sha256), `perms` (0700 dirs/0600 files, symlink-swap reject, owner-verify до chmod; `geteuid` через `libc` в scoped `#[allow(unsafe_code)]`). `[SPEC]` amendment 02 §2.1: `LOCAL_RAG_HOME` override распространён на `config_dir=$LOCAL_RAG_HOME/config`; `CONTRIBUTING.md` — allowlist `libc`. Windows dirs/pipe реализованы под `cfg`, SID-lookup отложен (не в CI). Отклонений нет; `DEVIATIONS.md` без изменений | Claude Opus 4.8 / 2026-07-16 |
| T01-03 | коммит `T01-03: forward-only migration runner` (строка evidence в том же коммите) | `cargo test -p local-rag-store --test migrate` OK (7 тестов: empty→latest, older→latest, checksum-drift reject, newer-store reject, concurrent-migrator exclusion, repeated no-op, `StateDb::open` bootstrap+идемпотентность); `--test state` OK (6, регресс не сломан bootstrap'ом); `cargo test -p local-rag-core` OK (21 unit + 4 doctests — SHA-256 promotion не сломала `paths`); `cargo xtask ci` = all checks passed; offline `CARGO_NET_OFFLINE=true cargo xtask ci` = passed; `RUSTDOCFLAGS="-D warnings" cargo doc -p local-rag-store -p local-rag-core --no-deps` clean; аудит `Cargo.lock` = 36 внешних источников (без изменений с T01-02 → новых зависимостей НЕ добавлено, всё на std); grep всех `Cargo.toml` на dense/model/tree-sitter/net SDK = NONE (T10-инвариант держится) | Крейт `local-rag-store`: `migrate/{mod,lock}.rs` + `clock.rs`. `migrate::run(conn,&[Migration],lock_path,now_ms)` — под L1 (`migration.lock`, `std::fs::File::lock`, RAII), bootstrap `schema_migrations`/`store_settings` (`IF NOT EXISTS`, точный DDL 03 §2.1), compat-check (`IncompatibleStore`→`INCOMPATIBLE_STORE`), checksum-drift reject, apply-loop по одной tx на миграцию (шов для T01-04 resume). `Migration{version,name,sql}`+`checksum()` (SHA-256 текста SQL). `MigrationReport{applied,store_version}`; типизированный `MigrationError`. `pub const ALL: &[Migration] = &[]` (первая реальная миграция — T02-02). `StateDb::open` прогоняет `migrate::run` между `open_state_rw` и спавном writer (02 §4.1). `OpenError += Migration(Box<..>)`. Core: vendored SHA-256 поднят в `local_rag_core::hash::sha256_hex` (был `pub(crate)` в `paths/hash.rs`); добавлен `StoreLayout::migration_lock()`. `store_instance_uuid` НЕ сеется (defer в T01-05: UUIDv7-генератор — T02-01). `[SPEC]`-уточнения (as-built precision, тот же коммит): 02 §2 layout += `migration.lock`; 02 §6 нота — `ChecksumDrift`/rewritten history мапится под `INCOMPATIBLE_STORE` (protocol-mapping — T15). Clock seam — параметр `now_ms:i64` + `clock::system_now_ms()` (без зависимости от test-support). Отклонений нет; `DEVIATIONS.md` без изменений | Claude Opus 4.8 / 2026-07-16 |
| T01-04 | коммит `T01-04: resumable/destructive migration mechanics` (строка evidence в том же коммите) | `cargo test -p local-rag-store --test migrate_resumable` OK (6 default: resume-after-each-checkpoint, failed-step/retry, backup-pre-change, backup-idempotent-on-resume, destructive-sql-only, finalize-pending-resume); `--features failpoints` → 7 (та же 6 + `resumable_hard_kill_via_sigabrt`: реальный SIGABRT-child через re-exec, parent резюмит); регресс `--test migrate` 7 OK, `--test state` 6 OK; `cargo test -p local-rag-core` 21 unit + 4 doctests OK; `cargo xtask ci` = all checks passed (fmt → clippy `-D warnings` → test --workspace → doc → clippy+test `--features failpoints`); offline `CARGO_NET_OFFLINE=true cargo xtask ci` = passed; `RUSTDOCFLAGS="-D warnings" cargo doc -p local-rag-store -p local-rag-core --no-deps` (и `--features failpoints`) clean; аудит `Cargo.lock` = 36 внешних источников (git diff vs HEAD = 0 новых; `failpoints` тянет optional path-dep `test-support`, 0 внешних); grep всех `Cargo.toml` на qdrant/usearch/ollama/onnx/candle/tree-sitter/net = NONE (T10 держится) | Крейт `local-rag-store` `migrate/mod.rs`: `Migration` += `destructive: bool` и `steps: &[MigrationStep]` (+ `const fn sql/destructive/with_steps`, `StepFn`); `checksum()` над SQL (drift-тест T01-03 не тронут). Bootstrap += `migration_progress(version,seq,label,done_at)` (03 §2.1). `run()`: простые миграции — атомарный путь T01-03; сложные (destructive/steps) — `apply_complex` пер-юнитовыми чекпойнтами `[backup?][sql?][steps…]`, финализация одной tx (insert `schema_migrations` + очистка прогресса). Backup: `VACUUM INTO <root>/backups/state-<version>-<now_ms>.sqlite` до мутации (dir 0700, файл 0600), progress backup — отдельной tx (VACUUM вне транзакции); resume пропускает закоммиченные юниты. `MigrationError` += `Backup`/`BackupPath`. Restore-шов задокументирован в docstring. Feature-gated seam `#[cfg(feature="failpoints")] fail_point!("migrate:after_backup")` для hard-kill теста; `Cargo.toml` store += optional `test-support` + фича `failpoints`; `xtask ci` += 2 шага под фичей; `CONTRIBUTING.md` обновлён. Core: `StoreLayout::backups_dir()` + создание в `ensure()` (0700). `[SPEC]`-уточнения (тот же коммит): 02 §2 дерево += `backups/`; 03 §2.1 += `migration_progress`. Найдено отклонение D-001 (флейк concurrent-теста), исправлено до T01-04 done | Claude Opus 4.8 / 2026-07-17 |
| T01-05 | коммит `T01-05: cache.sqlite binding + recreation + bounded writer` (строка evidence в том же коммите) | `cargo test -p local-rag-store --test cache` OK (9: pragmas WAL/FK=0/synchronous=1(NORMAL)/busy=5000, matching-reopen-preserves-rows→Reused, uuid-mismatch→Recreated, schema-version-999→Recreated, corrupt-file→Recreated+usable, state-untouched (sha256 state.sqlite до/после rebuild равен + строка store_settings читается), writer-backpressure-cancels-cleanly, recreate-idempotent-on-retry, source-lint no-writable-ATTACH); `--features failpoints` → 10 (+ `cache_recreate_hard_kill_resumes`: реальный SIGABRT-child в `cache:after_delete` между delete и seed, parent резюмит → Recreated bound to B); регресс `--test state` 6 OK, `--test migrate` 7 OK, `--test migrate_resumable` 6/7 OK; `cargo xtask ci` = all checks passed (fmt → clippy `-D warnings` → test --workspace → doc → clippy+test `--features failpoints`); offline `CARGO_NET_OFFLINE=true cargo xtask ci` = passed; `RUSTDOCFLAGS="-D warnings" cargo doc -p local-rag-store --no-deps` clean; аудит `Cargo.lock` = 36 внешних источников (git diff vs HEAD = 0 новых — только std + уже-разрешённые rusqlite/tokio); grep всех `Cargo.toml` на qdrant/usearch/ollama/onnx/candle/tree-sitter/net = NONE (T10 держится) | Крейт `local-rag-store` новый модуль `cache/{mod,open,writer}.rs` (зеркало `state/`): `CacheDb::open[_with_capacity](path, store_instance_uuid, [cap])`. `open_cache_rw`/`open_cache_read_only` применяют cache-pragmas (03 §4: WAL set-and-verify, FK=OFF, synchronous=NORMAL, busy=5000; отличие от state — FK/synchronous). `open_and_bind`: `inspect_existing` (open RW → read `cache_meta`) → валиден+совпал uuid/`CACHE_SCHEMA_VERSION`(=1) ⇒ `Reused`; иначе (нет файла / mismatch / порча-NOTADB / unreadable) ⇒ `recreate` (unlink `cache.sqlite`/`-wal`/`-shm`, ignore NotFound) → fresh `open_cache_rw` → `seed_binding` одной tx (`cache_meta` DDL + rows uuid/version/created_at) ⇒ `Created`|`Recreated`. Порядок open→validate/recreate→serve (writer получает уже связанный conn). Отдельный `CacheWriter` (02 §5 L4b): выделенный поток `local-rag-cache-writer`, bounded mpsc→oneshot, `transaction`/`queue_capacity`/`available_slots`, writable conn наружу не выходит. Cross-DB (03 §1.4): без ATTACH; source-lint тест охраняет инвариант. Feature-gated seam `#[cfg(feature="failpoints")] fail_point!("cache:after_delete")`; `Cargo.toml`/`xtask` не менялись (фича уже есть). `lib.rs` += `mod cache` + re-exports (`CACHE_SCHEMA_VERSION`,`CacheDb`,`CacheOpenError`,`CacheOpenOutcome`,`CacheWriteError`,`CacheWriter`). Scope-guard: payload-таблицы (`embedding_cache`/`normalized_text_cache`/`fts_*`) НЕ создаются (T03/T08/T11); §4.4 шаги 3–4 (FTS/per-row checksum) — лениво позже. Решение (утверждено пользователем): `store_instance_uuid` подаётся вызывающей стороной, посев в state отложен до T02-01 (UUIDv7)+T15-01 (daemon startup) — не deviation (ни один `[FIXED]`/`[SPEC]` не нарушен, карточка посев не требует). `[SPEC]`-уточнение (тот же коммит): 03 §4.4 as-built-нота (delete-and-recreate + идемпотентная сходимость; `cache_schema_version`=1; uuid — от caller). Отклонений не обнаружено; `DEVIATIONS.md` без изменений | Claude Opus 4.8 / 2026-07-17 |
| D-001 | коммит `T01-04: resumable/destructive migration mechanics` (та же правка) | `migrate.rs` concurrent-тест: 40/40 прогонов зелёные (`--exact concurrent_migrators_apply_exactly_once --test-threads=1`), до фикса ~4/5 виснул на `Barrier::wait` или паниковал в `raw_conn:51` (`enable WAL`); полный `cargo xtask ci` не виснет | Недетерминизм T01-03-теста: два worker-потока гонялись за однократным `PRAGMA journal_mode=WAL` на свежей БД (эксклюзивная перезапись заголовка → `SQLITE_BUSY`), проигравший паниковал до `Barrier::wait(2)` → deadlock второго worker'а; обнажено добавленной T01-04 параллельной нагрузкой (`migrate_resumable`). Фикс: пред-инициализация БД в WAL в главном потоке до spawn (`drop(raw_conn(&state_path))`), открытие уже-WAL БД идемпотентно → остаётся только целевая гонка за migration lock (L1). Ассерт не ослаблен. Продуктовый flock-код корректен. `DEVIATIONS.md` → resolved | Claude Opus 4.8 / 2026-07-17 |
| G01 | этот коммит (gate-hardening тесты + docs + `PROGRESS.md` в одной правке) | узкие тесты группы 01: `cargo test -p local-rag-core` OK (22 unit + 4 doctests; +`ensure_rejects_symlink_swap`, +pinned pipe_name KAT, +config_dir empty/relative); `-p local-rag-store --test state` 6 OK, `--test migrate` 8 OK (+`malformed_set_is_rejected`), `--test migrate_resumable` 6 OK, `--test cache` 9 OK; под `--features failpoints` → `--test cache` 10 (`cache_recreate_hard_kill_resumes`), `--test migrate_resumable` 7 (`resumable_hard_kill_via_sigabrt` — реальный SIGABRT-child); `cargo xtask ci` = all checks passed (fmt --check → clippy `-D warnings` → test --workspace → doc --no-deps → clippy+test `--features failpoints`); offline `CARGO_NET_OFFLINE=true cargo xtask ci` = passed; `RUSTDOCFLAGS="-D warnings" cargo doc -p local-rag-store -p local-rag-core --no-deps` clean; аудит `Cargo.lock` = 36 `source=` (все crates.io), grep всех `Cargo.toml` на dense/model/tree-sitter/сеть = 0 | Сверка G01 (перечитаны spec 02 §2/§2.1/§4–5; 03 §1.4/§2.1/§3/§4/§5; 13 §3): перестроен трейс «требование → код → тест» (блок ниже), расхождений код↔спека НЕ обнаружено. Все нормативные требования группы (layout+directory resolution, state/cache pragmas, bounded single-writer L4a/L4b, L1 migration lock, cross-DB запрет, framework bootstrap DDL, migration runner/compat/drift/resumable/backup, `StateDb::open` order) = as-built + verified. Backend-coupling guardrail держится: 3 allowlisted прямых dep (`libc`/`rusqlite bundled`/`tokio sync`), 0 dense/model/tree-sitter/network. Отложено by-design (seam на месте, не преждевременная реализация): wire-code mapping `INCOMPATIBLE_STORE`/`MIGRATION_IN_PROGRESS` → T15; посев `store_instance_uuid` → T02-01/T15; Windows SID → T17; batching/checkpoint/VACUUM-by-metrics → позже; v1→v2 migration `[OPEN]` не закрыт молча. Gate-hardening (тесты + docs; не features, не D-NNN): закрыты дешёвые пробелы покрытия защитного кода — `MalformedSet`, config_dir empty/relative, реальный symlink-swap reject, pinned pipe_name KAT, синхронизирован allowlist транзитивных deps в `CONTRIBUTING.md`. Отклонений не обнаружено; `DEVIATIONS.md` без изменений | Claude Opus 4.8 / 2026-07-17 |
| G00 | этот коммит (строка evidence в том же коммите; изменён только `PROGRESS.md`) | `python3 fixtures/tooling/validate.py` exit 0 (built-in subset validator; 6 семейств, 114 уникальных id, 20 matrix-строк, no-backend-keys); `cargo xtask ci` = all checks passed; offline `CARGO_NET_OFFLINE=true cargo xtask ci` = passed (fmt --check → clippy `-D warnings` → test --workspace → doc --no-deps; прогнаны все group-тесты: test-support 7 unit + 8 integration + 6 doctests, `harness_smoke`, три пары `cli_version`, `xtask ci_config`, core doctests); аудит `Cargo.lock` = 0 строк `source=`/`checksum=`, 11 пакетов; grep всех `Cargo.toml` по dense/model/tree-sitter/sqlite/сеть = NONE; 10/10 baseline-порогов = `"TBD"`; manifest families = ровно 6, gaps = GAP-01..06 (все `blocking:false`) | Сверка G00: перечитаны spec 01/14/15, каждый маркер `[FIXED]`/`[SPEC]`/`[OPEN]` сопоставлен с as-built (трейс ниже). Fixture families 1–6 учтены (spec 14 §1), deferred scope в workspace отсутствует (spec 15 §3, 01 §2 O1), gaps GAP-01..06 видимы в `manifest.json`. Ни один `[OPEN]` (O1 backend, O2 пороги, O4 языки, O8 split) не закрыт молча. Отклонений не обнаружено → `DEVIATIONS.md` без изменений, D-NNN не заводился. `jsonschema==4.23.0` вариант validate.py — сетезависим (в системе не установлен), авторитетно подтверждён ещё в T00-01; для G00 сетенезависимым путём выступает built-in валидатор | Claude Opus 4.8 / 2026-07-16 |

### G00 — трейс требование → artifact/test

Дата 2026-07-16, исполнитель Claude Opus 4.8. Все команды воспроизводимы из README-плана,
раздел «Проверка». `отложено` = требование нормативно, но реализуется позже по плану; на G00
проверено лишь отсутствие преждевременного нарушения/coupling.

Spec 01 — Overview:

| Требование (маркер) | Artifact / Test | Статус на G00 |
| --- | --- | --- |
| Rust impl `[FIXED]` 01§1 | Rust workspace `Cargo.toml` (9 default-members, edition 2024, MSRV 1.96) | as-built |
| npm distribution `[FIXED]` 01§1 | packaging → T17; `manifest.json → deferred[]` фиксирует границы | отложено, не нарушено |
| No mandatory external daemons `[FIXED]` 01§1 | `Cargo.lock` 0 внешних источников; нет qdrant/ollama SDK | as-built |
| Claude Code only harness `[FIXED]` 01§2 | нет multi-harness кода; multi-harness в `deferred[]` | as-built |
| Spool-only hook ingestion `[FIXED]` 01§2 | бинарник `local-rag-hook` (seam) → логика G13 | отложено, seam есть |
| Platform targets; win32-arm64/FreeBSD deferred `[FIXED]` 01§2 | CI single host (T17 добавит остальные); `deferred[]` | as-built |
| Dense backend выбирается на step 11 `[OPEN]` 01§2 / O1 | нет backend-крейтов (`Cargo.lock`, grep Cargo.toml); только план `ProjectionStore` | не закрыт молча |
| No process-global current project `[FIXED]` 01§3 | кода маршрутизации ещё нет → G02/G15 | отложено, не нарушено |
| No in-place re-embed/migration `[FIXED]` 01§3 | → G11 | отложено |
| `non_rebuildable` отвергается `[FIXED]` 01§3 | → G03 | отложено |
| Correctness budget `[FIXED]` 01§4 | fixtures `memory/`+`fault/` кодируют ожидания; harness `Failpoints` | mechanism готов; реализация G07/G13/G14 |
| Two identity ladders + audit rule `[FIXED]` 01§5 | `validate.py` denylist (no payload/backend поля в fixtures); schema-lint DDL → G01+ | fixtures neutral; DDL позже |
| v1 behavioral contract `[FIXED]` 01§7 | fixtures импортированы implementation-neutral (6 семейств, 114 id) | as-built |

Spec 14 — Acceptance & Testing:

| Требование (маркер) | Artifact / Test | Статус на G00 |
| --- | --- | --- |
| 6 fixture families, implementation-neutral `[FIXED]` 14§1 | `fixtures/{parser,reconcile,search,memory,adversarial,fault}`; `validate.py` форсит множество семейств | учтены (parser gap-only) |
| Acceptance gates existence/shape `[FIXED set]` 14§2 | `fault/matrix.json` (F1–F12/S1–S8 декларативно); пороги `TBD` | shape зафиксирован |
| Baseline numbers `[BASELINE]`/`[OPEN]` 14§2–3 / O2 | `manifest.baseline`: метрики сняты, 10/10 порогов = `"TBD"` | O2 соблюдён |
| Fault-injection harness `[FIXED]`+`[SPEC]` 14§3 | `test-support::Failpoints`/`fail_point!`; `harness.rs` (8); `matrix.json` | mechanism (GAP-06 закрыт); скрипты → T07-05/T13-06 |
| Consistency tests `[SPEC mechanics]` 14§4 | → G07/G08/G09 | отложено |
| Determinism (parser/ids/addlContext/schema-lint) 14§5 `[FIXED]` | `validate.py` denylist; `Clock`/`SeqUuids` в harness; schema-lint → G01+ | partial |
| Adversarial `[FIXED]` 14§6 | `adversarial/index.json` (12); v2-spec injection round-trip → GAP-05 | импортирован subset + gap |
| 49-query benchmark `[FIXED]` 14§7 | `search/corpus.json` (49); `manifest.baseline` снят на v1 | as-built |
| Step-11 spike matrix `[FIXED]` 14§7 | → T10 | отложено |

Spec 15 — Roadmap:

| Требование (маркер) | Artifact / Test | Статус на G00 |
| --- | --- | --- |
| Implementation order `[FIXED]` 15§1 | порядок групп 00→17 в `PROGRESS.md` | соблюдён |
| Backend fixed at step 11 `[FIXED]` 15§1 | нет backend coupling (см. O1) | соблюдён |
| Steps 1–7 без open question `[FIXED]` 15§1 | foundations без hardcoded `[OPEN]` | соблюдён |
| MVP v0 scope `[FIXED]` 15§2 | план покрывает; tree-sitter 2–3 языка `[OPEN which]` → GAP-01/O4 | соблюдён |
| Deferred (all additive) `[FIXED]` 15§3 | `manifest.deferred[]`; grep `Cargo.toml` = deferred SDK NONE | не в workspace |
| Open questions O1–O8 `[OPEN]` 15§4 | O1→нет coupling; O2→пороги `TBD`; O4→GAP-01; O8 (`split now [FIXED]`)→state/cache split запланирован T01-05 | ни один не закрыт молча |

Заметки G00 (не отклонения):

- Имена директорий `fixtures/{memory,adversarial,fault}` — слаги; spec 14 §1 называет семейства
  «Memory-quality», «Adversarial recall», «Fault-injection scripts». Покрытие соответствует;
  недостающая часть каждого явно висит как GAP (rev6 memory-quality op-корпус → GAP-04 → T14-07;
  v2 injection round-trip → GAP-05 → T14-08/T16-04). Это не spec↔code mismatch, D-NNN не требуется.
- Строка evidence `T00-01` выше гласит «не коммичено», хотя задача фактически отгружена в коммите
  `055a27a` (`T00-01: import v1 behavioral fixtures and baseline inventory`); правило «коммить
  каждую задачу» появилось позже (`6d0546d`). Историческое evidence не переписывается (CLAUDE.md,
  «Repository hygiene») — фиксирую факт здесь.

### G01 — трейс требование → artifact/test

Дата 2026-07-17, исполнитель Claude Opus 4.8. Команды воспроизводимы из README-плана и строки
evidence G01 выше. `отложено` = требование нормативно, но реализуется позже по плану; на G01
проверено лишь отсутствие преждевременного нарушения/coupling и наличие seam. Ссылки на код —
`file:symbol`/`file:line` на момент сверки.

Spec 02 — Architecture / lifecycle / locking:

| Требование (маркер) | Artifact | Verifying test | Статус |
| --- | --- | --- | --- |
| Store layout, все 11 путей `[SPEC]` 02 §2 | `StoreLayout` core `paths/mod.rs:234+` (incl. `migration_lock`, `backups_dir`) | `store_layout_maps_every_path` | as-built |
| Directory resolution precedence `[SPEC]` 02 §2.1 | `data_dir`/`config_dir` + `nonempty_var`/`absolute` (`paths/mod.rs:162/175/144/153`) | `data_dir_precedence_posix`, `config_dir_precedence_posix` (empty/relative добавлены) | as-built |
| `LOCAL_RAG_HOME` overrides all incl config_dir `[SPEC]` 02 §2.1 | early-return `paths/mod.rs:163/176` | `local_rag_home_overrides_data_dir`, `config_dir_under_local_rag_home` | as-built |
| state pragmas WAL(set-and-verify)/FK=ON/FULL/busy=5000 `[SPEC]` 02 §5 ↔ 03 §2 | `apply_state_pragmas` store `state/open.rs:98` (verify 101-104) | `state_pragmas_are_applied` | as-built |
| Bounded single-writer L4a `[FIXED]` 02 §5 | `state/writer.rs` (spawn/transaction/run_transaction); writable conn не публичен | `concurrent_producers_serialize`, `queue_saturation_waits_then_cancels_cleanly`, `foreign_keys_are_enforced`, `closure_error_rolls_back_then_retry_is_idempotent`, `read_only_connection_cannot_write` | as-built |
| Bounded single-writer L4b (cache) `[FIXED]` 02 §5 | `cache/writer.rs` (отдельный поток `local-rag-cache-writer`) | `cache_writer_backpressure_cancels_cleanly` | as-built |
| L1 migration lock, held only while migrating, RAII `[SPEC]` 02 §5 | `migrate/lock.rs` (Drop 50-55), `mod.rs:299` `_guard` | `concurrent_migrators_apply_exactly_once` | as-built |
| `StateDb::open` order (open_rw → migrate под L1 → spawn writer) `[FIXED, mechanics [SPEC]]` 02 §4.1 | `state/mod.rs:53-73` | `state_db_open_bootstraps_and_is_idempotent` | as-built |
| Degraded modes / wire-code taxonomy `[SPEC]` 02 §6 | типизированные ошибки в `OpenError::Migration`; mapping → protocol boundary | — | отложено (T15) |

Spec 03 — Data model / DDL / hash rules:

| Требование (маркер) | Artifact | Verifying test | Статус |
| --- | --- | --- | --- |
| Cross-DB запрет: no writable ATTACH `[FIXED]` 03 §1.4 | два физически раздельных writer-потока; нет ATTACH в src (только read-only в docstring) | `no_writable_cross_db_attach` (source-lint скан `crates/store/src/**`) | as-built |
| Framework bootstrap DDL байт-в-байт `[SPEC]` 03 §2.1 | `bootstrap()` `migrate/mod.rs:554-578` (`schema_migrations`/`store_settings`/`migration_progress`) | `state_db_open_bootstraps_and_is_idempotent`, `empty_to_latest_applies_all` | as-built |
| state write policy: single bounded queue `[FIXED, numbers [SPEC]]` 03 §3 | L4a (см. 02 §5 выше) | (L4a-тесты) | as-built (queue) / отложено (batched last_used/checkpoint/VACUUM-by-metrics) |
| cache pragmas WAL/FK=OFF/NORMAL/busy=5000 `[SPEC]` 03 §4 | `apply_cache_pragmas` `cache/open.rs:147` | `cache_pragmas_are_applied` | as-built |
| cache binding/recreation `[FIXED principle]` 03 §4.4 (`CACHE_SCHEMA_VERSION=1`) | `cache/open.rs` (`open_and_bind`:173/`inspect_existing`:208/`seed_binding`:251/`recreate`:279) | `matching_reopen_preserves_rows`, `uuid_mismatch_rebuilds`, `schema_version_mismatch_rebuilds`, `corrupt_cache_yields_clean_cache`, `state_untouched_on_cache_rebuild`, `recreate_is_idempotent_on_retry`, `cache_recreate_hard_kill_resumes` | as-built |
| payload-таблицы (embedding/normalized_text/fts) НЕ создаются | только `cache_meta` DDL `cache/open.rs:39` | — | scope-guard (T03/T08/T11) |
| Migration boundaries: hash schema/`occurrence_id`/`worktree_id`/`received_seq` стабильны; deferred additive `[FIXED]` 03 §5 | `ALL: &[] ` `mod.rs:149` (первая миграция T02-02); `store_instance_uuid` не засеян | — | as-built / отложено (посев T02-01/T15) |

Spec 13 §3 — Migration framework:

| Требование (маркер) | Artifact | Verifying test | Статус |
| --- | --- | --- | --- |
| numbered/checksummed(over SQL)/forward-only `[FIXED]` | `Migration.version`/`checksum()=sha256_hex(SQL)` `mod.rs:88/132`; `filter(v>store_version)`:348 | `checksum_drift_is_rejected`, `older_to_latest_applies_only_new` | as-built |
| well-formed set (strictly-increasing-contiguous-from-1) | `validate_set` `mod.rs:532` → `MalformedSet` | `malformed_set_is_rejected` (добавлен) | as-built |
| compat-check: refuse newer store `[FIXED]` | `run()` `mod.rs:310-316` `IncompatibleStore` | `newer_store_is_rejected` | as-built (wire-code → T15) |
| checksum drift / rewritten history `[FIXED]` | `mod.rs:319-337` `ChecksumDrift`/`UnknownAppliedVersion` | `checksum_drift_is_rejected` | as-built |
| resumable: idempotent steps, progress row/step, crash⇒resume `[FIXED]` | `apply_complex` `mod.rs:375-433` + `migration_progress` | `resume_after_each_checkpoint`, `resume_from_finalize_pending`, `resumable_hard_kill_via_sigabrt`, `failed_step_leaves_version_unapplied_then_retry_succeeds` | as-built |
| backup before destructive (`VACUUM INTO backups/state-<v>-<ts>.sqlite`) `[SPEC mechanics]` | `mod.rs:387-399`, `take_backup`:442, `backup_path`:469 | `destructive_backup_has_pre_change_data`, `destructive_backup_idempotent_on_resume`, `destructive_sql_only_backs_up_then_applies_sql` | as-built |
| cache never migrated (bump ⇒ drop&rebuild) `[FIXED]` ↔ 03 §4.4 | cache recreate path | `schema_version_mismatch_rebuilds` | as-built |
| migration tests on every prior released schema `[FIXED]` | синтетические наборы M1..M4 (matrix); реальный `ALL` пуст | migrate matrix (8) | as-built (released schema нет до T02-02) |
| v1→v2 data migration `[OPEN]` | — | — | не закрыт молча (решение до GA) |

Backend-coupling guardrail (архитектурный инвариант, T10):

| Проверка | Результат |
| --- | --- |
| прямые внешние deps | ровно 3 allowlisted: `libc` (unix), `rusqlite` (bundled), `tokio` (sync) |
| `Cargo.lock` `source=` | 36 (все crates.io; wasm-подсемейство rusqlite 0.40 target-gated, не линкуется в native) |
| grep dense/model/tree-sitter/network SDK по всем `Cargo.toml` | 0 (qdrant/usearch/ollama/onnx/candle/tree-sitter/reqwest/… = NONE) |

Заметки G01 (не отклонения):

- Все затронутые группой маркеры `[FIXED]`/`[SPEC]` = as-built + verified; единственный
  затронутый `[OPEN]` — O8 (state/cache split, уже `[FIXED]`) соблюдён, а v1→v2 migration
  (`[OPEN]` 13 §3) не закрыт молча. Ни один `[OPEN]` не разрешён имплицитно.
- Спека «single writer *task*» (02 §5 L4a/L4b) реализована выделенным OS-потоком с
  `blocking_recv` (не tokio-task). Семантически эквивалентно (ровно один физический писатель
  на БД); не расхождение — docstring `state/mod.rs`/`cache/mod.rs` использует слово «task».
- Gate-hardening (закрытие дешёвых пробелов покрытия защитного/edge кода; только тесты + docs,
  продуктовый код не тронут — это не features и не D-NNN): `malformed_set_is_rejected` (store
  `tests/migrate.rs`), edge-cases empty/relative `XDG_CONFIG_HOME` в `config_dir_precedence_posix`,
  реальный symlink-swap reject `ensure_rejects_symlink_swap` (core `paths/perms.rs`), pinned
  12-hex KAT в `pipe_name_fixture` (ground truth вычислен независимо), синхронизация перечня
  транзитивных deps `rusqlite` в `CONTRIBUTING.md` с `Cargo.lock`.
- Отложенные by-design seam (проверено наличие, не преждевременная реализация): protocol
  wire-code mapping (T15), посев `store_instance_uuid` (T02-01/T15), Windows SID (T17),
  batched `last_used_at`/WAL checkpoint/VACUUM-by-metrics (позже). Отклонений не обнаружено;
  `DEVIATIONS.md` без изменений; historical evidence не переписывалось.

Примечания к T00-03:

- Крейт `test-support` — dev-only по образцу `xtask`: добавлен в `members`, но не в
  `default-members`, поэтому продуктовый `cargo build`/дистрибуция его не трогают, а
  `cargo test --workspace` (внутри `cargo xtask ci`) покрывает. Потребляется только как
  `[dev-dependencies]` (сейчас — из `local-rag`).
- Изоляция temp store — по путям: каждый `TempHome` владеет своим каталогом; `LOCAL_RAG_HOME`
  ставится только в окружение дочернего процесса (`Command::env`), в родительский процесс env не
  пишется (иначе параллельные тесты гонялись бы за глобал). `$HOME` не читается нигде — база
  `std::env::temp_dir()`; страж-тест проверяет, что путь не под `$HOME`, `.command()` дополнительно
  делает `env_remove("HOME")` для дочернего процесса.
- Failpoints реализованы registry-strict (карточка: «неизвестный failpoint отвергается»): имя
  надо объявить (`register`) до `arm`; `arm`/`eval`/`disarm` неизвестного имени → `Err(Unknown)`.
  `Action::Abort` = crash-точка (симуляция kill), `Panic`, `Error` (early-return через 2-арг
  `fail_point!`). Механизм закрывает GAP-06; сами строки F1–F12 (T07-05) и S1–S8 (T13-06) здесь
  НЕ писались (scope-guard).
- Осознанное решение по зависимостям: харнесс на чистом `std`, без `tempfile`/`uuid`/`serde`,
  чтобы сохранить свойство T00-02 «`Cargo.lock` = 0 внешних источников». `fixtures` отдаёт пути и
  сырые байты/строки; типизированный JSON-парсинг отложен до задачи-потребителя, которая обоснует
  зависимость по dependency policy (`CONTRIBUTING.md`).
- Doctest edition 2024: `gen` — зарезервированное слово, переменная в примере переименована.
- Отклонений не обнаружено; DEVIATIONS.md без изменений; `fixtures/manifest.json` GAP-06 не
  переписан (историю не трогаем).

Примечания к T00-02:

- Три бинарника соответствуют spec 13 §1 (`local-rag` = daemon+CLI, `local-rag-proxy` = stdio
  MCP proxy, `local-rag-hook` = spool writer); бизнес-логики нет — единственная команда `version`.
- Единый full-check = `cargo xtask ci` (dev-only крейт `xtask`, `.cargo/config.toml` alias); тот же
  command вызывает CI. `xtask` исключён из `default-members`, но покрыт `cargo test --workspace`.
- CI config lint реализован как `xtask/tests/ci_config.rs`: текстовые проверки, что
  `.github/workflows/ci.yml` вызывает `cargo xtask ci` на одном host, а toolchain запинен на 1.96.1.
- Deferred scope не заведён: нет dense/model SDK, tree-sitter, rusqlite и сетевых крейтов.
  Мультиплексирование бинарников через argv0 (spec 13 §1) отложено до дистрибуции (T17).
- Отклонений не обнаружено; DEVIATIONS.md без изменений.

Примечания к T00-01:

- Формат fixtures: JSON + JSON Schema (Draft 2020-12) в `fixtures/schema/`; валидатор
  `fixtures/tooling/validate.py` покрывает 4 проверки карточки (schema validation, ID
  uniqueness, runner dry-run, no backend-specific fields). Это временный bootstrap: Rust-harness
  из T00-03 переиспользует те же fixtures и схемы.
- Baseline v1 снят живым прогоном `scripts/benchmark.ts` (Qdrant 1.18.2 + Ollama на localhost,
  docker не использовался). Правки применялись только к сборочному артефакту
  `dist/scripts/benchmark.js` (форс code-only; исключение вендоренных `node_modules` из обхода
  корпуса) и **возвращены** после прогона; исходники v1 не менялись. Сырые артефакты —
  `fixtures/search/baseline/`.
- Общая workspace quality-команда ещё не существует (создаётся в T00-02), поэтому она не
  запускалась; выполнены доступные проверки: `validate.py` (stdlib + jsonschema), негативный
  self-test и well-formedness всех JSON. Rust/cargo проверки неприменимы (production-кода нет).
- DEVIATIONS.md без изменений: gap-register разрешён карточкой («manifest OR explicitly
  registered blocking gap»), нормативных расхождений спека↔v1 не обнаружено.
