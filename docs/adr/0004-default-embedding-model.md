# ADR-0004: Default embedding model for v0

## Status

Accepted — 2026-07-24. **Amended 2026-07-26 (D-016)** — see
"Amendment: sequence window" below: the canonical key's `representation_version`
is now `2` and the sequence window is 1024 tokens. The model, dimensionality and
distance metric this ADR decided are unchanged.

Closes the **embedding half** of open question **O3 "Default embedding model +
weights delivery; local generator crate"**
([spec 15 §4](../specification/15-roadmap.md)), whose resolution path is "model
evaluation + [10 §5](../specification/10-models-and-embeddings.md)". Delivered by
task **T11-03** ([group 11](../implementation-plan/groups/11-embeddings-and-model-spaces.md)).
The other two halves of O3 stay open and visible, each with its own owner:
**weights delivery** is T11-06 (spec 10 §5's installer), and the **local
generator crate** (`llama-cpp-2` / `mistral.rs` / `kalosm`, spec 10 §1
`[OPEN — pick with default model]`) is T14-07. Partial closure follows the
precedent of ADR-0002, which resolved the `SyntaxLocator` half of O7 and left
graph semantics open. Convention (`docs/adr/NNNN-title.md`, Nygard sections,
English) is ADR-0001's.

## Context

Spec 10 §1 `[FIXED]` already fixes the *execution model* — "Embeddings run
**in-process**: `fastembed` (ONNX Runtime) or `Candle`… **the local backend is
the working default**; Ollama/remote providers are strictly optional" — and
spec 10 §5 `[FIXED policy]` fixes *delivery*: weights are not in npm, they are
installed by `local-rag init --download-models` with a checksum-verified
manifest recording "source, size, sha256, license". What neither fixes is
**which model** — "Default model choice and delivery details `[OPEN]`".

The choice is load-bearing in three places. It fixes `dimensions` and
`distance_metric` inside the canonical six-field `RepresentationKey`
([spec 03 §2.2](../specification/03-data-model.md)), so it decides the shard
layout — spec 10 §4 `[FIXED]`: "different dimensions ⇒ separate shard layout /
named-vector — never in place". It decides what a user must download before code
search works at all (init UX). And it sets the semantic ceiling the 49-query
benchmark measures in T12-05.

Two constraints narrow the field before any measurement:

* the model must be usable **offline, in-process, on the CPU**, with no external
  daemon (spec 01 §1 `[FIXED]`: no mandatory external daemons — explicitly
  including Ollama, which is what the v1 baseline used);
* the license must permit ordinary commercial software development, since this
  is a developer tool; the license string is recorded per model in
  `models/<model_id>/manifest.json` (spec 10 §5).

### The v1 anchor

The v1 baseline run — the reference point the v2 gate is measured against
(spec 14 §7; `fixtures/search/baseline/baseline.md`) — used
`embeddinggemma:300m` (dim 768) via Ollama, scoring Hit@1 0.5918, Hit@5 0.8367,
MRR 0.6963 over 544 chunks. ADR-0003 already anchored the dense-backend spike's
datasets on that same `dims=768`. A v2 default that changes both the model *and*
the pipeline would make the T12-05 comparison ambiguous: a regression could not
be attributed.

### Candidates and measurements

Five ONNX-published candidates were considered. Local measurements (one host,
macOS arm64, ONNX Runtime 1.27 CPU execution provider, batch of 32 code snippets
at `max_length=256`, 1 warmup + 3 timed runs, median) are committed as
[`artifacts/0004-embedding-measurements.json`](artifacts/0004-embedding-measurements.json);
weights were downloaded into the local Hugging Face cache and are **not**
committed (`CLAUDE.md`).

| Model | License | dim | Quantized asset | `load_ms` | ms/text | Deterministic |
| --- | --- | --- | --- | --- | --- | --- |
| EmbeddingGemma-300M | Gemma Terms of Use | 768 (MRL 512/256/128) | 295.1 MiB (q8) | 64.3 | 43.2 | yes |
| jina-embeddings-v2-base-code | Apache-2.0 | 768 | 154.4 MiB | 149.9 | 52.6 | yes |
| nomic-embed-text-v1.5 | Apache-2.0 | 768 (MRL) | 130.9 MiB | 165.7 | 54.4 | yes |
| bge-small-en-v1.5 | MIT | 384 | 126.9 MiB (fp32 only) | 52.2 | 13.7 | yes |
| Qwen3-Embedding-0.6B | Apache-2.0 | ≤1024 | 871.8 MiB (q4) | 1173.5 | — | — |

fp32 variants were measured too (EmbeddingGemma 1177.8 MiB / 41.2 ms per text;
jina-code 611.8 / 50.3; nomic 522.0 / 41.7); EmbeddingGemma's q4 export is
smaller but slower on CPU (188.1 MiB, 69.1 ms per text), so q8 is the operating
point this ADR quotes.

Findings that are load-bearing:

* **Qwen3-Embedding-0.6B is not usable as a plain ONNX encoder.** Its published
  ONNX export is decoder-style and demands 56 `past_key_values.*` inputs; a
  straightforward `input_ids`/`attention_mask` feed fails outright, and the q4
  asset is 871.8 MiB with a 1.17 s session load. Recorded as measured, excluded
  on platform grounds.
* **jina-code-embeddings-0.5b (the newer, stronger Jina code model) is
  CC-BY-NC-4.0** — non-commercial. A tool developers run on commercial code
  cannot default to it, so it is excluded on license grounds regardless of
  quality.
* **Only EmbeddingGemma publishes comparable code-retrieval numbers.** Its model
  card reports MTEB (Code, v1) mean 68.76 at 768d and 66.74 at 256d (MTEB
  English v2 mean 69.67). bge-small-en-v1.5 publishes MTEB (English) 62.17 and
  nomic-embed-text-v1.5 publishes 62.28, neither with code figures;
  jina-embeddings-v2-base-code publishes no comparable score at all — its card
  and product page claim leadership on "nine out of fifteen" CodeSearchNet
  benchmarks without stating values.
* **Matryoshka truncation is a real operational lever.** EmbeddingGemma's own
  numbers put the 768 → 256 cost at 2.02 MTEB-Code points (68.76 → 66.74) for a
  3× smaller vector — a decision this project can take later, per model space,
  without re-picking a model.

## Decision

**The default embedding model for v0 is EmbeddingGemma-300M at 768 dimensions
with cosine distance.**

The canonical representation key (spec 03 §2.2) for the default `code_raw`
representation is therefore:

```
kind                   = code_raw
representation_version = 2          # was 1; raised by D-016, see the amendment
normalization_version  = 1          # local_rag_store::code::normalize
model_id               = embeddinggemma-300m
dimensions             = 768
distance_metric        = cosine
```

### Explicit weights

| Dimension | Weight | EmbeddingGemma | jina-code | nomic-1.5 | bge-small |
| --- | --- | --- | --- | --- | --- |
| Published code-retrieval quality | 0.35 | **5** (MTEB Code 68.76) | 4 (specialized, unquantified) | 3 | 2 |
| License suitability | 0.25 | 2 (Gemma Terms, not OSI) | **5** | **5** | **5** |
| Platform: size, load, latency | 0.20 | 3 | 4 | 3 | **5** |
| Comparability with the v1 baseline | 0.10 | **5** (same model, same dim) | 4 | 4 | 2 |
| Operational flexibility (MRL) | 0.10 | **5** | 2 | **5** | 2 |
| **Weighted total** | | **3.85** | 4.05 | 3.80 | 3.35 |

The arithmetic favors jina-embeddings-v2-base-code, and the decision does not.
That is deliberate and is the one judgment call in this ADR: the license column
is the only place jina wins materially, and its advantage there is smaller than
it looks, while its loss in the quality column is larger than it looks.

* **On license.** This project never redistributes weights — spec 10 §5
  `[FIXED policy]`: "Weights are **not** in npm". The user's own
  `init --download-models` fetches them from upstream, and the installer records
  `license` in `models/<model_id>/manifest.json`. Gemma Terms permit commercial
  use and redistribution subject to the Prohibited Use Policy and passing the
  terms along; since we distribute no weights, the obligation that reaches this
  project is disclosure, which the manifest already carries by spec. A non-OSI
  license is a genuine cost, recorded here rather than smoothed over — it is not
  a blocker for a tool that downloads weights on the user's behalf.
* **On quality.** "Specialized for code" is an architecture claim, not a
  measurement. jina-embeddings-v2-base-code is a 2023-vintage 137M-parameter
  BERT variant whose only public evidence is an unquantified leaderboard claim;
  EmbeddingGemma is a 2025 300M model with a published, third-party-comparable
  MTEB Code score. Choosing the unmeasured candidate over the measured one, in a
  project whose own rule is "collect metrics, do not invent thresholds"
  (O2, spec 14 §2), would be exactly the inversion that rule exists to prevent.
* **On the gate.** T12-05 compares v2 against a v1 baseline produced by *this
  model family*. Keeping the model constant makes that comparison attributable
  to the v2 pipeline, which is what spec 14 §7 asks the gate to measure.

**Runner-up: jina-embeddings-v2-base-code (Apache-2.0, 768d).** If T12-05 shows
the default failing the quality gate, or if the Gemma terms become unacceptable
for a distribution channel, switching costs exactly one model-space migration —
spec 10 §4's `[FIXED]` double-buffer: register representations under a new model
space, backfill, flip per worktree, retire the old. The dimension is identical,
so even the shard layout is unchanged. This ADR is a *default*, not a lock-in,
and the machinery that makes it revisable is already built (T11-01/T11-02).

### What this ADR does not decide

* **The runtime.** Spec 10 §1 `[FIXED]` allows `fastembed` (ONNX Runtime) *or*
  `Candle`; picking between them is an implementation choice that belongs with
  the task that actually links one — T11-06, together with the weights it
  installs and the "ORT bundling verified before the final CI matrix"
  `[FIXED]` check (spec 10 §5, 13 §1). Measurements above used ONNX Runtime
  1.27 because every candidate publishes ONNX weights, not as a runtime
  commitment.
* **Quantization for distribution.** q8 is the measured operating point;
  fp32/q4/fp16 remain available. The installer's manifest records exactly which
  file was fetched, so this is a delivery decision (T11-06), not an identity
  one — quantization does not change `RepresentationKey`, and any drift that did
  would be caught by `representation_version`.
* **Quality thresholds.** O2 stays open: the numbers above are published
  third-party benchmarks, not this project's gate. The gate is the 49-query run
  in T12-05, against `fixtures/search/baseline/baseline.md`.
* **Matryoshka truncation to 256d.** Available, quantified above, deliberately
  not taken in v0 — 768 keeps baseline comparability. Taking it later is a
  model-space migration with a different `dimensions`, i.e. a new shard layout,
  exactly as spec 10 §4 requires.

## Amendment: sequence window (2026-07-26, D-016)

**The sequence window is 1024 tokens, and the canonical key's
`representation_version` is therefore `2`.** Model, dimensionality and distance
metric are untouched; this amendment changes neither the comparison above nor its
verdict.

### Why

T12-05 measured v0 search on the 49-query benchmark at MRR 0.5646 against the v1
baseline's 0.6963. Investigating the gap showed that v1 truncated embedding input
at **3000 characters** (`scripts/benchmark.ts::MODEL_CONFIGS`) — roughly 750–1000
tokens of code — while this project truncated at `MAX_SEQUENCE_TOKENS = 256`, about
three times more aggressively. Comparing retrieval quality across that difference
measures the truncation as much as the retrieval. 1024 covers v1's effective window
with room to spare and still sits at half of EmbeddingGemma's 2048-token context.
Measured effect on the benchmark: **+0.0102 MRR, +0.0204 Hit@5**.

### Why the key had to move with it

The window is **not** one of the six `RepresentationKey` fields (spec 03 §2.2), so
raising it alone would have produced different vectors under an unchanged
`representation_id` — `embedding_cache` would then serve 256-token rows as though
they were 1024-token ones, with nothing to detect it. `representation_version`
exists precisely to make a vector-affecting change outside the other five fields
addressable, so it moves `1 → 2` in the same change. Every cached vector from the
256-token era becomes unaddressable rather than silently reused.

### What this invalidates in the text above

The **latency and throughput figures** in this ADR's Context and Consequences were
measured at `max_length = 256` (they say so at their point of use). They no longer
describe the shipped configuration: cost per sequence scales with sequence length,
so the ≈23 snippets/s figure is an upper bound for 256-token inputs, not a
prediction for 1024. The *relative* comparison between candidate models is
unaffected — every candidate was measured under the same conditions — so the
decision stands. Re-measuring absolute throughput at the shipped window belongs to
T17-05's resource gate, which owns those numbers anyway.


## Consequences

* Spec 10 §5's `[OPEN]` "Default model choice" is resolved to
  `embeddinggemma-300m` and recorded as a `[SPEC]` as-built note citing this
  ADR; spec 15 §4's O3 row is amended to show the embedding half resolved with
  the delivery and generator halves still open (T11-06, T14-07). No `[FIXED]`
  text is changed by this ADR; only `[OPEN]` → resolved `[SPEC]` amendments.
* **T11-03 ships no ONNX runtime.** The provider pool, the policy guard, the
  retry/fallback contract and a deterministic in-process provider land now;
  loading *this* model lands in T11-06 with its weights. That split is recorded
  as `D-008` in `docs/implementation-plan/DEVIATIONS.md`, and T11-06's card was
  extended so the requirement has an owner rather than a backlog entry. Until
  then the working local default is `local-hashing-v1`
  (`local_rag_embed::HashingEmbedder`) — a bootstrap representation under its own
  `model_id`, which by construction cannot be confused with this ADR's model:
  `model_id` is one of the six fields of the canonical `RepresentationKey`.
* **The installer inherits a license obligation.** T11-06 must surface the
  Gemma Terms (source URL and license string) before download and persist them
  in `models/embeddinggemma-300m/manifest.json`, per spec 10 §5.
* **Asset budget.** A first-run download is ≈295 MiB at q8 (or ≈188 MiB at q4,
  at ~1.6× the CPU latency); embedding throughput on this host is ≈43 ms per
  256-token snippet single-threaded-ish (4 intra-op threads), i.e. ≈23 snippets
  per second, which sets expectations for T11-04's backfill worker.
* **The measurement artifact is reproducible and committed**
  (`artifacts/0004-embedding-measurements.json`), including the two negative
  results (Qwen3's decoder-style export, jina-code-0.5b's non-commercial
  license) so a future revisit starts from evidence rather than from scratch.
