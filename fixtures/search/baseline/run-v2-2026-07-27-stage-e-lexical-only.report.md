# 49-query search benchmark — v2 run

## Run provenance

| Field | Value |
| --- | --- |
| v2 commit | `2cd4dd3` |
| Corpus | `/opt/soft/local-rag/src` @ `31dfba2` |
| Model | `embeddinggemma-300m` |
| Mode | `lexical` |
| Dense representation | `code_raw` |
| Lexical fusion weight | 0.2375 |
| Corpus size | 93 files, 545 occurrences |
| Host | aarch64-macos |

## Metrics vs v1

| Metric | v1 | v2 | Δ |
| --- | --- | --- | --- |
| Hit@1 | 0.5918 | 0.3061 | -0.2857 |
| Hit@3 | 0.7959 | 0.5510 | -0.2449 |
| Hit@5 / Recall@5 | 0.8367 | 0.6327 | -0.2041 |
| MRR | 0.6963 | 0.4344 | -0.2619 |

## Latency

| Stage | ms |
| --- | --- |
| index | 1395 |
| embed | 242787 |
| warm search p50 | 2.290 |
| warm search p95 | 2.611 |

## Per-query (v2)

v1 recorded no per-query ranks (D-015), so this table is v2-only.

| id | group | rank | matched |
| --- | --- | --- | --- |
| sc-01 | embedder | 4 | `embedder.ts` / `embedBatch` |
| sc-02 | embedder | — | — |
| sc-03 | embedder | 1 | `embedder.ts` / `embedOllama` |
| sc-04 | embedder | 1 | `embedder.ts` / `embedOpenAI` |
| sc-05 | embedder | 1 | `embedder.ts` / `embedVoyage` |
| sc-06 | embedder | — | — |
| sc-07 | embedder | 1 | `embedder.ts` / `generateDescription` |
| sc-08 | embedder | 3 | `embedder.ts` / `llmFilter` |
| sc-09 | embedder | — | — |
| sc-10 | embedder | — | — |
| sc-11 | search_code | 1 | `tools/search_code.ts` / `searchCodeTool` |
| sc-12 | search_code | 3 | `tools/search_code.ts` / `searchCodeTool` |
| sc-13 | search_code | 3 | `tools/search_code.ts` / `SearchCodeArgs` |
| sc-14 | indexer | 1 | `indexer/indexer.ts` / `indexAll` |
| sc-15 | indexer | 2 | `indexer/indexer.ts` / `_indexFileImpl` |
| sc-16 | indexer | 1 | `indexer/indexer.ts` / `buildEmbedContext` |
| sc-17 | indexer | 1 | `indexer/indexer.ts` / `collectFiles` |
| sc-18 | indexer | — | — |
| sc-19 | parser | 2 | `indexer/parser.ts` / `parseFile` |
| sc-20 | parser | — | — |
| sc-21 | parser | 2 | `indexer/parser.ts` / `extractDoc` |
| sc-22 | parser | 2 | `indexer/parser.ts` / `parseYaml` |
| sc-23 | parser | — | — |
| sc-24 | parser | — | — |
| sc-25 | types | 1 | `types.ts` / `CodeChunkPayload` |
| sc-26 | types | 2 | `types.ts` / `CodeChunkPayload` |
| sc-27 | qdrant | 1 | `qdrant.ts` / `ensureCodeChunks` |
| sc-28 | qdrant | — | — |
| sc-29 | qdrant | 4 | `qdrant.ts` / `ensureCollections` |
| sc-30 | config | — | — |
| sc-31 | storage | — | — |
| sc-32 | storage | 5 | `storage.ts` / `getReverseDeps` |
| sc-33 | storage | — | — |
| sc-34 | storage | 2 | `storage.ts` / `topFilesByRevDeps` |
| sc-35 | server | — | — |
| sc-36 | server | — | — |
| sc-37 | tools | 3 | `tools/remember.ts` / `rememberTool` |
| sc-38 | tools | — | — |
| sc-39 | tools | 1 | `tools/get_file_context.ts` / `getFileContextTool` |
| sc-40 | tools | 2 | `tools/get_dependencies.ts` / `getDependenciesTool` |
| sc-41 | tools | 1 | `tools/project_overview.ts` / `projectOverviewTool` |
| sc-42 | tools | 4 | `tools/stats.ts` / `statsTool` |
| sc-43 | tools | — | — |
| sc-44 | util | 1 | `util.ts` / `storeMemory` |
| sc-45 | util | — | — |
| sc-46 | scoring | — | — |
| sc-47 | scoring | 2 | `scoring.ts` / `timeDecay` |
| sc-48 | indexer_cli | 1 | `indexer/cli.ts` / `expandRoots` |
| sc-49 | bin | 1 | `bin.ts` |
