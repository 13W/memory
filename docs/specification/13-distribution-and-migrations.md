# 13 — Distribution & Migration Framework

## 1. Packaging `[FIXED]`

One native service binary, no mandatory external daemons; model assets delivered separately.

- npm: one package, `@13w/memory`, whose job is to obtain and expose the binaries rather than to
  contain them; the native binaries themselves are the project's own GitHub release assets,
  produced by the tagged CI release `[FIXED, ADR-0013]`. `win32-arm64` deferred until the chosen
  dense backend / ORT / fastembed / SQLite / tree-sitter / local generator / npm platform
  detection / CI smoke all pass on it `[FIXED]`.
- Build tooling: `cargo-dist` `[FIXED, ADR-0013]`.
- Binaries per release asset `[SPEC, ADR-0013]`: `local-rag` (daemon+CLI multiplexed),
  `local-rag-proxy` (stdio MCP proxy), `local-rag-hook` (spool writer) and `local-rag-tui`
  (11 §7) — one archive per binary per target, each with its own checksum sidecar. Single binary
  with argv0/subcommand multiplexing is acceptable; hooks path must be exec-fast (<50 ms cold
  `[SPEC]`). The Claude Code plugin's own MCP server entry point resolves an **executable**, not an
  npm package — the ordered contract is §2's — and the launcher-only overhead of that resolution
  must stay under a p95 < 100 ms cold budget `[SPEC]`.

Amendment note (T22-03, `[FIXED]` change under
[ADR-0013](../adr/0013-binary-delivery-via-release-assets.md)). The three bullets above are this
ADR's. The channel the first one used to describe — five per-platform npm packages selected by
`optionalDependencies` — was never built: their `bin/` is gitignored and was populated by hand, no
script, xtask subcommand or CI step ever did it, and nothing was ever published to the registry
(`D-102`). The channel replacing it was already running: `cargo-dist` emits one archive per binary
per target on a tag, each with a `.sha256` sidecar, alongside a `dist-manifest.json`.
`cargo-zigbuild` leaves the `[FIXED]` tooling list with that ritual — its only role was the by-hand
cross-build that filled those packages, and it appears in neither `.github/workflows/release.yml`
nor `dist-workspace.toml`; `T22-17` confirms the pipeline needs nothing further. `local-rag-tui` is
listed because `[package.metadata.dist] dist = true` makes it the fourth product binary `dist plan`
emits (11 §7); tag `0.0.0` predates that crate and carries only three. Deliberately **not** settled
here: how a client finds the binary, what it trusts, and what triggers an upgrade — §2 and §4,
amended separately.

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

As-built note (T22-04, `[SPEC]`, [ADR-0013](../adr/0013-binary-delivery-via-release-assets.md)).
**The next two notes are superseded and kept as history**, but not wholly, and the difference
matters because part of what they recorded is the reason the replacement has the shape it does.

What is superseded: the three-tier ladder in its entirety, including the `npx` last resort and the
`${CLAUDE_PLUGIN_DATA}/bin` cache tier; the p50 ≈ 39 ms / p95 ≈ 42 ms measurement, which timed
that cache tier specifically and therefore no longer measures anything that exists; and, from the
D-055 note, the whole `require.resolve` mechanism — the two anchors, `npmGlobalNodeModules()`, the
accepted miss for a custom npm prefix, and the `LOCAL_RAG_TEST_GLOBAL_NODE_MODULES` seam. §2 above
states what replaces them.

What still holds, and is load-bearing for `T22-12`/`T22-13`. `.mcp.json` keeps `"node"` +
`${CLAUDE_PLUGIN_ROOT}/…` rather than a bare shebanged path, for the reason that note gives: a
shell-less direct spawn does not consult shebangs, so a bare path would not work on `win32` at
all. And the MCP budget is structurally larger than the hook's for the reason it gives too —
`.mcp.json` has no shell in which to chain a fallback, so Node startup is unavoidable there, while
the hook can exec a native binary directly with no Node at all. Those two facts are precisely why
the replacement keeps a Node launcher on the MCP path and gives the hook a POSIX `sh` resolver.
`${CLAUDE_PLUGIN_DATA}` remains real, documented Claude Code behaviour, as that note's audit
established; only this project's use of it as a binary cache ends.

D-055's finding is not overturned either — it is carried out. It established that "installed on
this machine" is what a user actually has, and that a single global install is the only route to a
network-free cold start. This change removes the last path that contradicted it.

