# local-rag v2 — design (rev 6)

> Ревизия после внешнего ревью rev 5. rev 5 закрыла crash-корректность projection switch
> и класс бага «context/path в content-addressed артефакте»; rev 6 закрывает
> **durable-семантику проекции** (validate-on-open вместо недоказуемого durable barrier),
> **двухосевой projection state** (generation × model space), **stable worktree identity**
> (тот же класс path-протечки, но на уровне durable ID), упрощает ingestion до
> **spool-only** и добавляет **quality-гейты памяти** (до rev 6 все memory-гейты были
> про plumbing). Schema-констрейнты и dedup приведены к реальной семантике SQLite.

---

## TL;DR

Локальный co-located MCP-сервис для Claude Code: **persistent memory**, **semantic
code search**, **observations**. Rust, npm-дистрибуция, без обязательных внешних
демонов. `state.sqlite` — источник истины; `cache.sqlite` и dense-проекция —
перестраиваемые, **независимо валидируемые при открытии** кэши. Известные проблемы v1
(identity / projection / hook consistency) имеют явные invariants и acceptance tests
(§15) — формулировка «все проблемы решены» заменена на проверяемую.

---

## Принцип бюджета корректности

Единственный невосстановимый актив системы — **память** (envelopes, entries, audit):
для неё — полная транзакционная строгость, no-loss, идемпотентность. **Код-индекс
перестраиваем по построению**: его режим — «detect on open → rebuild on doubt», а не
доказательство durability стороннего движка. Протоколы §5–§7 следуют этому принципу:
детектируемость вместо durable barriers, один ingestion path вместо ACK-протокола.

---

## Scope и зафиксированные решения

- **Только Claude Code.**
- **Co-located daemon** + тонкий stdio MCP-proxy на сессию + **spool-only hook
  ingestion** (§7): hooks никогда не общаются с daemon напрямую.
- **Dense backend — за `ProjectionStore`-трейтом**; семантика **per-worktree shard**
  сохраняется для любого бэкенда. Кандидаты: Qdrant Edge / usearch / brute-force по
  `embedding_cache`. Выбор — по **сравнительному спайку** (шаг 11), не по умолчанию.
- **Projection protocol — Variant A + write-ahead + validate-on-open** (§5): проекция
  всегда трактуется как недоверенный кэш; durable barrier от бэкенда не требуется.
- **Хранилища разделены:** `state.sqlite` (durable) + `cache.sqlite` (rebuildable,
  независимо валидируемый) + `projection/` + `spool/` + `models/` (§2).
- **Migration-ready** identity model (§12).
- **`specification.md` → `specification-v1.md`** (behavioral archaeology, не норматив);
  новая спека пишется executable-level по мере шагов (§18).
- **Targets:** `darwin-{x64,arm64}`, `linux-{x64,arm64}`, `win32-x64`; `win32-arm64`
  отложен.

---

## 1. Две лестницы идентичностей + системный аудит

**Код:** `content blob ≠ parsed unit ≠ generation occurrence ≠ vector representation ≠ generation`.
**Память:** `raw observation ≠ evidence ≠ durable memory ≠ recalled context`.

**Системное правило (аудит, расширен в rev 6):** ни одна строка, которая **шарится по
контенту** (`content_blob`, `file_revision`, `parsed_unit`, `embedding_cache`
content-subject), не несёт **ни одного** context/path-specific поля. Всё
path/generation-зависимое живёт только в `generation_unit_occurrence`,
`resolved_graph_edge`, `generation_file` и FTS-проекции occurrences.
**Дополнение rev 6: ни один durable ID не является производным от path.** rev 5 сама
нарушала это в `worktree_id = hash(path)` — path-derived hash допустим только как
lookup key, никогда как FK-цель для durable state (§3). Это тот же класс бага
(chunk_id→blob; occurrence→generation; path в parsed_unit; FTS по parsed_unit;
worktree_id←path), закрытый теперь и на уровне identity.

---

## 2. Хранилища

```
state.sqlite (source of truth, компактный backup):
  repositories, repository_paths, worktrees, worktree_paths, generations
  worktree_projection_state, model_spaces, representations
  file_revisions (+ source_blob), content_blobs (identity+metadata), parsed_units
  generation_unit_occurrences, generation_files, skipped_files
  unresolved_references, resolved_graph_edges
  observation_envelopes, observation_payloads, observation_paths
  memory_entries, memory_evidence, pending_memory_candidates, candidate_evidence
  processing_cursors, consolidation_runs, audit_events
  schema_migrations

cache.sqlite (rebuildable, очищаемый без риска, независимо валидируемый):
  embedding_cache (векторы как BLOB)
  normalized_text_cache        # производная от source_blob; в state — только hash-identity
  fts_occurrences (SQLite FTS5) + fts_projection_head

projection/  per-worktree dense shards (активная проекция; формат зависит от backend)
spool/       per-session hook spool segments — единственный ingestion path (§7)
models/      скачанные веса
```
Инварианты: потеря `cache.sqlite`/`projection/` не теряет ничего — восстановимы из
`state.sqlite`. **`state.sqlite` содержит exact `source_blob` для каждого searchable
файла** (строгий инвариант §10) → поколение восстановимо, даже если файл на диске уже
изменился. **Запрещены атомарные транзакции через `ATTACH state+cache`**: в WAL-режиме
cross-DB атомарность при крэше не гарантируется; cache — независимо валидируемая
materialized view со своим head (§6).

