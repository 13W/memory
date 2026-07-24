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

As-built note (T11-03, `[SPEC]`): the pool is `local_rag_embed`
(`crates/embed`, workspace crate depending only on `core`/`store`/`protocol`).

* **Trait**: [`Embedder`] is exactly the two `[FIXED]` methods above. Locality is deliberately
  *not* a trait method — it lives on the pool's `ProviderEntry` (`local()`/`remote()`), so the
  guard's input cannot be supplied by the provider being guarded.
* **Shapes** (the spec names but does not shape them): `EmbedRequest { kind, texts }` is a batch
  by construction (the `[FIXED]` return type is `Vec<Vector>`), and results are **positional** —
  `result[i]` embeds `texts[i]`, `result.len() == texts.len()`. The pool enforces both, plus
  "every vector has exactly `key().dimensions` components", turning a provider's contract
  violation into a typed `ResultCountMismatch`/`DimensionMismatch` instead of letting a malformed
  vector reach `embedding_cache` (whose key is the subject, not the position). `Vector` is a
  newtype so a raw vector is never confused with §4.2's little-endian *bytes*. An empty batch is
  answered without selecting a provider.
* **Order of operations**: guard → primary/fallback → retry. The guard filters candidates before
  selection (`allows(policy, locality)`), so under `local_only` a remote provider is never
  invoked — not even as a fallback — and a remote-only pool yields
  `EmbedError::PolicyBlockedRemote`, whose protocol envelope is the new
  `ErrorCode::PolicyBlockedRemote` (02 §6). The *effective* policy is computed by the caller via
  `local_rag_store::effective_data_policy` (T02-05) and never recomputed or relaxed here. The
  remaining policies differ in payload semantics (metadata-only/redaction/full), which stay
  T16-01's card; this task ships the seam and the one `[FIXED]` rule that is testable today.
* **Retry numbers** (`[SPEC]`, the spec pins none): 4 attempts per provider — the same budget the
  imported v1 fixtures use — with a 250 ms exponential floor doubling to a 4 s cap, the shape 02
  §4.2 already fixed for the proxy handshake, chosen because both are short user-facing
  operations. A server-supplied hint (`Retry-After`, or v1's "retry in Xs" body hint) wins over
  the floor but is still capped, so a hostile provider cannot park a worker thread. A permanent
  failure is never retried; it falls through to the next provider. The seven `fault.llm.*` cases
  of `fixtures/fault/index.json` are replayed case-by-case in `crates/embed/tests/retry.rs`.
* **In-process default**: `HashingEmbedder` — deterministic feature hashing, no ML runtime, no
  weights, no network — registered under its own bootstrap `model_id` `local-hashing-v1`. It
  makes "the local backend is the working default" literally true today and is the deterministic
  model fixture the tests embed with. The ONNX provider for the model ADR-0004 selects arrives
  with its weights in T11-06 (§5); the split is recorded as `D-008`. Because `model_id` is one of
  the six canonical key fields (03 §2.2), bootstrap vectors can never be mistaken for production
  ones.

## 2. Representations registry

`representation` rows are the canonical serialization of `RepresentationKey`
(03 §2.2, unique across all six fields) — duplicate registrations caused by serialization
drift are impossible by constraint `[FIXED]`. `embedding_cache` rows reference
`representation_id`, never inline model params.

Representation kinds: `code_raw`, `code_context`, `structural_description` (post-v0),
`memory`. Subject hashing per kind: 03 §1.2.

As-built note (T11-02, `[SPEC]`): `embedding_cache` itself now exists
(`local_rag_store::cache::embedding`, migration 4, spec 03 §4.2's own as-built note has the full
detail — integrity/checksum, little-endian vectors, the batched `last_used_at` seam, and
budget-LRU eviction with active/rebuild pins, `local_rag_store::eviction`). Real per-subject
coverage counting against these rows (T11-04) and the local embedder provider that writes them
(T11-03) are still separate, later tasks — T11-02 only shipped the cache and its own integrity/
eviction guarantees.

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

As-built note (T11-03, `[SPEC]`): the **default model choice** half of that `[OPEN]` is resolved —
**`embeddinggemma-300m`, 768 dimensions, cosine** (ADR-0004, which records the measured candidate
comparison, the explicit criteria weights, and the runner-up
`jina-embeddings-v2-base-code` should the Gemma terms or the T12-05 gate force a switch; a switch
costs one model-space migration per §4, not a redesign). Delivery details — which quantization is
fetched, the runtime that loads it (`fastembed` vs `Candle`, §1), and the installer itself — remain
`[OPEN]`/T11-06; quantization is a delivery choice and does not change `RepresentationKey`.
The installer MUST surface and persist the model's license (Gemma Terms of Use, not an OSI license)
in `models/embeddinggemma-300m/manifest.json`, which is what makes a non-redistributed,
user-downloaded non-OSI default acceptable here.

The consumer half of the `.ok` contract already exists: `local_rag_embed::require_model_assets`
treats a model directory as usable **only** with its `.ok` marker present (a `.part`/half-renamed
download is "missing"), returning a typed `ModelAssetsMissing` and performing no network access.

## 6. Memory relevance backend

v0: FTS + brute-force cosine over active memory entries (bounded cardinality) behind the
relevance trait; switch to ANN only on cardinality/latency metrics `[FIXED]` (08 §6).