As-built note (T22-09, `[SPEC]`, [ADR-0013](../adr/0013-binary-delivery-via-release-assets.md)).
**The npm package resolves too, and its order is not §2's.** §2 states the order for a client that
did not install the binaries — an override, then `PATH`, then well-known global-bin directories —
and its own amendment note assigns it to `T22-12`/`T22-13`. `@13w/memory` is the other side of the
same sentence in §1 above: it *obtains and exposes* them, so it knows where it put them, and it
carries one obligation the plugin does not — a developer's own build must win over anything
downloaded. `npm/memory/src/locate.js` therefore resolves, in order: `LOCAL_RAG_BIN_DIR`; a source
checkout containing this very package (`target/release`, then `target/debug`); the package's own
`bin/`; the per-user cache at `<data_dir>/local-rag/bin/<target-triple>`; then "not installed".

Three properties of that order are load-bearing rather than incidental.

- **A rung yields a directory, and counts only when that directory holds every required binary.**
  §4's co-location clause is what makes "the version comes from whichever binary is found next to
  this proxy" a definition; a resolver answering per binary could return a proxy from one rung and
  a daemon from another, and nothing downstream would notice until the versions disagreed.
- **The first two rungs are terminal.** An override that does not hold the binaries is an error,
  never a reason to look further — ADR-0013 introduced it as the air-gapped answer, "which wins
  over everything and never downloads", and an override silently ignored is worse than none. A
  checkout with nothing built is likewise an error naming `cargo build`, because falling through
  would run a download that does not correspond to the source sitting right there.
- **The checkout outranks the package's own `bin/`.** In a checkout that directory holds committed
  shims, and a stray `npm install` inside it can drop downloaded binaries there; the order is what
  stops those from shadowing a local build.

The last two rungs require a current install manifest (`packageVersion` and platform key matching,
every file it claims present); the first two do not, because a manifest certifies what the
*installer* put down, and neither an override nor a local build was installed.

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