SQLite pragmas/policy: `WAL`, `foreign_keys=ON`, `busy_timeout`, продуманный
`synchronous`, **единая bounded global write-queue** (у SQLite один физический
writer; per-worktree writers сходятся в неё), WAL checkpoint policy, batched
`last_used_at`, cache eviction по размеру.

---

## 3. Идентичность: repository registry / worktree / generation

```
repository:      repo_id = стабильный UUID; git_remote_fingerprint?
                 (credentials удалены, SSH/HTTPS нормализованы, хранится hash — §10);
                 created_at, last_seen_at
repository_path: repo_id, observed_path, is_current, first_seen_at, last_seen_at
                 # единственный источник current path (canonical_path из repository убран —
                 # два источника истины недопустимы)

worktree:
  worktree_id = stable UUID                # durable identity, НЕ производная от path
  repo_id, kind: main | linked | non_git
  current_generation_id                    # constraint: generation принадлежит этому же
                                           # worktree и находится в допустимом state
  state, created_at, last_seen_at

worktree_path:
  worktree_id, observed_canonical_path, is_current
  path_fingerprint                         # lookup accelerator, не identity
  first_seen_at, last_seen_at

generation:
  generation_id = глобальный UUID/row
  worktree_id, generation_number           # unique(worktree_id, generation_number)
  state: building | projection_ready | active | retiring | failed
  created_at
```
Move каталога → `local-rag repo attach <repo_id>` переустанавливает связь main
worktree; для неоднозначных linked worktrees — явный attach (common-dir/admin-dir
fingerprint допустим как подсказка, не как единственный ID). Memory scoped к `repo_id`
(или global/worktree); code index — к `worktree_id`+`generation_id`. Никаких
process-global `current_project`/`current_branch`.

Normalization: symlinks, drive letters, UNC, case-insensitive FS, deleted/recreated
dirs, non-git roots; remote URL не единственный ID. **Пути в identity — canonical
internal form (slash/case); display path хранится отдельно** (исходный регистр на
case-insensitive FS не теряется).

`retiring` — состояние для GC/audit, **не для routing**: per-worktree read/write lock
гарантирует, что к моменту final commit старых readers нет.

---

## 4. Схема данных (canonical)

### Код-сторона
```
file_revision:                       # path-independent, шарится
  file_revision_id
  content_hash, parser_fingerprint   # unique(content_hash, parser_fingerprint)
  source_blob                        # exact original bytes (opt. compressed)
  source_encoding, newline_style: lf | crlf | mixed
  source_size, created_at
```
`parser_fingerprint` (вместо расплывчатого `parse_version`) включает: language
selection, grammar/version, tree-sitter query version, chunk policy/version,
normalization, влияющую на границы юнитов. Следствие фиксируется явно: byte-identical
source под `.c` и `.cpp` — **разные** file revisions (language выбирается по
extension/path). **Spans — всегда byte offsets в exact `source_blob`**; неподдерживаемая
encoding → skip файла (никакого transcoding без offset mapping).

```
content_blob:                        # path-independent, шарится; identity + metadata
  blob_id = hash(algo_version, language, normalization_version, normalized_text)
  # сам normalized_text — в cache.sqlite (производная от source_blob, не durable)

parsed_unit:                         # path-independent, шарится
  unit_id
  file_revision_id
  unit_kind: symbol | file | config_section | text_section | fallback_chunk
  syntax_locator                     # SyntaxLocator БЕЗ path; canonical serialization
  blob_id, span                      # byte offsets в source_blob
  local_name?, kind?, parent_unit_id?
  unique(file_revision_id, unit_kind, syntax_locator, span)

generation_file:                     # membership; неизменённые файлы reuse revision
  generation_id, normalized_path, file_revision_id
  unique(generation_id, normalized_path)

skipped_file:                        # metadata о причине skip; occurrences НЕ создаются (§10)
  generation_id, normalized_path
  reason: binary | lfs | huge | secret | ignored | encoding
  content_hash?

generation_unit_occurrence:          # path-dependent
  occurrence_id = hash(generation_id, normalized_path, unit_id)   # deterministic
  generation_id, normalized_path, unit_id
  qualified_name?, context_hash?
  unique(generation_id, normalized_path, unit_id)

unresolved_reference:                # parse-local (per file_revision)
  file_revision_id, source_unit_id, reference_text, reference_kind
resolved_graph_edge:                 # per generation, на occurrence-IDs
  generation_id, src_occurrence_id, dst_occurrence_id, edge_kind
  unique(generation_id, src_occurrence_id, dst_occurrence_id, edge_kind)
```
Deterministic IDs (`occurrence_id`, projection point IDs) стабильны при retry
reconcile и не зависят от порядка вставки rows. **Все content/manifest/subject-хэши —
domain-separated и version-tagged.**

