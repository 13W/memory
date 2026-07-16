# 09 — Semantic Code Search

## 1. Pipeline `[FIXED]`

```
search_code(query, mode, limit, name_pattern?)
  → resolve worktree from request context (02 §3.3)
  → L2.read for the WHOLE pipeline
  → resolve active tuple (generation, model_space)
  → validate fts_projection_head (06 §4)          — else degraded dense-only
  → validate shard availability (05 §6 was run at open) — else degraded lexical-only
  → legs per mode:
      lexical: FTS5 over occurrences of the active generation
      dense:   shard search (code_raw leg; code_context leg iff active model space has it)
      hybrid:  both → app-side RRF
  → optional name_pattern filter (prefix-tokenized on local_name/qualified_name)
  → graph/context enrichment (parent unit, file, qualified name; edges post-v0)
  → release L2.read → format results
```

Indexed population: **document units of all kinds** (symbol/file/config/text/fallback) —
anything less is a parity regression vs v1 `[FIXED]`.

## 2. Lexical leg — FTS5 `[FIXED]`

App-side code-aware preprocessing before insert (versioned as `tokenizer_version`; bump ⇒ head
invalidation ⇒ FTS rebuild):

- identifiers split on camelCase / snake_case / kebab-case, original + parts emitted, lowercased;
- qualified-name components and path components as separate columns;
- signature tokens (params/return types where the grammar exposes them).

Ranking: `bm25(fts_occurrences, w_name, w_qualified, w_path, w_signature, w_body)` with
default weights `4.0, 3.0, 1.5, 2.0, 1.0` `[SPEC — tuned by the 49-query benchmark]`.

## 3. Dense leg

- Query embedding computed with the representation of the active model space; **content vs
  context representation choice is decided by the benchmark** `[OPEN]` — v0 ships `code_raw`,
  `code_context` participates in the spike/benchmark.
- Distance per `representation.distance_metric`.

## 4. Fusion `[SPEC]`

Reciprocal Rank Fusion: `score(d) = Σ_legs 1 / (k + rank_leg(d))`, `k = 60`. Deterministic
tie-break: `(score desc, occurrence_id asc)`. Per-leg candidate depth: `max(limit·4, 50)`.

## 5. Modes (v0) `[SPEC mapping of v1 modes]`

| mode | legs |
| --- | --- |
| `hybrid` (default) | lexical + dense(code_raw) [+ description leg post-v0] |
| `lexical` | FTS only |
| `code` | dense code leg only |
| `semantic` | description leg — **post-v0** (only if it wins the benchmark `[FIXED]`); until then returns `UNSUPPORTED_MODE` |

Cross-encoder reranker (`rerank`, `rerank_k`): **post-v0**, additive, only after baseline
`[FIXED]`.

## 6. Symbol graph `[FIXED semantics, final shape [OPEN]]`

Graph = **occurrence identity** (`OccurrenceLocator`); edges on occurrence IDs, per generation.
Cross-generation identity is a heuristic, never a correctness dependency. Edge resolution
classes are explicit: `heuristic` (name/usage match), `syntax` (resolved by parser queries),
`lsp` (deferred). `find_usages` / `get_dependencies` MUST label every hit with its resolution
class. LLM calls are removed from the per-save hot path `[FIXED]`; structural descriptions are
an async drainer, post-v0, benchmark-gated.

## 7. Response format `[SPEC]`

```json
{
  "results": [{
    "occurrence_id": "…", "path": "src/a.ts", "name": "extractImports",
    "qualified_name": "parser.extractImports", "unit_kind": "symbol",
    "span": [248, 264], "language": "typescript",
    "score": 0.031, "legs": {"lexical": 3, "dense": 1},
    "snippet": "…"          // from source_blob, span-bounded, size-capped
  }],
  "generation": {"id": "…", "number": 41},
  "degraded": null | "dense_only" | "lexical_only",
  "diagnostics": []
}
```

Snippets are cut from the exact `source_blob` by byte span — never from the live disk file
(the file may have changed since the generation) — reproducibility is exactly what the
source-blob invariant buys `[FIXED]`.

## 8. Latency gates (numbers after baseline `[OPEN]`)

warm search p95; one-file reconcile p95; branch-checkout reconcile — tracked per 14 §2.
