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

## 2. Launcher requirements (verified by packaging tests) `[FIXED list]`

- signal forwarding + reliable termination of the stdio child; CTRL-C / SIGTERM correctness;
  orphan cleanup;
- resolution under pnpm / npm / yarn layouts (hoisting differences);
- a clear, actionable error when the platform package is missing;
- fully offline operation after `local-rag init --download-models`;
- checksum/manifest + atomic model download (10 §5);
- ORT bundling settled before the final CI matrix.

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