### Projection state — двухосевой (generation × model space)
```
worktree_projection_state:
  worktree_id
  active_generation_id,    active_model_space_id
  projected_generation_id, projected_model_space_id
  target_generation_id?,   target_model_space_id?
  projection_op_id?, projection_schema_version
  status: clean | updating | dirty | rebuilding
  last_error?, updated_at

ProjectionVersion = (worktree_id, generation_id, model_space_id, projection_schema_version)

model_space:                         # registry модели + build state, НЕ deployment state
  model_space_id, representation_id
  state: building | projection_ready | active | retiring | failed
  coverage                           # expected/ready set по каждому обязательному
                                     # representation kind, не только счётчик failed
  benchmark_result

representations:                     # registry RepresentationKey, каноническая сериализация
  representation_id
  kind: code_raw | code_context | structural_description | memory
  representation_version, normalization_version, model_id, dimensions, distance_metric
```
Deployment (какой model space активен/спроецирован в каком worktree) живёт **только**
в `worktree_projection_state`. **Активация модели — per-worktree** + `default_model_space`
для новых/открываемых worktrees; offline/dormant worktree мигрирует при следующем
открытии (глобального write barrier нет).

Локаторы разведены:
```
SyntaxLocator:      language, syntax_path|local_ordinal, signature_fingerprint, blob_id   # без path
OccurrenceLocator:  normalized_path, qualified_name, SyntaxLocator
```

### embedding_cache (в cache.sqlite)
```
embedding_cache:
  subject_kind: content_blob | occurrence_context | memory_entry
  subject_hash                       # full-size, domain-separated, algo/version-tagged
  representation_id                  # FK на registry — никаких дублей из-за сериализации
  dimensions, vector_f32_blob, byte_size, checksum, created_at, last_used_at
  unique(subject_kind, subject_hash, representation_id)
```
(`content_blob`-эмбеддинги шарятся; `occurrence_context`-эмбеддинги per-occurrence —
это корректно, т.к. context path-зависим по определению.)

### Память-сторона
```
observation_envelope:                # durable
  observation_id
  source_event_id                    # event-specific identity (§8.3)
  dedup_key?                         # partial unique index WHERE dedup_key IS NOT NULL —
                                     # только для событий со stable identity;
                                     # best-effort fingerprints НЕ под UNIQUE
                                     # (два легитимных одинаковых prompt/Stop легальны),
                                     # их дедуп — bounded retry window
  payload_hash
  received_seq                       # монотонный, транзакционный → основа cursor
  event_type, evidence_kind, trust, source_timestamp?
  repo_id, worktree_id?, session_id, agent_id?, turn_id?, batch_id?
  commit?, short_evidence_excerpt?

observation_path:                    # join table вместо paths[] (query/indexing)
  observation_id, normalized_path

observation_payload:                 # короткий TTL
  observation_id, redacted_payload, byte_size, expires_at

memory_entry:
  id, kind: fact | decision | convention | procedure | task | question | hypothesis
  state                              # kind-specific переходы (§8.1); kind = origin, неизменяем
  text, canonical_key?
  scope_kind: global | repository | worktree
  scope_owner_id NOT NULL            # global → фиксированный singleton UUID;
                                     # repository → repo_id; worktree → worktree_id
                                     # (в SQLite NULL в unique index различны — nullable
                                     # scope-колонки дедуп не обеспечивают)
  confidence, importance, valid_from_tree?, last_verified_tree?, supersedes?
  created_at, updated_at
  unique(scope_kind, scope_owner_id, canonical_key)
  # scope_repo_id для worktree-scope избыточен: repo выводится через worktree

memory_evidence:                     # FK → observation_envelope (переживает TTL payload)
  memory_id, observation_id
  evidence_kind: user_statement | tool_result | test_result | code_state | model_claim
  session_id, agent_id?, commit?

pending_memory_candidate: candidate_id, proposed_operation, conflicts[], created_at, review_state
candidate_evidence:       candidate_id, observation_id   # FK, durable provenance,
                                                         # не embedded snapshot
processing_cursor:        session_id -> last_consolidated_received_seq
consolidation_run:        run_id, session_id, from_received_seq, to_received_seq,
                          router_version, state: pending|running|applied|failed, lease_until?
audit_event:              versioned изменения memory
```

---

## 5. Projection protocol — write-ahead + validate-on-open (per-worktree shard)

**Принцип: dense-проекция — всегда недоверенный кэш.** Write-ahead делает crash до
final commit видимым; **ProjectionHead + validate-on-open** делает видимым всё
остальное — включая потерю durability бэкенда *после* clean commit. Durable barrier /
distributed-transaction-семантика от бэкенда **не требуется** — требуется только
детектируемость расхождения. (SQLite `clean` сам по себе не доказывает, что бэкенд
физически durable; вместо доказательства — проверка при каждом open.)

**Shard: per-worktree.** Нет tenant-фильтра, чистая active-only семантика,
изолированный rebuild одного worktree. Variant A убирает **generation**-фильтр;
per-worktree shard убирает и **tenant**-фильтр → filtered-HNSW не на критическом пути
(но включить в spike). Цена — shard-manager с LRU-eviction (в co-located число
активных worktree мало).

**Deterministic point IDs:** `projection_point_id = hash(worktree_id, occurrence_id,
model_space_id, representation_kind)` → повторный upsert перезаписывает, повторный
delete безопасен.

