# Группа 14 — Durable memory

Цель: транзакционно строгая, аудируемая память и измеряемое качество router. Ссылки: spec 03
§2.5; 04 §4–6; 08; 11 §2/§5; 12 §3–4; 14.

## T14-01 — Memory DDL и legal transitions

- **Результат:** memory/evidence/candidate/cursor/run/audit schema and typed kind-specific
  state guards; global singleton owner; immutable kind.
- **Тесты:** every legal/illegal transition, scope/canonical uniqueness incl global, terminal
  recall exclusion, hypothesis confirm vs fact supersede, constraints/FKs.

## T14-02 — Базовый transactional memory-op engine

- **Результат:** shared transaction/idempotency framework плюс create/reinforce/noop; mutation,
  evidence и audit атомарны; reinforce never edits text.
- **Тесты:** три operation contracts, optimistic conflict, same key returns original result,
  rollback failpoints, audit versions contiguous.

## T14-03 — Lifecycle/edit memory operations

- **Результат:** resolve/supersede/retract/edit поверх общего engine с expected_version и
  kind/state guards.
- **Тесты:** contract каждого op, illegal transition rollback, promotion creates fact via
  supersede, retract not delete, user/router actor audit.

## T14-04 — Merge memory operation

- **Результат:** one-tx merge: survivor absorbs evidence, losers become superseded and audit
  records exact merge set.
- **Тесты:** 2+ entries, duplicate evidence, incompatible scope/error, failpoint rollback,
  optimistic conflict and retry idempotence.

## T14-05 — Candidates и review operations

- **Результат:** propose/edit/approve/reject/expire; approval routes through same op engine with
  actor user and FK evidence.
- **Тесты:** state machine, 30-day fake-clock expiry, double approval idempotence, conflicting
  edit/version, rejected never materializes, list exposes version/provenance.

## T14-06 — Consolidation lease/cursor runner

- **Результат:** bounded snapshot, 120s lease/30s renew, generator outside tx, ordered op apply+
  cursor+run applied in one short tx; startup expired-lease retry/checkpoint triggers.
- **Тесты:** crash each step, lease expiry/renewal, never past to_seq, generator observes no DB
  tx, op retry no duplicates, cursor cannot advance on partial apply.

## T14-07 — Local router и quality fixture set

- **Результат:** ADR closes generator part O3; local_only default router distinguishes durable
  decision/hypothesis/question/negation/model claim and emits allowed ops/candidates; RU/EN set.
- **Тесты:** labeled create/reinforce/supersede/noop fixtures, adversarial/mixed-language cases,
  precision/recall report; O2 P/R threshold established from approved baseline, not invented.

## T14-08 — Recall relevance и safe formatting

- **Результат:** scope union, eligible filter, FTS+bounded brute cosine behind trait, RRF/budget,
  deterministic order; additionalContext sanitation/length/caps/delimiter escape and empty output.
- **Тесты:** scope isolation/union, ≤20k guard, terminal exclusion, tie order, 1500-token budget,
  control/injection/`</memory`/1KiB cases, exact byte len, empty emits zero bytes.

## T14-09 — Обобщённая поддержка chat-template рантайма (без хардкода per-model)

Добавлено при планировании T14-07 (запрос пользователя: "разобраться с шаблонами, чтоб
поддерживать все модели и не хардкодить"). T14-07 ship'нул `GeneratorCatalogEntry::
chat_template_override: Option<&'static str>` как точечный, задокументированный обход для
одной конкретной модели (`Gemma 4 E2B` → `Some("gemma")`), после того как обнаружилось, что
`llama-chat.cpp`'s `llm_chat_detect_template` (эта версия `llama-cpp-sys-2`) не распознаёт
встроенный Jinja-шаблон Gemma 4 и падает в `LLM_CHAT_TEMPLATE_UNKNOWN` → `FfiError(-1)`. Это
рабочий, но НЕ масштабируемый механизм: каждая новая модель, чей шаблон не входит в
захардкоженный список `llama.cpp`, требует ручного расследования + ручной строки-имени.

- **Результат:** router может подключить произвольную GGUF-модель (в пределах архитектур,
  которые понимает вендоренный `llama.cpp`) без ручного per-catalog-entry override —
  либо через (a) обновление `llama-cpp-sys-2` до версии с нативным детектом свежих шаблонов
  (проверить перед стартом задачи, не изобретать раньше времени), либо (b) собственный
  Jinja-интерпретатор поверх `LlamaModel::chat_template`'s сырой строки (например
  `minijinja`, на который `llama-cpp-2`'s собственная документация прямо ссылается как на
  альтернативу движку `llama.cpp`), либо (c) типизированный, тестируемый набор
  hand-rolled форматтеров на распознанные семейства (ChatML/Gemma/Llama3/...) с явным,
  громким отказом (typed error), когда семейство не распознано — вместо тихого
  единственного override-поля. Выбор конкретного механизма — предмет самой задачи, не
  предрешён здесь.
- **Не в scope:** сама смена дефолтной модели (уже сделано в T14-07/ADR-0006); поддержка
  sampling-режимов кроме greedy; grammar-constrained decoding (отдельный, уже
  зарегистрированный as-built-отказ T14-07).
- **Тесты:** минимум одна модель НЕ из уже проверенных семейств (Qwen ChatML, Gemma
  `<start_of_turn>`) реально грузится и генерирует осмысленный текст без нового
  `chat_template_override`; регресс на существующих трёх catalog-entries (Qwen×2 + Gemma 4)
  не ломается.

### Добавлено D-013 (G11) — `memory`-представление дефолтного model space

Группа 14 владеет **memory-половиной** spec 10 §3's «at minimum `code_raw` + `memory` in v0»:
subject-функцией kind'а `memory` (сегодня её отсутствие — осознанный отказ
`BackfillError::UnsupportedRequiredKind`, а не «expected = 0») и регистрацией самого
представления как `required` для дефолтного model space. До этого момента kind `memory`
**не** должен помечаться `required` ни в одной карточке: полный `Coverage` тогда недостижим и
ни один space не сможет получить `projection_ready`. `code_raw`-половина — T15-07.

## G14 — Сверка memory correctness/quality

Перечитать spec 04 §4–6, 08, 11 §5, 12 §3–4, 14. Run state/transaction crash suite,
router benchmark and adversarial recall corpus. Проверить cursor atomicity, model claims never
auto-fact, evidence survives TTL and quality thresholds recorded from real baseline.
