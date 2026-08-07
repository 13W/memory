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
  multiplexing is acceptable; hooks path must be exec-fast (<50 ms cold `[SPEC]`). The Claude Code
  plugin's own MCP server entry point resolves through a three-tier fallback (a locally installed
  npm package, a known-path cache, `npx` last resort — not a bare, uncached `npx` invocation) and
  its cached-tier launcher-only overhead must stay under a p95 < 100 ms cold budget `[SPEC]`.

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

As-built note (T19-03, `[SPEC]`, group 19 plan). Two changes to the plugin's own launch path,
distinct from the npm launcher T17-01/T17-03 already cover above:

- **`${CLAUDE_PLUGIN_DATA}` audit, closed without a code change.** The group 19 card asked
  whether this variable, load-bearing for `hooks.json`'s fast path since T17-02, is actually
  documented Claude Code plugin behavior. Verified against the official reference
  (`code.claude.com/docs/en/plugins-reference`): yes — a real, persistent-per-plugin directory
  distinct from the ephemeral `${CLAUDE_PLUGIN_ROOT}`, used here exactly the way the docs' own
  worked example does. No code changed as a result; this note is the confirmation.
- **A three-tier MCP launcher replaces the bare `npx` `.mcp.json` used before.**
  `plugin/bin/local-rag-mcp-launcher.js` tries, in order: (1) a locally installed `@13w/memory`,
  anchored at `${CLAUDE_PROJECT_DIR}/node_modules` only, `require()`-delegated into
  `npm/memory/bin/local-rag-mcp.js` behind a preflight check (that file's own resolution failure
  is fatal by design for the standalone case — `process.exit(1)` — so the preflight, not a bare
  `require()`, is what keeps a missing platform `optionalDependency` from killing this launcher
  before the remaining tiers run); (2) the same `${CLAUDE_PLUGIN_DATA}/bin/` cache the hook already
  uses, a second entry (`local-rag-proxy`) populated by any prior successful tier-1 or tier-3 run
  (`npm/memory/src/binary-cache.js`, renamed and generalized from the hook-only `hook-cache.js` it
  replaces); (3) `npx --yes --package=@13w/memory local-rag-mcp`, today's only prior behavior, kept
  as the unconditional last resort. `.mcp.json`'s `command`/`args` became `"node"` +
  `${CLAUDE_PLUGIN_ROOT}/bin/local-rag-mcp-launcher.js` — not a bare shebang'd path, which the
  official docs' own worked example for a plugin-bundled JS server also avoids, and which would
  not work on `win32-x64` at all (a shell-less direct spawn does not consult shebangs). Real
  measurement of the launcher's own overhead on the cached tier-2 path: p50 ≈ 39 ms / p95 ≈ 42 ms
  (`plugin/test/mcp-cold-start.test.js`), under the p95 < 100 ms budget chosen above — structurally
  larger than the hook's own <50 ms budget because `.mcp.json` has no shell for `||` fallback
  chaining, so Node startup itself is unavoidable on every tier (unlike the hook's fast path, which
  execs the cached native binary directly, no Node at all); the MCP server pays this once per
  session, not once per event the way the hook's own tighter budget matters for.

As-built note (T19-04, `[SPEC]`, group 19 plan). A fifth adoption channel, alongside
`SERVER_INSTRUCTIONS` (D-041), the tool catalog (T19-01), and the recall trailer (T19-02):
`plugin/skills/memory-first-workflow/SKILL.md`, a Claude Code plugin skill — a compact
built-in-tool → local-rag-tool routing table reusing the exact trigger phrasing T19-01 settled on
for all five read-heavy tools (`search_code`, `get_file_context`, `project_overview`, `recall`,
`remember`), plus the `RECALL → SEARCH_CODE → THINK → ACT → REMEMBER` cycle `SERVER_INSTRUCTIONS`
(D-041) already carries, quoted verbatim (same arrow character, U+2192) for cross-channel
consistency. Verified against the official reference (`code.claude.com/docs/en/{plugins-reference,
skills,plugins}`): skills ship at `<plugin>/skills/<name>/SKILL.md` — **not**
`.claude-plugin/skills/`, that directory holds only `plugin.json` — auto-discovered on install with
no `plugin.json` entry, the identical default-location convention `hooks/hooks.json`/`.mcp.json`
already use; the manifest's own `skills` field exists only to *add* extra non-default paths, never
to register the default one. No `disable-model-invocation`/`user-invocable` override: both stay
default (`true`), so the skill's `description` stays in every session's skill listing and Claude
can route to it without an explicit invocation — the entire point of this channel, and a deliberate
divergence from the official quickstart's own canonical example, which defaults the other way
(`disable-model-invocation: true`, user-invocable only). Confirmed via a real
`claude plugin marketplace add`/`install`/`details` round trip:
`claude plugin details memory@memory` reports `Skills (1)  memory-first-workflow`, ~90 always-on /
~200 on-invoke projected tokens. `install-uninstall.test.js`'s existing repo-wide
`git status --porcelain` diff (unchanged since T17-02) already proves the skill's own static files
never write into a user's project on install/uninstall — no new test needed for that half of the
"plugin packaging must not modify users' repositories" guardrail.

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