**ProjectionHead** (в shard, пишется последней операцией delta/rebuild):
```
ProjectionHead:
  worktree_id, generation_id, model_space_id
  projection_op_id, projection_schema_version
  point_count
  manifest_hash        # digest deterministic expected point ID set
```

**Switch** (generation-switch и model-space-switch — один протокол, сериализованы
единым per-worktree writer; нельзя одновременно применять обе оси к одному worktree):
```
1. Подготовить target (generation N+1 и/или model space B) в state.sqlite + cache
   (векторы из cache, без переэмбеддинга неизменённого контента).
2. SQLite tx ДО backend:
     status=updating, target tuple (generation, model_space), projection_op_id=uuid
3. Захватить per-worktree WRITE lock. Desired-set reconciliation:
     expected = expected_point_ids(target tuple)      # детерминированно из state.sqlite
     upsert (expected \ existing) + changed
     delete (existing \ expected)
     записать ProjectionHead
   # не императивный «повтор списка команд» — recovery после неизвестного partial
   # delta не требует знания истории
4. ОДНА SQLite tx ПОСЛЕ backend:
     active tuple = target; projected tuple = target; target = NULL; status=clean
     generation N+1 -> active; N -> retiring
5. Освободить lock. Delayed GC поколения N.
```

**Open/startup validation (до допуска search, при каждом открытии shard):**
```
status != clean                            -> rebuild
projected tuple != active tuple            -> rebuild
ProjectionHead missing / op_id mismatch    -> rebuild
tuple в head != clean tuple в SQLite       -> rebuild
point_count или manifest_hash mismatch     -> rebuild
```
**Full rebuild — recovery default; delta — normal fast path.** Для локального shard
rebuild дёшев по построению (векторы из `embedding_cache`).

**Fault-detection matrix** (каждый случай обязан приводить к dirty→rebuild при open):
kill процесса между любыми шагами; потеря/обрезание shard WAL после clean commit;
удаление части points при сохранённом каталоге; отсутствующий/старый head; совпавший
point_count при отличающемся ID set; ошибка последнего upsert/delete; отказ flush/sync.
Доказываются **два свойства**: (a) любое расхождение детектируется при open;
(b) rebuild корректен и идемпотентен. Это сжимает fault-injection matrix rev 5:
не «recovery из каждого промежуточного состояния», а «детекция + один recovery path».

**Shard lifecycle следует registry lifecycle:** attach/move без создания второго shard;
remove/detach с grace period; GC orphan directories; quarantine повреждённого shard;
bounded concurrent opens и background jobs; cancellation rebuild при закрытии/удалении
worktree; disk budget между shards.

**optimize policy** — по метрикам (deleted/stale ratio, segment count, disk
amplification, idle time, max query-latency impact), не после каждого reconcile.

---

## 6. Worktree reconcile: watcher = подсказка, reconcile = истина

Watcher/`.git/HEAD` — триггеры; `(mtime,size,file-id)` — fast-path cache. Строгий
reconcile после startup/checkout/rebase/overflow и периодически.
```
trigger -> schedule reconcile -> fast stat scan -> content-hash подозрительных
  -> изменённые: parse -> new file_revision(+source_blob) -> parsed_units
  -> неизменённые: reuse file_revision_id (structural sharing)
  -> build generation N+1 (generation_file + occurrences + skipped_files)
  -> write-ahead projection switch (§5)
  -> delayed GC
```
checkout ≠ «ноль работы» (нет переэмбеддинга известного контента, но стоимость ∝
чтению/проверке изменившегося дерева + обновлению occurrences/графа); rename бесплатен
только для content-embedding. Один writer на worktree; lockfile на store.

**Hybrid search берёт per-worktree READ lock на весь pipeline** (иначе lexical читает
N, dense — промежуточный N+1):
```
per-worktree read lock -> resolve active tuple -> FTS5 (по occurrences активного
generation) -> dense -> RRF -> graph/context enrichment -> release
```

**FTS — независимо валидируемая materialized view** (живёт вне canonical transaction):
```
fts_projection_head (в cache.sqlite):
  worktree_id, generation_id
  lexical_schema_version, tokenizer_version
  occurrence_count, manifest_hash
```
missing/mismatch (потеря cache после switch; частичный FTS build; версия схемы/
tokenizer не совпала с binary) → rebuild FTS до ready **либо** явный degraded
dense-only response с diagnostic flag. **Пустой FTS никогда молча не считается
корректным lexical result.** Read lock предотвращает смешивание generations, но не
детектирует неполную lexical projection — детектирует head.

**Retention / GC canonical source:**
```
pin roots:  active + building/projection-target generations
            последние K retired generations или retention T (rollback/debug)
            memory evidence / audit / export ссылки
            активные rebuild/embedding job leases (временный pin)
sweep:      unreferenced file_revisions -> mark-and-sweep под writer queue
метрики:    source bytes / current worktree bytes; backup size;
            VACUUM/checkpoint policy по метрикам, не по расписанию
```

---

## 7. Process topology & lifecycle — spool-only ingestion

