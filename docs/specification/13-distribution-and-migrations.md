# 13 — Distribution & Migration Framework

## 1. Packaging `[FIXED]`

One native service binary, no mandatory external daemons; model assets delivered separately.

- npm: `@13w/local-rag` (thin JS launcher) + per-platform packages as `optionalDependencies`
  (`@13w/local-rag-darwin-arm64`, `…-darwin-x64`, `…-linux-x64`, `…-linux-arm64`,
  `…-win32-x64`). `win32-arm64` deferred until the chosen dense backend / ORT / fastembed /
  SQLite / tree-sitter / local generator / npm platform detection / CI smoke all pass on it
  `[FIXED]`.
- Build tooling: `cargo-dist`, `cargo-zigbuild` `[FIXED]`.
- Binaries per platform package `[SPEC]`: `local-rag` (daemon+CLI multiplexed), `local-rag-proxy`
  (stdio MCP proxy), `local-rag-hook` (spool writer). Single binary with argv0/subcommand
  multiplexing is acceptable; hooks path must be exec-fast (<50 ms cold `[SPEC]`).

As-built note (T17-03, `[SPEC]`). The npm scope stays `@13w`, but the launcher and platform
package **names** are `@13w/memory` / `@13w/memory-{darwin-arm64,darwin-x64,linux-x64,
linux-arm64,win32-x64}` — an owner decision made when `cargo dist generate` needed a real
`repository` URL, at which point the GitHub repository was created as `13W/memory` (public;
`13W/local-rag` could not be created — the account hit GitHub's own "Trade controls restricted
owner" account-level restriction on **private** repo creation specifically, resolved by creating
a public repo instead) and the npm/plugin identity was renamed to match in the same pass. Product
binary names, crate names, the `local-rag` CLI command, and every `LOCAL_RAG_*` env var /
on-disk store path are **not** part of this rename — see `npm/memory/` and `plugin/`
(`.claude-plugin/marketplace.json`, `plugin/.claude-plugin/plugin.json`) for the exact as-built
names. Historical evidence in `docs/implementation-plan/PROGRESS.md` for T17-01/T17-02 still cites
`@13w/local-rag*`, correctly, as that was the real name at the time those tasks executed — it is
not rewritten (`CLAUDE.md`: prior evidence is never edited after the fact).

## 2. Launcher requirements (verified by packaging tests) `[FIXED list]`

- signal forwarding + reliable termination of the stdio child; CTRL-C / SIGTERM correctness;
  orphan cleanup;
- resolution under pnpm / npm / yarn layouts (hoisting differences);
- a clear, actionable error when the platform package is missing;
- fully offline operation after `local-rag init --download-models`;
- checksum/manifest + atomic model download (10 §5);
- ORT bundling settled before the final CI matrix.

