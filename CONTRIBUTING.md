# Contributing to local-rag

## Full check (single command)

Run the complete quality gate — formatting, lint (warnings denied), tests, and
docs — with one command:

```
cargo xtask ci
```

This is exactly what CI runs. `xtask` is a thin Rust runner (crate
`crates/xtask`) that still reports exactly one pass/fail result, but
internally splits the gate into **16 jobs** and runs the independent ones
concurrently over a bounded worker pool (size: `available_parallelism()`,
override with `XTASK_CI_JOBS=<n>`) instead of one at a time. Every job runs to
completion regardless of its siblings, so one run reports every failure
instead of stopping at the first — the jobs are:

- All `fmt --check`/`clippy` steps (root workspace, each crate's `--features
  failpoints` lint, both spike workspace members) are fully independent jobs
  and run concurrently with everything else.
- Every `cargo nextest run` step against the **root** workspace — the
  default-feature `--workspace` run plus each `--features failpoints` crate's
  run (`local-rag-store`, `-hook`, `-index`, `-projection`, `-search`,
  `-embed`, `-models`, `-generate`; see "Workspace layout" below for what
  each seam covers) — is chained into **one** job, `root:test`, run in that
  order, and every step still runs even if an earlier one fails (so the run
  still reports every crate's result). Each nextest step is paired with a
  `cargo test --doc` step for the same scope, since nextest never runs
  doctests.
- Likewise, the two spike-workspace `nextest run` steps (`harness`, then
  `qdrant-edge`) are chained into one job, `spike:test`.
- `cargo doc --workspace --no-deps` is its own independent job.

`nextest run` steps against the **same** workspace are chained into one job
rather than run as separate concurrent jobs for a measured reason: two
`cargo nextest run` processes started at once against the same workspace
visibly block on cargo's own build-directory lock (confirmed directly — one
sits at 0% CPU until the other's build finishes), so running one per crate
concurrently would silently re-serialize exactly the cost this pipeline most
needs to shrink (the ~100+ sequential test-binary spawns `cargo test
--workspace` used to do on its own, per `docs/implementation-plan/PROGRESS.md`
T14-05's evidence). `clippy`/`fmt`/`doc` do not show this lock behavior, so
those stay one job each and run fully concurrently. `root:test` and
`spike:test` — the two jobs no amount of concurrency can shrink further — are
queued first so a free worker never idles behind a cheap `clippy` job while
they are still running.

`cargo-nextest` is a required host tool (not a workspace dependency `cargo
fetch` pulls in): install it once with `cargo install cargo-nextest --locked`.
`cargo xtask ci` checks for it up front and fails immediately with that
install command if it is missing, rather than failing confusingly partway
through. After that one-time install and the initial `cargo fetch`, the full
check runs **offline**.

## Search benchmark (`cargo xtask bench`)

The 49-query search benchmark (spec 14 §7, T12-05) is a **separate** command and
deliberately *not* part of `cargo xtask ci`:

```
ORT_DYLIB_PATH=<libonnxruntime.dylib> cargo xtask bench --corpus <checkout>
```

It indexes a real corpus checkout, embeds it with the default model, projects it,
runs all 49 queries, and writes a report next to the recorded baselines in
`fixtures/search/baseline/`. Three flags matter for comparability, and each one
changes *what is measured*, so a run is only comparable to another with the same
values: `--subdir <rel>` indexes one subtree instead of the whole checkout (the v1
baseline indexed `src/` alone), `--mode hybrid|lexical|code` picks the legs, and
`--dense-kind code_raw|code_context` picks the representation the dense leg
searches (default `code_raw`, spec 09 §3). All three land in the report's
`provenance`. Three reasons it stays out of the gate: it needs
model weights (~315 MiB, fetched by the pinned catalog on first run and cached in
`$LOCAL_RAG_BENCH_MODEL_HOME`, default `~/.local/share/local-rag-bench`), a
`libonnxruntime` this repository does not ship (bundling is T17-03), and a corpus
checkout that is not part of this repository. Everything *scored* — corpus
integrity, matching semantics, metric math, the gate — is ordinary
`cargo test -p xtask` and therefore does run in `ci`.

`crates/xtask` consequently depends on the product crates. They are all already
workspace members, so this adds **no** package to `Cargo.lock`; the cost is build
time for a dev-only crate that `default-members` already excludes.

## Memory-quality benchmark (`cargo xtask memory-bench`)

The memory-router benchmark (spec 08 §7, T14-07) is a **separate** command and
deliberately *not* part of `cargo xtask ci`:

```
cargo xtask memory-bench [--corpus <path>] [--model <catalog-id>] [--out <path>]
```

It runs `local_rag_memory::router::route` against every `memory.router.op.*` case
in `fixtures/memory/index.json` (default `--corpus`), one fresh throwaway
`state.sqlite` per case, and writes a report next to the recorded baselines in
`fixtures/memory/baseline/`. `--model` selects a different
`crates/generate::catalog::CATALOG` entry (default
`local_rag_generate::DEFAULT_MODEL_ID`) — how ADR-0006's two comparison rounds
(0.5B vs. 1.5B Qwen2.5, then Gemma 4 E2B) were actually measured, not guessed at;
`--out` writes the report (and its `.report.md`) somewhere other than
`memory/baseline/run.json`, so a comparison run does not overwrite the shipped
baseline. Two reasons it stays out of the gate: it needs the installed GGUF
weights (~3.2 GiB for the default model, `Gemma 4 E2B` — fetched by the pinned
catalog on first run and cached in `$LOCAL_RAG_BENCH_MODEL_HOME` — the same
cache root the search benchmark's ONNX weights use, namespaced by `model_id`) and
the `cmake`/`libclang` toolchain `llama-cpp-sys-2` needs to compile llama.cpp from
its vendored source. Everything *scored* — corpus loading, op-kind matching,
metric math, the gate — is ordinary `cargo test -p xtask` and therefore does run
in `ci`.

Unlike the search benchmark, there is no v1 baseline to diff against (GAP-04): the
report carries no `baseline`/`diff` fields, and the gate
(`fixtures/memory/baseline/thresholds.json`) is a floor on the one real run that
exists, not a regression budget.

## Toolchain / MSRV

- Pinned toolchain: **1.96.1** via `rust-toolchain.toml` (components `rustfmt`,
  `clippy`), installed automatically by `rustup`.
- Edition **2024**; MSRV **1.96** (`rust-version` in `[workspace.package]`).
- `cargo-nextest`, required by `cargo xtask ci`'s test jobs (see "Full check"
  above): `cargo install cargo-nextest --locked`. Not a `rustup` component and
  not resolved by `cargo fetch` — install it once per machine.

## Dependency policy

- `Cargo.lock` is committed for reproducible builds.
- No dense-vector or embedding-model SDK enters the workspace before the T10
  comparative spike / T11 embeddings work (see `docs/implementation-plan`).
  Nothing may couple to a concrete dense backend before then. As of T11-03 the
  embedding-model half of that rule is still in force by construction:
  ADR-0004 selects the default model, but the ONNX runtime that loads it —
  together with the weights themselves — arrives with the model-asset installer
  (T11-06, spec 10 §5), so `crates/embed` links **no** model or network SDK
  today and a test asserts that structurally
  (`crates/embed/tests/offline_smoke.rs`).
  T11-06 links that runtime, in a **separate** crate: `crates/models` owns `ort`,
  `tokenizers` and `ureq`, while `crates/embed`'s structural lint stays intact
  rather than being widened to accommodate it — the same isolation principle
  `spike/` applies to the dense-backend candidates. The offline rule above is
  unaffected: `ort`'s `load-dynamic` feature resolves `libonnxruntime` at
  *runtime*, so no build step reaches the network (ADR-0005, closing `D-008`).
- New dependencies require justification (why the standard library is
  insufficient) and a license check; prefer `std` and small, well-vetted crates.
- Lints are centralized in `[workspace.lints]`; each crate opts in via
  `[lints] workspace = true`.

### Approved external dependencies

The workspace was dependency-free through G00. As of T01-01 the following
external crates are allowed; each entry records why `std` is insufficient and
the license check. Additions require a new row here.

| Crate | Scope | Why std is insufficient | License |
| --- | --- | --- | --- |
| `libc` | `crates/core`, `cfg(unix)` only | `std` exposes no `geteuid`/`getuid`; POSIX owner verification (spec 02 §2.1) needs the current effective uid. `libc` is rust-lang-maintained with zero transitive dependencies. | MIT OR Apache-2.0 |
| `rusqlite` (feature `bundled`) | `crates/store` | The store *is* SQLite (spec 03); `std` has no SQLite. `bundled` compiles SQLite from source for reproducible, offline builds and a self-contained binary with no system `libsqlite3` (spec 13 §1/§2). Native-linking transitive set: `libsqlite3-sys`, `cc`/`shlex`/`pkg-config`/`vcpkg`/`find-msvc-tools` (build), `bitflags`, `hashlink`, `hashbrown`/`foldhash`, `fallible-iterator`, `fallible-streaming-iterator`, `smallvec`. rusqlite 0.40 additionally resolves a **wasm32-target-only** subtree (`sqlite-wasm-rs`, `rsqlite-vfs`, `wasm-bindgen`(+macro/-support/-shared), `js-sys`, `bumpalo`, `once_cell`, `thiserror`(+impl), `rustversion`, `proc-macro2`/`quote`/`syn`/`unicode-ident`) that is gated to `cfg(target_arch = "wasm32")` and never links into the native builds this project ships. None are dense-backend/model/network SDKs. **No Cargo-feature change for T08-01's `fts_occurrences` FTS5 virtual table (spec 03 §4.3)**: `libsqlite3-sys`'s `build_bundled::main` unconditionally compiles with `-DSQLITE_ENABLE_FTS5` whenever `bundled` is enabled (verified directly in `libsqlite3-sys 0.38.1`'s `build.rs`; there is no separate `fts5` feature on either `rusqlite` or `libsqlite3-sys`) — FTS5 was already available, just unused before T08-01. | MIT |
| `tokio` | `crates/store` (lib features `sync`/`rt`) and `crates/index` (lib features `rt`/`macros`/`sync`/`time`); dev-only in `crates/projection` (`rt-multi-thread`/`macros`) | The store's bounded write queue is an async `mpsc` + `oneshot` with backpressure/cancellation (spec 02 §5 L4); `std` has no async channels with cancellable backpressure, so `crates/store` originally linked only `sync` (no runtime/net/mio). As of T05-04 the reconcile scheduler (`crates/index`) also uses tokio's runtime primitives — the per-worktree `select!` loop (`macros`), task spawning (`rt`), and the debounce/periodic timers (`time`) — to drive `scan → build_generation`; the daemon (T15) still selects the runtime flavor. As of T07-03, `crates/projection`'s `switch()` awaits `StateWriter::transaction` (an async fn defined in `crates/store`), so its tests need an executor — `#[tokio::test]` over `rt-multi-thread`/`macros`, dev-only (the library itself pulls no tokio dependency; `async`/`.await` are language features). As of T09-01, `crates/store` additionally enables `rt`: the lock-order tracker (`lock::order`, spec 02 §5) needs `tokio::task_local!`/`LocalKey`, which lives behind tokio's own `rt` feature (verified in tokio 1.52's `src/task/mod.rs`: `mod task_local;` sits inside `cfg_rt! { … }`). `rt = []` in tokio's own manifest adds **no transitive crate** — it only compiles in more of tokio's own already-vendored code — so this still pulls no `mio`/net/runtime-executor dependency into the library. The `rt`/`time`/`macros` features add only the build-time proc-macro `tokio-macros` (promoted from the prior test-only use) and reuse `pin-project-lite` (already present); they pull no `mio`/net stack of their own. None are dense-backend/model/network SDKs. | MIT |
| `blake3` (`default-features = false`, feature `pure`) | `crates/core` | The domain-separated content/manifest/subject identity hash (spec 03 §1.2) is BLAKE3; `std` has no BLAKE3, and this is the `[FIXED]` hash schema whose correctness must match the reference algorithm. `pure` forces portable Rust — no `cc`/assembly SIMD — so the **native-linked** set is just `blake3` + `arrayref` (BSD-2-Clause) + `arrayvec` (MIT OR Apache-2.0) + `cfg-if` (MIT OR Apache-2.0, already present) + `constant_time_eq` (CC0-1.0). `Cargo.lock` additionally resolves `cpufeatures` (0.3.0) — and reuses `cc`/`shlex`/`find-msvc-tools` already pulled by rusqlite — for blake3's non-`pure` SIMD path; `pure` gates them out so they never compile into or link the native builds this project ships, exactly like rusqlite's wasm subtree. None are dense-backend/model/network SDKs. | CC0-1.0 OR Apache-2.0 OR Apache-2.0 WITH LLVM-exception |
| `unicode-normalization` | `crates/core`, `crates/store` | Path canonicalization requires Unicode **NFC** (spec 03 §1.3); `std` has no Unicode normalization and the tables are impractical to vendor by hand. T03-04 reuses the same crate in `crates/store` for the versioned `content_blob` text normalization (`normalization_version=1`, spec 03 §4.2) — already in `Cargo.lock` via `crates/core`, so this adds **0 new external sources**. Maintained by the unicode-rs org. Native-linking transitive set: `tinyvec` (Zlib OR Apache-2.0 OR MIT), `tinyvec_macros` (MIT OR Apache-2.0 OR Zlib). | MIT OR Apache-2.0 |
| `casefold` | `crates/core`, `crates/store` | Case-insensitive filesystems need Unicode **simple case folding** for path identity (spec 03 §1.3); `std` offers only full case *mapping* (`to_lowercase`), not simple folding. From GitHub's `github/rust-gems` monorepo (the code-search domain); a ≈1 KB paged-bitmap/run-length table with **zero** transitive dependencies. T08-01 reuses the same primitive in `crates/store` for the FTS tokenizer's token-lowering step (spec 09 §2) — `simple_fold` is a strict 1:1 fold, unlike `to_lowercase()` which can expand length (e.g. Turkish `İ`); already resolved via `crates/core`, 0 new external sources. | MIT |
| `serde` (feature `derive`) | `crates/core`, `crates/store`, `crates/index`, `crates/models`, `crates/protocol`, `crates/local-rag-hook` | Typed deserialization of the versioned `config.toml` sections (spec 02 §3.1); `std` has no deserialization framework, and hand-mapping every field is error-prone for a security-relevant config (`data_policy`). The `derive` feature generates the `Deserialize` impls. Native-linking transitive set: `serde_core`; the `derive` feature additionally pulls the build-time proc-macro `serde_derive`, reusing `proc-macro2`/`quote`/`syn`/`unicode-ident` already resolved by rusqlite's wasm subtree. None are dense-backend/model/network SDKs. T12-03 adds `crates/protocol` as a consumer for the **serialization** direction: the `search_code` response shape is `[SPEC]`-fixed by spec 09 §7, so `Serialize` there implements a decision rather than making one, and it is what makes "repeated output is byte-stable" a byte assertion. T13-02 adds `crates/local-rag-hook`: `Deserialize` for Claude Code's real hook JSON event shapes (spec 07 §4), `Serialize` for the LRSP frame payload (spec 07 §3). No package is added to the lockfile — only dependency edges to crates already in the graph. | MIT OR Apache-2.0 |
| `toml` | `crates/core` | Parse the versioned global config `<config_dir>/config.toml` (spec 02 §3.1); `std` has no TOML parser, and hand-rolling one for a versioned, nested, security-relevant config is disproportionate and error-prone. Native-linking transitive set: `toml_edit`, `toml_datetime`, `toml_write`, `serde_spanned`, `winnow`, `memchr`, `indexmap`, `equivalent` — reusing `hashbrown` already resolved by rusqlite/hashlink. All are toml-rs / parsing utilities; none are dense-backend/model/network SDKs. | MIT OR Apache-2.0 (winnow MIT; memchr Unlicense OR MIT) |
| `ignore` (`default-features = false`) | `crates/index` | The `ignored` skip reason needs correct gitignore semantics (anchoring, `**`, negation, nested-`.gitignore` precedence) which the spec (06 §2) explicitly delegates to this crate; hand-rolling them is error-prone and would drift from Git. T03-02 uses only the `ignore::gitignore` matcher; the parallel `Walk` tree scan is T05-02. Maintained in ripgrep / BurntSushi's monorepo. Native-linking transitive set: `globset`, `aho-corasick`, `regex-automata`, `regex-syntax`, `bstr`, `memchr` (reused from `toml`), `log`, `same-file`, `walkdir`, `crossbeam-deque`/`crossbeam-epoch`/`crossbeam-utils` (+ `winapi-util`/`windows-sys`/`windows-link` on Windows only). All are ripgrep-ecosystem matching/traversal utilities; none are dense-backend/model/network SDKs. | Unlicense OR MIT |
| `zstd` | `crates/store` | The exact-byte `source_blob` invariant (spec 03 §2.3 `source_compression IN ('none','zstd')`, 12 §5 `[FIXED]`) stores a local copy of every indexed source; zstd is the `[FIXED]` compressor. `std` has no zstd. Default features vendor libzstd from C source via `zstd-sys`, so builds stay reproducible and offline exactly like rusqlite's bundled SQLite. T03-03 uses only `zstd::encode_all`/`decode_all` (in-memory round-trip). Native-linking transitive set: `zstd`, `zstd-safe`, `zstd-sys`, reusing `cc`/`shlex`/`pkg-config`/`vcpkg` already pulled by rusqlite's `bundled` build. Adding `zstd-sys`'s `cc` build refreshed `cc`'s **build-time** `jobserver` (parallel-build coordination), which on some non-host targets resolves `getrandom`/`r-efi`; all three are build-dependencies of `cc`, compiled to run the C build and never linked into the shipped binary. Not a dense-backend/model/network SDK (not on the T10 guardrail list). | MIT (crates) / BSD-3-Clause OR GPL-2.0 (vendored libzstd) |
| `tree-sitter` | `crates/index` | The incremental parser is `[FIXED]` for symbol extraction (spec 06 §2.1); `std` has no parser. T04-03 links the first real grammar and lifts `tree-sitter` from the T10 guardrail's manual grep-hold (it is a parser, **not** a dense/model backend). It compiles a small generated C runtime via `cc`, offline/bundled exactly like rusqlite. Pinned at 0.24 to pair with the grammar (below). Native-linking transitive set: `tree-sitter-language`, `streaming-iterator`, and `regex`/`regex-automata`/`regex-syntax`/`aho-corasick`/`memchr` (all reused from `ignore`), plus build-time `cc`/`shlex`/`jobserver`/`find-msvc-tools` (reused from rusqlite/zstd). None are dense-backend/model/network SDKs. | MIT |
| `tree-sitter-typescript` | `crates/index` | The first-release language grammar (ADR-0001); ships generated C parser tables compiled by `cc`. Pinned at 0.23 to pair with `tree-sitter 0.24` (its declared `^0.24`; no grammar targets 0.25/0.26 yet). T04-03 uses the `tsx` variant for every TypeScript extension (ADR-0002, determinism). Native-linking transitive set: `tree-sitter-language` (shared with `tree-sitter`); build-time `cc` (reused). Not a dense-backend/model/network SDK. | MIT |
| `tree-sitter-javascript` | `crates/index` | The second-release language grammar (ADR-0001, T04-04); ships generated C parser tables compiled by `cc`. Pinned at **0.23** (not the newer 0.25) for ABI compatibility: the 0.25 grammar is ABI **15**, which `tree-sitter 0.24` (max supported language ABI 14) refuses to load — it would silently degrade to a file-only parse; 0.23.x is ABI 14, the same reason `tree-sitter-typescript` is held at 0.23. Its only runtime dependency is `tree-sitter-language` (already in the tree, shared with `tree-sitter`), so it adds **no new transitive crate**; build-time `cc` reused. Not a dense-backend/model/network SDK. | MIT |
| `tree-sitter-rust` | `crates/index` | The third-release language grammar (ADR-0001 dogfooding, T04-05); ships generated C parser tables compiled by `cc`. Pinned at **0.23** (not 0.24) for ABI compatibility: the grammar's 0.24 line is ABI **15**, which `tree-sitter 0.24` (max supported language ABI 14) refuses to load — it would silently degrade to a file-only parse; 0.23.x is ABI 14, the same reason the TypeScript/JavaScript grammars are held at 0.23. Its only runtime dependency is `tree-sitter-language` (already in the tree, shared with `tree-sitter`), so it adds **no new transitive crate**; build-time `cc` reused. Not a dense-backend/model/network SDK. | MIT |
| `streaming-iterator` | `crates/index` | tree-sitter 0.24 yields query matches as a `StreamingIterator`; driving `QueryCursor::matches` needs the trait in scope (also a transitive dep of `tree-sitter`, promoted to direct). Zero transitive dependencies. Not a dense-backend/model/network SDK. | MIT OR Apache-2.0 |
| `serde_json` | dev-dependency in `crates/index`/`crates/embed` (test fixtures); real dependency in `crates/core` (LRSP frame payload encode/decode, spec 07 §2/§3, T13-03), `crates/store` (`model_space.coverage`, T11-01; LRSP spool decoder, spec 07 §2-§4, T13-03), `crates/protocol`/`crates/search` (wire response shapes), `crates/models`/`crates/xtask` (manifests/reports/bench config), `crates/local-rag-hook` (hook JSON parsing, spec 07 §2, T13-02) | T04-03 is the first task to consume typed JSON fixtures (the parser family, spec 14 §1.1); typed models with `#[serde(deny_unknown_fields)]` make deserialization the schema check, so no runtime `jsonschema` dep is needed. T11-03 reuses it for the same reason on the `fault.llm.*` family (`fixtures/fault/index.json`), which carries the v1 provider retry contract spec 10 §1 inherits. As of T11-01/T12-03/T13-02 it is also a **real** (non-dev) dependency of several shipped binaries — `local-rag` (via `store`/`search`/`protocol`) and `local-rag-hook` — not dev-only; `local-rag-hook` parses Claude Code's real, external hook JSON contract, where hand-rolling a JSON parser/serializer would be disproportionate and error-prone next to a well-audited, ubiquitous crate. T13-03 relocates the canonical LRSP frame payload's serialize/deserialize (`FramePayload`, `encode_frame`) from `local-rag-hook` into `crates/core` (`spool` module), so both the hook-side writer and the new `crates/store` daemon-side decoder share exactly one implementation rather than risking two that could drift — `crates/core` becomes a new real consumer, and `crates/store` gains a second reason (decode, not just `model_space.coverage`'s encode). Already in `Cargo.lock` throughout, so every addition is **0 new external sources** — only new dependency edges. `serde` (`derive`) is already approved (crates/core). Native-linking transitive set: `itoa`, `zmij`, and `memchr`/`serde`/`serde_core` (reused). None are dense-backend/model/network SDKs. | MIT OR Apache-2.0 |
| `ureq` (`default-features = false`, feature `rustls`) | `crates/models` | Spec 10 §5 `[FIXED policy]` requires `init --download-models` to fetch weights over the network; `std` has neither HTTP nor TLS, and shelling out to `curl` would reintroduce the external runtime dependency spec 01 §1 forbids. `ureq` is blocking (matching the installer's shape) and `rustls` keeps TLS in pure Rust — no OpenSSL, no per-platform system TLS stack to bundle. It sits behind the `AssetFetcher` trait seam, so every test in the suite runs against a loopback fixture server or a local directory, never the internet (T11-06, ADR-0005). Native-linking transitive set: `ureq-proto`, `rustls` (ISC), `rustls-pki-types`, `rustls-webpki` (ISC), `ring` (Apache-2.0 AND ISC), `webpki-roots` (CDLA-Permissive-2.0), `untrusted` (ISC), `subtle` (BSD-3-Clause), `zeroize`, `http`, `httparse`, `base64`, `percent-encoding`, `utf8-zero`, `cfg-if`/`log`/`libc` (reused). Not a dense-backend or model SDK. | MIT OR Apache-2.0 |
| `ort` (`default-features = false`, features `load-dynamic`, `ndarray`, `api-24`) + `ndarray` | `crates/models` | Spec 10 §1 `[FIXED]` fixes that embeddings run **in-process** on ONNX Runtime or Candle; ADR-0005 picks ONNX Runtime, and `ort` is its Rust binding. **`load-dynamic` is the load-bearing feature**: `ort-sys` compiles with linking disabled and resolves `libonnxruntime` at runtime through `libloading`, so `cargo build` downloads no binary and the offline rule below still holds — the exact objection `D-008` raised against the default `download-binaries`. `api-24` pins the ONNX Runtime API level (without an explicit level the crate does not compile). `ndarray` is `ort`'s own tensor type (pinned to **0.17**, the version `ort` resolves) and is how token ids/attention masks are handed to a session; `std` has no n-dimensional array. Which shared library ships per platform package is T17-03's "ORT bundling verified before the final CI matrix". Native-linking transitive set: `ort-sys`, `libloading` (ISC), `ndarray`, `matrixmultiply`, `rawpointer`, `num-traits`/`num-complex`/`num-integer`, `portable-atomic`(-util), `tracing`/`tracing-core`, `rayon`(-core) (shared with `tokenizers`), `either`. | MIT OR Apache-2.0 |
| `tokenizers` (`default-features = false`, feature `onig`) | `crates/models` | An embedding model's vectors are only meaningful if input is encoded with the *same* tokenizer it was trained on; `tokenizer.json` ships with the weights and `std` has no BPE/SentencePiece implementation. This is the reference implementation the model card assumes (T11-06). `default-features = false` drops `progressbar` (indicatif) and `esaxx_fast` (a C++ trainer this project never uses — inference only); `onig` stays because it is the only regex backend a non-wasm build can select (the alternative, `fancy-regex`, is reachable only through `unstable_wasm`, which forces `getrandom/wasm_js`). `onig_sys` vendors and compiles the Oniguruma C library (BSD-2-Clause) via the already-present `cc`. Native-linking transitive set: `onig`/`onig_sys` (MIT; vendored C BSD-2-Clause), `esaxx-rs` (pure-Rust without `cpp`), `spm_precompiled`, `compact_str`, `castaway`, `daachorse`, `derive_builder`(+`darling`, build-time), `monostate`, `macro_rules_attribute`, `unicode-normalization-alignments`, `unicode-segmentation`, `unicode_categories`, `itertools`, `ahash`, `rand`/`rand_chacha`/`rand_core`/`getrandom`/`ppv-lite86`/`zerocopy`, `rayon`(-core, -cond), `dary_heap`, `fnv`, `paste`, `static_assertions`, `nom`/`minimal-lexical`. Not a network or dense-backend SDK: it neither downloads (`http`/`hf-hub` features off) nor stores vectors. | Apache-2.0 |
| `notify` (`default-features = false`, feature `macos_fsevent`) | `crates/index` | The reconcile scheduler needs filesystem-change notifications (spec 06 §1: "Watcher (`notify`) events"; the `[FIXED]` principle is watcher = hint, reconcile = truth). `std` has no FS-notification API. Live watching is confined to `reconcile::watcher`; the pure `WatchEvent → Trigger` mapping is what the tests cover, and the OS watcher itself is never in the CI suite (its event timing is not reproducible). `default-features = false` drops the `crossbeam-channel` default (the thin wrapper uses `std`/tokio channels), keeping the set minimal like `ignore`. Runtime crates compiled on the targets this project ships: `notify` (CC0-1.0) + `notify-types` (MIT OR Apache-2.0); on macOS `fsevent-sys` (MIT); on Linux `inotify` + `inotify-sys` (ISC) and `mio` (MIT). Reused (already in the tree): `bitflags`, `libc`, `log`, `walkdir`, `same-file`. `Cargo.lock` additionally records — but never compiles for v0's macOS/Linux targets — `kqueue`/`kqueue-sys` (MIT, BSD), the `windows-sys`/`windows-targets`/`windows_*` family (MIT OR Apache-2.0, `cfg(windows)`), and `wasi` (wasm), gated exactly like rusqlite's wasm subtree. Not a dense-backend/model/network SDK. | CC0-1.0 |
| `llama-cpp-2` (`default-features = false`, feature `common`) + `llama-cpp-sys-2` | `crates/generate` | Spec 10 §1 `[FIXED]` fixes that generation runs **in-process**; ADR-0006 picks `llama.cpp` for the local generative model (Qwen2.5-Instruct and Gemma 4 GGUF, both measured; Gemma 4 shipped as the default), and `llama-cpp-2` is its Rust binding — the generation-side analog of `ort` for embeddings. Unlike `ort`'s `load-dynamic` (resolves `libonnxruntime` at runtime, no C++ toolchain needed), `llama-cpp-sys-2` **vendors and compiles** the llama.cpp/GGML C/C++ source tree from the published crate package via `cmake`/`bindgen` — the real, disclosed cost (ADR-0006): every build host needs `cmake` + `libclang` (bindgen), which `ort`'s runtime-resolution approach specifically avoided. `cargo fetch` alone still gets everything needed (the source is vendored in the crate, not downloaded at build time), so the offline-build rule holds. Native-linking transitive set: `encoding_rs` ((Apache-2.0 OR MIT) AND BSD-3-Clause, token→text decoding), `enumflags2`/`enumflags2_derive` (MIT OR Apache-2.0, sampler/model-param bitflags), `bindgen` (BSD-3-Clause) + `cexpr` (Apache-2.0/MIT) + `clang-sys` (Apache-2.0) + `prettyplease`/`rustc-hash`/`shlex` (build-time only, generating the FFI bindings), `cmake` (MIT OR Apache-2.0, invokes the system `cmake` to build GGML), `find_cuda_helper` (MIT OR Apache-2.0, build-time CUDA toolkit probe — always resolved, never compiled into anything: this build enables no CUDA feature) , `glob` (MIT OR Apache-2.0). `tracing`/`tracing-core`/`thiserror`/`cfg-if` are reused (already in the tree); `tracing-attributes`/`valuable` are new but pulled in only as `tracing`'s own optional-feature transitives. Two packages resolve a **second, older version** alongside one `ort` already pinned — `itertools 0.13.0` (`ort`'s tree already has 0.14.0) and `libloading 0.8.9` (`ort`'s tree already has 0.9.0) — both build-time/FFI-glue only, not a security-relevant duplication, disclosed here rather than silently accepted. None of the above are network SDKs — `llama-cpp-sys-2`'s own feature list (`common`/`cuda`/`metal`/`vulkan`/`rocm`/`opencl`/`mkl`/`openmp`/`system-ggml`/`dynamic-link`/...) is entirely backend/build-toggle, with no download- or network-shaped feature at all. | MIT OR Apache-2.0 (llama-cpp-2/llama-cpp-sys-2) |
| `minijinja` (features `json`, `loop_controls`) + `minijinja-contrib` (`default-features = false`, feature `pycompat`) | `crates/generate` | T14-09: `llama-cpp-2::apply_chat_template` calls the vendored `llama.cpp`'s `llm_chat_detect_template`, a fixed-signature heuristic matcher (not a Jinja interpreter) that does not recognize every model's own embedded chat template (verified: this is why ADR-0006 needed a one-off `chat_template_override` for Gemma 4). `std` has no Jinja/Jinja2 engine. `minijinja` renders each catalog entry's raw `tokenizer.chat_template` GGUF metadata directly, bypassing that detector entirely — the exact escape hatch `LlamaModel::chat_template`'s own doc comment (`llama-cpp-2` 0.1.152) names. `json` enables the `tojson` filter, `loop_controls` enables `{% break %}`/`{% continue %}` — both used by real HuggingFace-authored templates (default features otherwise kept: `builtins`/`macros`/`multi_template`/`serde`/etc. are needed for ordinary filters and macro-defining templates like Gemma 4's own). `minijinja-contrib`'s `pycompat` (`default-features = false`, only this one feature) registers `Environment::set_unknown_method_callback` with Python dict/str method compatibility (`.get()`, `.split()`, …) — verified live necessary: Gemma 4's real template calls `message.get('reasoning')`, which fails with `unknown method: map has no method named get` without it; written by `minijinja`'s own author specifically for this class of real-world-template gap. Pure Rust, no new host-toolchain requirement (`crates/generate` already needs `cmake`+`libclang` for `llama-cpp-sys-2`). Native-linking transitive set verified via `cargo tree -p local-rag-generate`: exactly one new crate, `memo-map` (Apache-2.0, minijinja's own internal memoization cache) — `serde`/`serde_json` are already resolved elsewhere in the tree, and `pycompat`'s only feature dependency (`minijinja/builtins`) is a feature flag on a crate already present, not a new crate. Not a dense-backend/model/network SDK. | Apache-2.0 (minijinja, minijinja-contrib, memo-map) |

The earlier "zero external sources" property (T00-02) is therefore superseded by
this explicit allowlist plus the no-dense/model-SDK-before-T10 rule above.
Historical T00-*/G00 evidence in `docs/implementation-plan/PROGRESS.md` is not
rewritten.

## Workspace layout

- Libraries (`crates/*`): `core`, `store`, `index`, `projection`, `embed`,
  `models`, `generate`, `search`, `memory`, `protocol`. `embed` (T11-03) is the
  embedding provider pool — the `Embedder`/`Generator` contracts (spec 10 §1),
  the central remote-policy guard, primary/fallback + retry, and the in-process
  default/no-op providers — plus, as of T11-04, the resumable coverage backfill
  worker. It depends only on other workspace crates. `models` (T11-06) and
  `generate` (T14-07) are the two heavy-runtime sides of the model-asset
  contract: `models` is the checksum-verified ONNX installer/provider (spec 10
  §5), `generate` is its `llama.cpp`/GGUF analog for the local router (spec 08
  §4, ADR-0006) — a **separate** crate from `models` rather than folded in,
  because `llama-cpp-sys-2` needs a materially different build toolchain
  (`cmake`+`libclang`) than ONNX's `load-dynamic` (no C++ toolchain at all);
  mixing them would force every ONNX-only contributor to also carry a C++
  toolchain they do not need. `memory` (T14-07) is the router itself
  (`local_rag_memory::router::route`) — it depends on `embed`'s `Generator`
  trait/pool only, never on `generate` directly, the same seam `search` uses
  for `Embedder`/`models`. The split is deliberate throughout — the heavy,
  externally-facing dependencies (`ort`/`tokenizers`/`ureq` in `models`;
  `llama-cpp-2`/`llama-cpp-sys-2` in `generate`) live in isolated crates so
  `embed`'s "no network client, no model runtime" lint stays a true statement
  about the pool rather than being relaxed.
- Product binaries: `local-rag` (daemon + CLI), `local-rag-proxy` (stdio MCP
  proxy), `local-rag-hook` (spool writer).
- Dev-only crates (workspace members, excluded from `default-members`, never
  distributed): `xtask` (task runner) and `test-support` (shared test harness —
  temp `LOCAL_RAG_HOME`, controllable clock/UUID, subprocess capture, named
  failpoints). Downstream crates depend on `test-support` as a
  `[dev-dependencies]`, and — for `crates/store`, `crates/index` and
  `crates/projection` — additionally as an **optional** dependency gated by the
  `failpoints` cargo feature (off by default). That feature compiles named
  crash/error seams: `store`'s migration runner (spec 13 §3 hard-kill resume
  tests); `index`'s generation builder
  (`reconcile.build.{after_allocate,persist_file,before_finalize}`, the per-phase
  `building → failed` injection for spec 04 §1 / T05-05 retry-failure tests); and
  `projection`'s fake shard (`projection.fake.{upsert,delete,write_head}` op-ordering
  seams plus the `inspect`/`corrupt` controls for the spec 05 §10 fault matrix,
  T07-01); as of T11-04, `embed`'s backfill worker
  (`embed.backfill.between_batches`, the crash point after a committed cache-write
  batch — spec 10 §4 step 2's "resumable"); as of T11-06, `models`'s installer
  (`models.install.between_files`, fired after a file is verified and renamed into
  place and before the next one starts — spec 10 §5's resumable download); as of T14-07,
  `generate`'s installer has the identical seam under its own name
  (`generate.install.between_files`) — a deliberate duplicate rather than a shared
  helper (see the "Approved external dependencies" table's `llama-cpp-2` row for why
  `generate` is a separate crate from `models`); and, as of T07-05, the write-ahead
  switch's own `projection.switch.before_commit` seam (fires after the shard write lands but
  before the final `state.sqlite` commit — spec 05 §10 F4, the one kill point
  none of the fake shard's own seams could reach). It is never enabled in a
  release or distribution build, so the shipped binary never links
  `test-support`. As of T09-04, `crates/search`'s own `failpoints` feature has
  no seams of its own — it forwards to `local-rag-projection/failpoints` so
  `projection.switch.before_commit` fires under a concurrent search load in
  `crates/search/tests/switch_failpoint_load.rs` (`test-support` is already an
  unconditional dev-dependency there, used for `TempHome`). `cargo xtask ci`
  runs each of those crates once more with `--features failpoints` to lint
  and exercise those code paths.
- Isolated dev workspace `spike/` (T10-01): a **separate** Cargo workspace with
  its **own `Cargo.lock`**, `exclude`d from the root `[workspace]`. It holds the
  roadmap-step-11 dense-backend spike harness (open question O1) and the
  candidate adapters (brute-force T10-02, `usearch` T10-03, Qdrant Edge T10-04)
  that pull real dense-vector crates. Keeping those in the spike's lockfile is
  what makes the "no dense SDK in the product workspace before T10" rule above a
  *structural* fact rather than a review promise — the product `Cargo.lock` never
  resolves them. Two members as of T10-04: `harness` (the shared conformance/
  benchmark harness plus the brute-force/usearch candidates, both cheap enough
  to live as modules there) and `qdrant-edge` (isolated in its own crate: the
  `qdrant-edge` dependency republishes the *actual* Qdrant server's WAL/segment
  storage engine, ~80 transitive dependencies — a different risk class from
  usearch's compact C++ library, isolated so a build/platform problem there can
  never make `harness`'s already-passing candidates uncompilable; `qdrant-edge`
  depends on `harness`, so it ships its own `spike` binary rather than a match
  arm in `harness`'s). `cargo xtask ci` runs the spike's `fmt` blanket across the
  whole workspace, and `clippy`/`test` scoped per member (`-p
  local-rag-spike-harness`, `-p local-rag-spike-qdrant-edge`) so a problem in
  one candidate's build doesn't mask the other's pass/fail signal (root `cargo
  test --workspace` does not reach an excluded sub-workspace at all). The
  winning backend is promoted into the product workspace at T12-02. Never
  distributed.

## npm packages (`npm/`)

T17-01's six npm packages (spec 13 §1), parallel to `crates/` rather than inside it — a
self-contained subtree with different tooling, the same pattern `spike/` already established for
its own separate Cargo workspace:

- `npm/local-rag/` — `@13w/local-rag`, the thin JS launcher (`bin/local-rag-mcp.js`, glue only)
  that resolves the caller's platform package (`src/resolve.js`, `require.resolve`/
  `createRequire`-based — the same hoisting-aware algorithm every package manager's
  npm/pnpm/yarn layout already targets, not a hand-rolled `node_modules` walk) and execs the
  native `local-rag-proxy` in place (`stdio: 'inherit'`), forwarding `SIGINT`/`SIGTERM`
  (`src/lifecycle.js`) 1:1 to it — never `detached`, the inverse of `crates/local-rag-proxy/src/
  connect.rs::spawn_detached_daemon`'s own process-group isolation. T17-02 adds a second
  entrypoint, `bin/local-rag-hook.js`, for the ingestion hook path — it must **always** exit 0
  (fail-open, unlike the MCP launcher) and best-effort refreshes a direct-exec cache symlink
  under `${CLAUDE_PLUGIN_DATA}` (`src/hook-cache.js`) so a Claude Code plugin's hook commands can
  skip Node/npx entirely on the steady-state path — see `## Claude Code plugin` below.
- `npm/local-rag-{darwin-arm64,darwin-x64,linux-x64,linux-arm64,win32-x64}/` — the five
  `optionalDependencies` platform packages (`os`/`cpu` fields select the right one at install
  time). `win32-arm64` is deferred `[FIXED]`, spec 13 §1 — no sixth package.
- In this checkout, every platform package ships `package.json`/`README.md` only — `bin/` (the
  three product binaries `local-rag`/`local-rag-proxy`/`local-rag-hook`) is populated by T17-03's
  release build, not committed here. T17-01's own tests never depend on a real compiled binary:
  they build synthetic fixture trees (`npm/local-rag/test/helpers/fixture-layout.js`) standing in
  for npm-flat, npm-nested, and pnpm-symlinked installs, and a scriptable stand-in
  (`test/helpers/fake-binary.js`) for `local-rag-proxy` in the real-subprocess signal tests.

Run the suite: `cd npm/local-rag && node --test test/*.test.js` — **not** bare `node --test`
(Node's default test-file discovery treats every `.js` file under a directory named `test` as a
test file, which would try to run `test/helpers/fake-binary.js` itself and hang forever in its
own `setInterval`; the explicit glob scopes discovery to the top-level `*.test.js` files only).
Zero `dependencies`/`devDependencies` — only `node:test`/`node:assert`/`node:child_process`/
`node:module`/`node:path`/`node:os`/`node:fs` built-ins, the same "built-ins over an external
dependency" stance the Rust "Dependency policy" section above takes; `npm` itself (bundled with
Node) is the one host tool `test/package-contents.test.js` needs, purely locally (`npm pack
--dry-run`), no registry contact. Requires Node.js ≥20.

Not yet wired into `cargo xtask ci` / `.github/workflows/ci.yml` — that file's own comment already
earmarks "additional platform targets are added by the distribution work (T17)"; running the npm
suite locally is a manual step for anyone touching `npm/` until T17-03 adds real CI coverage.

## Claude Code plugin (`plugin/`, `.claude-plugin/`)

T17-02's plugin registration (spec 11 §3.1, spec 13 §1-2), a real Claude Code plugin/marketplace
pair, not a stub — every manifest here is verified against the actual `claude` CLI, not a
reimplemented schema check:

- `.claude-plugin/marketplace.json` — repo root (required location for `claude plugin marketplace
  add <this-repo>`); one entry, `"source": "./plugin"`.
- `plugin/.claude-plugin/plugin.json` — the plugin manifest (`name`, `version`, `author`; hooks/
  MCP config stay on their default locations, `hooks/hooks.json`/`.mcp.json`, not duplicated
  explicitly).
- `plugin/hooks/hooks.json` — the seven spec 11 §3.1 `[FIXED]` events (`SessionStart`,
  `UserPromptSubmit`, `PostToolUse`, `PostToolUseFailure`, `Stop`, `SubagentStop`, `SessionEnd`),
  every one the identical shell-form command: exec a cached direct path under
  `${CLAUDE_PLUGIN_DATA}` if the previous run populated it, else fall back to
  `npx --yes --package=@13w/local-rag local-rag-hook spool-write`, else `true` — the trailing
  `|| true` is load-bearing: spec 11 §3.1's "always exit 0" is a `[FIXED]` contract on the whole
  command a `hooks.json` entry invokes, not just the native binary once it is running, so it must
  hold even when both the cache and `npx` fail (e.g. first run, offline).
- `plugin/.mcp.json` — the `local-rag` MCP server, `npx --yes --package=@13w/local-rag
  local-rag-mcp` (verified empirically: `npx <pkg> <bin-name>` — without `--package=`/`--yes` —
  does **not** select a non-default bin from a multi-bin package; `--package=` is the form that
  actually works).
- `plugin/bin/local-rag-hook.js`'s own cache write is what makes the hooks.json fast path
  possible: after the first (necessarily slower, `npx`-mediated) bootstrap run, every subsequent
  hook invocation execs the native binary directly — a real measurement against the cargo-built
  binary lands around p50 ≈ 5ms / p95 ≈ 6ms, comfortably under spec 13 §1's <50ms budget
  (`plugin/test/cold-start.test.js`).

Run the suite: `node --test plugin/test/*.test.js` (same explicit-glob reasoning as `npm/`'s own
section above). Three tiers: pure JSON/logic checks (always run); real cargo-built
`target/debug/local-rag-hook`-backed end-to-end checks (`no-writes-in-sample-repo.test.js`,
`cold-start.test.js` — skip with a named reason if that binary is not built, `cargo build -p
local-rag-hook`); real `claude` CLI checks (`manifest-validate.test.js`,
`install-uninstall.test.js`, both gated on `claude` being on `PATH`). All `claude`-CLI-mutating
round trips (`marketplace add`/`install`/`uninstall`/`details`) live in one file
(`install-uninstall.test.js`) deliberately — Node's test runner parallelizes across files, and
concurrent `claude` invocations against the same source repo path raced when split across files;
tests within one file run sequentially by default, which avoids it. Every mutating round trip runs
under an isolated `CLAUDE_CONFIG_DIR` (a real, respected env var — this project's own dev sessions
run under one), the same isolation idiom `LOCAL_RAG_HOME` gives every Rust test here.

Windows: the `${CLAUDE_PLUGIN_DATA}` symlink cache and the hooks.json shell-form command are not
verified on Windows in this task (current CI is Ubuntu-only; the platform matrix is T17-03) — the
same named, scoped deferral pattern the daemon's own Windows named-pipe gap already used (group
16). The fallback path (`npx`) still runs correctly there; only the <50ms fast path is unverified.

## Committing

Each completed task lands as a single focused commit; see the task execution
contract in `CLAUDE.md`.