```
Claude Code  -> thin stdio MCP proxy -> shared daemon -> state/cache.sqlite + shards + workers
Claude hooks -> atomic append в per-session spool segment -> daemon tail/import
```

**Hook никогда не общается с daemon напрямую.** Единственный ingestion path — durable
spool append. Это устраняет ACK-протокол целиком (нет состояния «daemon принял bytes,
но упал до commit» — durable-момент события определён как успешный append) и убирает
dual-path identity: в rev 5 spool и прямой IPC были обязаны вычислять бит-в-бит
одинаковый source identity — два кодовых пути с таким контрактом расходятся всегда.

```
hook:    redaction ДО записи -> source identity вычисляется при записи ->
         atomic append в per-session segment
         # per-session сегменты: O_APPEND не гарантирует неперемешивание больших
         # записей между процессами
         0600/ACL, size caps, versioned format, rotation
daemon:  notify/debounce tail -> идемпотентный import (dedup по source identity, §8.3)
         -> advance durable import cursor -> truncate/rotate только после committed import
инвариант: durable-момент события = успешный atomic append; сегмент не усекается
           до committed import
```
Цена — секунды задержки до консолидации; для памяти безразлично. Fail-open сохранён
тривиально: недоступность daemon вообще не влияет на hook path.

**Lifecycle (зафиксировано для первой реализации):**
```
startup:   proxy пытается connect; нет daemon -> запускает platform binary;
           ждёт readiness + version handshake; retries connect
endpoint:  Unix domain socket (macOS/Linux); named pipe | loopback TCP (Windows)
           # endpoint нужен только MCP proxy; hooks идут через spool
ownership: один daemon на OS-user; global store lock; instance UUID + PID (не только PID-file)
shutdown:  idle timeout только если нет MCP sessions, pending spool import,
           index/consolidation jobs
```
**Daemon определяет configuration ownership и routing, не только packaging.** До
daemon-этапа определить: физическое расположение global store на каждой ОС; передачу
worktree root/repo context в каждом request; выбор config при нескольких repositories;
конфликты model/data-policy между repos; permissions для shared machine/container;
protocol version negotiation + совместимость spool-форматов; binary upgrade при
удерживаемом старым daemon migration lock. Также: proxy/daemon version mismatch;
stale socket/lockfile recovery; permissions/ACL endpoint; orphan cleanup.

Readiness-критерий implementation-ready — **core storage/parser/reconcile**; daemon
lifecycle закрывается на своём этапе (§17).

---

## 8. Три столпа

### 8.1 Memory
- **`kind` = происхождение, неизменяем; `state` = текущая подтверждённость.** Confirmed
  hypothesis остаётся `kind=hypothesis, state=confirmed` (recall/router трактуют как
  высокий trust); промоушен в `fact` — только явным `supersede`, не мутацией kind.
  Переходы: task/question `active→resolved|retracted`; hypothesis
  `active→confirmed|rejected|superseded`; fact/decision/convention/procedure
  `active→superseded|retracted`.
- **Confidence LLM ≠ вероятность.** Policy-score из source reliability + explicit user
  decision + tool/test evidence + повторность + согласие с кодом − противоречия −
  model-only − stale.
- **Auto-save только для explicit durable decision/instruction.** Вопрос/brainstorm/
  отрицание/временное предложение → pending-candidate или hypothesis. Router-prompt и
  тесты различают «мы решили X» / «а что если X?» / «не использовать X».
- **Memory-quality benchmark (новое в rev 6):** размеченный fixture-set
  observation-стримов → ожидаемые memory ops (`create|reinforce|supersede|noop`),
  включая различение decision/hypothesis/negation и RU/EN-смешанные транскрипты.
  Precision/recall consolidation-роутера — acceptance gate (§15) наравне с 49-query
  code-search benchmark. Без этого memory-столп имеет критерии только на plumbing.
  Особенно критично при `data_policy=local_only`: роутер работает на локальной модели.
- **Транзакционность:** операция (`create|reinforce|resolve|supersede|retract|noop`)
  + evidence + audit в одной SQLite tx; preconditions; ответ с version/audit ID;
  повтор идемпотентен. `reinforce` добавляет evidence, может поднять confidence, текст
  не меняет; edit → новая версия через audit; user-edit vs router-edit различаются.
- **Recall v0:** scope → relevance → lifecycle → token budget; пусто → пустой
  additionalContext. **Relevance backend v0: FTS + brute-force cosine по active memory
  entries** (bounded cardinality; тот же `representation_id`; вектора из
  `embedding_cache`), за трейтом; переход в ANN — по метрике cardinality/latency, не
  по умолчанию. Model-space migration покрывает memory-representation так же, как code.
  Full recall (отложен): + tree-validity/provenance → evidence trust → weak
  recency/importance → diversity/dedup, top 20–50.
- **Recalled memory — недоверенные данные** (§10).
- Review-инструменты: `list/approve/reject/edit_memory_candidate`, `edit_memory`,
  `retract_memory`, `merge_memories`, `inspect_memory_evidence`.

### 8.2 Semantic code search
- Индексируем **document units всех kind** (symbol/file/config/text/fallback) — иначе
  регресс parity vs v1.
