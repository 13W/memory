# Группа 18 — TUI dashboard (post-v0)

Первая группа, открытая после закрытия `T00–T17` (`G17: PASS`) — по явному продуктовому решению,
задокументированному в `docs/adr/0008-tui-dashboard.md`. Не переоткрывает ни один гейт `G00–G17`.
Цель: терминальный дашборд (`ratatui`+`crossterm`), покрывающий тот же функционал, что был в
Web-дашборде v1 (логи/статистика, память, настройки проекта/каталогов, настройки сервера), явно
**без** плейграунда. Ссылки: ADR-0008; spec 11 §7; 02 §3 (config/repo_settings); 08 (memory);
04 §6 (candidate lifecycle).

## T18-00 — Регистрация scope

- **Результат:** этот файл, `docs/adr/0008-tui-dashboard.md`, `docs/specification/11-interfaces.md`
  §7, правка `TRACEABILITY.md`, секция `## 18` в `PROGRESS.md` — весь пакет регистрации, до
  которого ни одна из карточек T18-01+ не может считаться начатой (`CLAUDE.md`: "Do not assume
  production code... until introduced by a completed task").
- **Тесты:** нет (документация); приёмка — все перечисленные файлы существуют и внутренне
  непротиворечивы (перекрёстные ссылки резолвятся).

## T18-01 — Скелет крейта `local-rag-tui`

- **Результат:** новый крейт `crates/local-rag-tui` (workspace member, `default-members`,
  `[package.metadata.dist] dist = true`); минимальный `ratatui`+`crossterm` event loop (raw mode
  enter/restore, resize handling, чистый выход по панике — терминал не должен оставаться в raw
  mode); зависимости — `local-rag` (lib half), `local-rag-store`, `local-rag-core`,
  `local-rag-protocol` напрямую; новые записи в `CONTRIBUTING.md`'s dependency-policy таблице для
  `ratatui`/`crossterm`. Дистрибуция: `npm/memory/bin/local-rag-dashboard.js` лаунчер по образцу
  существующих, `npm/memory/src/resolve.js`'s `binaryPath` расширен на `'local-rag-tui'`.
- **Тесты:** крейт собирается (`cargo build -p local-rag-tui`); dist-манифест включает новый
  бинарник; npm-лаунчер резолвит путь для текущей платформы (мокнутый layout-тест по образцу
  T17-01).

## T18-02 — Server status (офлайн-safe)

- **Результат:** экран Status — pid/instance_uuid/daemon_version/daemon_mode/socket_path (когда
  daemon жив, через `daemon::probe::fetch_welcome`; когда нет — `daemon::lock::
  read_store_lock_file` даёт best-effort состояние) + таблица durable counts (memory entries by
  kind/state, pending candidates by state, projection status) через прямой `state.sqlite`.
- **Тесты:** экран корректно рендерит оба состояния (daemon жив / не жив) на фикстурном сторе;
  live-проба через реальный `local-rag serve` подпроцесс.

## T18-03 — Repo & worktree browser

- **Результат:** экран Repositories — список репозиториев (`local_rag_store::registry::
  all_repository_ids`, `current_path`, `worktrees_of_repo`) с drill-down в worktree
  (`worktree_summary`, `current_worktree_path`, `path_history`). Только чтение.
- **Тесты:** рендер на фикстуре с несколькими repo/detached-worktree; drill-down навигация.

## T18-04 — Memory browser (read-only)

- **Результат:** экран Memory — список записей/кандидатов с фильтрами (kind/state/scope,
  candidates-переключатель) через `list_memory_entries_for_scope`/`list_candidates`; панель
  деталей + evidence (`memory_evidence_for`) для выбранной записи.
- **Тесты:** рендер списка/фильтров/пагинации и evidence-панели на fixtures из `fixtures/memory/`.

## T18-05 — Memory mutations

- **Результат:** действия `approve`/`reject`/`edit`/`retract`/`merge` поверх T18-04, каждое —
  прямой вызов `apply_edit`/`apply_merge`/`apply_retract`/`approve_candidate`/`reject_candidate`
  внутри `state.writer().transaction(...)`, `Actor::User` (зеркалит `cli/memory.rs` буквально).
  Confirm-модал обязателен и **только** для операций с `destructiveHint: true` в
  `daemon/mcp/tools.rs::catalog()` (сегодня — исключительно `retract_memory`); TUI читает этот
  список как источник истины, не кодирует свой.
- **Тесты:** каждое действие — round-trip тест (mutation применяется, optimistic-conflict/
  illegal-transition ошибки показываются пользователю, не паникуют UI); confirm-модал
  тестируется явно на `retract`, явно **не** появляется на `merge`/`edit`.

## T18-06 — Repo settings screen

- **Результат:** экран Repo Settings — форма `data_policy` (4 значения, подсказка "most
  restrictive wins" со ссылкой на 02 §3.2) + список generic `(key,value)` через
  `crates/store/src/registry/settings.rs` (`repo_settings`/`set_repo_setting`/
  `set_repo_data_policy`) — backend уже полный, эта карточка — первый UI над ним.
- **Тесты:** запись/чтение `data_policy` round-trip на фикстурном сторе; generic key/value
  CRUD.

## T18-07 — Server settings: `Config::save` + экран

- **Результат:** новый `Serialize` на секциях `Config`/`RawConfig`
  (`crates/core/src/config/mod.rs`), `Config::to_raw()`, `Config::to_toml_string()`,
  `Config::save(&self, config_dir)` (атомарная запись — `.tmp` + `fs::rename`); экран Server
  Settings — форма по секциям `daemon`/`storage`/`models`/`index`/`spool`/`memory`, `Ctrl+S` →
  `save` + модал "вступит в силу после `local-rag restart`" с опцией сразу вызвать
  `local-rag restart`.
- **Не в scope:** live config reload в демоне (не существует и не добавляется).
- **Тесты:** `Config::load` → mutate → `save` → `load` round-trip сохраняет все поля кроме заведомо
  отброшенных незнакомых ключей (задокументированное ограничение, ADR-0008/§7); atomic-write не
  оставляет частично записанный файл при симулированном сбое (failpoint по конвенции проекта).

## T18-08 — Демон-телеметрия (backend)

- **Результат:** новый `daemon/telemetry.rs` — `TelemetryState` (`Arc<Mutex<...>>`, по образцу
  `daemon/session.rs::SessionRegistry`): кольцевой буфer `VecDeque<CallRecord>` (bounded ~500) +
  `HashMap<tool, ToolStats>` (calls/errors/bytes_in/bytes_out/total_ms). Точка записи —
  `daemon/handshake.rs::handle_connection`, вокруг `handler.handle(env.context, env.mcp).await`;
  новое поле `telemetry: TelemetryState` в `HandshakeContext`. `source` (mcp/hook) — из
  `hello.harness`, захваченного локально в `handle_connection`; включает отдельную правку
  `local-rag-hook/src/recall.rs`, чтобы дать хуку отличимый от прокси `harness` (сейчас оба шлют
  одинаковый `"claude-code"`). Два новых JSON-RPC метода в `dispatch()`'s матче: `admin/
  tail_calls`, `admin/tool_stats` — тот же `RequestEnvelope`/`ResponseEnvelope` транспорт;
  `admin/*`-вызовы сами не попадают в собственный лог (фильтр по имени метода).
- **Не в scope:** файловый лог (`logs_dir()` остаётся зарезервированным, не заполняется этой
  карточкой); push/`broadcast`-семантика; изменения в `local_rag_protocol` (протокол-крейт).
- **Тесты:** unit-тесты на `TelemetryState` (bounded buffer eviction, per-tool агрегация);
  end-to-end тест — поднять demon, прогнать вызовы через реальные `local-rag-proxy` и
  `local-rag-hook` подпроцессы, проверить что `admin/tail_calls` видит их с корректным,
  различимым `source`; `admin/*`-вызовы не самозасоряют буфер.

## T18-09 — Logs + per-tool stats screen (TUI)

- **Результат:** экран Logs — таблица последних вызовов (time/source/tool/duration/bytes/status)
  + таблица per-tool агрегатов, оба поллингом `admin/tail_calls`/`admin/tool_stats` (~1с) через
  новый долгоживущий async UDS-клиент в `local-rag-tui` (по форме ближе к
  `local-rag-proxy::connect`, но без stdin/stdout relay). Явная заглушка "daemon not running",
  когда демон недоступен.
- **Тесты:** экран корректно показывает заглушку при недоступном демоне и живые данные при
  доступном (реальный `local-rag serve` подпроцесс, T18-08's e2e-тест как основа).

## G18 — Сверка TUI dashboard

Перечитать ADR-0008 и spec 11 §7 целиком; построить трейс `требование -> код -> тест` по каждому
из шести экранов; убедиться, что ни одна карточка не задела код групп 00–17 (только аддитивные
правки — новый крейт, новые поля в уже существующих структурах, два новых JSON-RPC метода); что
плейграунд нигде не реализован (явная проверка `rg`); что confirm-модал появляется ровно на
операциях с `destructiveHint: true` и нигде больше; что `local-rag-tui` работает офлайн (без
демона) для Status/Repositories/Memory/Repo Settings/Server Settings и корректно деградирует на
Logs; что одновременная работа TUI и живого `local-rag serve` не приводит к конфликтам блокировок
(WAL+busy_timeout). Зарегистрировать deviations, если найдены; результат — `PASS`, `PASS after
D-NNN` или `BLOCKED` в `PROGRESS.md`'s Gate results.
