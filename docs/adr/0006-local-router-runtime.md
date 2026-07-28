# ADR-0006: Local router runtime and model

## Status

Accepted — 2026-07-28.

Closes the **local generator crate half** of open question **O3 "Default embedding
model + weights delivery; local generator crate"**
([spec 15 §4](../specification/15-roadmap.md)) — the third and last half of O3,
continuing from [ADR-0004](0004-default-embedding-model.md) (embedding model) and
[ADR-0005](0005-model-delivery.md) (weights delivery). Also produces the
first real, measured numbers for **O2**'s memory-router half (spec 08 §7's
`[OPEN]` "target P/R numbers are set after the baseline run"). Delivered by
**T14-07** ([group 14](../implementation-plan/groups/14-memory.md)).
Convention (`docs/adr/NNNN-title.md`, Nygard sections, English) is ADR-0001's.

## Context

Spec 10 §1 `[FIXED]` fixes the execution model the same way it fixes embeddings:
*"the router runs on the **local generator**"* under `data_policy=local_only`, and
spec 08 §4 explicitly ties this to a quality gate: *"which is precisely why the
quality gate (§7) exists."* Spec 08 §7 `[FIXED, new in rev 6]` states the gate's
shape — a labeled fixture set of observation streams → expected ops, precision/
recall on it an acceptance gate — but leaves the actual numbers `[OPEN]` until a
baseline run exists.

Unlike embeddings, there is no existing local generation runtime anywhere in this
workspace to extend — this ADR starts from the same blank slate ADR-0005 did for
the embedding runtime, but for **generation** rather than encoding: a chat-style
request/response contract (`local_rag_embed::{GenRequest, GenResponse,
Generator}`, spec 10 §1), not a fixed-size vector.

### Runtime candidates

Three Rust-native local-inference crates were considered. Two were independently
verified rather than taken on their own documentation's word — the same
discipline ADR-0005 applied to `ort`'s `load-dynamic` feature.

| Option | Offline build | Real dependency cost (verified) | Notes |
| --- | --- | --- | --- |
| `mistral.rs` | **no**, verified via crates.io's dependency API against `mistralrs-core` 0.8.1 | non-optional `reqwest`, `hf-hub`, `image`, `scraper`, `html2text`, `tokio-tungstenite`, `symphonia` | exactly the dependency-bloat/silent-network-reach class `CONTRIBUTING.md`'s dependency policy and `D-008` already rejected once, for a different SDK |
| `kalosm` | not independently verified this session | — | flagged as a research gap for whoever next revisits model selection; not the leading candidate given the other two findings, so not pursued for v0 |
| `llama-cpp-2` (Rust bindings to `llama.cpp`) | **yes**, verified: its published `Cargo.toml` `include` list vendors the actual llama.cpp/GGML C/C++ source tree inside the crate package (confirmed against the real file on GitHub) — `cargo fetch` alone gets everything, no network call during `cargo build` | needs `cmake` + `libclang` (`bindgen`) at build time — a real, heavier host-toolchain surface than ONNX's `load-dynamic` was specifically chosen to avoid | selected |

`mistral.rs`'s rejection is a verified fact, not a vibe: `mistralrs-core` 0.8.1's
dependency tree was fetched directly from crates.io's dependency API and shows
those seven crates as non-optional. `llama-cpp-2`'s offline-build claim is
likewise a verified fact: `llama-cpp-sys-2`'s `Cargo.toml` `include` list was
fetched from its real published source and vendors llama.cpp/GGML directly in the
crate package, the same way ADR-0005 fought to keep ONNX Runtime linkage out of
the build step (there it was `load-dynamic` resolving at runtime instead; here it
is the C/C++ source shipping *inside* the Rust crate instead of being fetched
separately).

### Model candidates

The first round compared two same-family, same-quantization Qwen2.5 sizes —
a clean A/B differing only in parameter count. Digests were verified live
against HuggingFace's tree API (Git LFS `oid` is the file's real SHA-256,
confirmed by byte length and format), not transcribed from a model card,
mirroring ADR-0004/0005's own precedent.

| Model | Size (q4_k_m/q4_0) | Context | License |
| --- | --- | --- | --- |
| `Qwen2.5-0.5B-Instruct` | 468.6 MiB | 32768 | Apache-2.0 |
| `Qwen2.5-1.5B-Instruct` | 1065.6 MiB | 32768 | Apache-2.0 |

Which one to ship as the default was explicitly left to measurement rather than
"bigger is better" — spec 08 §7's own fixture corpus (T14-07 Phase 4, 42
`memory.router.op.*` cases in `fixtures/memory/index.json`, RU/EN, code-switched,
decision vs hypothesis vs negation) exists precisely to make that decision a real
one instead of a guess. The first round's real measured result: both Qwen2.5
sizes landed within ~2 points of F1 of each other (~0.35), so the smaller
0.5B was provisionally selected as the default — see *Consequences* for the
exact numbers.