- **content vs context representation** решает benchmark.
- **Lexical = SQLite FTS5** по **occurrences** (path-dependent) с code-aware
  препроцессингом (identifier + lowercase, camel/snake split, qualified-name/path
  components, signature tokens); dense (per-worktree shard) + lexical → app-side RRF;
  весь read под per-worktree read lock; FTS-валидность — через `fts_projection_head` (§6).
- **Symbol graph = occurrence identity** (`OccurrenceLocator`); edges на occurrence-IDs;
  cross-generation identity — эвристика, не корректность. Различать heuristic usages /
  syntax-resolved / LSP.
- **LLM из per-save hot path убран**; описания — только если выигрывают benchmark,
  через async drainer. Transcript-адаптер — diagnostic opt-in, low-trust, off по
  умолчанию.

### 8.3 Observations
- **Capture set:** `SessionStart`, `UserPromptSubmit`, `PostToolUse`,
  `PostToolUseFailure`, `Stop`, `SubagentStop`, `SessionEnd`.
- **Доставка at-least-once через spool-only path (§7).** Event-specific identity:
  PostToolUse = `session+tool_use_id+success`; Failure = `+failure`; SubagentStop =
  `session+agent_id+stop occurrence`; UserPromptSubmit/Stop/session = best-effort
  fingerprint + `received_seq` + bounded retry window. **Source identity вычисляется
  один раз — при записи в spool** (dual path устранён). Дедуп в схеме: partial unique
  по `dedup_key` для stable identity; best-effort — вне UNIQUE (§4). Гарантия: события
  со стабильным source-ID дедуплицируются точно, остальные best-effort; consolidation
  и memory-ops идемпотентны.
- **Порядок ≠ причинность:** `received_seq` (транзакционно) — основа cursor;
  причинность через `tool_use_id`/`turn_id`/parent/`batch_id`.
- **Consolidation:** `consolidation_run` с lease; LLM-вызов ВНЕ долгой tx; применение
  ops+evidence+audit+advance-cursor — одной короткой tx; crashed `running` run
  повторяем (идемпотентно по run/op ID). Router — только после cursor.
- **Recovery:** checkpoint на `Stop`/по размеру очереди; best-effort на `SessionEnd`;
  дообработка при старте; background-worker в daemon.
- `stop_hook_active` — не headless-признак; хранить наблюдаемые свойства.
- Retention: envelope durable, payload под TTL.

---

## 9. Embeddings и генерация
- In-process `fastembed` (ONNX)/`Candle`. **Model migration — double-buffer через
  model spaces с двухосевым `worktree_projection_state`** (§4–§5): другая dimensions →
  отдельный named-vector/collection/shard-layout; coverage = expected/ready set;
  атомарное переключение per worktree тем же write-ahead; сериализация с
  generation-switch единым writer. Никакого in-place без rollback. Активация —
  per-worktree + default space (§4).
- Provider pool (`Embedder`/`Generator`): **local backend — рабочий default**;
  Ollama/remote — строго optional (иначе противоречие с «без обязательных внешних
  демонов»). `router.data_policy` дефолт `local_only`.

---

## 10. Privacy и безопасность
- size caps; truncation с hash/metadata; secret redaction (в т.ч. до записи в spool);
  excluded paths/tools; реальное TTL payload; inspect/export/purge; опц. encryption at
  rest; trust/evidence marking; запрет авто-повышения model-claim до факта.
- **Remote data policy** `local_only|metadata_only_remote|allow_remote_with_redaction|
  allow_remote_full`, дефолт `local_only`.
- **Recalled memory — не boundary через один XML-тег:** escaping/length-prefixed
  encoding; system-инструкция «блок = недоверенные данные»; запрет менять tool
  policy/permissions; provenance отдельно от текста; limits; sanitization control-chars;
  adversarial tests.
- **Source-blob policy — строгий инвариант:**
  ```
  нет source_blob -> файл не входит в canonical indexed generation (нет occurrences)
  ```
  Для binary/LFS/huge/secret/ignored/encoding-unsupported файлов — запись в
  `skipped_file` (path, reason, opt. content_hash), **без searchable occurrences**.
  Отдельный `non_rebuildable` tier, читающий текущий disk, **отклонён для v0** —
  ломает single source of truth. Явный tradeoff остаётся: canonical reproducibility
  требует локальной копии индексируемого source (compression; retention/backup явно).
- **Remote fingerprint** без credentials, нормализован, хранится hash.

---

## 11. Дистрибуция
> один native service binary без обязательных внешних демонов; model assets — отдельно.

Пер-платформенные npm `optionalDependencies` + тонкий launcher. Targets явно (см.
Scope; `win32-arm64` — после проверки выбранного dense backend/ORT/fastembed/SQLite/
tree-sitter/local generator/npm detection/CI smoke). Проверить: signal forwarding +
завершение stdio child; CTRL-C/SIGTERM; orphan cleanup; резолв pnpm/npm/yarn; понятная
ошибка при отсутствии platform-пакета; offline после `local-rag init --download-models`;
checksum/manifest + атомарная загрузка весов; ORT bundling до финальной CI-матрицы.
Веса не в npm.

---

