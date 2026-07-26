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

As-built note (G11/D-013, `[SPEC]`): the "at minimum `code_raw` + `memory`" bundle is **not yet
populated for the seeded default space**, and the gate records who owns each half rather than
leaving it unowned. The seeded space (`SCHEMA_V4`) is `active` with no `model_space_representation`
rows at all, so on a store nobody has configured, `required_code_kinds` refuses with
`NoCodeRepresentation` and `params_for_model_space` with `NoShardParams` — loud, not silent. The
registration API exists (`local_rag_embed::register_embedder_representation`, T11-03); what is
missing is a caller.

Deliberately not seeded by a migration: the key's `model_id` is the ADR-0004 choice, and baking it
into `state.sqlite`'s DDL would both hard-code a decision §4 exists to keep migratable and make
every fresh store claim a model whose weights may never have been downloaded (§5). It therefore
belongs where the concrete provider is constructed — the `code_raw` half to **T15-07**'s `init`
(the same command that installs the weights), the `memory` half to **group 14**, which owns the
memory subject function `Coverage` would otherwise count against (§3's `UnsupportedRequiredKind`
refusal exists precisely so an unowned required kind cannot read as "covered").

As-built note (T11-04, `[SPEC]`): the backfill worker is `local_rag_embed::backfill`
(`run_backfill`), and it fixes the two things §3/§4 leave to the implementation.

**"the content they are expected to cover" (04 §3) is the retention pin roots** (06 §5), unioned
across every worktree: generations in `active`/`building`/`projection_ready` unconditionally plus
`retiring` ones inside the `K`/`T` window. Computed once in `local_rag_store::subjects` and consumed
from both sides — the worker embeds what is missing from that set, eviction refuses to evict what is
in it — so the two can never chase each other. Expected **subjects**, not points: spec 05 §4's point
set is `occurrences × required kinds`, collapsed here by each kind's subject function, which for
`code_raw` is a real N:1 collapse over `blob_id` (§4.2 `[FIXED]`: content-blob embeddings are shared
across paths). A `required` kind with no subject function — `code_context` (`[OPEN]`, 09 §3) or
`memory` (group 14) — makes the worker refuse with `UnsupportedRequiredKind` rather than report zero
expected, which `Coverage::fully_covered` would read as "covered".

