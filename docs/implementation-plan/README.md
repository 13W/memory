# План имплементации local-rag v2

Этот каталог — исполняемая очередь работ для Claude Code. Нормативные требования находятся
в `../specification/`; этот план не заменяет спецификацию и не вправе менять решения `[FIXED]`.
При конфликте `idea.md` rev 6 имеет приоритет над спецификацией, а конфликт регистрируется как
дефект спецификации.

## Как устроен план

- `PROGRESS.md` — единственный реестр статуса и рекомендуемый порядок задач.
- `TRACEABILITY.md` — покрытие разделов спецификации группами и контрольными воротами.
- `TASK-TEMPLATE.md` — шаблон для корректирующих и вновь обнаруженных задач.
- `groups/NN-*.md` — задания. Каждая секция `TNN-NN` рассчитана на один подход агента.
- `DEVIATIONS.md` — журнал расхождений, неясностей и решений.

Группы выполняются строго по номеру. Внутри группы задачи также выполняются по номеру, если в
самой группе не указано иное. Последняя задача каждой группы — обязательная сверка `GNN`, а не
формальность и не пакетная code review в конце проекта.

## Порядок групп

| Группа | Содержание |
| --- | --- |
| [00](groups/00-foundations.md) | fixtures, baseline и каркас разработки |
| [01](groups/01-storage-and-migrations.md) | SQLite foundation и миграции |
| [02](groups/02-registry-and-identity.md) | stable repository/worktree identity |
| [03](groups/03-source-and-skips.md) | exact source и skip policy |
| [04](groups/04-parsing.md) | parser adapters и path-independent units |
| [05](groups/05-generations-and-reconcile.md) | generations и authoritative reconcile |
| [06](groups/06-retention-and-gc.md) | retention и GC |
| [07](groups/07-projection-protocol.md) | fake projection и crash protocol |
| [08](groups/08-fts-view.md) | independently validated FTS view |
| [09](groups/09-locking-and-shards.md) | locking и shard lifecycle |
| [10](groups/10-dense-backend-spike.md) | сравнительный dense-backend spike |
| [11](groups/11-embeddings-and-model-spaces.md) | embeddings и model spaces |
| [12](groups/12-hybrid-search.md) | hybrid code search |
| [13](groups/13-spool-and-observations.md) | spool-only observations |
| [14](groups/14-memory.md) | durable memory, router и recall |
| [15](groups/15-daemon-interfaces-cli.md) | daemon, MCP, hooks и CLI |
| [16](groups/16-security-and-recovery.md) | security, privacy и recovery UX |
| [17](groups/17-distribution-and-release.md) | packaging и release gates |

## Рабочий цикл одной задачи

1. Выбрать первый незавершённый пункт в `PROGRESS.md`. Проверить, что зависимости и gate
   предыдущей группы закрыты.
2. Прочитать всю карточку задачи и перечисленные разделы спецификации. Перед кодом проверить
   соседние интерфейсы и существующие тестовые fixtures.
3. Реализовать только заявленный scope. `[OPEN]` нельзя закрывать молча: нужен отдельный spike
   или ADR, указанный планом. `[FIXED]` нельзя менять без новой ревизии design.
4. Добавить/обновить тесты из карточки. Каждый исправленный дефект получает regression test.
5. Запустить узкие тесты изменённого crate/module, затем все доступные workspace checks:
   форматирование, lint с запретом warnings, unit/integration/doc tests. Конкретные команды
   фиксируются в корневом `CONTRIBUTING.md` задачей T00-02.
6. Обновить `PROGRESS.md`: отметить задачу, добавить короткую строку Evidence с commit/PR,
   командами тестов и результатом. Не отмечать задачу по факту написания кода — только после
   зелёных критериев приёмки.
7. Если обнаружено расхождение, немедленно внести запись в `DEVIATIONS.md`, создать по
   `TASK-TEMPLATE.md` корректирующую задачу `D-NNN` и выполнить её **до следующей плановой
   задачи**. После исправления повторить затронутые тесты и сверку.

Для запуска отдельной итерации Claude Code используйте один и тот же короткий prompt:

```text
Выполни ровно задачу <ID> из docs/implementation-plan/groups/<file>.md.
Сначала прочитай docs/implementation-plan/README.md, PROGRESS.md, DEVIATIONS.md,
карточку задачи и указанные в группе разделы спецификации. Не расширяй scope.
Добавь требуемые тесты, выполни Definition of Done и обнови PROGRESS.md evidence.
При любом расхождении останови плановую задачу, зарегистрируй deviation и исправь его
по правилам README до перехода дальше.
```

Для `GNN` замените первую строку на «Выполни gate GNN»; агент не пишет новые features, кроме
обязательных корректирующих D-NNN, найденных сверкой.

## Definition of Done для любой задачи

- изменения минимальны и укладываются в scope карточки;
- все новые ветви поведения покрыты unit или integration/fixture тестами;
- негативный сценарий и граничный случай проверены там, где они осмысленны;
- публичные типы/протоколы документированы, ошибки типизированы;
- нет скрытого выбора по `[OPEN]`, backend-specific зависимости до T10;
- тесты детерминированы: без сети, wall-clock race и зависимости от пользовательского home;
- изменённые нормативные детали отражены в спецификации как `[SPEC]` amendment либо оформлены
  как deviation; `[FIXED]` меняются только через новую design revision;
- `PROGRESS.md` содержит воспроизводимое evidence.

Тестовые данные создаются только во временном `LOCAL_RAG_HOME`. Тесты, меняющие состояние
SQLite/спула/шарда, обязаны проверять повторный запуск. Для crash-сценариев используются
именованные failpoints, а не случайные kill/sleep.

## Gate группы и запрет продвижения

На задаче `GNN` исполнитель строит таблицу `требование → код → тест`, запускает полный набор
тестов группы и перечитывает указанные разделы спецификации. Результат записывается в таблицу
Gate results и секцию Evidence в `PROGRESS.md`.

Gate может завершиться только одним из результатов:

- `PASS` — расхождений нет, можно начинать следующую группу;
- `PASS after D-NNN` — расхождения исправлены и проверки повторены;
- `BLOCKED` — требуется решение владельца продукта/новая design revision; следующая группа не
  начинается.

Нельзя переносить отклонение в backlog под видом «починим позже», если оно затрагивает уже
реализованное нормативное поведение. Допустимо отложить только явно deferred/post-v0 scope.

## Изменение плана

Новая задача получает ID `D-NNN` (исправление deviation) или `X-NNN` (новый согласованный
scope), зависимости и тесты. Её добавляют в `PROGRESS.md` непосредственно перед ближайшим
gate. Удалять завершённые задачи и переписывать evidence нельзя; ошибочную отметку снимают с
пояснением.
