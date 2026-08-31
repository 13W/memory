# Матрица трассируемости

Матрица показывает основной владеющий gate. Перекрёстные требования дополнительно
перепроверяются там, где становятся observable end-to-end.

| Спецификация | Основные группы/gates | End-to-end повторная проверка | Report-артефакт |
| --- | --- | --- | --- |
| 01 Overview, correctness budget, identity ladders | G00, G02, G03 | G17, G22 | — |
| 02 Architecture/lifecycle/locking | G01, G09, G15 | G17, G20 | — |
| 03 Data model/DDL/hash rules | G01–G04, G07, G08, G11, G13, G14 | G17 schema lint, G21 | — |
| 04 State machines | G05, G07, G11, G14 | G15, G17 | — |
| 05 Projection protocol | G07, G09, G10 | G11, G12, G17 | — |
| 06 Reconcile/FTS/GC | G05, G06, G08 | G12, G17, G20 | — |
| 07 Observation spool | G13 | G15, G17 | — |
| 08 Memory | G14 | G15, G16, G17, G21 | `fixtures/memory/baseline/run-2026-07-29-g14-verify.json` |
| 09 Search | G12 | G15, G17, G21 | `fixtures/search/baseline/run-v2-2026-07-27-g12-verify.{json,report.md}` |
| 10 Models/embeddings | G10, G11 | G12, G17, G21, G22 | ADR-0004/0005/0006 |
| 11 Interfaces | G15 | G17, G22 | — |
| 12 Security/privacy | G03, G13, G14, G16 | G17, G21, G22 | — |
| 13 Distribution/migrations | G01, G17, G22 | G17, G22 | `fixtures/release/run-2026-08-05.{json,report.md}`; release tag `0.0.0` CI matrix run (D-029) |
| 14 Acceptance/testing | каждый GNN | G17 | `fixtures/release/run-2026-08-05.{json,report.md}` (O2 numbers) |
| 15 Roadmap/MVP/open questions | G00 и каждый GNN | G17 | см. «Open questions» ниже |

## Open questions

