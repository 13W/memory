# Группа 20 — Daemon-managed indexing нескольких проектов (post-v0)

Вторая группа, открытая после закрытия `T00–T17` (`G17: PASS`) по явному продуктовому решению —
`docs/adr/0009-daemon-managed-indexing.md`. Не переоткрывает ни один гейт `G00–G19` и не зависит
от незакрытого `G18`. Цель: демон сам держит по одному фоновому таску на каждый **явно
зарегистрированный** worktree (watcher + reconcile + embed/activate/materialize), список
зарегистрированных проектов переживает рестарт демона, а `local-rag index/reindex/watch`
сохраняются как есть (решение владельца: путь **аддитивный**, не замещающий).

Ссылки: ADR-0009; spec 02 §1 `[FIXED]` (топология: `background workers (reconcile …)`), §3.3
`[FIXED]` (нет ambient current project), §4.1 step 4/§4.3/§4.4 (as-built-заметки «no card names
an owner»), §5 `[SPEC]` (L2 + `[OPEN]` про eviction), §6 (`BUSY_RETRY`); spec 03 §2.1; spec 06
§1 `[FIXED]` (watcher = hint, reconcile = truth), §3; spec 11 §6/§8; spec 12 §1;
`crates/index/src/reconcile/{watcher,driver,schedule}.rs`; `crates/store/src/lock/worktree.rs`;
`crates/local-rag/src/daemon/{lifecycle,jobs,idle,search,query_embedder,mcp/dispatch}.rs`;
`crates/local-rag/src/cli/{index,watch,mod}.rs`; D-043.

Формат карточек — по `TASK-TEMPLATE.md` (поля `Зависит от`/`Спецификация`/`Результат`/
`В scope`/`Не в scope`/`Тесты`), как в группе 19; структура группы — как в группе 18.

## Диагноз (контекст для исполнителя — прочитать до начала)

1. **Write-сторона L2 в production не существует.** `rg` даёт ровно один продовый потребитель
   `WorktreeLockRegistry` — `daemon/search.rs:82`, и он создаёт **приватный** экземпляр только
   для чтения. `crates/store/src/lock/worktree.rs:6` указывает на `T11-05` — указатель
   устарел: `T11-05` закрыт `[x]`, а `crates/projection/src/model_switch.rs:51-54` переадресует
   адопцию обратно в «group 15's wiring», которая её не сделала. Это `D-043`.
2. **`crates/projection/tests/switch_concurrency.rs:24-27`** прямо называет своей предпосылкой
   «the actual property under test — L2.write serialization». Значит все гарантии для
   конкурентных switch'ей сегодня держатся на свойстве, которого в проде нет.
3. **Пайплайн индексации недостижим из библиотеки.** `IndexCtx`/`index_worktree`/
   `project_generation` — `pub(crate)` бинарного таргета `crates/local-rag`; `lib.rs`
   экспортирует только `pub mod daemon`.
4. **ONNX-сессии.** Демон открывает модель только за query-адаптерами
   (`daemon/query_embedder.rs`, две сессии code_raw + memory, D-036) и не отдаёт
   `Arc<dyn Embedder>`. Наивная реализация даст четыре сессии в одном процессе.
5. **`admin/*` уже легитимен** как «TUI-only admin surface, not MCP tools, not in
   `tools::catalog()`» (`daemon/mcp/dispatch.rs:96-103`) — новые verbs идут туда, а **не** в
   каталог MCP (T19-01 только что ввёл `MAX_CATALOG_BYTES`).
6. **Idle-gate.** `JobRegistry`/`JobGuard` + `idle_eligible` — три `&&` (`daemon/idle.rs:28`).
   Прецедент D-024 (`consolidation_trigger`) — держать guard **только на время активного
   тика**, не на время ожидания. Эта группа обязана следовать D-024, а не изобретать своё.

Порядок работ: сначала три независимых предпосылки (`T20-01` схема, `T20-02` библиотечный
пайплайн, `T20-03` общий эмбеддер), затем блокирующая `T20-04` (L2.write), затем ядро
(`T20-05` один worktree, `T20-06` супервизор N проектов), затем поверхности управления
(`T20-07` admin, `T20-08` CLI), затем защита от двойной индексации (`T20-09`), затем
владельческое решение об idle-семантике (`T20-10`).

Перед стартом: вставить очередь `T20-00…G20` в `PROGRESS.md` (после блока `## 19`), вести
статусы `[~]`/`[x]` и evidence по общему контракту. Один таск — одна итерация — один коммит.

