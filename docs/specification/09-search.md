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

As-built note (T08-01, `[SPEC]`): the splitter is
`local_rag_store::tokenize_identifier`/`tokenize_qualified_name`/`tokenize_path`/
`tokenize_signature` (`crates/store/src/cache/fts.rs`). Splitting runs on the
original casing before lowering — lowering first would destroy the
lower/upper-case boundary signal the split depends on. Boundaries: a hard
delimiter at any non-alphanumeric character (runs collapse, never emitted, so
the same rule covers `_`/`-` and — reused for the qualified-name/path columns —
`.`/`:`/`/`); within an alphanumeric run, lower→upper, an acronym run's last
uppercase letter joining a following lowercase word (`HTTPServer` →
`HTTP`+`Server`), and a letter↔digit transition in either direction
(`parseHTML2Response` → `parse`+`HTML`+`2`+`Response`)
`[SPEC — digit-boundary splitting is not spec-mandated; chosen for recall parity
with the retained fused original]`. Each token is folded to lowercase via
`casefold::simple_fold` (the codebase's existing case-insensitive-comparison
primitive, spec 03 §1.3), not `str::to_lowercase()`, to avoid a length-changing
full-casing surprise `[SPEC]`. A whole-atom "fused" token (the atom unsplit,
lowered) is emitted only when the atom has no internal punctuation — `unicode61`
already separates on punctuation, so re-emitting a punctuated fused string would
only inflate term frequency `[SPEC]`; `tokenize_path`/`tokenize_qualified_name`
make this fusion decision per path/qualifier component (split first), not once
over the whole string, so a punctuation-free component (e.g. a `camelCase` file
stem) still gets its own fused token. `tokenize_qualified_name(None)` (today's
universal case — no v2 caller derives a qualified name yet, 06 §2) tokenizes to
the empty string; `tokenize_signature` takes already-extracted fragments and
emits only their split parts, never a fused whole fragment.
`LEXICAL_SCHEMA_VERSION`/`TOKENIZER_VERSION` are both `1`.

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