#### A second round: Gemma 4 E2B (T14-07 Phase 7, user-requested)

After the first round shipped, the user asked directly whether a Gemma model
could be used, which reopened model selection with a third, cross-family
candidate: `Gemma 4 E2B` (Google DeepMind), Google's own official
quantization-aware-trained `q4_0` GGUF release.

| Model | Size (q4_0) | Context (native) | License |
| --- | --- | --- | --- |
| `Gemma 4 E2B` (instruction-tuned) | 3194.3 MiB | 128K | Apache-2.0 |

Two things were verified live before measuring anything, per this ADR's own
"verify, don't assume" discipline:

* **Licensing changed from prior Gemma generations.** `embeddinggemma-300m`
  (ADR-0004) and Gemma 1-3 ship under Google's custom "Gemma Terms of Use"
  (non-OSI, a Prohibited Use Policy attached). Gemma 4's own license page
  (`https://ai.google.dev/gemma/docs/gemma_4_license`) was fetched directly
  and contains the genuine, verbatim Apache License 2.0 text ("TERMS AND
  CONDITIONS FOR USE, REPRODUCTION, AND DISTRIBUTION..."), a real policy
  change — though Google's docs site still links a separate, generic "Gemma
  Prohibited Use Policy" page from the same navigation, whose exact binding
  relationship to the Apache-2.0 grant was not fully resolved; disclosed
  rather than assumed away, the same judgment call ADR-0004's license column
  already models for this project.
* **HuggingFace gates `google/gemma-3-*` (manual approval required)** — its
  tree API returns a masked LFS digest (`"oid":
  "****************************************************************"`) for
  an unauthenticated request, confirmed against three separate Gemma 3
  repositories. A Gemma 3 catalog entry could not be added without either an
  approved HuggingFace account or accepting an unverified digest, which this
  catalog does not do (never done anywhere else in this project either).
  `google/gemma-4-*` repositories are **not** gated — confirmed against
  three separate repos (E2B, E4B, 12B) — which is what made Gemma 4, not
  Gemma 3, the one actually measurable here. This is recorded as a real
  constraint for whoever next revisits model selection, not a preference.

A third finding surfaced only once real inference was attempted: the
selected runtime's own template-application engine did not recognize Gemma
4's embedded chat template at all (see *Consequences* for the full trace and
the `chat_template_override` fix this ADR ships).

## Decision

**Ship `llama-cpp-2`/`llama-cpp-sys-2` as the local generation runtime, with
`Gemma 4 E2B` (`q4_0`, HuggingFace `google/gemma-4-E2B-it-qat-q4_0-gguf`) as
the default model via an explicit `chat_template_override`, greedy-only
decoding, and no grammar-constrained output in v0.**

Concretely:

1. **Runtime: `llama-cpp-2 = { default-features = false, features = ["common"]
   }`.** New isolated crate `crates/generate` (mirrors `crates/models`' own
   isolation from `crates/embed`): `llama-cpp-sys-2` needs a materially
   different build toolchain (`cmake`+`libclang`/bindgen) than ONNX's
   `load-dynamic` (no C++ toolchain at all), so mixing the two into one crate
   would force every ONNX-only contributor to also carry a C++ toolchain they
   do not need. `local_rag_memory` (the router) depends on
   `local_rag_embed::Generator`/`GeneratorPool` only, never on
   `local_rag_generate` directly — the same trait-level seam `crates/search`
   uses for `Embedder`, verified structurally by `crates/embed/tests/
   offline_smoke.rs`'s existing manifest lint (already forbade
   `candle`/`llama`/`tokenizers`/`ollama`/`hf-hub` before this task touched
   anything).
2. **Default model: `Gemma 4 E2B`, `q4_0`.** Measured, not assumed
   bigger-is-better or smaller-is-fine (see *Consequences*): Gemma 4 E2B
   scored roughly double the F1 of either same-generation-cost Qwen2.5
   candidate on the identical 42-case fixture corpus, which is why the
   default moved despite Gemma 4 E2B being markedly larger (~3.2 GiB vs.
   ~470 MiB) — a real, disclosed trade-off, not a free upgrade. All three
   entries stay in `crates/generate::catalog::CATALOG` with real, verified
   digests (no placeholder ever committed) — a future re-measurement (a
   `llama-cpp-sys-2` upgrade with native Gemma 4 template detection, better
   prompting, more fixture cases, a weight retune) can revisit this without
   re-verifying any candidate's digest from scratch.