## T20-00 — Регистрация scope

- **Зависит от:** —.
- **Результат:** этот файл, `docs/adr/0009-daemon-managed-indexing.md`,
  `docs/specification/11-interfaces.md` §8 (новая секция `[SPEC surface]`) + строка
  `local-rag project …` в §6, строка `D-043` в `DEVIATIONS.md` (статус `open`, корректирующие
  задачи `T20-04`/`T20-06`), правка `TRACEABILITY.md` (строки `02`/`06` → `G20`; закрывающий
  абзац называет ADR-0009 вторым прецедентом), секция `## 20` в `PROGRESS.md`, cross-reference
  note к `T15-07` в `groups/15-daemon-interfaces-cli.md` («`watch` дополняется, не заменяется»).
- **В scope:** только документация; все перекрёстные ссылки обязаны резолвиться.
- **Не в scope:** любые правки `[FIXED]`-текста (в частности spec 02 §4.3 — это `T20-10`).
- **Тесты:** нет; приёмка — перечисленные файлы существуют, внутренне непротиворечивы, ни одна
  карточка `T20-01+` не считается начатой до этого.

## T20-01 — Persisted-реестр: таблица `managed_worktree` (schema v10)

- **Зависит от:** T20-00.
- **Спецификация:** spec 03 §2.1 `[SPEC]`; spec 02 §3.2 (почему не `repo_settings`); spec 04 §7
  (почему не `worktree.state`); `crates/store/src/migrate/mod.rs::ALL`.
- **Результат:** миграция №10 (`Migration::sql(10, "managed_worktree",
  crate::registry::SCHEMA_V10)`) и типизированный модуль `crates/store/src/registry/managed.rs`,
  дающий демону и CLI один источник истины о том, какие worktree индексируются демоном.
- **В scope:**
  - DDL: `managed_worktree(worktree_id TEXT PRIMARY KEY REFERENCES worktree(worktree_id),
    enabled INTEGER NOT NULL DEFAULT 1, registered_at INTEGER NOT NULL, updated_at INTEGER NOT
    NULL)`. Ключ — стабильный UUID, не путь (system-wide invariant). Никаких runtime-полей
    (`last_error`, `running`) в схеме — они in-memory и отдаются через `T20-07`.
  - API: `register_managed_worktree(tx, worktree_id, now_ms)` (идемпотентный upsert),
    `unregister_managed_worktree`, `set_managed_enabled(tx, id, bool, now_ms)`,
    `managed_worktrees(conn) -> Vec<ManagedWorktree>` (детерминированный `ORDER BY
    worktree_id`), `is_managed(conn, id) -> bool`.
  - Обновление doc-комментария `migrate::ALL` (перечисление версий) и spec 03 §2.1.
- **Не в scope:** ссылки на этот реестр из демона/CLI (это `T20-06`/`T20-08`); авто-регистрация
  («проиндексировал → значит управляемый») — регистрация только явная, ADR-0009.
- **Тесты:** миграция применяется на свежем сторе и на существующем v9 (forward-only), checksum
  зафиксирован; FK отвергает неизвестный `worktree_id` и откатывает транзакцию; повторный
  `register` идемпотентен (одна строка, `updated_at` обновлён); `enabled=0` виден в чтении;
  детерминированный порядок; существующий schema-lint/`LOCAL_RAG_TEST_MAX_SCHEMA_VERSION`
  остаются зелёными.

## T20-02 — Библиотечная половина indexing-пайплайна

- **Зависит от:** —(параллельно T20-01).
- **Спецификация:** spec 06 §1–2; spec 05 §5; `crates/local-rag/src/{lib,cli/index}.rs`.
- **Результат:** `IndexCtx`, `index_worktree`, `project_generation`, `open_state`/`open_cache`/
  `finish_index_ctx`, `register_new_worktree` переехали из бинарного `cli/index.rs` в
  библиотечную половину (`crates/local-rag/src/indexing/`, `pub mod indexing;` в `lib.rs`);
  `cli::index`/`cli::watch` остаются тонкими вызывающими без изменения поведения.
- **В scope:** механический перенос + повышение видимости до `pub`; сохранение существующих
  тестов `cli/index.rs` (переезжают вместе с кодом); doc-комментарии обновлены на «библиотечный
  пайплайн, у которого теперь два вызывающих: CLI и демон».
