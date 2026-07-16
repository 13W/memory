# Embedding Model Benchmark Report

**Date:** 2026-07-16T13:12:13.607Z
**Project:** /opt/soft/local-rag
**LLM:** gemma3n:e2b

## Quality Metrics

| Model | Chunks | Hit@1 | Hit@3 | Hit@5 | MRR | IndexMs | QEmbedMs | SearchMs |
|----------------------|-------:|------:|------:|------:|-----:|--------:|---------:|---------:|
| embeddinggemma:300m | 544 | 0.59 | 0.80 | 0.84 | 0.70 | 13006 | 4008 | 229 |

## Timing Breakdown

| Model | CodeEmbedMs | DescGenMs | DescEmbedMs |
|----------------------|------------:|----------:|------------:|
| embeddinggemma:300m | 12562 | 0 | 0 |