3. **Chat templating goes through the model's own embedded template, not a
   hand-rolled formatter.** `LlamaGenerator::build_prompt` calls
   `LlamaModel::chat_template` + `LlamaModel::apply_chat_template` — the
   same `llama_chat_apply_template` engine `llama.cpp`'s own CLI uses,
   reading the Jinja template GGUF conversion bakes into the file, rather
   than this crate assuming a specific format. An earlier version of this
   code hand-rolled Qwen2.5's ChatML template directly; that happened to
   work for the Qwen entries but would have silently produced garbled
   prompts for any model expecting a different template — caught only
   because a second model family was actually measured. See *Consequences*
   for the one real gap this still left (Gemma 4's own template goes
   unrecognized by this pinned runtime, needing the explicit override
   below).
4. **`GeneratorCatalogEntry::chat_template_override: Option<&'static str>`,
   set only where verified necessary.** `None` for both Qwen entries (their
   embedded ChatML template is correctly auto-detected). `Some("gemma")` for
   the Gemma 4 default — see *Consequences* for why detection fails for
   Gemma 4 specifically and what the named-template fallback actually costs.
5. **Sampling: greedy only.** `Sampling::Temperature` returns a typed
   `LlamaError::UnsupportedSampling` rather than silently ignoring the
   request. Spec 08 §7's benchmark needs reproducible runs; nothing in this
   task's design calls for sampled decoding.
6. **No grammar-constrained decoding in v0.** `llama-cpp-2`'s
   `LlamaSampler::grammar` takes raw GBNF, not JSON Schema — the crate does
   not expose llama.cpp's own JSON-Schema-to-grammar conversion, and
   hand-authoring/maintaining a full converter was judged unjustified scope
   for this task. `GenRequest::json_schema` is advisory per spec 10 §1's own
   wording ("a runtime that doesn't [support it] ignores it") — `crates/
   generate` ignores it; output reliability comes from the prompt
   (`local_rag_memory::prompt`) and the router's own two-tier malformed-output
   handling (`local_rag_memory::parse`) instead.
7. **Installer duplicated from `crates/models`, not extracted.** The
   atomic-install shape (`.part` → hash → verify → rename → fsync →
   manifest → `.ok` last) is identical in structure to ADR-0005's, but
   re-implemented in `crates/generate::install` rather than factored into a
   shared `local_rag_core::assets` module — the lower-risk choice given the
   already-shipped, gate-passed status of `crates/models`' code and this
   task's already-large scope; a future task may still extract the common
   core if a third installer ever needs it.

## Consequences

* **O3 is fully resolved.** All three ADR-0004/0005/0006 halves (model,
  delivery, local generator crate) are closed; spec 15 §4's O3 row and 10 §1's
  local-generator note become `[SPEC]` as-built citations. No `[FIXED]` text
  changes.
* **O2's memory-router half gets its first real numbers, not an invented
  target — twice, once per round.** Round one (both Qwen2.5 candidates,
  greedy, 42 cases, this ADR's host): 0.5B scored precision 0.3784, recall
  0.3182, F1 0.3457, exact-match rate 0.3095
  (`fixtures/memory/baseline/run.json`); 1.5B scored precision 0.3659, recall
  0.3409, F1 0.3529 (`fixtures/memory/baseline/run-1.5b.json`). Round two
  (Gemma 4 E2B, identical corpus and harness): precision 0.6667, recall
  0.6364, F1 0.6512, exact-match rate 0.6190
  (`fixtures/memory/baseline/run-gemma-4-e2b.json`) — roughly double either
  Qwen2.5 candidate's F1. `fixtures/memory/baseline/thresholds.json` was
  re-derived from the round-two (Gemma 4) run, since it is now the shipped
  default: `min_precision = 0.60`, `min_recall = 0.55`, a real margin below
  the measured 0.6667/0.6364, never invented ahead of them. The round-one
  Qwen numbers stay recorded (both run files kept, not deleted) as real
  evidence for whoever revisits model selection next, exactly like this
  ADR's own two rounds did.
* **Absolute quality moved from "honestly modest" to "meaningfully usable,
  still imperfect."** Round one's 0.5–1.5B Qwen2.5 candidates misclassified
  roughly two-thirds of the fixture corpus; Gemma 4 E2B still misses roughly
  a third. Both are disclosed as measured, not smoothed into a rounder
  number — spec 08 §7's own purpose is "the gate exists to prevent [criteria
  for plumbing only]," and a real number, however imperfect, does that;
  an invented target would not. Raising the floor further is future work
  (better prompting, T14-09's own generalized templating possibly unlocking
  Gemma 4's *native* system-role template — see below, more/better fixture
  coverage, or `router_version`-tracked confidence-weight tuning per spec 08
  §2), not something this ADR papers over.