As-built note (T17-04, `[OPEN]` — still open; boundary made explicit, not resolved). No v1
memory-store schema reader/importer exists in this codebase, and this task adds none: T00-01's
v1 fixtures are behavioral test fixtures only (`input tree / event stream / query → expected
behavior`), never a live data source (CLAUDE.md: this is a greenfield rewrite — "do not assume
production code... legacy fixtures exist"). **v0/MVP ships clean-start only** — a v1→v2 upgrade
starts with an empty memory store; there is no automatic or documented manual data-migration
path in this release. This is a deliberate MVP/GA scope boundary, not a silently dropped
requirement: *whether* GA ships a real v1 importer (versus formally making clean-start
permanent) remains the actual open product decision, tracked as a pre-GA release-gate item
alongside O2/O6 (see G17: "O2/O6 remaining values resolved by evidence or release blocked").

As-built note (T17-04, `[SPEC]`). "Migration tests run on fixture stores of every prior
released schema version" resolved to fixtures **built on the fly**, not committed `.sqlite`
binaries (CLAUDE.md forbids committing generated stores): `crates/store/tests/support/
mod.rs::build_store_at_version(layout, n, now_ms)` migrates a fresh store through
`local_rag_store::migrate::ALL[..n]` only. This is trustworthy specifically because
`Migration::checksum` freezes each entry's SQL once shipped — `&ALL[..n]` for a historical
`n` is byte-identical to what the real release at that version produced, not a second,
separately-maintained encoding of it. `crates/store/tests/migrate_fixtures.rs` drives every
`n` in `1..=ALL.len()` through the real forward chain to head (plus a real seeded row
surviving the whole chain), and carries a tripwire asserting every released migration is
still simple (non-destructive, no Rust steps) — true as of this writing, so no real
migration has ever exercised the checkpoint/backup machinery; that machinery is proven
generically instead, against synthetic complex migrations
(`crates/store/tests/migrate_resumable.rs`, T01-04). The restore mechanic described above
("rollback = restore + old binary") is likewise proven end to end against a synthetic
destructive set for the same reason —
`crates/store/tests/migrate_restore.rs::restoring_a_pre_destructive_backup_recovers_the_old_binarys_data`
copies a `VACUUM INTO` backup back over `state.sqlite`, reopens with a migration set
restricted to the pre-upgrade version (standing in for "the previous binary"), and confirms
both that the data is recovered with no `IncompatibleStore` refusal and that the restored
store still forward-migrates cleanly afterward (an operator's restore does not strand the
store off the upgrade path).

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

As-built note (T17-04, `[SPEC]`). D-026 (spec 11 §4/`DEVIATIONS.md`) already proved the upgrade
loop itself end to end with two real processes, but every existing scenario there starts from a
fresh, empty store, so none of them exercised "the new side actually migrates something the old
side left behind" — only that the protocol handshake retries and converges. Closed by
`local-rag-proxy/tests/subprocess.rs::a_real_older_daemon_binary_drains_and_a_real_new_daemon_
migrates_the_store_to_head`, gated on `--features failpoints`: a real, older-standing-in `local-rag
serve` process (configured via the new `LOCAL_RAG_TEST_FAKE_DAEMON_VERSION`/`LOCAL_RAG_TEST_MAX_
SCHEMA_VERSION` env-var overrides — `main.rs::test_daemon_version_override`,
`local_rag_store::state::migration_set_for_this_open`, both feature-gated off by default, zero
effect on a release build) answers a mismatched `daemon_version` and migrates only through a
restricted schema version; a clean-environment real proxy drives the real upgrade loop against it,
and a clean-environment `local-rag doctor --json` afterward proves `store_version` genuinely
advanced past the old process's own cap with nothing left pending. This is honestly one compiled
artifact configured two ways, not a second historical binary — there is no real second release or
machine available (no network, no second checkout) — documented as such in the test's own doc
comment.
