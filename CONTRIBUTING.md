# Contributing to local-rag

## Full check (single command)

Run the complete quality gate — formatting, lint (warnings denied), tests, and
docs — with one command:

```
cargo xtask ci
```

This is exactly what CI runs. `xtask` is a thin Rust runner (crate
`crates/xtask`) that executes the following, failing on the first error:

1. `cargo fmt --all --check`
2. `cargo clippy --workspace --all-targets -- -D warnings`
3. `cargo test --workspace`
4. `cargo doc --workspace --no-deps`

After the initial `cargo fetch`, the full check runs **offline**.

## Toolchain / MSRV

- Pinned toolchain: **1.96.1** via `rust-toolchain.toml` (components `rustfmt`,
  `clippy`), installed automatically by `rustup`.
- Edition **2024**; MSRV **1.96** (`rust-version` in `[workspace.package]`).

## Dependency policy

- `Cargo.lock` is committed for reproducible builds.
- No dense-vector or embedding-model SDK enters the workspace before the T10
  comparative spike / T11 embeddings work (see `docs/implementation-plan`).
  Nothing may couple to a concrete dense backend before then.
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
| `serde` (feature `derive`) | `crates/core` | Typed deserialization of the versioned `config.toml` sections (spec 02 §3.1); `std` has no deserialization framework, and hand-mapping every field is error-prone for a security-relevant config (`data_policy`). The `derive` feature generates the `Deserialize` impls. Native-linking transitive set: `serde_core`; the `derive` feature additionally pulls the build-time proc-macro `serde_derive`, reusing `proc-macro2`/`quote`/`syn`/`unicode-ident` already resolved by rusqlite's wasm subtree. None are dense-backend/model/network SDKs. | MIT OR Apache-2.0 |
| `toml` | `crates/core` | Parse the versioned global config `<config_dir>/config.toml` (spec 02 §3.1); `std` has no TOML parser, and hand-rolling one for a versioned, nested, security-relevant config is disproportionate and error-prone. Native-linking transitive set: `toml_edit`, `toml_datetime`, `toml_write`, `serde_spanned`, `winnow`, `memchr`, `indexmap`, `equivalent` — reusing `hashbrown` already resolved by rusqlite/hashlink. All are toml-rs / parsing utilities; none are dense-backend/model/network SDKs. | MIT OR Apache-2.0 (winnow MIT; memchr Unlicense OR MIT) |
| `ignore` (`default-features = false`) | `crates/index` | The `ignored` skip reason needs correct gitignore semantics (anchoring, `**`, negation, nested-`.gitignore` precedence) which the spec (06 §2) explicitly delegates to this crate; hand-rolling them is error-prone and would drift from Git. T03-02 uses only the `ignore::gitignore` matcher; the parallel `Walk` tree scan is T05-02. Maintained in ripgrep / BurntSushi's monorepo. Native-linking transitive set: `globset`, `aho-corasick`, `regex-automata`, `regex-syntax`, `bstr`, `memchr` (reused from `toml`), `log`, `same-file`, `walkdir`, `crossbeam-deque`/`crossbeam-epoch`/`crossbeam-utils` (+ `winapi-util`/`windows-sys`/`windows-link` on Windows only). All are ripgrep-ecosystem matching/traversal utilities; none are dense-backend/model/network SDKs. | Unlicense OR MIT |
| `zstd` | `crates/store` | The exact-byte `source_blob` invariant (spec 03 §2.3 `source_compression IN ('none','zstd')`, 12 §5 `[FIXED]`) stores a local copy of every indexed source; zstd is the `[FIXED]` compressor. `std` has no zstd. Default features vendor libzstd from C source via `zstd-sys`, so builds stay reproducible and offline exactly like rusqlite's bundled SQLite. T03-03 uses only `zstd::encode_all`/`decode_all` (in-memory round-trip). Native-linking transitive set: `zstd`, `zstd-safe`, `zstd-sys`, reusing `cc`/`shlex`/`pkg-config`/`vcpkg` already pulled by rusqlite's `bundled` build. Adding `zstd-sys`'s `cc` build refreshed `cc`'s **build-time** `jobserver` (parallel-build coordination), which on some non-host targets resolves `getrandom`/`r-efi`; all three are build-dependencies of `cc`, compiled to run the C build and never linked into the shipped binary. Not a dense-backend/model/network SDK (not on the T10 guardrail list). | MIT (crates) / BSD-3-Clause OR GPL-2.0 (vendored libzstd) |
| `tree-sitter` | `crates/index` | The incremental parser is `[FIXED]` for symbol extraction (spec 06 §2.1); `std` has no parser. T04-03 links the first real grammar and lifts `tree-sitter` from the T10 guardrail's manual grep-hold (it is a parser, **not** a dense/model backend). It compiles a small generated C runtime via `cc`, offline/bundled exactly like rusqlite. Pinned at 0.24 to pair with the grammar (below). Native-linking transitive set: `tree-sitter-language`, `streaming-iterator`, and `regex`/`regex-automata`/`regex-syntax`/`aho-corasick`/`memchr` (all reused from `ignore`), plus build-time `cc`/`shlex`/`jobserver`/`find-msvc-tools` (reused from rusqlite/zstd). None are dense-backend/model/network SDKs. | MIT |
| `tree-sitter-typescript` | `crates/index` | The first-release language grammar (ADR-0001); ships generated C parser tables compiled by `cc`. Pinned at 0.23 to pair with `tree-sitter 0.24` (its declared `^0.24`; no grammar targets 0.25/0.26 yet). T04-03 uses the `tsx` variant for every TypeScript extension (ADR-0002, determinism). Native-linking transitive set: `tree-sitter-language` (shared with `tree-sitter`); build-time `cc` (reused). Not a dense-backend/model/network SDK. | MIT |
| `tree-sitter-javascript` | `crates/index` | The second-release language grammar (ADR-0001, T04-04); ships generated C parser tables compiled by `cc`. Pinned at **0.23** (not the newer 0.25) for ABI compatibility: the 0.25 grammar is ABI **15**, which `tree-sitter 0.24` (max supported language ABI 14) refuses to load — it would silently degrade to a file-only parse; 0.23.x is ABI 14, the same reason `tree-sitter-typescript` is held at 0.23. Its only runtime dependency is `tree-sitter-language` (already in the tree, shared with `tree-sitter`), so it adds **no new transitive crate**; build-time `cc` reused. Not a dense-backend/model/network SDK. | MIT |
| `tree-sitter-rust` | `crates/index` | The third-release language grammar (ADR-0001 dogfooding, T04-05); ships generated C parser tables compiled by `cc`. Pinned at **0.23** (not 0.24) for ABI compatibility: the grammar's 0.24 line is ABI **15**, which `tree-sitter 0.24` (max supported language ABI 14) refuses to load — it would silently degrade to a file-only parse; 0.23.x is ABI 14, the same reason the TypeScript/JavaScript grammars are held at 0.23. Its only runtime dependency is `tree-sitter-language` (already in the tree, shared with `tree-sitter`), so it adds **no new transitive crate**; build-time `cc` reused. Not a dense-backend/model/network SDK. | MIT |
| `streaming-iterator` | `crates/index` | tree-sitter 0.24 yields query matches as a `StreamingIterator`; driving `QueryCursor::matches` needs the trait in scope (also a transitive dep of `tree-sitter`, promoted to direct). Zero transitive dependencies. Not a dense-backend/model/network SDK. | MIT OR Apache-2.0 |
| `serde_json` (dev-dependency) | `crates/index` (tests only) | T04-03 is the first task to consume typed JSON fixtures (the parser family, spec 14 §1.1); typed models with `#[serde(deny_unknown_fields)]` make deserialization the schema check, so no runtime `jsonschema` dep is needed. Dev-only — the shipped binary never links it. `serde` (`derive`) is already approved (crates/core). Native-linking transitive set: `itoa`, `zmij`, and `memchr`/`serde`/`serde_core` (reused). None are dense-backend/model/network SDKs. | MIT OR Apache-2.0 |
| `notify` (`default-features = false`, feature `macos_fsevent`) | `crates/index` | The reconcile scheduler needs filesystem-change notifications (spec 06 §1: "Watcher (`notify`) events"; the `[FIXED]` principle is watcher = hint, reconcile = truth). `std` has no FS-notification API. Live watching is confined to `reconcile::watcher`; the pure `WatchEvent → Trigger` mapping is what the tests cover, and the OS watcher itself is never in the CI suite (its event timing is not reproducible). `default-features = false` drops the `crossbeam-channel` default (the thin wrapper uses `std`/tokio channels), keeping the set minimal like `ignore`. Runtime crates compiled on the targets this project ships: `notify` (CC0-1.0) + `notify-types` (MIT OR Apache-2.0); on macOS `fsevent-sys` (MIT); on Linux `inotify` + `inotify-sys` (ISC) and `mio` (MIT). Reused (already in the tree): `bitflags`, `libc`, `log`, `walkdir`, `same-file`. `Cargo.lock` additionally records — but never compiles for v0's macOS/Linux targets — `kqueue`/`kqueue-sys` (MIT, BSD), the `windows-sys`/`windows-targets`/`windows_*` family (MIT OR Apache-2.0, `cfg(windows)`), and `wasi` (wasm), gated exactly like rusqlite's wasm subtree. Not a dense-backend/model/network SDK. | CC0-1.0 |

The earlier "zero external sources" property (T00-02) is therefore superseded by
this explicit allowlist plus the no-dense/model-SDK-before-T10 rule above.
Historical T00-*/G00 evidence in `docs/implementation-plan/PROGRESS.md` is not
rewritten.

## Workspace layout

- Libraries (`crates/*`): `core`, `store`, `index`, `projection`, `search`,
  `memory`, `protocol`.
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
  T07-01) and, as of T07-05, the write-ahead switch's own
  `projection.switch.before_commit` seam (fires after the shard write lands but
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

## Committing

Each completed task lands as a single focused commit; see the task execution
contract in `CLAUDE.md`.