As-built note (D-055, `[SPEC]` correction). "A locally installed `@13w/memory`" above was
ambiguous, and T19-03 silently resolved it toward the narrower of its two readings: tier 1's
`require.resolve` preflight anchored at exactly one path, `${CLAUDE_PROJECT_DIR}/node_modules`,
and its own doc comment explicitly rejected checking any machine-global npm directory ("not `npm
root -g`... `@13w/memory` isn't shaped as a global-install tool anyway") — an unflagged product
assumption, unlike the sibling T19-02 ambiguity in the same group-19 card, which got an explicit
owner flag and became D-042. Confirmed by the owner in a live dogfooding session (D-055): "locally
installed" for an npm package, everywhere in this project's usage, means installed on the machine
(`--global`), not scoped to whichever project happens to be open — the marketplace-installed
plugin never installs the server itself (`[FIXED]`: "plugin packaging must not modify users'
repositories"), so a real user's only route to a network-free cold start, usable from every
project, is `npm install --global @13w/memory` once. `tier1()` now tries two anchors in order:
`${CLAUDE_PROJECT_DIR}/node_modules` first (kept — an explicit local override/pin for monorepo
vendoring or version pinning, "local beats global" the same way most npm CLI tools already let a
project devDependency shadow a global install), then this machine's global npm modules directory,
computed synchronously from `process.execPath` (`npmGlobalNodeModules()`:
`<node-install>/lib/node_modules` on POSIX, `<node-install>/node_modules` on win32 — npm's own
default-prefix convention absent a custom `.npmrc` prefix) rather than shelling out to `npm root
-g`/`npm config get prefix` (100-300 ms subprocess spawn, too slow for this tier) — a custom global
prefix remains a known, accepted miss, same trade-off the code already made, just now aimed at the
right target instead of skipping it. `pnpm link --global`/`npm link` (global-only, no project-level
second link step) do **not** land here or anywhere in this resolution chain — confirmed by reading
both `tier1()`'s explicit `paths` anchoring and `npm/memory/src/resolve.js`'s
`createRequire(fromFile).resolve(...)` directory walk, neither of which ever reaches a package
manager's global link store; only a real `npm install --global`-style install (or the file
literally present under the computed default prefix) resolves. Test-only override
`LOCAL_RAG_TEST_GLOBAL_NODE_MODULES` lets `plugin/test/mcp-launcher-tiers.test.js` (and
`mcp-cold-start.test.js`'s two launcher-spawning tests, updated to force a miss so they keep
testing tier 2/the real-binary handoff specifically) exercise this deterministically — this
machine's own *real* global npm directory lives under the user's home directory, which CLAUDE.md
forbids tests depending on.

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
- resolution of an **executable**, not an npm package `[FIXED, ADR-0013]`: an explicit
  `LOCAL_RAG_BIN_DIR` override first, then the elements of `PATH` in order, then a list of
  well-known global-bin directories. The plugin **never downloads anything** — obtaining the
  binaries is the npm package's job (§1) and never the plugin's, the same way the recall RPC never
  spawns a daemon (11 §3.2). Package-manager layouts stop mattering once a file rather than a
  package is resolved; the failure axes that replace them are lifecycle-script suppression
  (`npm ci --ignore-scripts`, pnpm's default policy for dependency scripts, Yarn PnP) and a `PATH`
  a GUI-launched client does not inherit — which is exactly why the well-known-directories rung and
  the override both exist;
- a clear, actionable error when **the server is not installed** `[FIXED, ADR-0013]`, stated
  per channel because their contracts differ. MCP: stdout stays **byte-empty** — it is the
  JSON-RPC stream (11 §4) and any stray write corrupts framing — the diagnostic goes to stderr and
  names both the install command and `LOCAL_RAG_BIN_DIR`, and the process exits non-zero so the
  client shows a failed server rather than a silent one. Hooks: exit 0 always (11 §3.1 `[FIXED]`,
  unchanged), `SessionStart` says so through `additionalContext` (11 §3.2), and the other six
  events stay silent;
- fully offline operation after `local-rag init --download-models` `[FIXED]`, with the
  install-time/runtime split made explicit: obtaining the binaries and the runtime needs the
  network once (12 §1); nothing after that does;
- checksum/manifest + atomic download, for the ONNX Runtime on the same terms as model weights
  (10 §5);
- the ONNX Runtime is an artifact of first run, installed beside the weights rather than bundled
  into a package `[FIXED, ADR-0013]` (10 §5).

Amendment note (T22-04, `[FIXED]` change under
[ADR-0013](../adr/0013-binary-delivery-via-release-assets.md)). Four of the six requirements above
are this ADR's; the first and the checksum one are untouched in substance. Two of them replace
requirements that had become unanswerable rather than merely wrong: there is no package layout to
resolve under, and no platform package that can be missing. The actionable-error requirement is
**strengthened**, not relaxed — it used to cover a missing sub-package and now covers the case a
real user actually hits, no server at all, with a stated contract on each channel instead of the
silent "MCP server not connected" that prompted this group. The resolution order itself is new
normative text: §1 points here for it, and `T22-12`/`T22-13` implement it. What is deliberately
*not* here: how the binaries are obtained (§1), what the download is trusted on (12 §1), and what
triggers an upgrade (§4).

As-built note (T22-03, `[SPEC]`, [ADR-0013](../adr/0013-binary-delivery-via-release-assets.md)).
The next note is superseded in the one part that names *where* the runtime lives, and kept as
history for everything else. `libonnxruntime` is no longer bundled into a platform package's
`bin/`: it becomes an artifact of first run, installed into the store beside the model weights by
the same verify-before-trust machinery those weights already use (10 §5), which is `T22-15`'s.

Everything else in that note still holds, and that is why it is kept rather than rewritten: the
resolution order at process start, the prohibition on ever falling through to `ort`'s own implicit
search (`D-028`, which showed it can hang the calling thread instead of erroring), the pinned URL
and SHA-256 per platform in `crates/xtask/src/dist_ort.rs::ORT_ASSETS`, the per-platform runtime
versions with the reason `darwin-x64` pins the older one, and the record of what was actually
verified end to end versus only structurally. The pinned table in particular is untouched — under
`latest` the product binaries lose the compiled-in-digest standard (ADR-0013 §Decision 2), and the
runtime deliberately does not.

One consequence inverts that note's last sentence and is worth naming here. The stated reason
`win32` ships no bundled runtime is `cargo-zigbuild`, which has no role at all after this change;
a Windows ONNX Runtime therefore becomes obtainable. `D-108` separately established that Windows
*product* binaries have been shipping all along — release `0.0.0` carries all three. Acting on
either is `T22-15`'s, not this note's.

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

A new `local-rag` binary on disk `[SPEC, ADR-0013]` → next proxy spawn detects version mismatch →
`SHUTDOWN_REQUEST` to old daemon (02 §4.2) → old daemon drains and exits → new daemon migrates
(if needed) → serves. Spool format compatibility is part of the handshake (11 §4): a daemon MUST
be able to import all spool `format_version`s ≤ its own.

The daemon binary MUST sit beside the proxy that spawns it `[SPEC, ADR-0013]`. That co-location
is what makes "the version comes from whichever binary is found next to this proxy" a definition
rather than a coincidence, and it is the reason the upgrade needs no separate detect-and-spawn
step. Under the previous channel it fell out of how npm laid packages out; it is now a
requirement on the layout, bought structurally by the installer placing all product binaries in
one flat directory (`T22-10`).

Amendment note (T22-04, `[SPEC]` change under
[ADR-0013](../adr/0013-binary-delivery-via-release-assets.md)). Only the trigger and the
co-location clause are this ADR's. The mechanism is unchanged in every other respect — the
handshake retry loop, `SHUTDOWN_REQUEST`, the drain, the migration, and `MAX_UPGRADE_ROUNDS = 2`
all behave exactly as the T15-02 note below describes. What changed is what makes the on-disk
binary new: npm no longer swaps a package directory, the installer replaces the files in place
(§1), so the trigger is stated in terms of the binary rather than the package manager. The next
note is otherwise still accurate and is kept.

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
