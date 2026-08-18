# Memory recall benchmark — configuration comparison

## Run provenance

| Field | Value |
| --- | --- |
| v2 commit | `2bcac29` |
| Corpus | `/opt/soft/local-rag-v2/crates/xtask/../../fixtures/memory-recall/corpus.json` @ `1.0.0` |
| Model | `embeddinggemma-300m` |
| Corpus size | 24 entries, 24 queries |

## Overall MRR by configuration

| Config | Hit@1 | Hit@3 | Hit@5 | MRR | Δ MRR vs baseline |
| --- | --- | --- | --- | --- | --- |
| baseline | 0.7917 | 0.7917 | 0.8333 | 0.8021 | — |
| store_en | 0.9583 | 1.0000 | 1.0000 | 0.9792 | +0.1771 |
| query_en | 0.8750 | 0.9167 | 0.9583 | 0.9062 | +0.1042 |
| both_en | 1.0000 | 1.0000 | 1.0000 | 1.0000 | +0.1979 |

## MRR by lang_pair — where a normalized configuration would actually help

An aggregate delta dominated by same-language controls already at 1.0 hides
the cross-lingual effect this table exists to show — read `ru-en`/`en-ru`
rows, not just `overall`, before drawing a conclusion.

| lang_pair | baseline MRR | store_en MRR (Δ) | query_en MRR (Δ) | both_en MRR (Δ) | 
| --- | --- | --- | --- | --- | 
| en-en | 1.0000 | 0.9375 (-0.0625) | 1.0000 (+0.0000) | 1.0000 (+0.0000) | 
| en-ru | 0.2500 | 1.0000 (+0.7500) | 1.0000 (+0.7500) | 1.0000 (+0.7500) | 
| ru-en | 0.5625 | 1.0000 (+0.4375) | 0.5625 (+0.0000) | 1.0000 (+0.4375) | 
| ru-ru | 1.0000 | 1.0000 (+0.0000) | 0.9375 (-0.0625) | 1.0000 (+0.0000) | 