| ID из spec | Где закрывается | Правило до решения | **Финальная v0-диспозиция (G17, 2026-08-05)** |
| --- | --- | --- | --- |
| O1 Dense backend | T10-01…T10-05 | только `ProjectionStore`/fake; никакой coupling | **resolved** — `docs/adr/0003-dense-backend-selection.md` (brute-force, 485/500 vs usearch 355/500 vs qdrant-edge 190/500); подтверждено `rg`: ноль зависимостей на dense SDK в продакшен-крейтах |
| O2 Числа gates | T00-01, T12-05, T14-07, T17-05 | метрики собирать, пороги не выдумывать | **resolved** — quality: `fixtures/search/baseline/thresholds.json` (D-016/D-017/D-018, MRR 0.7007 PASS); memory-quality: `fixtures/memory/baseline/thresholds.json` (T14-07/ADR-0006/T14-09, P 0.6757/R 0.5682 PASS); latency/resources: `fixtures/release/run-2026-08-05.{json,report.md}` (T17-05, первый v2-бейзлайн, намеренно не гейтится — нет v1-аналога для сравнения) |
| O3 Models/generator | T11-03, T11-06, T14-07 | ADR по измерениям и лицензии | **resolved** — ADR-0004 (embeddinggemma-300m, +D-016/D-017 амендменты), ADR-0005 (ort+load-dynamic, pinned-digest доставка), ADR-0006 (llama-cpp-2, Gemma 4 E2B q4_0 + T14-09 minijinja-амендмент) |
| O4 Языки v0 | T04-01 | parser core не привязывать к конкретному набору | **resolved** — `docs/adr/0001-first-release-language-set.md` (TypeScript/JavaScript/Rust) |
| O5 v1→v2 memory | T17-04/решение до GA | не блокирует MVP; migration seam сохранить | **легитимно открыт для v0 по правилу** — граница явно задокументирована (13§3, T17-04 as-built note); reader/importer-кода нет (проверено `rg`), v0 = clean-start only; GA-решение остаётся pre-GA release-gate item |
| O6 Retention K/T | T06-01 и метрики | значения конфигурируемы, не считать нормативными | **resolved (X-001, пост-G17, 2026-08-05)** — `docs/adr/0007-retention-k-t-final-values.md`: K=2/T=168h приняты как финальные v0/GA-значения продуктовым решением владельца, без телеметрии (её построение явно не коммишенится, T17-05's граница остаётся в силе как причина, не как блокер) |
| O7 Locator/graph | deferred v0.x | не выпускать graph tools в v0 | **split**: SyntaxLocator-половина **resolved** (`docs/adr/0002-syntax-locator-derivation.md`); symbol-graph-половина (`find_usages`/`get_dependencies`) — **deferred v0.x подтверждено**: `rg 'find_usages\|get_dependencies' crates/` = 0 совпадений вне спецификации/ADR; `edge_kind`-колонка и `insert_resolved_edge` существуют в схеме, но вызываются только тестами — не production ingestion path |
| O8 DB split | уже `[FIXED]` | state/cache никогда не писать через общий ATTACH tx | **resolved/fixed** — подтверждено `rg`-проверкой на реальный SQL `ATTACH` в продакшен-коде (ноль совпадений); инвариант проверяется гейтами G08/G13 |

## Deferred, не смешивать с v0

Description leg, reranker, full recall, ANN memory, LSP graph, cross-generation matching,
multi-harness, FreeBSD и win32-arm64 не входят в очередь T00–T17. После `G17` для них создаются
отдельные `X-NNN` только при явном продуктовом решении и сохранении additive design.

**Новая scope (не из списка deferred выше)** также требует явного продуктового решения, но не
обязана укладываться в одну атомарную `X-NNN` карточку: `TASK-TEMPLATE.md`'s собственное правило
("если описание требует двух независимых результатов... задачу надо разделить") делает
многочастную фичу нечестной для одной `X-NNN`. Прецедент — `docs/adr/0008-tui-dashboard.md`: та же
планка «явное продуктовое решение», но реализуется отдельной пронумерованной группой
(`groups/18-tui-dashboard.md`, гейт `G18`) вне закрытой очереди `T00–T17`, а не последовательностью
`X-NNN`. Выбор между лёгким путём (`X-NNN`) и тяжёлым (новая группа) — за владельцем, документируется
ADR-ом в каждом случае, когда выбран тяжёлый путь.

Второй прецедент —
`docs/adr/0009-daemon-managed-indexing.md`: та же планка, но с дополнительным нюансом — часть
scope (daemon-hosted background workers) уже была `[FIXED]` в spec 02 §1's топологии и не
реализована ни одной карточкой (гейт `G20`, `groups/20-daemon-managed-indexing.md`), что ADR
регистрирует как `D-043`, а не как новую scope.

Третий прецедент — `docs/adr/0010-memory-english-normalization.md`: та же планка «явное продуктовое
решение», scope снова полностью новая (ни `idea.md` rev 6, ни спецификация, ни план до этого ADR не
говорят, на каком языке хранится и ищется durable-память), и снова многочастная — схема, эффективный
текст, детектор, переводчик, порядок записи, фоновый воркер, privacy-поверхности, наблюдаемость и
измерение отдельно тестируемы. Реализуется группой `21` с гейтом `G21`
(`groups/21-memory-english-normalization.md`); попутно ADR регистрирует два дефекта, найденных при
планировании, — `D-067` (плотная нога recall'а произвольно теряет кандидатов) и `D-068` (бенч
memory-recall недетерминирован), — как deviations, а не как новую scope.

Четвёртый прецедент — и первый **иного рода**: `docs/adr/0011-english-canon-for-durable-memory.md`
не открывает scope, а **частично отменяет** предыдущий ADR (0010, Decisions 1/3/6) после того, как
приёмочный замер `T21-09` показал, что спроектированная интервенция и измеренная — разные вещи
(`D-075`). Отсюда правило, которое этот случай задаёт: супэрсид называет **решения**, а не запись
целиком; отменённый ADR сохраняет свой текст и получает указатель на преемника; ошибочные
рассуждения не переписываются, потому что ADR — свидетельство о принятом решении, а не документ,
приводимый задним числом в правильный вид. Реализуется той же группой `21` (фаза 2, `T21-11`…
`T21-19`), тем же гейтом `G21`, без открытия новой scope.

Пятый прецедент — `docs/adr/0013-binary-delivery-via-release-assets.md`: снова новая scope, как
у 0008 и 0010, но с двумя отличиями, каждое из которых задаёт правило. Первое: это первый ADR,
который не просто добавляет scope, а **опровергает действующий `[FIXED]`-текст** — заявленный
канал доставки (`01 §1`, `13 §1`, `CLAUDE.md`) оказался нереализованным, а работающий канал
спецификацией не описан вовсе. Отсюда правило: ADR не может сам изменить `[FIXED]`; он лишь
делает изменение легальным, а сама правка идёт отдельными карточками и регистрируется
отклонением (`D-102`), потому что расхождение обнаружено в as-built состоянии, а не придумано.
Второе, и более тонкое: это первый случай, когда решение **понижает планку, заданную предыдущим
ADR, для одного класса артефактов и сохраняет её для остальных** — стандарт ADR-0005
(«verification uses the compiled-in catalog») остаётся в силе для весов и для ONNX Runtime, но
недостижим для продуктовых бинарников, раз владелец выбрал канал `latest`. Правило, которое
этот случай задаёт: понижение планки обязано назвать саму планку, класс, для которого она
понижена, и класс, где она сохранена; умолчание прочиталось бы как глобальное ослабление.
Отдельно ADR **не** отменяет ни одного решения ADR-0005: вопрос упаковки ORT тот сознательно
оставил открытым и адресовал `T17-03` («this ADR decides the binding, not the packaging»),
поэтому замещается as-built ответ `T17-03`, а не запись ADR. Реализуется группой `22` с гейтом
`G22` (`groups/22-binary-delivery-and-resolution.md`); ни один гейт `G00–G21` не переоткрывается.

The sixth precedent is `docs/adr/0014-consolidation-recovery-and-candidate-dedup.md`, and what is
new about it is not the scope but **where the evidence for opening it came from**. Groups 18, 20,
21 and 22 were all opened from a reading: a design wish, a `[FIXED]` line with no owner, an
unanswered question about language, a delivery channel that turned out to be unimplemented. Group
23 was opened from a **running store**. Every figure in its diagnosis — the backlog of 1386
observations sitting entirely behind four sessions, 27 failed runs across 25 sessions, 9564
pending candidates over 3294 distinct texts, `conflicts` non-empty on none of them — was measured
against the live store's `state.sqlite` with the daemon up, and none of it is visible from the source alone.

Two rules follow, and they are the reason this is written down rather than left as a habit.

**A group opened from a live store owes that store a before-and-after.** Group 23 records its
baseline in its own file and in `PROGRESS.md` before any card runs, and `G23` is required to
remeasure it. A group whose premise is "the system is stopped" cannot be closed by a green test
suite alone; the thing that was stopped has to be shown moving.

**A specification may accept a cost, and a later release may falsify the assumption the acceptance
rested on — and that is a deviation, not a change of mind.** Spec 08 §4 accepted, in as many
words, that a parked consolidation run stays parked "until the binary is rebuilt". The reasoning
was sound while the only user was a developer with `cargo build` at hand. Publishing `0.1.0` — a
binary whose `BUILD_ID` is fixed for the life of the release — removed the escape without touching
a line of the text that names it. So the discrepancy is registered as `D-117` and repaired by
cards, on the same principle ADR-0013 established: an ADR makes a change legal, it does not make
it. What this case adds is that the trigger for such a registration can be an event outside the
repository entirely.

Implemented by group `23` with gate `G23` (`groups/23-consolidation-recovery.md`); no gate
`G00–G22` is reopened.