**Resumability is recomputation, not a journal.** There is no progress table: each run recomputes
`missing = expected \ valid_cached`, embeds in bounded batches outside any transaction (02 §5, "L4
queues are leaves"), commits ≤ `write_batch_rows` rows per `cache.sqlite` transaction, and finally
writes coverage in its own `state.sqlite` transaction (03 §1.4 forbids one transaction across both).
A kill at any point is healed by running again — proven by `backfill_resume.rs`, which kills at every
batch boundary through the named `embed.backfill.between_batches` failpoint and asserts the sequence
converges without re-embedding anything. A row that fails `verify_cached_embedding` is deleted and
re-embedded (§4.4 step 4); a provider failure counts into `failed`, never `ready`, so an incomplete
run cannot promote the space. `promote_if_covered` applies the gate through
`transition_model_space`, which reads the **stored** coverage.

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

As-built note (T11-05, `[SPEC]`): steps 4–6 are `local_rag_projection::model_switch`.

* **Step 4** is `switch_model_space` — the *same* `switch()` the generation axis uses (05 §5), given
  the worktree's current generation so only the model axis moves; the one-axis rule is enforced
  independently by `check_invariants`'s `BothAxesMovedAtOnce`. It refuses before the write-ahead
  when the target is not `active` or its stored coverage is short, so a refusal leaves both
  `state.sqlite` and the shard untouched.
* **Step 2's "different dimensions ⇒ separate shard layout"** is realized as a per-model-space shard
  directory (05 §2's own T11-05 note): `ShardParams` are derived from the space's `code_raw`
  `representation.dimensions`, and each space owns `projection/<worktree_id>/<model_space_id>/`.
  This is also what makes the closing `[FIXED]` sentence above literally true — a kill anywhere
  between the write-ahead and the commit leaves A's shard complete and serving, proven by
  `model_switch_faults.rs`.
* **Step 5** is `local_rag_store::set_default_model_space_id`, the only writer of that pointer; it
  refuses a space that is not `active`, so 04 §3's "the default space MUST be `active`" is enforced
  where the value is established. "No global write barrier" is structural rather than promised:
  every operation here is per-worktree and spec 02 §5's hierarchy has no store-wide write lock at
  all.
* **Step 6** needs no new code: once the last worktree has moved off A and A is `retiring`,
  `local_rag_store::subjects::protected_model_space_ids` (T11-04) stops pinning it, which is exactly
  "its cache rows become evictable when no worktree references A".

As-built note (G11/D-011+D-012, `[SPEC]`): the gate found two gaps around steps 5–6, both now
closed in the store.

* Step 6 covers A's **cache rows**; A's per-worktree **shard directory** (the one the T11-05 split
  gave it) was reclaimed by nothing. `local_rag_store::housekeeping::run_unreferenced_space_sweep`
  is that reclamation — see 05 §8's own D-011 note for the liveness rule and why it is race-free
  against a switch in flight.
* Steps 5 and 6 are ordered in the recipe, and that order is now **enforced** rather than assumed:
  retiring A while it is still `default_model_space_id` is refused (04 §3's D-012 note). Doing step 6
  first used to produce a store whose default was unusable.

## 5. Model assets `[FIXED policy]`

Weights are **not** in npm. `local-rag init --download-models`: checksum-verified manifest,
atomic download (`.part` → fsync → rename → `.ok` marker), offline operation afterwards.
`models/<model_id>/manifest.json` records source, size, sha256, license. Default model choice
and delivery details `[OPEN]`. ORT bundling verified before the final CI matrix `[FIXED]`.

As-built note (T11-03, `[SPEC]`): the **default model choice** half of that `[OPEN]` is resolved —
**`embeddinggemma-300m`, 768 dimensions, cosine** (ADR-0004, which records the measured candidate
comparison, the explicit criteria weights, and the runner-up
`jina-embeddings-v2-base-code` should the Gemma terms or the T12-05 gate force a switch; a switch
costs one model-space migration per §4, not a redesign).
The installer MUST surface and persist the model's license (Gemma Terms of Use, not an OSI license)
in `models/embeddinggemma-300m/manifest.json`, which is what makes a non-redistributed,
user-downloaded non-OSI default acceptable here.

The consumer half of the `.ok` contract already exists: `local_rag_embed::require_model_assets`
treats a model directory as usable **only** with its `.ok` marker present (a `.part`/half-renamed
download is "missing"), returning a typed `ModelAssetsMissing` and performing no network access.

As-built note (T11-06, `[SPEC]`): the **delivery details** half of the `[OPEN]` above is now
resolved too (ADR-0005), closing `D-008`. `local_rag_models` is the producing half of the same
contract:

* **Runtime**: ONNX Runtime through `ort` with `load-dynamic` — nothing is downloaded or linked at
  build time, `libonnxruntime` is resolved at runtime (`ORT_DYLIB_PATH` or the loader path), and its
  absence is a typed error. This is what keeps a clean build and the whole quality gate offline;
  bundling the library per platform package stays the "ORT bundling verified before the final CI
  matrix" `[FIXED]` item above, owned by T17-03.
* **Quantization**: q8 (`model_quantized`) — three files totalling 314.5 MiB
  (`model_quantized.onnx`, `model_quantized.onnx_data`, `tokenizer.json`). Still a delivery choice:
  it does not appear in `RepresentationKey`.
* **Verification**: every file's `size` and lowercase-hex `sha256` are pinned in the binary and the
  source URL pins an immutable upstream revision. "Checksum-verified" means verified against the
  compiled-in catalog; `manifest.json` is disclosure, never the authority a download is checked
  against.
* **Ordering**, per file: `<name>.part` → stream while hashing → `sync_all` → verify size **and**
  digest → `rename` → fsync the directory. Then `manifest.json` by the same atomic path, and `.ok`
  last. Everything before the marker is by construction indistinguishable from "not installed".
* **`manifest.json` schema**: `model_id`, `source`, `revision`, `license`, `license_url`,
  `dimensions`, and `files[] = {path, size, sha256}` — a superset of the four fields this section
  requires.
* **Resumability**: no journal. Each run re-derives what is missing by hashing what is on disk
  against the pinned digests; a leftover `.part` is overwritten, never trusted or appended to. A
  rerun after an interrupt refetches only the missing files, and a run after a completed install is
  a no-op.
* **The license notice** is written to a caller-supplied sink **before** the first fetch and without
  prompting, so `init` stays scriptable; a no-op install does not reprint it.
* **Data policy**: downloading model assets is **not** gated on `data_policy` (12 §1). See that
  section's as-built note — the guard governs repository content leaving the machine, and this is an
  explicit user command pulling public bytes in.
* Permissions follow 02 §2.1: `models/<model_id>/` is 0700 and every installed file is 0600 on unix.

`local-rag init --download-models` as a **command** is T15-07's CLI surface (`serve/status/stop/
restart/init`); T11-06 delivers the typed API it calls. Excluding weights from the npm packages is
T17-01's packaging test.

## 6. Memory relevance backend

v0: FTS + brute-force cosine over active memory entries (bounded cardinality) behind the
relevance trait; switch to ANN only on cardinality/latency metrics `[FIXED]` (08 §6).