## 12. Migration framework
Не «ноль миграций», а «не менять фундаментальную идентичность повторно». С первого дня:
`schema_migrations`, app compatibility check, resumable migrations, backup/rollback
перед destructive, migration tests на fixtures. Отложенные фичи — additive.

---

## 13. Стек / крейты

| Задача | Крейт |
| --- | --- |
| Durable + cache store, FTS5, BLOB | SQLite (`rusqlite`/`sqlx`), два файла + migrations |
| Dense projection | `ProjectionStore` trait; кандидаты: `qdrant-edge` \| `usearch` \| brute-force по `embedding_cache` — выбор по сравнительному спайку (шаг 11) |
| FS events / gitignore | `notify` / `ignore` |
| Git | `git2` |
| Парсинг | `tree-sitter` + грамматики |
| Эмбеддинги | `fastembed` (ONNX) / `Candle` |
| Генерация (in-process) | один из `llama-cpp-2`/`mistral.rs`/`kalosm` |
| Транскрипт (diagnostic opt-in) | `claude-code-transcripts` |
| Дистрибуция | `cargo-dist`, `cargo-zigbuild` |

---

## 14. Что сохранить из v1 (behavioral contract)
Hook fail-open; recall через `additionalContext`; пустой recall без текста;
deterministic formatting; provider primary/fallback + retry; async description
backfill; parser/resolver tests; gitignore; parent/child chunks (если benchmark);
code-search benchmark; изоляция индексации от MCP path.

**v1 tests → implementation-neutral fixtures до переписывания кода:** входное
дерево / event stream / query → ожидаемое поведение, не ожидаемая внутренняя
payload-схема vector store.

**Не переносить как дизайн:** branch tags/manifests внутри vector store; vector store
как canonical store; mutable process-level current branch/project; in-place
re-embed/migration коллекций; вывод headless из `stop_hook_active`; разделение agent
memory по физическим collections.

---

## 15. Acceptance gates (provisional; числа после baseline)
```
quality:        MRR не хуже baseline v1 более чем на X; Recall@5 >= Y
memory-quality: precision/recall consolidation-роутера на fixture-set >= P/R
                (decision vs hypothesis vs negation; RU/EN mixed)          # НОВОЕ
latency:        warm search p95; one-file reconcile p95; branch-checkout reconcile
resources:      idle RAM; index bytes/symbol; embedding cache budget;
                source bytes / worktree bytes
reliability:    crash/restart: ЛЮБОЕ расхождение проекции детектируется при open ->
                rebuild без manual clear; watcher overflow ловится строгим reconcile;
                событие со stable identity не теряется после spool append
                (kill daemon в любой точке import)
consistency:    validate-on-open matrix (§5) полностью зелёная; hybrid lexical+dense
                никогда не мешает generations; пустой/частичный/устаревший FTS
                детектируется fts_projection_head
sharing:        изменение одного файла не дублирует units неизменённых файлов
idempotency:    повтор spool event / retry reconcile -> нет duplicate memory operation
                / duplicate rows (deterministic IDs)
rebuild:        удалённая dense-проекция/cache полностью восстанавливается из state.sqlite
```
Fault-injection suite: два свойства §5 (детекция при open; корректный идемпотентный
rebuild) + corruption-cases как detection-тесты + spool kill-matrix (§7).

---

## 16. MVP (v0) — идентичности/протоколы фиксированы, логика минимальна
Rust binary + минимальный CC plugin/launcher; `state.sqlite`+`cache.sqlite` +
migration framework; repo registry + **stable worktree UUID** + `generation` + locking;
tree-sitter 2–3 языка; `file_revision`(+source_blob+parser_fingerprint) +
`parsed_unit` + occurrence + structural sharing + `skipped_file`; authoritative
reconcile + write-ahead switch + **validate-on-open** (per-worktree shard); один
embedding model + representations registry; `embedding_cache` (BLOB); **dense leg =
простейший backend, проходящий benchmark** (возможно brute-force — решает спайк шага
11); FTS5(occurrences) + `fts_projection_head` + RRF под read-lock; 49-query benchmark
baseline; **spool-only ingestion** + envelope/payload + идемпотентная консолидация по
cursor с lease; **memory-quality fixture-set + гейт роутера**; recall v0
(FTS + brute-force cosine); memory state-machine + evidence + `list/approve/reject/edit`;
fault-injection: detection matrix + rebuild-корректность.

**Отложено (additive):** LLM descriptions; reranker; тонкий evidence-scoring; полный
recall; ANN для memory; несколько генераторов; cross-generation matching; LSP graph;
multi-harness; FreeBSD; win32-arm64.

---

## 17. Порядок реализации
**Можно начинать сейчас (core storage, до dense backend) — параллельно rev-циклам:**
1. Migration framework.
2. Repository/worktree registry (**stable worktree UUID + worktree_path**) + path
   normalization.
3. Exact file revisions / `source_blob` / `parser_fingerprint` / `skipped_file`.
4. `parsed_unit` + parser fixtures + uniqueness constraints.
5. Generation membership + structural-sharing тесты + deterministic occurrence IDs.
6. Строгий reconcile без embeddings + retention/GC pin roots.
7. Fake projection backend + fault-injection: detection matrix + rebuild-тесты.