- **Не в scope:** любое изменение семантики пайплайна; новый крейт (перенос внутри
  `crates/local-rag` достаточен и не создаёт цикла зависимостей).
- **Тесты:** все существующие тесты `cli::index` зелёные без правок ассертов; новый smoke-тест
  из `tests/`-таргета (доказывает, что API реально виден снаружи бинарника — это и есть суть
  карточки); CLI-снапшоты парсинга не изменились.

## T20-03 — Общий ленивый Embedder-провайдер (query + backfill)

- **Зависит от:** —(параллельно T20-01/T20-02).
- **Спецификация:** spec 10 §1/§3; D-036, D-037; `crates/local-rag/src/daemon/query_embedder.rs`.
- **Результат:** демон открывает **не более двух** ONNX-сессий на процесс (`code_raw` +
  `memory`), и эти же `Arc<dyn Embedder>` доступны как для query-адаптеров, так и для
  backfill-пула индексации.
- **В scope:** `LazyQueryEmbedder` хранит `Arc<dyn Embedder>` и отдаёт его наружу (например
  `LazyEmbedderProvider { code(), memory() } -> Option<Arc<dyn Embedder>>`), сохраняя D-037's
  «модель, установленная после старта демона, подхватывается без рестарта» и D-036's
  fail-open-to-degraded; `StartOptions` получает провайдера вместо/вдобавок к двум
  `QueryEmbedder`.
- **Не в scope:** смена модели/рантайма (ADR-0004/0005); прогрев; выгрузка сессии по
  бездействию.
- **Тесты:** провайдер открывает сессию ровно один раз на kind (счётчик открытий в тесте с
  фейковым провайдером); `search_code` остаётся `lexical_only`, когда модель не установлена;
  после появления `.ok` провайдер становится Ready без рестарта (регрессия D-037).

## T20-04 — Первое production-принятие `L2.write` (D-043)

- **Зависит от:** T20-02, T20-03.
- **Спецификация:** spec 02 §5 `[SPEC]` (write path: `L2.write → compute → L4a tx`), §6
  (`BUSY_RETRY`), `[OPEN]` про eviction (02 §5 / `lock/worktree.rs:26-28`);
  `crates/projection/tests/switch_concurrency.rs`.
- **Результат:** один `Arc<WorktreeLockRegistry>` на весь демон, разделяемый между
  `SearchEngine` и всеми пишущими путями; полный цикл `reconcile_once → project_generation`
  для одного worktree выполняется внутри `locks.write(worktree_id, …)`.
- **В scope:**
  - Реестр поднят на уровень `StartOptions`/`DaemonHandle` и **передаётся** в
    `build_search_engine` вместо приватного `WorktreeLockRegistry::new()` (`daemon/search.rs:82`).
  - Типизированная обёртка (`local_rag::indexing::write_locked`), внутри которой выполняется
    цикл; порядок `L2Write → L4a` не нарушает `checked_scope_*` (проверяется тестом, не
    рассуждением).
  - Явная диспозиция `[OPEN]` про eviction записей реестра: решение + аргумент в doc-комментарии
    и as-built-заметке spec 02 §5 (рекомендация: eviction не вводить — число записей ограничено
    числом worktree, которых демон коснулся за одну жизнь процесса, а процесс завершается по
    idle; ввод eviction потребовал бы refcount'а на живые guard'ы и не окупается).
  - Обновление устаревшего doc-комментария `crates/store/src/lock/worktree.rs:1-9`.
- **Не в scope:** межпроцессная сериализация (L2 — in-process `RwLock`, это принципиально
  недостижимо; см. `T20-09`); `L3`; миграционный `L1`; адопция L2 внутрь крейта `projection`
  (остаётся ответственностью вызывающего, как и документировано).
- **Тесты:** два разных worktree пишут одновременно и **не** блокируют друг друга (это и есть
  жалоба пользователя, выраженная как тест); `read_bounded` на worktree с активным писателем
  отдаёт `ReadTimedOut → BUSY_RETRY` (первый достижимый в проде случай — по образцу
  `crates/store/tests/lock.rs::read_bounded_times_out_while_a_writer_holds_the_lock` и
  `search/tests/pipeline.rs`); **тест-детектор двух реестров**: `SearchEngine` и писатель обязаны
  разделять один экземпляр (тест обязан падать, если конструируются два); `debug_assert`
  порядка блокировок не срабатывает под `cargo test`.

