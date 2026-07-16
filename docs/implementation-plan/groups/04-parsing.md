# Группа 04 — Парсинг и path-independent units

Цель: deterministic parse products не содержат контекст пути. Ссылки: spec 03 §2.3–2.4;
06 §2.1; 14 §1, §5; 15 O4/O7.

## T04-01 — Закрыть O4: языки v0

- **Результат:** ADR выбирает 2–3 языка по доступному 49-query corpus, пользовательской ценности,
  parser maturity/platform support; обновлены `[OPEN]` места спецификации.
- **Тесты:** corpus manifest проверяет coverage выбранных языков; ADR CI link-check.
- **Приёмка:** выбор явно согласован; без решения следующие language tasks blocked.

## T04-02 — Parser fingerprint и abstraction

- **Результат:** parser trait, language-by-path selector, canonical sorted fingerprint со всеми
  boundary-affecting versions, path-free SyntaxLocator serialization.
- **Тесты:** exact fingerprint golden; extension changes language/fingerprint; reordered config
  gives same value; any version bump changes value; locator rejects path fields.

## T04-03 — Первый язык и parser fixtures

- **Результат:** tree-sitter adapter produces symbol/file/config/text/fallback as applicable,
  byte spans, parents, unresolved references for first ADR language.
- **Тесты:** representative syntax/error/empty/Unicode fixtures; exact byte spans; same input
  twice byte-identical; all relevant unit kinds searchable.

## T04-04 — Второй v0 язык

- **Результат:** отдельный adapter/query set для второго языка из ADR без ослабления общего
  контракта.
- **Тесты:** те же fixture categories; identical bytes under ambiguous extensions form
  different revisions; malformed syntax deterministic fallback.

## T04-05 — Третий v0 язык, если выбран ADR

- **Результат:** adapter/query set для третьего языка. Если ADR выбрал ровно два языка,
  карточка закрывается как `N/A by ADR` с ссылкой и проверкой corpus coverage, без пустого кода.
- **Тесты:** те же fixture categories и cross-language fingerprint cases.

## T04-06 — Deterministic parsed-unit persistence

- **Результат:** atomic create/reuse content blobs, units and unresolved refs; canonical unit
  ordering independent of insert order; duplicate parses reuse rows.
- **Тесты:** retry and shuffled insert property tests; same revision no duplicates; shared rows
  have no path/context; transaction rollback leaves no partial graph.

## G04 — Сверка parsing identity

Перечитать spec 03 §2.3–2.4, 06 §2.1, 14 §5. Запустить весь parser corpus дважды и сравнить
байты; проверить span against exact blob и systemic schema/code audit. Не начинать generation
builder при неполном language fixture coverage.
