# ADR-0005: Model runtime and weights delivery

## Status

Accepted — 2026-07-26.

Closes the **weights-delivery half** of open question **O3 "Default embedding
model + weights delivery; local generator crate"**
([spec 15 §4](../specification/15-roadmap.md)), continuing from
[ADR-0004](0004-default-embedding-model.md), which closed the model half and
explicitly deferred "the runtime" and "quantization for distribution" to this
task. Delivered by **T11-06**
([group 11](../implementation-plan/groups/11-embeddings-and-model-spaces.md)),
which is also the owner of `D-008`
([DEVIATIONS](../implementation-plan/DEVIATIONS.md)). The **local generator
crate** half of O3 stays open under T14-07. Convention
(`docs/adr/NNNN-title.md`, Nygard sections, English) is ADR-0001's.

## Context

Two `[FIXED]` statements bound this decision from opposite ends.

Spec 10 §1 `[FIXED]` fixes the execution model: *"Embeddings run **in-process**:
`fastembed` (ONNX Runtime) or `Candle`… the local backend is the working
default."* Spec 10 §5 `[FIXED policy]` fixes delivery: *"Weights are **not** in
npm. `local-rag init --download-models`: checksum-verified manifest, atomic
download (`.part` → fsync → rename → `.ok` marker), offline operation afterwards.
`models/<model_id>/manifest.json` records source, size, sha256, license."*

Between them sits everything this ADR has to settle: which runtime binding, how
the runtime library is obtained without breaking the offline build rule, which
quantization is fetched, what the manifest actually contains, and what an
interrupted download leaves behind.

The offline rule is the sharp constraint. `CONTRIBUTING.md` requires the full
quality gate to run with no network after `cargo fetch`, and spec 14 §1 requires
tests to be hermetic. `D-008` was raised precisely because the obvious way to
link ONNX Runtime — `ort`'s default `download-binaries` feature, which
`fastembed` turns on transitively — downloads a platform binary **during
`cargo build`**. That would make a clean build network-dependent, so T11-03
shipped the provider pool without a runtime and left this half to T11-06 rather
than accept the regression.

The consumer half of the contract already exists and constrains the producer:
`local_rag_embed::require_model_assets` (T11-03) treats a model directory as
usable **only** when `.ok` is present, and T11-04 classifies the resulting
`EmbedError::ModelAssetsMissing` as fatal for a backfill run. So "installed" has
exactly one on-disk definition, and this ADR must not invent a second one.

### Runtime candidates

| Option | Build stays offline | Model reuse | Cost |
| --- | --- | --- | --- |
| `fastembed` | **no** — pulls `ort` with `download-binaries` | its own model catalog and cache, parallel to `models/` | would fight spec 10 §5's layout and D-008 |
| `ort` + `load-dynamic` | **yes** — `ort-sys` compiles with linking disabled; `libonnxruntime` is resolved at runtime via `libloading` | ADR-0004's ONNX weights load as-is | the host must supply the shared library (T17-03) |
| `Candle` | yes — pure Rust | needs `safetensors`, so every ADR-0004 measurement would have to be retaken | re-opens a closed decision |

`ort` with `load-dynamic` was verified rather than assumed: the crate compiles
and the whole workspace gate runs with no network, and a real inference run
against ONNX Runtime 1.27 produced 768-dimensional unit vectors from the weights
ADR-0004 selected (numbers under *Consequences*).

### Delivery shape

The asset set for `embeddinggemma-300m` at q8 is three files: the graph
(`model_quantized.onnx`, 554.6 KiB), its external tensor data
(`model_quantized.onnx_data`, 294.6 MiB — ONNX splits graphs whose tensors exceed
the protobuf limit), and `tokenizer.json` (19.4 MiB), which is mandatory because
encoding must match the training-time tokenizer byte for byte. That is 314.5 MiB
in total; ADR-0004's "≈295 MiB" quoted the weights alone.

"Checksum-verified" only means something if the expected digest is known
*before* the transfer — otherwise the installer would be certifying whatever it
received. So digests have to be pinned in the binary, and the source URL has to
pin an immutable revision, or a changed upstream branch would turn every install
into a checksum failure.

## Decision

**Load ONNX Runtime through `ort` with the `load-dynamic` feature; fetch the q8
export over HTTPS with `ureq`+rustls behind a trait seam; install it atomically
under `models/<model_id>/` with pinned digests and an `.ok` marker written
last.**

Concretely:

1. **Runtime: `ort = { default-features = false, features = ["load-dynamic",
   "ndarray", "api-24"] }`.** No build-time download, no build-time link. The
   shared library is resolved at runtime from `ORT_DYLIB_PATH` or the loader
   path, and its absence is the typed `OnnxError::Runtime`, never a panic.
   *Which* library ships in each npm platform package is **T17-03**'s "ORT
   bundling verified before the final CI matrix" (spec 10 §5, 13 §1) — this ADR
   decides the binding, not the packaging.
2. **Quantization: q8 (`model_quantized`).** ADR-0004's measured operating point:
   295 MiB of weights against fp32's 1.15 GiB at the same 768 dimensions and
   comparable CPU latency, where q4 is smaller but ~1.6× slower. Quantization is
   not part of `RepresentationKey`, so this stays a delivery decision; the
   manifest records exactly which file was fetched.