## T20-05 — Per-worktree indexing task («daemon watch loop»)

- **Зависит от:** T20-04.
- **Спецификация:** spec 06 §1 `[FIXED]` (watcher = hint; `Startup`/`Periodic`/`WatcherOverflow`);
  `crates/index/src/reconcile/{watcher,driver}.rs`; `crates/local-rag/src/cli/watch.rs:100-190`
  (референс-композиция).
- **Результат:** `crates/local-rag/src/daemon/indexing/worktree_task.rs` — один async-таск на
  worktree: `spawn_reconciler` + `spawn_watcher`, форсированный `TriggerKind::Startup` при
  запуске, на каждый новый `successes` — `project_generation` под `L2.write`; на `failures` —
  диагностика без остановки цикла; корректный останов по сигналу с flush'ем последнего
  успешного поколения (как в `cli::watch`).
- **В scope:**
  - `JobGuard(JobKind::Reconcile)` (новый вариант в `#[non_exhaustive] JobKind`) берётся при
    принятии триггера и держится до конца проекции, **не** держится во время простого наблюдения
    — дословно дисциплина D-024.
  - In-memory статус таска (`last_generation_id`, `last_success_ms`, `consecutive_failures`,
    `last_error`) для `T20-07`.
  - `CaseSensitivity` — из `daemon::gitroot::case_sensitivity()` (out-of-band, как требует
    `WorktreeMeta`).
- **Не в scope:** множественность проектов и их жизненный цикл (это `T20-06`); реестр
  (`T20-01`); admin-verbs.
- **Тесты:** на фикстурном сторе с реальным временным деревом — запуск таска даёт активное
  поколение без единого внешнего процесса; изменение файла → новое поколение (тест ждёт
  события таска, а не `sleep`); падение reconcile не останавливает цикл, следующий триггер
  успешен; отмена таска в середине цикла не оставляет активированного поколения (drop-safety,
  spec 06 §1); `JobRegistry::len()` > 0 во время цикла и == 0 в покое.

## T20-06 — Супервизор: N проектов, старт из реестра, reload, shutdown

- **Зависит от:** T20-01, T20-05.
- **Спецификация:** spec 02 §1 `[FIXED]`, §4.1 step 4 («start workers»), §4.3 (idle-gate),
  §3.3 `[FIXED]` (нет ambient current project); `daemon/lifecycle.rs:360-410`.
- **Результат:** `crates/local-rag/src/daemon/indexing/supervisor.rs` — на старте демона читает
  `managed_worktrees`, поднимает по одному `T20-05`-таску на каждый `enabled` worktree,
  переживает рестарт демона; на `shutdown` — сигнал + await всех тасков по образцу
  `consolidation_trigger_stop`/`_join`; `reload()` приводит множество живых тасков в
  соответствие с таблицей.
- **В scope:**
  - Ограниченная стартовая конкуренция (`[SPEC]`-константа, например
    `MAX_CONCURRENT_STARTUP_RECONCILES = 2`, выбранная и задокументированная как выбранная, не
    выведенная — прецедент `LIVENESS_PROBE_TIMEOUT_MS`) со staggered-запуском: `N` проектов не
    должны запускать `N` строгих сканов одновременно.
  - Медленный backstop-поллинг таблицы (`[SPEC]`-интервал, ~60 с) — «notify = hint, таблица =
    истина»: регистрация из другого процесса подхватывается, даже если `admin`-нотификация не
    дошла.
  - Никогда не запускается в `DaemonMode::MigrationOnly` (тот же guard, что у двух resume-пассов).
  - Явная фиксация в as-built-заметке spec 02 §3.3: множество managed-worktree — это список
    фоновых работ, **не** ambient current project; маршрутизация запросов не меняется.
- **Не в scope:** периодический GC/housekeeping-планировщик (вторая половина «unclaimed
  scheduling» из spec 02 §4.3/§4.4 — отдельный владелец, сюда не протаскивать); авто-детект
  проектов; удаление индексов при unregister.
- **Тесты:** два зарегистрированных worktree индексируются **параллельно** end-to-end (реальный
  `local-rag serve`-подпроцесс, два временных дерева, обе search-выдачи корректны) — прямой тест
  исходной жалобы; регистрация переживает рестарт демона; `enabled=0` не поднимается; `reload`
  поднимает/останавливает ровно дельту; `shutdown` не оставляет висящих тасков и осиротевших
  `building`-поколений; демон с зарегистрированными, но тихими проектами **по-прежнему** уходит
  в idle-shutdown (регрессия на неизменность `[FIXED]` §4.3); в `MigrationOnly` супервизор не
  стартует.