**Перед реальным dense backend:**
8. Write-ahead + validate-on-open protocol (§5) на fake backend, включая двухосевой
   `worktree_projection_state`.
9. `fts_projection_head` + degraded-mode semantics.
10. Shard strategy (per-worktree) + hybrid read-locking.

**Затем:**
11. **Сравнительный спайк dense backend:** Qdrant Edge vs usearch vs brute-force по
    `embedding_cache`. Метрики: warm search p95; RAM/shard; open/close cost; startup
    большого registry; поведение LRU; durability/validate-on-open семантика; платформы
    (win32). **Выбор backend фиксируется здесь**, не раньше.
12. Embedding cache / representations registry / model spaces + per-worktree activation.
13. FTS5 + dense RRF + benchmark baseline.
14. Spool-only ingestion + observations + cursor/lease + **memory-quality fixture-set**.
15. Memory state machine + evidence + review tools + **гейт роутера**.
16. Description leg / reranker — только после baseline.
17. Daemon lifecycle (+ store/config discovery, §7) + platform packaging.

---

## 18. Решения

**Решено (rev 6):** stable worktree UUID + `worktree_path` (path hash — только lookup);
двухосевой `worktree_projection_state` (generation × model space × schema version);
**validate-on-open вместо durable barrier** (проекция = недоверенный кэш);
desired-set reconciliation, full rebuild = recovery default; **spool-only ingestion**
(без IPC ACK, без dual-path identity); per-worktree model activation + default space;
`scope_owner_id NOT NULL` + singleton global; строгий source_blob invariant
(нет source → нет occurrences); `normalized_text` в cache; deterministic IDs +
`parser_fingerprint` + uniqueness constraints; `fts_projection_head`; retention pin
roots; `paths[]` → join tables; candidate evidence → FK; **`specification.md` →
`specification-v1.md`** (behavioral archaeology; новая спека — executable-level:
DDL/constraints/migration boundaries, state machines generation/projection/model/
consolidation/memory, lock order, crash/detection matrix, spool format, config/store
layout, MCP/hook contracts, rebuild/GC algorithms, error/degraded-mode semantics —
пишется по мере шагов 1–10, не одним документом заранее).

**Решено ранее (rev 5, в силе):** co-located daemon; per-worktree shard; Variant A +
write-ahead; `state.sqlite`+`cache.sqlite`; lexical=FTS5 по occurrences; repo registry
(UUID + attach); `kind`=origin неизменяем + supersede-промоушен; auto-save только для
explicit durable decision; migration-ready; транскрипт diagnostic opt-in; win32-arm64
отложен.

**Открыто (spike/метрики, не блокирует core storage):** **dense backend** (шаг 11 —
сравнительный спайк); числа gates, включая P/R memory-роутера; default модель и
доставка весов; языки первого релиза; миграция v1-memory vs clean start; K/T retention;
финальный `SyntaxLocator`/graph semantics; одна общая DB vs `state`+`cache` при росте
(сейчас — split).

---

## Критерий core-projection readiness (обновлён)

Projection готов к реальному dense backend, когда на каждый вопрос есть однозначный и
**тестируемый** ответ:
1. Что доказывает соответствие SQLite `clean` содержимому проекции? →
   ProjectionHead + manifest, validate-on-open — §5.
2. Какая точная active tuple? → (generation, model_space, projection_schema_version)
   в `worktree_projection_state` — §4.
3. Как recovery проверяет tuple в каждом shard? → open-validation matrix — §5.
4. Как активная модель определяется для offline/dormant worktree? → per-worktree
   activation + default space при открытии — §4.
5. Как обнаруживается пустая/частичная/устаревшая FTS? → `fts_projection_head` — §6.
6. Какие IDs стабильны после retry reconcile и move repository? → deterministic IDs
   (§4) + stable worktree UUID (§3).
7. Может ли searchable файл не иметь exact source в canonical store? → нет — §10.
8. Какие rows/leases pin-ят source revisions от GC? → pin roots — §6.

Memory/observations implementation-ready, когда дополнительно отвечено:
1. В какой момент событие durable? → успешный atomic spool append — §7.
2. Как stable и unstable events представлены под dedup constraints? → partial unique
   `dedup_key` + bounded window — §4, §8.3.
3. Какой backend использует memory recall v0? → FTS + brute-force cosine — §8.1.
4. Как scope uniqueness реализована без NULL-дыр? → `scope_owner_id NOT NULL` — §4.
5. Как измеряется качество консолидации? → memory-quality benchmark/гейт — §8.1, §15.

---

## Финал

rev 6 не меняет parse identity — цепочка `file_revision → parsed_unit → occurrence`
подтверждена третьим ревью и внешним аудитом. Ревизия заменяет недоказуемое
(durability стороннего движка, dual-path identity) на детектируемое (validate-on-open,
spool-only), закрывает schema-констрейнты (worktree UUID, scope uniqueness,
deterministic IDs, source-blob invariant) и добавляет качественные гейты памяти.
Бюджет корректности распределён явно: строгие транзакции — памяти, detect+rebuild —
код-индексу. Шаги 1–7 стартуют немедленно и не зависят от открытых вопросов; выбор
dense backend вынесен в сравнительный спайк шага 11 и до него ничего не блокирует.
