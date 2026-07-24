# 10 — Embeddings, Representations, Model Spaces

## 1. Execution `[FIXED]`

Embeddings run **in-process**: `fastembed` (ONNX Runtime) or `Candle`. Local generation
(consolidation router, descriptions post-v0) via one of `llama-cpp-2` / `mistral.rs` /
`kalosm` `[OPEN — pick with default model]`. **The local backend is the working default**;
Ollama/remote providers are strictly optional (anything else contradicts "no mandatory
external daemons") `[FIXED]`. `data_policy` default `local_only` `[FIXED]`.

Provider pool traits `[FIXED]`:

```rust
trait Embedder  { fn embed(&self, req: EmbedRequest) -> Result<Vec<Vector>>; fn key(&self) -> RepresentationKey; }
trait Generator { fn generate(&self, req: GenRequest) -> Result<GenResponse>; }
```

Primary/fallback + retry semantics inherited from the v1 behavioral contract `[FIXED]`.
Every remote call is gated by the effective `data_policy` (02 §3.2) *before* the provider is
selected; `local_only` never falls back to remote.

## 2. Representations registry

`representation` rows are the canonical serialization of `RepresentationKey`
(03 §2.2, unique across all six fields) — duplicate registrations caused by serialization
drift are impossible by constraint `[FIXED]`. `embedding_cache` rows reference
`representation_id`, never inline model params.

Representation kinds: `code_raw`, `code_context`, `structural_description` (post-v0),
`memory`. Subject hashing per kind: 03 §1.2.

## 3. Model spaces

A model space bundles the representations that must be coherent together (at minimum
`code_raw` + `memory` in v0 `[SPEC]`). Registry/build state machine: 04 §3. Coverage =
expected/ready set per **required** representation kind — not just a failed counter `[FIXED]`;
stored advisory JSON, always recomputable from `state.sqlite` × `embedding_cache`.

As-built note (T11-01, `[SPEC]`): both sections are `local_rag_store::registry::representation`
(migration 6, `SCHEMA_V6`). §2's canonical `RepresentationKey` is a six-field struct (`kind`,
`representation_version`, `normalization_version`, `model_id`, `dimensions`, `distance_metric`);
`register_representation` is a single atomic `INSERT ... ON CONFLICT (the six fields) DO UPDATE ...
RETURNING representation_id`, so a duplicate key converges on the first-registered id rather than
erroring — the constraint alone would only prevent a second row, not hand back the existing one.
§3's build machine (`ModelSpaceState`) is byte-for-byte the same transition shape as 04 §1's
Generation machine (`building → projection_ready → active → retiring`, plus
`building|projection_ready → failed`); `eligible_as_target` is `true` only for `Active`, so an
incomplete or retiring model space can never become a switch target. Coverage
(`CoverageEntry{expected,ready,failed}` per required kind) is tracked only for kinds
`model_space_representation` marks `required`; `recompute_coverage` is a pure function over
caller-supplied counts (real per-subject counting against `embedding_cache` is T11-04's backfill
worker, not this registry's job), and `transition_model_space` requires
`Coverage::fully_covered` before allowing `building → projection_ready`. `model_space` itself and its
default-`active` seed already existed (T07-02, `SCHEMA_V4`); T11-01 added only the missing state
machine over it. This task did not wire `crates/projection`'s hardcoded
`REQUIRED_REPRESENTATION_KINDS` constant to this registry — that is T11-05's, once a real
multi-model-space switch needs it (05 §4/§5's own as-built notes carry the same forward reference).

## 4. Model migration — double-buffer via model spaces `[FIXED]`

Changing the embedding model (or dimensions/metric/normalization):

```
1. Register new representations + model space B (state='building').
2. Backfill worker embeds expected content into embedding_cache under B's
   representation_ids (batch, resumable; coverage tracks progress).
   Different dimensions ⇒ separate shard layout / named-vector — never in place [FIXED].
3. B → projection_ready (full required coverage) → benchmark (optional gate) → active.
4. Per-worktree switch: standard write-ahead switch (05 §5) on the MODEL axis,
   serialized with generation switches by the same per-worktree writer [FIXED].
5. default_model_space := B. Dormant worktrees migrate at next open (05 §8) —
   no global write barrier [FIXED].
6. A → retiring; its cache rows become evictable when no worktree references A.
```

No in-place re-embed, no migration without rollback: until step 4 commits for a worktree, that
worktree still runs A entirely `[FIXED]`.

## 5. Model assets `[FIXED policy]`

Weights are **not** in npm. `local-rag init --download-models`: checksum-verified manifest,
atomic download (`.part` → fsync → rename → `.ok` marker), offline operation afterwards.
`models/<model_id>/manifest.json` records source, size, sha256, license. Default model choice
and delivery details `[OPEN]`. ORT bundling verified before the final CI matrix `[FIXED]`.

## 6. Memory relevance backend

v0: FTS + brute-force cosine over active memory entries (bounded cardinality) behind the
relevance trait; switch to ANN only on cardinality/latency metrics `[FIXED]` (08 §6).