As-built note (T17-03, `[SPEC]`). "Settled" resolved to: each platform package's `bin/` directory
carries one extra flat file, `libonnxruntime.dylib`/`libonnxruntime.so` (no `win32` entry — see
below), sitting next to the three product binaries with no manifest of its own —
`crates/models/src/onnx.rs::bundled_ort_dylib_path` looks for it there, by the running
executable's own directory, mirroring `local-rag-proxy::connect::resolve_daemon_binary_path`'s
existing convention exactly. Resolution order at process start: an explicit `ORT_DYLIB_PATH`
first, else the bundled file, else a typed `OnnxError::Runtime` — never `ort`'s own implicit
default search, which a corrective finding (`DEVIATIONS.md` D-028) showed can hang the calling
thread indefinitely instead of erroring when nothing is found. The bundled file itself comes from
ONNX Runtime's own official release archives, pinned by exact URL and SHA-256 per platform
(`crates/xtask/src/dist_ort.rs::ORT_ASSETS`, fetched via `cargo xtask dist-ort`) — the same
verify-before-trust shape 10 §5's model-weight installer already uses, applied to a build/release
tool rather than a runtime command. `darwin-arm64`/`linux-x64`/`linux-arm64` pin the same ONNX
Runtime release this project's own G11 gate already validated end to end (v1.27.0); `darwin-x64`
pins the older v1.20.0 specifically, because Microsoft stopped shipping prebuilt Intel-Mac
binaries as of v1.27.0 and v1.20.0 is the newest tag that still has one. Verification reached in
this environment (macOS arm64, no Docker/QEMU, no real Windows or Intel-Mac host — `DEVIATIONS.md`
D-029): checksum-verified for all four reachable platforms; full real inference (indexing a real
fixture end to end through the bundled library, a real cold-spawn MCP handshake) verified natively
on `darwin-arm64`; on `darwin-x64`, only reachable here through Rosetta emulation, the pinned
v1.20.0 library is unsigned and failed to load under that emulation — the watchdog fix (D-028)
turns that into a clean, bounded error rather than a hang, but does not establish whether the same
binary loads correctly on real Intel Mac hardware, which this environment cannot test;
`linux-x64`/`linux-arm64` are structurally verified only (correct ELF format/architecture via
`file`), not executed. `win32-x64`/`win32-arm64` ship no bundled runtime at all: `cargo-zigbuild`
does not support Windows targets, so this machine has no reachable build to bundle one into
(D-029) — the platform package's `bin/` stays without a runtime until a real Windows build exists.

## 3. Migration framework `[FIXED]`

Goal: not "zero migrations" but "never re-key fundamental identity again". From day one:

- `schema_migrations` table (03 §2.1); every migration = numbered, checksummed, forward-only
  SQL + optional Rust step.
- **App compatibility check** at open: binary refuses stores newer than it supports
  (`MIGRATION_IN_PROGRESS` / `INCOMPATIBLE_STORE` errors); older stores are migrated under the
  migration lock (L1) with the daemon otherwise quiescent.
- **Resumable migrations**: each step idempotent; progress row per step; crash ⇒ resume.
- **Backup before destructive**: `state.sqlite` copied (`VACUUM INTO`) to
  `backups/state-<version>-<ts>.sqlite` before any destructive step; rollback = restore +
  old binary `[SPEC mechanics]`.
- Migration tests run on fixture stores of every prior released schema version `[FIXED]`.
- `cache.sqlite` is never migrated: version bump ⇒ drop & rebuild (03 §4.4).
- Deferred features are additive by design (03 §5).
- v1 → v2 data migration: **[OPEN]** (migrate v1 memory vs clean start) — decided before GA,
  not before MVP.

## 4. Upgrade flow `[SPEC]`

New binary via npm → next proxy spawn detects version mismatch → `SHUTDOWN_REQUEST` to old
daemon (02 §4.2) → old daemon drains and exits → new daemon migrates (if needed) → serves.
Spool format compatibility is part of the handshake (11 §4): a daemon MUST be able to import
all spool `format_version`s ≤ its own.

As-built note (T15-02, `[SPEC]`): "next proxy spawn detects version mismatch" is
`local-rag-proxy::handshake::establish_session`'s retry loop — after a compatible `WELCOME` whose
`daemon_version` differs from this proxy's own build, it sends `SHUTDOWN_REQUEST`, waits up to 30 s
for the old daemon to close the connection (`wait_for_close`, 02 §4.2's as-built note), then calls
`connect_or_spawn` again. Because the old daemon has by then released `store.lock`, that second
call spawns the *current* on-disk binary — there is no separate "detect the new version and spawn
it" step; the version comes from whichever `local-rag` binary `resolve_daemon_binary_path` finds
next to this proxy at that moment, which npm's own package swap is what makes new. Bounded by
`MAX_UPGRADE_ROUNDS = 2`, so a daemon that keeps answering with a mismatched version (a
misconfigured install, not a normal one-shot upgrade) surfaces as `ProxyError::
UpgradeLoopExceeded` rather than looping forever.
