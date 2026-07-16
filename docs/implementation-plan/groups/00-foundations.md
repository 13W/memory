# Группа 00 — Контракт разработки и baseline

Цель: получить воспроизводимый каркас разработки и перенести поведенческие ожидания до
написания production-кода. Ссылки: spec 01 §7, 14, 15 §4.

## T00-01 — Импортировать v1 behavioral fixtures и baseline inventory

- **Результат:** versioned fixture corpus с manifest источника и списком пробелов.
- **Scope:** найти/получить v1 tests и 49-query corpus; преобразовать assertions в формат
  `input tree/event/query → expected behavior`; не переносить payload schema vector store;
  записать доступные baseline метрики и неизвестные как `TBD`, не придумывая пороги.
- **Тесты:** schema validation всех fixtures; уникальность ID; runner dry-run; fixture,
  доказывающий отсутствие backend-specific полей.
- **Приёмка:** parser/reconcile/search/memory/adversarial/fault families имеют manifest либо
  явно зарегистрированный blocking gap. Если v1 artifacts недоступны, задача получает `[!]`
  и решение владельца, а не фиктивные данные.

## T00-02 — Создать Rust workspace, quality commands и CI smoke

- **Результат:** минимальный workspace с разделением `core/store/index/projection/memory/
  protocol` и тремя binary targets без бизнес-логики; `CONTRIBUTING.md` содержит одну команду
  полного check.
- **Scope:** pinned toolchain/MSRV, format, clippy `-D warnings`, tests, dependency policy;
  CI на одном host target. Не добавлять dense/model SDK.
- **Тесты:** workspace compiles; каждый binary отвечает version в unit/CLI smoke; CI config
  lint; полный quality command проходит.
- **Приёмка:** clean checkout проверяется одной документированной командой без сети после
  fetch зависимостей.

## T00-03 — Создать общий fixture/failpoint test harness

- **Результат:** test-support crate для temp `LOCAL_RAG_HOME`, controllable clock/UUID,
  fixtures, subprocess и именованных failpoints.
- **Тесты:** два параллельных temp stores изолированы; fixed clock/ID воспроизводимы;
  неизвестный failpoint отвергается; subprocess crash оставляет доступный artifact bundle.
- **Приёмка:** harness используется хотя бы одним smoke test и не читает пользовательский home.

## G00 — Сверка foundations и testing contract

Перечитать spec 01, 14, 15. Проверить, что fixture families 1–6 учтены, deferred scope не
попал в workspace, а все gaps видимы. Запустить полный quality command и fixture validation;
составить `требование → artifact/test`. Любое отклонение исправить через D-NNN до G01.