3. **Transport: `ureq` 3 with `rustls`, behind `AssetFetcher`.** Pure-Rust TLS,
   no OpenSSL and no system TLS stack to bundle per platform; blocking, matching
   the installer's shape. The trait seam is the same idiom `Clock`, `Env`,
   `UuidSource` and `Sleeper` already use here, so the installer's *policy* is
   tested exhaustively against a loopback fixture server and a local-directory
   fetcher, with no external network anywhere in the suite.
4. **Layout and ordering.** Per file: `<name>.part` → stream while hashing →
   `sync_all` → verify size **and** sha256 against the pinned catalog → `rename`
   → fsync the directory. After every file: `manifest.json` by the same atomic
   path, then `.ok` last. Everything before the marker is, by construction,
   indistinguishable from "not installed".
5. **Resumable without a journal.** No progress file. Each run re-derives what is
   missing by hashing what is on disk against the pinned digests; a leftover
   `.part` is overwritten, never trusted, because nothing recorded how far it
   got. This is the same "recompute, don't journal" model the retention sweep and
   T11-04's backfill use.
6. **Manifest contents.** `model_id`, `source`, `revision`, `license`,
   `license_url`, `dimensions`, and per file `path`/`size`/`sha256`. Spec 10 §5
   requires source/size/sha256/license; the rest makes the record
   self-describing. The manifest is **disclosure, not authority**: verification
   uses the compiled-in catalog, so a tampered manifest cannot talk the installer
   into accepting different bytes.
7. **Downloading is not gated on `data_policy`.** Spec 12 §1's `local_only` and
   spec 10 §1's "every remote call is gated before the provider is selected"
   govern *sending repository content out*. Fetching weights is the opposite
   direction: an explicit user command pulls public bytes in, and nothing about
   the user's code leaves the machine. Gating it on the default policy would make
   a `local_only` installation unable to obtain the local model at all —
   inverting the policy's intent. Recorded as a `[SPEC]` amendment in 12 §1 and
   10 §5 rather than left implicit.
8. **The license is surfaced before the first byte moves**, to a caller-supplied
   sink and without prompting — ADR-0004's obligation, and `init` must stay
   scriptable. The terms are then persisted in the manifest.

### Why a separate `crates/models`

`crates/embed` carries a structural guarantee, asserted by a test since T11-03
and tightened by D-010: its manifest declares no network client and no model
runtime. The pool, the policy guard and the retry contract have no business
linking TLS or ONNX, and weakening that lint to accommodate this work would
trade a mechanical invariant for convenience. The heavy dependencies live in a
new crate instead — the same isolation `spike/` gives the dense-backend
candidates.

## Consequences

* **O3's delivery half is resolved**; spec 15 §4's row is amended to show the
  model and delivery halves closed (ADR-0004, ADR-0005) with the local generator
  crate still open under T14-07. Spec 10 §5's remaining `[OPEN]` delivery detail
  becomes a `[SPEC]` as-built note citing this ADR. No `[FIXED]` text changes.
* **D-008 is resolved.** The in-process provider spec 10 §1 requires now exists
  (`local_rag_models::OnnxEmbedder`), reaches its weights only through
  `require_model_assets`, and returns `ModelAssetsMissing` — the typed error
  T11-04 already treats as fatal — when they are absent.
* **The build stays offline.** `cargo xtask ci` runs with no network after
  `cargo fetch`; the workspace links no ONNX binary at build time.
* **The host must provide ONNX Runtime.** Until T17-03 bundles it per platform,
  `OnnxEmbedder::open` fails with a typed, actionable error naming
  `ORT_DYLIB_PATH`. The CI suite does not require it: the inference test runs
  when a runtime and weights are supplied and prints an explicit `SKIP` line
  otherwise, so a missing prerequisite is visible rather than silent.
* **Measured, not assumed.** On the ADR-0004 host (macOS arm64, ONNX Runtime
  1.27 CPU EP, `ort` 2.0.0-rc.12, release build), across two runs: session load
  363.6 / 400.4 ms; a **cold** batch of 3 code snippets took 253.6 / 342.3 ms
  (84.5 / 114.1 ms per text). That is well above ADR-0004's 43.2 ms per text,
  which was a *warmed* batch of 32 — the two numbers are not comparable, and no
  claim is made here about steady-state throughput. Output vectors are
  768-dimensional, unit-length and deterministic across calls; `cos` between two
  equivalent config parsers was 0.860 against 0.483 for an unrelated SQL
  statement, in both runs.
* **The pinned digests are verified against real bytes**, not transcribed: an
  opt-in test installs the shipped catalog entry from a local mirror and fails if
  any `sha256`/`size` disagrees. Weights remain uncommitted (`CLAUDE.md`).
* **An interrupted install is safe rather than merely recoverable.** The named
  crash point `models.install.between_files` (spec 14 §3) proves the three
  properties that matter: the model stays unusable, a rerun refetches only what
  is missing, and a third run is a no-op.
* **`local-rag init --download-models` remains unimplemented as a *command***;
  T11-06 delivers the typed API, and T15-07 (`serve/status/stop/restart/init`)
  owns the CLI surface. Excluding weights from the npm packages is T17-01's
  packaging test.