## T20-07 — Admin JSON-RPC verbs + клиент

- **Зависит от:** T20-06.
- **Спецификация:** spec 11 §8 (новая), §4; `daemon/mcp/dispatch.rs:96-103` (прецедент T18-08).
- **Результат:** три метода в существующем `dispatch()`-матче — `admin/projects_list`,
  `admin/projects_reload`, `admin/reconcile_now {worktree_id}` — и минимальный
  синхронный клиент (по форме `daemon/probe.rs::fetch_welcome`, без stdin/stdout-релея),
  которым пользуется CLI `T20-08`.
- **В scope:** новое поле в `DispatchContext` (handle супервизора, `Option<&…>`); ответ
  `projects_list` = durable-поля из таблицы + in-memory статус из `T20-05`;
  `reconcile_now` инжектирует `TriggerKind::Manual`; ошибки — JSON-RPC-канал (`-32602` для
  неизвестного/неуправляемого worktree), не `isError`-контент; `admin/*` не попадает в
  собственную телеметрию (уже обеспечено `handshake.rs`).
- **Не в scope:** MCP-инструменты (`tools::catalog()` не трогать — T19-01's `MAX_CATALOG_BYTES`);
  push/подписки (`local_rag_protocol` остаётся request/response, 02 §4.2); правки методов
  T18-08 (пока `G18` не закрыт).
- **Тесты:** контрактные снапшоты трёх ответов; `admin/*` отвечает и в `MigrationOnly`
  (супервизора нет → пустой список + явный признак, не выдуманные числа); `reconcile_now` на
  незарегистрированном worktree — типизированная ошибка; `tools/list` **не** содержит ни одного
  нового имени (явный регрессионный ассерт); бюджет каталога T19-01 не изменился.

## T20-08 — CLI `local-rag project add|remove|enable|disable|list|status`

- **Зависит от:** T20-01 (обязательно), T20-07 (для `status`/уведомления — деградирует
  корректно, если демон не запущен).
- **Спецификация:** spec 11 §6 `[SPEC surface]` + §8; `cli/mod.rs:23-31` (прямой доступ к
  `state.sqlite`, без `store.lock`); `cli/index.rs::{resolve_facts, register_new_worktree}`.
- **Результат:** семейство команд `project`, работающее **и без живого демона**: запись в
  `managed_worktree` идёт напрямую в `state.sqlite`, живому демону отправляется best-effort
  `admin/projects_reload`.
  ```
  local-rag project add <path>          # resolve → (при GlobalOnly) зарегистрировать worktree → managed
  local-rag project remove <path>       # только снятие с управления; индекс не трогается
  local-rag project enable|disable <path>
  local-rag project list [--json]
  local-rag project status [--json]     # durable + live (admin/projects_list), «daemon not running» явно
  local-rag project reindex [<path>]    # admin/reconcile_now; без демона — подсказка про `local-rag reindex`
  ```
- **В scope:** `Ambiguous` — тот же `print_ambiguous`-отказ, что у `index`/`reindex`; `add` для
  неизвестного пути создаёт repo/worktree и помечает managed **в одной транзакции**; вывод
  печатает `worktree_id` и текущий путь.
- **Не в scope:** удаление индекса/шардов при `remove` (это `rebuild`/`gc`/`worktree`);
  рекурсивный `add` каталога проектов; TUI-экран (может стать `T18`-соседом позже).
- **Тесты:** снапшоты парсинга `clap`; round-trip add → list → disable → list → remove на
  фикстурном сторе; `add` несуществующего пути — типизированный отказ; работа при выключенном
  демоне (никаких попыток спавна); `--json` стабилен по форме.

## T20-09 — Advisory-предупреждение о двойной индексации

- **Зависит от:** T20-01.
- **Спецификация:** `cli/mod.rs:23-31` («concurrent indexers … wasteful, never unsafe»);
  spec 11 §6 as-built (T15-07); spec 02 §6 (`ничто не деградирует молча`).
- **Результат:** `index`/`reindex`/`watch` при старте проверяют `is_managed(worktree_id)` и
  живость демона (`read_store_lock_file` + `fetch_welcome`, как в `cli::status`) и печатают
  **в stderr** одну строку-предупреждение с указанием `local-rag project reindex` как
  дедуплицированного пути — после чего **продолжают работу** (fail-open).
- **В scope:** формулировка предупреждения фиксируется константой и попадает в spec 11 §6
  as-built; флаг `--quiet`/переменная окружения для подавления — по усмотрению карточки, но
  подавление **не** должно быть значением по умолчанию.
- **Не в scope:** отказ выполнять команду (сломало бы CI и ручное восстановление — прямое
  нарушение решения владельца «CLI сохраняется как есть»); любые попытки межпроцессной
  блокировки.
- **Тесты:** предупреждение появляется ровно когда worktree managed **и** демон отвечает;
  отсутствует в остальных трёх комбинациях; exit-код и stdout не изменились (stdout остаётся
  контрактным); существующие тесты `index`/`reindex`/`watch` зелёные.

## T20-10 — [РЕШЕНИЕ ВЛАДЕЛЬЦА] Keep-alive: удерживает ли managed-проект демон от idle-shutdown

- **Зависит от:** T20-06; **заблокирована до явного продуктового решения** — не реализовывать
  по умолчанию.
- **Суть:** по умолчанию (`T20-05`/`T20-06`) демон с зарегистрированными, но тихими проектами
  уходит в idle-shutdown, а свежесть восстанавливается принудительным `TriggerKind::Startup` при
  следующем старте (spec 06 §1 `[FIXED]` «watcher = hint, reconcile = truth» делает пропущенные
  события безопасными по построению). Альтернатива — «пока есть хоть один managed-проект, демон
  не idle» — это **изменение `[FIXED]`-пункта spec 02 §4.3** («idle shutdown only when **all**
  hold») плюс новый ключ в spec 02 §3.1 (у которого закреплён pinned-тест
  `default_matches_spec_toml`).
- **Аргументы против по умолчанию:** демон спавнится прокси по требованию; окно «демон мёртв»
  почти совпадает с окном «потребителей нет»; вечно живой демон удерживает ONNX-сессии и шарды.
- **Аргументы за:** шумный watcher уже сейчас может почти непрерывно держать `JobGuard` и де-факто
  пиннить демон — тогда явный, наблюдаемый флаг честнее, чем случайный побочный эффект.
- **Статус в плане:** зафиксировать открытым пунктом в `DEVIATIONS.md` со ссылкой на это описание;
  реализация — отдельной задачей после решения. Решение «нет» — задокументировать отказ, карточка
  закрывается без кода.

## G20 — Сверка daemon-managed indexing

- Перечитать целиком: ADR-0009; spec 02 §1/§3.3/§4.1/§4.3/§4.4/§5/§6; spec 06 §1/§3; spec 03
  §2.1; spec 11 §6/§8; spec 12 §1. Построить трейс `требование → код → тест` по `T20-01…T20-09`.
- Проверить, что `D-043` закрыт фактическим кодом: `L2.write` берётся в проде, реестр блокировок
  **один** на демон (grep на `WorktreeLockRegistry::new()` — не более одного продового вызова),
  `[OPEN]` про eviction имеет записанную диспозицию.
- Проверить неизменность соседних контрактов: `tools/list` не вырос (T19-01's бюджет),
  `local-rag index/reindex/watch` сохраняют поведение и exit-коды (T15-07), spec 02 §4.3 остаётся
  `[FIXED]` в исходной формулировке (T20-10 не реализована без решения), нет ambient current
  project (spec 02 §3.3).
- **Обязательный сценарий, не покрытый ни одним существующим тестом** (см. «Диагноз» п. 2):
  живой демон управляет worktree **и одновременно** запускается внешний `local-rag reindex` того
  же worktree — проверить, что стор остаётся валидным (`local-rag doctor` чист), поиск отвечает,
  шард проходит validate-on-open, и итоговое активное поколение — одно. Если сценарий окажется
  небезопасным — регистрировать deviation, а не считать предпосылку доказанной.
- Многопроектный dogfood: два-три реальных репозитория зарегистрированы, правки в каждом,
  `search_code` в каждом отдаёт свежие результаты; замерить и записать в evidence: время старта
  демона с N проектами, число ONNX-сессий процесса (ожидание — 2), пиковую RSS.
- Все focused-тесты группы + workspace-команда качества из `CONTRIBUTING.md`.
- Зафиксировать `PASS` / `PASS after D-NNN` / `BLOCKED` и evidence в `PROGRESS.md`.
