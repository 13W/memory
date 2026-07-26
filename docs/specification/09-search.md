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

As-built note (T09-03, `[SPEC]`): `local_rag_search::SearchEngine::search_code`
(`crates/search/src/pipeline.rs`, new crate `local-rag-search`, depending on `core`/`protocol`/
`store`/`projection` — no existing crate claimed this scope; both `store::lock::worktree` and
`projection::manager`'s own doc comments deferred "adopting into a search executor" here) realizes
this pipeline's first two steps and the lock/validate/degrade skeleton around the remaining ones,
**not yet the tuned content** of the legs themselves:

- `resolve worktree from request context` → `local_rag_store::registry::resolve` (already built by
  T02-04), called before any lock.
- `L2.read for the WHOLE pipeline` / `resolve active tuple` → 02 §5's as-built note and 06 §3's
  as-built note (above).
- `validate fts_projection_head` → `open_and_validate_fts(.., ValidationDepth::Cheap, ..)` (06 §4),
  mapped to `degraded: "dense_only"` per 02 §6.
- `validate shard availability` → `ShardManager::acquire` (T09-02); any `AcquireError` (including
  a rebuild-on-acquire that itself fails, e.g. a withheld vector) maps to `degraded: "lexical_only"`
  per 02 §6, with **no internal retry** — retrying here would risk defeating `read_bounded`'s own
  bounded-wait contract.
- `legs per mode` / `optional name_pattern filter` / `graph/context enrichment`: **stubs** in this
  task (`Stage::LexicalLeg`, `Stage::Enrichment` — present as pipeline stages an instrumented
  observer can see run under the lock, but with no query/RRF/enrichment logic yet). The pipeline
  always attempts both legs unconditionally, mirroring the default `hybrid` mode; `mode`/`limit`/
  `name_pattern` are not yet request fields. The real BM25 query is T12-01, RRF/`results[]`/`legs`
  scoring is T12-02/T12-03, and real enrichment is T12-04 — each replaces its stub stage in place,
  still inside the same held `L2.read`.
- `format results`: not yet — `SearchEngine::search_code` returns
  `local_rag_search::PipelineSnapshot` (worktree/generation/model-space tuple, `degraded`,
  `diagnostics`), the envelope skeleton `[SPEC]` this task owns, not §7's full response shape
  below (T12-03 builds that on top).

As-built note (T12-01, `[SPEC]`): `Stage::LexicalLeg` is no longer a stub — it runs
`local_rag_store::lexical_leg` (`crates/store/src/cache/fts_query.rs`, the read-side sibling of
T08-02's write-side `materialize_fts`; both call the same `tokenize_identifier`, which is why query
and index vocabularies cannot drift), inside the same held `L2.read`. `SearchRequest` gained
`query`/`limit`/`name_pattern`; `mode` and per-mode leg selection (§5) stay with fusion in T12-03,
so the pipeline still attempts both legs. The leg runs **only** when the FTS view validated
(`FtsAvailability::Valid`): an invalid/stale head means the query is not issued at all and the
response is `dense_only` with its diagnostic, never a silently empty lexical result (06 §4
`[FIXED]`) — asserted end to end by `crates/search/tests/lexical.rs::
a_stale_head_degrades_to_dense_only_without_running_the_leg`, which keeps a *valid* head for a
previous generation physically present in the cache so the assertion cannot pass vacuously.
`PipelineSnapshot` grew a `lexical: Vec<LexicalHit>` field (occurrence id + 1-based leg rank + raw
`bm25`), the input T12-03 fuses into §7's `results[]`; the enrichment stage remains a stub
(T12-04). `name_pattern` is realized as an FTS5 column filter (§2's as-built note), so the
"prefix-tokenized on `local_name`/`qualified_name`" step needs no state-side join and no
cross-database `ATTACH`.

As-built note (T12-02, `[SPEC]`): `Stage::DenseLeg` is likewise real — it resolves the active
`code_raw` representation, embeds the query through the injected `QueryEmbedder`, and runs the
production brute-force backend, returning `PipelineSnapshot.dense: Vec<DenseHit>` alongside
`lexical` (§3's as-built note has the mechanics). `SearchEngine::new` keeps its signature and
defaults to `UnavailableEmbedder`; `SearchEngine::with_embedder` is the constructor the daemon
uses (group 15). Both legs now produce occurrence-identified, 1-based-ranked candidate lists cut at
the same `candidate_depth(limit)` — everything RRF needs, so T12-03 adds fusion and §7's response
shape without touching either leg. Still stubbed: enrichment (T12-04) and the `mode` field with
its per-mode leg selection (§5, T12-03).

As-built note (T12-03, `[SPEC]`): the pipeline is now complete end to end — `legs per mode`
(§5's as-built note), RRF fusion (§4's), and `format results` as §7's canonical
`SearchResponse` (§7's). The only remaining stub is `graph/context enrichment`
(`Stage::Enrichment`): T12-04 owns the `source_blob`-derived half (`snippet`, `get_file_context`,
the cached overview), and edges stay post-v0 (§6).

As-built note (T12-04, `[SPEC]`): the `source_blob`-derived half landed (§7's own T12-04 note,
11 §2's for the two tools). What remains behind `Stage::Enrichment` is only the *graph* half —
parent-unit and edge enrichment — which §6 keeps post-v0/v0.x; the stage marker stays as the
place that work will attach to.

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

As-built note (T12-01, `[SPEC]`): the query side lives in
`local_rag_store::cache::fts_query` (`crates/store/src/cache/fts_query.rs`) —
`fts_match_expression` builds the FTS5 `MATCH` string, `query_fts` runs the ranked SQL,
`lexical_leg` composes the two. The **default weights** are the constant
`BM25_DEFAULT_WEIGHTS = [4.0, 3.0, 1.5, 2.0, 1.0]`, bound as query parameters (not
interpolated), and the ranking SQL joins `fts_occurrences ⋈ fts_doc` on the shared rowid,
filtering `worktree_id` **and** `generation_id`. That generation predicate is defence in
depth behind 06 §4's head validation: even a stale head that somehow passed cannot leak
another generation's occurrences (06 §3's no-mixing guarantee, restated in SQL). Ordering
is `bm25 ASC` — SQLite's `bm25()` is more negative for a better match — with
`occurrence_id ASC` as tie-break, borrowing §4's fusion tie-break so a truncation at
candidate depth is reproducible rather than storage-order dependent.

Query preprocessing reuses `tokenize_identifier` verbatim, so the query's term vocabulary is
the indexed one by construction. Three `[SPEC]` decisions the spec text above does not fix:

- **Terms are combined with `OR`, not `AND`.** The 49-query corpus is natural language
  ("call Ollama embed API and parse embeddings response", `fixtures/search/corpus.json`);
  requiring every token would return nothing for nearly every query, while BM25's own IDF
  already ranks documents matching more and rarer terms first. Tunable by T12-05 alongside
  the weights.
- **Every term is emitted as a quoted FTS5 string** (`"embed"`, embedded `"` doubled). FTS5
  reads bare `AND`/`OR`/`NOT`/`NEAR` as operators, so an unquoted English query containing
  "and" would be `SQLITE_ERROR` rather than a search; quoting makes the expression total
  over arbitrary user input.
- **`name_pattern` becomes an FTS5 column filter**,
  `{name qualified_name} : ("extractimp"* AND "extract"* AND "imp"*)`, `AND`-ed onto the
  query expression. `AND` because a filter must narrow; prefix per token is what makes a
  partially typed pattern work. An empty/whitespace-only pattern tokenizes to nothing and is
  treated as **no filter**, not as an impossible one. When neither query nor pattern yields a
  term, the leg returns empty **without issuing SQL** (an empty `MATCH` expression is itself a
  syntax error).

One accepted asymmetry, documented rather than silently carried: `tokenize_path`/
`tokenize_qualified_name` make the fused-whole-atom decision *per component* at index time,
while a query string is tokenized as a single atom — so the query `src/foo/barBaz.rs` asks for
`src foo bar baz rs` but not the fused `barbaz` the `path` column also holds. Recall is
unaffected (`bar`/`baz` still match that row); splitting a free-text query on `/`/`.` to recover
the fused term would misfire on ordinary prose ("v2.1", "and/or").

The `signature` column's weight (`2.0`) is **inert on real data** for now: `materialize_fts`
writes `tokenize_signature(&[])` for every row (T08-02's as-built note above — raw
parameter/return-type text is still not plumbed out of the tree-sitter adapters). T12-01 owns
the query, not the ingest, so the weight is proven honored on directly seeded rows
(`crates/store/tests/fts_query.rs::signature_column_outranks_path_and_body`) and starts
affecting production ranking unchanged once that column is populated — no deviation is
registered, since this is the previously accepted T08-02 scope boundary, not a new mismatch.

## 3. Dense leg

- Query embedding computed with the representation of the active model space; **content vs
  context representation choice is decided by the benchmark** — v0 ships `code_raw`,
  `code_context` participates in the spike/benchmark. `[OPEN]` **closed by D-016**, see the
  as-built note below.
- Distance per `representation.distance_metric`.

As-built note (T12-02, `[SPEC]`): the leg is `SearchEngine::dense_leg`
(`crates/search/src/pipeline.rs`), running inside the same held `L2.read` as everything else, over
the production brute-force backend (`local_rag_projection::BruteForceProjectionStore`, 05 §1's
as-built note, ADR-0003).

**Representation selection.** The active model space's `RepresentationKey` for the searched kind
is read once per search via `local_rag_projection::representation_key_for` (D-016 generalized
T12-02's `code_raw_representation_key`, which survives as the `code_raw`-fixed wrapper) — the same lookup
`params_for_model_space` uses to size and score the shard, factored out so the query cannot be
embedded with a different `model_id`/`dimensions`/`distance_metric` than the points it is compared
against. Reading it under the lock is deliberate: the representation is part of the active tuple,
and resolving it before the lock would reintroduce exactly the mixing 06 §3 forbids. The cost of
that choice is that query inference happens while `L2.read` is held — readers do not block each
other, only writers (reconcile/switch) wait, which is the accepted v0 trade.

**Query embedding** is an injected seam, `local_rag_search::QueryEmbedder` (same "inject a trait
object, fake it in tests" idiom as `VectorSource`/`UuidSource`), not a dependency on
`crates/embed`: provider selection and the `data_policy` guard that must run **before** any remote
provider is considered (12 §1) belong to the daemon (group 15), and the seam keeps `crates/search`
free of an inference runtime so its tests stay offline and deterministic. The default is
`UnavailableEmbedder`, which fails with an explicit reason rather than returning a zero vector — a
store with no provider degrades visibly instead of silently serving a meaningless dense leg.

**Distance** comes from the representation and rides in `ShardParams::distance_metric` (03 §2.2 →
05 §1). Every backend scores through one shared helper,
`local_rag_projection::similarity(metric, query, point)`, in the "higher is closer" convention:
`dot` raw, `cosine` normalized (a zero-norm vector scores `0.0`, never `NaN`, which would poison
the sort), `l2` **negated** so nearer still sorts first.

**One kind only, and how.** The shard holds a point per (occurrence × required representation
kind), so a space that requires both also stores `code_context` points. The chosen backend has no
payload filter at all (ADR-0003: `filtered_hnsw_available = false`), so the leg requests
`candidate_depth(limit) × |required kinds|` and filters by kind afterwards. That over-fetch is a
heuristic, not a proof — so when the window comes back full and still yields fewer than
`candidate_depth(limit)` `code_raw` hits, the leg re-queries once for the whole shard (for a
linear-scan backend the same scan, only a larger sort), making the depth guaranteed at a bound of
exactly two backend calls. Exercised deliberately by
`crates/search/tests/dense.rs::request_limit_drives_the_dense_candidate_depth`, whose fixture ranks
every `code_context` point above every `code_raw` one.

**Identity.** `projection_point_id` is one-way (05 §3), so the occurrence behind a hit is recovered
by re-deriving the tuple's expected set with the existing `expected_points` — the same function the
switch uses to decide what belongs in the shard, which also carries each point's
`representation_kind` and therefore serves as the kind filter and the reverse map at once. The leg
returns `DenseHit { occurrence_id, rank, score }`, deliberately the same shape as T12-01's
`LexicalHit`, so T12-03's RRF fuses two lists of occurrences rather than translating identities.

**No filters inside a shard.** `DenseQuery` carries a vector and `k` and nothing else; a shard is
per `(worktree, model_space)` (05 §2) and holds only the active generation's points after a
`switch`, so "no tenant/generation filter dependence" is structural, not enforced at query time.

As-built note (D-016, `[SPEC]`, closes the `[OPEN]` above): **v0 searches `code_raw`.** The choice
was made the way this section required — by the benchmark, on one corpus with one model and one
window, changing nothing but the embedded text:

| dense representation | Hit@1 | Hit@3 | Hit@5 / Recall@5 | MRR |
| --- | --- | --- | --- | --- |
| `code_raw` | **0.4490** | 0.6735 | 0.7959 | **0.5782** |
| `code_context` | 0.4082 | **0.7347** | **0.8163** | 0.5748 |
| v1 baseline | 0.5918 | 0.7959 | 0.8367 | 0.6963 |

`code_context` renders the labelled envelope v1 embedded (03 §4.2's as-built note): path, unit
kind, name, doc block, signature, then the code. The hypothesis it was built to test — that this
envelope explains v2's regression against v1 — is **not confirmed**: MRR moves by −0.0034, a third
of what a single query changing rank would move it on a 49-query corpus. What the envelope does
buy is *recall over precision*: +0.0612 Hit@3 and +0.0204 Hit@5 against −0.0408 Hit@1. On this
corpus that trade even crosses one gate threshold — `code_context` passes `Recall@5 ≥ 0.80` where
`code_raw` does not — while both fail the MRR budget by roughly the same margin, so the trade
changes which threshold fails, not whether the gate fails.

v0 therefore ships `code_raw`: it is the better ranker at rank 1, it is the cheaper subject (N:1
content sharing — 538 subjects against `code_context`'s 544 for the same 545 occurrences, and no
re-embedding when a file moves), and nothing in the measurement argues for paying more. The
`code_context` implementation stays: it is a registered, searchable representation
(`SearchEngine::with_dense_kind`, `--dense-kind` in `cargo xtask bench`), so the decision is
re-measurable rather than re-implementable when the model, the window, or the corpus changes.

Artifacts: `fixtures/search/baseline/run-v2-2026-07-26-stage-c-{code-raw,code-context}.json`;
reproduce with `ORT_DYLIB_PATH=<lib> cargo xtask bench --corpus <v1 checkout> --subdir src
--dense-kind code_raw|code_context`.

**Degradation, never an error.** Every dense failure — no `code_raw` representation, no provider,
a provider error, an embedding whose length disagrees with the representation, an unopenable or
unrebuildable shard — produces `degraded: "lexical_only"` plus the reason in `diagnostics` (02 §6).
A query with no text is **not** a failure: nothing is embedded, no provider is called, and the leg
is empty but healthy — the same treatment §2's lexical leg gives a termless query.

## 4. Fusion `[SPEC]`

Reciprocal Rank Fusion: `score(d) = Σ_legs 1 / (k + rank_leg(d))`, `k = 60`. Deterministic
tie-break: `(score desc, occurrence_id asc)`. Per-leg candidate depth: `max(limit·4, 50)`.

As-built note (T12-01, `[SPEC]`): the candidate depth is
`local_rag_store::candidate_depth(limit)` (`crates/store/src/cache/fts_query.rs`,
`limit.saturating_mul(4).max(MIN_CANDIDATE_DEPTH)`, `MIN_CANDIDATE_DEPTH = 50`), introduced
with its first caller — the lexical leg. T12-02/T12-03 must take the dense leg's depth from
this same function rather than restate the formula: RRF ranks only mean anything when both
legs were cut at a comparable depth. The lexical leg already applies `(score, occurrence_id)`
ordering internally (§2's as-built note); RRF itself, `k = 60`, and the cross-leg tie-break are
T12-03.

As-built note (T12-03, `[SPEC]`): fusion is `local_rag_search::rrf(lexical, dense, limit)`
(`crates/search/src/fusion.rs`) — a pure function over two already-ranked lists, with no store,
clock or lock, which is what makes its arithmetic hand-checkable in unit tests. `RRF_K = 60`.

Four as-built decisions the section's formula leaves open:

- **Merge key is `occurrence_id`.** A document both legs found is one result carrying both
  ranks (`legs: {lexical, dense}`, §7), never two. Both legs were built to return
  occurrence-identified, 1-based-ranked candidates precisely so fusion never has to translate
  identities (T12-01/T12-02's as-built notes).
- **Ranks only, never scores.** The legs score in incomparable units — BM25 (unbounded, more
  negative is better) and a dense similarity under whichever `distance_metric` the model space
  declares. RRF compares neither; the raw per-leg scores travel for diagnostics and are never
  summed.
- **`f64` accumulator.** `1/(60 + rank)` is not representable in binary floating point, and in
  `f32` two documents whose true scores differ in the seventh digit can collapse into a tie —
  or compare differently depending on which leg was added first. `f64` keeps the error far
  below the smallest gap this fusion can produce (`1/61 - 1/62 ≈ 2.6e-4`).
- **`limit`, not `candidate_depth`, bounds the output.** The deeper per-leg lists exist so
  fusion has material to reorder, not so the caller receives them.

The `(score desc, occurrence_id asc)` tie-break is load-bearing, not cosmetic: fusion merges
through a `HashMap`, whose iteration order is randomized per process, so the sort is the only
thing that makes repeated output byte-stable (§7). Equal scores are the *common* case — any
document both legs return at the same ranks ties with every other such document.

## 5. Modes (v0) `[SPEC mapping of v1 modes]`

| mode | legs |
| --- | --- |
| `hybrid` (default) | lexical + dense(code_raw) [+ description leg post-v0] |
| `lexical` | FTS only |
| `code` | dense code leg only |
| `semantic` | description leg — **post-v0** (only if it wins the benchmark `[FIXED]`); until then returns `UNSUPPORTED_MODE` |

Cross-encoder reranker (`rerank`, `rerank_k`): **post-v0**, additive, only after baseline
`[FIXED]`.

As-built note (T12-03, `[SPEC]`): the mode is `local_rag_protocol::SearchMode`
(`Hybrid`/`Lexical`/`Code`/`Semantic`, default `Hybrid`), and `SearchRequest.mode` selects legs
through its `wants_lexical()`/`wants_dense()` predicates — the table above, in code.

A leg the mode does not ask for is **not run at all**, not run-and-discarded: no FTS validation,
no embedding-provider call, no shard open. `crates/search/tests/response.rs` proves this with an
embedder that panics if reached under `mode=lexical`.

`semantic` is refused with `UNSUPPORTED_MODE` (02 §6's as-built note) **before** worktree
resolution and before any lock — a post-v0 leg cannot become available by doing more work first.
It is deliberately a *recognized* mode (`SearchMode::from_wire("semantic")` succeeds), so a caller
gets "not supported yet" rather than "unknown mode".

**`degraded` in single-leg modes `[SPEC]`**: `degraded` means "you got less than you asked for",
so a `lexical`/`code` request whose one leg served reports `degraded: null` — nothing was skipped.
When that single leg *cannot* serve, the answer is `INDEX_UNAVAILABLE`, not a degraded response:
there is no second leg to fall back to, because the caller did not ask for one. Only `hybrid`
produces `lexical_only`/`dense_only`. This generalizes `local_rag_store::requires_index_unavailable`
(T08-03's both-legs-down predicate), which cannot express "not requested".

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

As-built note (T12-03, `[SPEC]`): the response is `local_rag_protocol::SearchResponse`
(`crates/protocol/src/search.rs`) with `SearchResult`, `LegRanks` and `GenerationRef` — in
`protocol` rather than `search` for the same reason `ErrorEnvelope` is: it is the wire contract
every caller of the MCP `search_code` tool sees (11 §2), not one subsystem's internal shape.
`SearchEngine::search_code` returns it directly.

**Serialization.** These types derive `serde::Serialize` in exactly this section's shape, and
`DegradedMode` serializes as its `dense_only`/`lexical_only` string (`None` → `null`). This is the
first serialization in `crates/protocol` — it implements a shape this section already fixes rather
than inventing one, and it is what makes T12-03's "repeated output is byte-stable" an assertion
about bytes: `serde_json` emits fields in declaration order, so §4's deterministic *value* yields
deterministic *bytes* (`crates/search/tests/response.rs::
repeated_identical_requests_serialize_to_identical_bytes`). `Deserialize` is deliberately not
derived — nothing reads these back before group 15. Transport, handshake and MCP framing remain
group 15's.

**Absent is absent, not null**: `snippet`, `qualified_name` and each `legs` entry are omitted when
they have no value, rather than serialized as `null` — a leg that did not match and a leg that
matched at "rank 0" must not look alike to a reader.

**Field sources.** `path`/`name`/`qualified_name`/`unit_kind`/`span`/`language` come from the new
`local_rag_store::occurrences_by_id` — the same `generation_unit_occurrence ⋈ parsed_unit ⋈
content_blob` join `occurrences_for_fts` uses, restricted to the fused hits instead of the whole
generation, and re-ordered to the caller's (fused) order, which SQL knows nothing about. `name` is
the empty string when the unit has no `local_name` (a file/text/config unit need not).
`generation.number` comes from the new `local_rag_store::generation_number`. A fused hit whose
occurrence row is missing — structurally impossible under a held `L2.read`, since a generation's
occurrence set is immutable once `projection_ready` — is dropped with a diagnostic rather than
presented with empty fields.

As-built note (T12-04, `[SPEC]`): `snippet` is now filled. `local_rag_search::snippet::cut`
slices `[span_start, span_end)` out of the revision's stored bytes
(`local_rag_store::source_bytes`) and caps the result at `SNIPPET_CAP_BYTES = 8 * 1024`
(12 §2's `[SPEC]` cap); a truncated snippet carries 12 §2's `[FIXED]`
`{hash, original_size}`, where the hash is over the **full** span (the new
`Domain::TruncatedExcerpt`, 03 §1.2) — hashing what survived would answer the wrong
question, since the metadata exists to describe what was cut.

Two details the cap creates and the span does not. **UTF-8**: unit spans are
character-aligned by construction (tree-sitter), but 8 KiB lands wherever it lands, so the cut
moves back to the nearest boundary (at most three bytes) — otherwise the first emoji- or
CJK-heavy file would fail `String::from_utf8` and lose its excerpt entirely. **Batching**: the
bytes are read once per `file_revision_id`, not once per hit — ten results from one file share
one revision, and `source_bytes` decompresses the whole revision on every call.

A snippet that cannot be produced (span outside the revision, non-UTF-8 content in a file that
classified as text — both corruption signals) leaves `snippet: None` **and** a diagnostic: the
hit keeps its metadata and its rank, because the ranking is still correct even when the excerpt
is not available.

Serialization keeps this section's documented shape: an untruncated snippet serializes as the
plain string shown above, and only a truncated one widens to
`{"text": …, "truncation": {"hash": …, "original_size": …}}` — the case that has something more
to say.

`PipelineSnapshot` survives as the **instrumented** return of
`SearchEngine::search_code_instrumented`, carrying the response plus what the wire deliberately
omits — the model space served (T09-04's "exactly one generation/model tuple" load test) and each
leg's raw pre-fusion candidates (T12-01/T12-02's per-leg suites). `search_code` itself returns
`SearchResponse` and nothing more.

## 8. Latency gates (numbers after baseline `[OPEN]`)

warm search p95; one-file reconcile p95; branch-checkout reconcile — tracked per 14 §2.
