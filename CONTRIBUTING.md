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
| `rusqlite` (feature `bundled`) | `crates/store` | The store *is* SQLite (spec 03); `std` has no SQLite. `bundled` compiles SQLite from source for reproducible, offline builds and a self-contained binary with no system `libsqlite3` (spec 13 §1/§2). Native-linking transitive set: `libsqlite3-sys`, `cc`/`shlex`/`pkg-config`/`vcpkg`/`find-msvc-tools` (build), `bitflags`, `hashlink`, `hashbrown`/`foldhash`, `fallible-iterator`, `fallible-streaming-iterator`, `smallvec`. rusqlite 0.40 additionally resolves a **wasm32-target-only** subtree (`sqlite-wasm-rs`, `rsqlite-vfs`, `wasm-bindgen`(+macro/-support/-shared), `js-sys`, `bumpalo`, `once_cell`, `thiserror`(+impl), `rustversion`, `proc-macro2`/`quote`/`syn`/`unicode-ident`) that is gated to `cfg(target_arch = "wasm32")` and never links into the native builds this project ships. None are dense-backend/model/network SDKs. | MIT |
| `tokio` (lib: feature `sync` only) | `crates/store` (+ test-only `rt-multi-thread`, `macros`) | The bounded write queue is an async `mpsc` + `oneshot` with backpressure/cancellation (spec 02 §5 L4). `std` has no async channels with cancellable backpressure. The library links only `sync` (no runtime/net/mio); the daemon supplies the runtime later (T15). | MIT |
| `blake3` (`default-features = false`, feature `pure`) | `crates/core` | The domain-separated content/manifest/subject identity hash (spec 03 §1.2) is BLAKE3; `std` has no BLAKE3, and this is the `[FIXED]` hash schema whose correctness must match the reference algorithm. `pure` forces portable Rust — no `cc`/assembly SIMD — so the **native-linked** set is just `blake3` + `arrayref` (BSD-2-Clause) + `arrayvec` (MIT OR Apache-2.0) + `cfg-if` (MIT OR Apache-2.0, already present) + `constant_time_eq` (CC0-1.0). `Cargo.lock` additionally resolves `cpufeatures` (0.3.0) — and reuses `cc`/`shlex`/`find-msvc-tools` already pulled by rusqlite — for blake3's non-`pure` SIMD path; `pure` gates them out so they never compile into or link the native builds this project ships, exactly like rusqlite's wasm subtree. None are dense-backend/model/network SDKs. | CC0-1.0 OR Apache-2.0 OR Apache-2.0 WITH LLVM-exception |
| `unicode-normalization` | `crates/core` | Path canonicalization requires Unicode **NFC** (spec 03 §1.3); `std` has no Unicode normalization and the tables are impractical to vendor by hand. Maintained by the unicode-rs org. Native-linking transitive set: `tinyvec` (Zlib OR Apache-2.0 OR MIT), `tinyvec_macros` (MIT OR Apache-2.0 OR Zlib). | MIT OR Apache-2.0 |
| `casefold` | `crates/core` | Case-insensitive filesystems need Unicode **simple case folding** for path identity (spec 03 §1.3); `std` offers only full case *mapping* (`to_lowercase`), not simple folding. From GitHub's `github/rust-gems` monorepo (the code-search domain); a ≈1 KB paged-bitmap/run-length table with **zero** transitive dependencies. | MIT |

The earlier "zero external sources" property (T00-02) is therefore superseded by
this explicit allowlist plus the no-dense/model-SDK-before-T10 rule above.
Historical T00-*/G00 evidence in `docs/implementation-plan/PROGRESS.md` is not
rewritten.

## Workspace layout

- Libraries (`crates/*`): `core`, `store`, `index`, `projection`, `memory`,
  `protocol`.
- Product binaries: `local-rag` (daemon + CLI), `local-rag-proxy` (stdio MCP
  proxy), `local-rag-hook` (spool writer).
- Dev-only crates (workspace members, excluded from `default-members`, never
  distributed): `xtask` (task runner) and `test-support` (shared test harness —
  temp `LOCAL_RAG_HOME`, controllable clock/UUID, subprocess capture, named
  failpoints). Downstream crates depend on `test-support` as a
  `[dev-dependencies]`, and — for `crates/store` only — additionally as an
  **optional** dependency gated by the `failpoints` cargo feature (off by
  default). That feature compiles the migration runner's named crash seams
  (spec 13 §3 hard-kill resume tests); it is never enabled in a release or
  distribution build, so the shipped binary never links `test-support`. `cargo
  xtask ci` runs the store crate once more with `--features failpoints` to lint
  and exercise that code path.

## Committing

Each completed task lands as a single focused commit; see the task execution
contract in `CLAUDE.md`.
