# Матрица трассируемости

Матрица показывает основной владеющий gate. Перекрёстные требования дополнительно
перепроверяются там, где становятся observable end-to-end.

| Спецификация | Основные группы/gates | End-to-end повторная проверка |
| --- | --- | --- |
| 01 Overview, correctness budget, identity ladders | G00, G02, G03 | G17 |
| 02 Architecture/lifecycle/locking | G01, G09, G15 | G17 |
| 03 Data model/DDL/hash rules | G01–G04, G07, G08, G11, G13, G14 | G17 schema lint |
| 04 State machines | G05, G07, G11, G14 | G15, G17 |
| 05 Projection protocol | G07, G09, G10 | G11, G12, G17 |
| 06 Reconcile/FTS/GC | G05, G06, G08 | G12, G17 |
| 07 Observation spool | G13 | G15, G17 |
| 08 Memory | G14 | G15, G16, G17 |
| 09 Search | G12 | G15, G17 |
| 10 Models/embeddings | G10, G11 | G12, G17 |
| 11 Interfaces | G15 | G17 |
| 12 Security/privacy | G03, G13, G14, G16 | G17 |
| 13 Distribution/migrations | G01, G17 | G17 |
| 14 Acceptance/testing | каждый GNN | G17 |
| 15 Roadmap/MVP/open questions | G00 и каждый GNN | G17 |

## Open questions

| ID из spec | Где закрывается | Правило до решения |
| --- | --- | --- |
| O1 Dense backend | T10-01…T10-05 | только `ProjectionStore`/fake; никакой coupling |
| O2 Числа gates | T00-01, T12-05, T14-07, T17-05 | метрики собирать, пороги не выдумывать |
| O3 Models/generator | T11-03, T11-06, T14-07 | ADR по измерениям и лицензии |
| O4 Языки v0 | T04-01 | parser core не привязывать к конкретному набору |
| O5 v1→v2 memory | T17-04/решение до GA | не блокирует MVP; migration seam сохранить |
| O6 Retention K/T | T06-01 и метрики | значения конфигурируемы, не считать нормативными |
| O7 Locator/graph | deferred v0.x | не выпускать graph tools в v0 |
| O8 DB split | уже `[FIXED]` | state/cache никогда не писать через общий ATTACH tx |

## Deferred, не смешивать с v0

Description leg, reranker, full recall, ANN memory, LSP graph, cross-generation matching,
multi-harness, FreeBSD и win32-arm64 не входят в очередь T00–T17. После `G17` для них создаются
отдельные `X-NNN` только при явном продуктовом решении и сохранении additive design.