* **The 1.5B Qwen2.5 candidate showed a distinct failure mode, not uniform
  improvement over the 0.5B.** It correctly identified `retract` cases but
  then also predicted `retract` for several `reinforce`/`supersede`/
  `resolve` cases whose windows happened to include existing-entry context —
  a real, measured pattern, superseded in relevance once Gemma 4 became the
  default but kept here as evidence.
* **The Gemma 4 chat template needed a real, traced-to-source fix, not a
  guess.** Real inference against the installed Gemma 4 E2B weights first
  failed outright with `ApplyChatTemplateError::FfiError(-1)`. Tracing this
  into the vendored `llama-chat.cpp` source (`llama-cpp-sys-2` 0.1.152)
  shows why: `llm_chat_detect_template` pattern-matches a **fixed set** of
  known template signatures against the model's raw embedded Jinja string —
  it is not a Jinja interpreter — and Gemma 4's own template text matches
  none of them (including the existing `tmpl_contains("<start_of_turn>")`
  check, apparently written against Gemma 1-3's simpler template), so
  detection falls through to `LLM_CHAT_TEMPLATE_UNKNOWN` and the function's
  final `else { return -1; }` fires. Passing the short **name** `"gemma"`
  instead of the raw template string (`GeneratorCatalogEntry::
  chat_template_override`) skips detection entirely and selects
  `LLM_CHAT_TEMPLATE_GEMMA` directly (the `google/gemma-7b-it`-era format) —
  confirmed by a real, successful `cargo xtask memory-bench --model
  gemma-4-e2b-it-gguf-q4-0` run reaching the 0.6667/0.6364 numbers above.
  **The real, disclosed cost:** `LLM_CHAT_TEMPLATE_GEMMA`'s own
  implementation carries a comment — *"there is no system message for
  gemma, but we will merge it with user prompt"* — merging the system turn
  into the first user turn rather than emitting it as its own turn. Gemma
  4's README advertises **native system-role support** as a new capability
  over prior generations; this override cannot exercise that (this pinned
  llama.cpp snapshot has no branch that knows Gemma 4's own newer template
  shape). The 0.6667/0.6364 numbers above are therefore Gemma 4 E2B on a
  *system-message-merged* prompt — its true native-template ceiling is
  unmeasured and could plausibly be higher, not lower. **T14-09** (new task,
  registered in `docs/implementation-plan/groups/14-memory.md`, requested
  directly by the user rather than left implicit) tracks generalizing this
  from one hand-picked per-model override into a real mechanism that
  supports arbitrary models without hardcoding — a newer `llama-cpp-sys-2`
  with native detection, a real Jinja interpreter over the model's own
  template string, or a small typed set of hand-rolled formatters with a
  loud typed failure when none match; not resolved by this ADR.
* **`n_batch` was a real, load-bearing bug this ADR's own measurement run
  found and fixed.** `LlamaContextParams::default()`'s `n_batch` (llama.cpp's
  own default) was smaller than a legal single-call prompt submission (the
  full window prompt, or — after one corrective re-prompt — that prompt plus
  the model's own first response), producing a hard
  `GGML_ASSERT(n_tokens_all <= cparams.n_batch)` abort partway through the
  first real 42-case run. Fixed by setting `n_batch` explicitly to
  `context_length` in `crates/generate::llama::LlamaGenerator::generate_greedy`
  (already bounded by the existing `ContextOverflow` check), rather than
  worked around by shrinking fixture prompts to fit — exactly the kind of
  defect a real end-to-end run, not a mocked one, is supposed to surface.
* **The build stays offline.** `llama-cpp-sys-2` vendors llama.cpp/GGML
  source inside the published crate; `cargo fetch` alone suffices, and
  `cargo xtask ci` never needs a live model (the memory-quality run is a
  separate, explicit `cargo xtask memory-bench` command, mirroring
  `cargo xtask bench`'s own precedent).
* **The host must provide `cmake` + `libclang`.** This is the one real,
  disclosed cost of choosing `llama-cpp-2` over a `load-dynamic`-style
  runtime-resolution approach: `crates/generate`'s own toolchain requirement
  is heavier than `crates/models`'. A missing toolchain fails the build
  loudly (a `cmake`/`bindgen` compiler error), not silently.
* **Weights remain uncommitted** (`CLAUDE.md`), cached under
  `$LOCAL_RAG_BENCH_MODEL_HOME` (default `~/.local/share/local-rag-bench`),
  reusing ADR-0005's own cache-root convention — the GGUF weights and the
  ONNX weights live in sibling `models/<model_id>/` subdirectories under one
  root, since `StoreLayout::model_dir` already namespaces by `model_id`.
