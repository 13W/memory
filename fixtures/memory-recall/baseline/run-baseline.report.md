# Memory recall benchmark

## Run provenance

| Field | Value |
| --- | --- |
| v2 commit | `eb4c6c9` |
| Corpus | `/opt/soft/local-rag-v2/crates/xtask/../../fixtures/memory-recall/corpus.json` @ `1.0.0` |
| Model | `embeddinggemma-300m` |
| Config | `baseline` |
| Corpus size | 24 entries, 24 queries |
| Host | aarch64-macos |

## Legs

| Signal | Value |
| --- | --- |
| dense leg ranked the expected entry #1 | 24/24 |
| dense leg surfaced it at all (top 5) | 24/24 |
| dense leg degraded | 0/24 |

## Metrics

| Group | Hit@1 | Hit@3 | Hit@5 | MRR | n |
| --- | --- | --- | --- | --- | --- |
| overall | 0.7917 | 0.7917 | 0.8333 | 0.8021 | 24 |
| en-en | 1.0000 | 1.0000 | 1.0000 | 1.0000 | 8 |
| en-ru | 0.2500 | 0.2500 | 0.2500 | 0.2500 | 4 |
| ru-en | 0.5000 | 0.5000 | 0.7500 | 0.5625 | 4 |
| ru-ru | 1.0000 | 1.0000 | 1.0000 | 1.0000 | 8 |

## Latency

| Stage | ms |
| --- | --- |
| install | 0 |
| embed | 887 |
| warm recall p50 | 39.883 |
| warm recall p95 | 45.672 |

## Per-query

| id | lang_pair | rank | top result |
| --- | --- | --- | --- |
| mrq-01 | ru-ru | 1 | mr-01 |
| mrq-02 | ru-ru | 1 | mr-02 |
| mrq-03 | ru-ru | 1 | mr-03 |
| mrq-04 | ru-ru | 1 | mr-04 |
| mrq-05 | ru-ru | 1 | mr-05 |
| mrq-06 | ru-ru | 1 | mr-06 |
| mrq-07 | ru-ru | 1 | mr-07 |
| mrq-08 | ru-ru | 1 | mr-08 |
| mrq-09 | en-en | 1 | mr-09 |
| mrq-10 | en-en | 1 | mr-10 |
| mrq-11 | en-en | 1 | mr-11 |
| mrq-12 | en-en | 1 | mr-12 |
| mrq-13 | en-en | 1 | mr-13 |
| mrq-14 | en-en | 1 | mr-14 |
| mrq-15 | en-en | 1 | mr-15 |
| mrq-16 | en-en | 1 | mr-16 |
| mrq-17 | ru-en | 1 | mr-17 |
| mrq-18 | ru-en | 1 | mr-18 |
| mrq-19 | ru-en | 4 | mr-10 |
| mrq-20 | ru-en | — | mr-12 |
| mrq-21 | en-ru | 1 | mr-21 |
| mrq-22 | en-ru | — | mr-05 |
| mrq-23 | en-ru | — | mr-06 |
| mrq-24 | en-ru | — | mr-20 |
