# ADR-0013: Binary delivery via release assets and executable-based resolution

## Status

Accepted — 2026-08-26. **Amended 2026-08-26 (D-108)** — see "Amendment: Windows binaries"
below: this record's Consequences claimed no Windows binaries exist, which is false. The four
decisions and their reasoning are untouched.

Opens a new scope by explicit owner decision: product binaries stop being delivered as
per-platform npm packages and start being taken from the GitHub release the CI already
produces, and the Claude Code plugin stops resolving an npm *package* in favour of resolving an
*executable*. Realized by group 22 with gate `G22`
([groups/22](../implementation-plan/groups/22-binary-delivery-and-resolution.md)), delivered by
`T22-01`…`T22-17`, outside the closed `T00–T17` queue. No earlier gate is reopened.

This ADR is the instrument that makes the `[FIXED]` amendments of `T22-02`…`T22-04` legal, and
it settles the six deviations registered by `T22-00`
([DEVIATIONS](../implementation-plan/DEVIATIONS.md)): `D-102` the delivery channel itself,
`D-103` the removal of the `npx` tier and the known-path cache, `D-104` the ONNX Runtime,
`D-105` the upgrade trigger, `D-106` the network-egress statement that does not exist today,
`D-107` the resolution and error contracts.

**Relationship to [ADR-0005](0005-model-delivery.md) — it is not a supersession, and the
distinction is load-bearing.** ADR-0005 Decision 1 says, of the ONNX Runtime shared library,
"*Which* library ships in each npm platform package is **T17-03**'s … — this ADR decides the
binding, not the packaging", and its Consequences say "Until T17-03 bundles it per platform,
`OnnxEmbedder::open` fails with a typed, actionable error naming `ORT_DYLIB_PATH`". So the
packaging question was deliberately left open and addressed to a task, not decided. Decision 4
below answers it differently from the answer T17-03 recorded; what is superseded is **T17-03's
as-built note** in spec 13 §2, not any ADR-0005 decision. ADR-0005's `ort`/`load-dynamic`
binding, its pinned-digest installer, and `D-028`'s prohibition on `ort`'s own implicit search
all stay in force verbatim.

What Decision 2 *does* do to ADR-0005 is narrower and must be said plainly: it **restricts the
scope of that ADR's Decision 6 standard** — "verification uses the compiled-in catalog, so a
tampered manifest cannot talk the installer into accepting different bytes". That standard
continues to hold, unchanged, for model weights and for the ONNX Runtime, because both are
pinned by URL and digest in code. It cannot hold for product binaries once the owner chose to
track `latest`, because the digest of a future release cannot be compiled into today's package.
Decision 2 states what replaces it and what that costs.

`[FIXED]` text amended under this ADR:
[01 §1](../specification/01-overview.md) (the distribution sentence),
[13 §1](../specification/13-distribution-and-migrations.md) (the channel, the build tooling and the
per-asset binary list), [10 §5](../specification/10-models-and-embeddings.md) (the ONNX Runtime
becomes a first-run artifact) — all by `T22-03`;
[13 §2](../specification/13-distribution-and-migrations.md) (the launcher requirements: an
executable is resolved rather than a package, and what happens when the server is not installed),
[13 §4](../specification/13-distribution-and-migrations.md) (the upgrade trigger and the
co-location invariant), [12 §1](../specification/12-security-privacy.md) (the network exemption
for binaries and the runtime, with its trust model),
[11 §3.2](../specification/11-interfaces.md) (the hook's not-installed case) — all by `T22-04`.
Each amendment points back here.

Convention (`docs/adr/NNNN-title.md`, Nygard sections, English) is
[ADR-0001](0001-first-release-language-set.md)'s.

## Context

The complaint that started this was narrow: the plugin reaches for `npx --yes
--package=@13w/memory` on every cold start where local resolution failed, which contacts the
registry, and offline or behind a proxy degrades silently into "MCP server not connected". The
audit that followed found something larger — the delivery channel the specification pins as
`[FIXED]` does not exist, while the channel it never mentions is fully automated.

| | npm platform packages | GitHub release assets |
| --- | --- | --- |
| Status in the spec | `[FIXED]` — [01 §1](../specification/01-overview.md), [13 §1](../specification/13-distribution-and-migrations.md), `CLAUDE.md` | not mentioned |
| How artifacts are produced | by hand: `cargo build --release`, `cargo zigbuild`, `cargo xtask dist-ort` | `cargo-dist` + `release.yml`, on a tag |
| `bin/` in the repository | `.gitignore:43-47`; no script, xtask or CI step fills it | — |
| Ever published | **no** — `npm publish`, `NPM_TOKEN`, `registry.npmjs` match nothing outside `publishConfig` | release `0.0.0`, 34 assets |
| Versioning | all six packages at `0.0.0`, no bump tooling | tag is the version |
| Platform coverage | `win32-x64` holds no binaries at all (`D-029`, `D-033`); `local-rag-tui` is in none of the five, though the `local-rag-dashboard` bin is declared | all five targets |

So this is not a redesign of delivery. It is a decision to stop maintaining a channel that was
never built and to start using the one that already works.

**The trap that only a measurement found.** Because ADR-0005 chose `ort`/`load-dynamic`,
`crates/models/src/onnx.rs::bundled_ort_dylib_path` looks for `libonnxruntime` beside the
running executable. Reading the `dist-manifest.json` of release `0.0.0` shows every archive
holds exactly one member — the executable. The library exists **only** inside the platform
packages: 38 313 360 bytes on `darwin-arm64`, 28 600 008 on `darwin-x64`, against a
`local-rag` release archive of 6 060 888 bytes. Deleting the packages without moving the
runtime first would leave indexing broken on every clean machine, and would do it as a runtime
error rather than an install error.

**The direction was already chosen once.** `D-055` established that "a locally installed
package" means installed on the machine, and that a real user's only route to a network-free
cold start is `npm install --global` once, from anywhere. Its disposition explicitly placed the
second and third launcher tiers out of scope. This ADR finishes that line of reasoning and, in
doing so, supersedes that scope boundary by reference; the `D-055` row itself is not rewritten.

**The failure axis changes.** Spec 13 §2 requires "resolution under pnpm / npm / yarn layouts
(hoisting differences)". Once an executable is resolved rather than a package, hoisting stops
mattering entirely — and a different axis appears that is written down nowhere: lifecycle
scripts can be suppressed. `npm ci --ignore-scripts`, pnpm 10's default of not running
dependency lifecycle scripts, and Yarn PnP all produce an installed package whose binaries were
never fetched. That is why the installer cannot rely on `postinstall` alone.

**Two mechanics make the `latest` channel implementable without the GitHub API.**
`/releases/latest/download/<asset>` answers `302` whose `Location` names the resolved tag, so
the tag is recoverable before any payload moves and no rate-limited API call is needed. The
checksum sidecars are coreutils binary-mode lines of the form `<64 hex> *<filename>`.

## Decision

**Ship product binaries as GitHub release assets tracked at `latest`, keep exactly one npm
package whose job is to install and expose them, resolve everything afterwards by finding an
executable rather than a package, and move the ONNX Runtime into the same first-run installer
that already delivers model weights.**

### 1. Product binaries are release assets; npm carries one installer package

The five `@13w/memory-<platform>` packages are deleted. `@13w/memory` remains, and its job
changes from *selecting* an already-present platform package to *obtaining* the binaries: a
`postinstall` that downloads and verifies, plus a lazy path that heals on first use when the
lifecycle script did not run.

Three consequences of that are decisions in their own right, not details. The package acquires
a `scripts` key, which it deliberately did not have — its zero-dependency, zero-script shape
was a property T17-01 chose, and it is being spent knowingly; the zero-*dependency* half is
kept. npm's `os`/`cpu` auto-selection disappears and is replaced by a platform key computed at
runtime, which is why the supported set has to be enumerated in code rather than by the
registry. And air-gapped installation, whose only previous answer was "vendor the platform
package", now needs an explicit one: a documented environment variable naming a directory of
prebuilt binaries, which wins over everything and never downloads.

### 2. The channel is `latest`, and "verified" means less here than in ADR-0005

The owner chose to track `latest` so that binaries can be rebuilt without republishing to npm.
That choice is honoured, and its two halves are recorded separately.

*What makes it work.* `postinstall` re-runs on every `npm install --global` and every
`npm update --global`, re-resolves `latest`, and re-downloads when the tag moved — so
"update the package, get the new services" holds by construction. The install manifest records
the version of the wrapper that wrote it; when the wrapper changes and the manifest does not
match, the installation counts as absent. That makes the pair "new wrapper, old native code"
unrepresentable rather than merely unlikely, which matters because the wrapper↔binary contract
was never semver-safe.

*What it costs, stated without euphemism.* ADR-0005 argued that "'Checksum-verified' only means
something if the expected digest is known *before* the transfer — otherwise the installer would
be certifying whatever it received." Under `latest` that standard is unreachable: no digest of
an unreleased artifact can be compiled in. Verification is therefore against the `.sha256`
sidecar published **in the same release** as the asset. That defends against corruption in
transit and against tampering on the wire; it does **not** defend against a compromised
release, because an attacker who can publish the asset can publish its digest. Saying
"checksum-verified" without that sentence would be false advertising of a security property.
The mitigation is GitHub artifact attestation, verified in addition to the digest; the residual
risk after that is trust in the release pipeline itself, and it is accepted deliberately.

*Direction of travel.* ADR-0005 Decision 7 established that fetching public bytes **in** is not
what `data_policy` governs, which is about repository content going **out**. The same argument
covers binaries and the runtime, and it is written into
[12 §1](../specification/12-security-privacy.md) explicitly rather than assumed by analogy —
the existing exemption names model assets and nothing else.

### 3. Resolution finds an executable, not a package

Both the MCP launcher and the hook path resolve a binary by name: an explicit override
directory first, then the entries of `PATH` scanned one by one, then a list of well-known
global-bin directories, then failure. Nothing consults `node_modules`, and nothing runs `npx`.

This is what makes every installation route work without per-route code — a global npm install,
`pnpm link --global` from a checkout, bun, volta — and it retires `D-055`'s accepted miss, in
which a custom npm prefix silently resolved nothing. The costs are real and are accepted: an
earlier `PATH` entry can shadow the intended binary, which is ordinary `PATH` semantics but is
now load-bearing; and a GUI-launched client inherits a login-daemon `PATH` rather than the
shell's, which is precisely why the well-known-directories rung and the override exist rather
than trusting `PATH` alone.

Two tiers disappear with it. The `npx` last resort goes, which is the entire point. The
known-path cache under the plugin's data directory goes too — after the installer places native
binaries on `PATH` there is no writer left for it, and a cached symlink to a *path* would aim the
daemon lookup at a stale sibling after an in-place upgrade. Its one genuine virtue, surviving a
`PATH` the client cannot see, is served deterministically by the override variable instead.

### 4. ONNX Runtime becomes a first-run artifact

The runtime is installed by `local-rag` itself on first use, into the store, beside the model
weights — the class of artifact it has always resembled. Resolution order becomes: the explicit
`ORT_DYLIB_PATH` override, then the copy installed in the store, then a library sitting beside
the executable, then a typed error. `D-028`'s rule that the process must never fall through to
`ort`'s own implicit search is preserved word for word, because that search can hang instead of
failing.

The pinned table in `crates/xtask/src/dist_ort.rs` remains the single source of truth for URLs
and SHA-256 digests, and the download reuses the existing fetcher seam and the
`.part` → fsync → verify → rename → marker ordering of the weights installer. **Here ADR-0005's
standard survives intact** — the digests are compiled in, exactly as Decision 6 requires. The
contrast with Decision 2 is deliberate and worth stating: the standard is relaxed for one
artifact class because the owner chose a moving channel for it, and for no other.

## Consequences

- **The dead channel stops costing maintenance.** Five package manifests, their manual build
  and copy ritual, the `optionalDependencies` lockstep and the platform-package resolution
  branch all go away. What replaces them is one installer with a real test surface.
- **First installation now requires the network, and installs stop being reproducible by
  construction.** Under `latest`, two installs of the same npm version can legitimately produce
  different binaries. This is the price of the owner's choice; the manifest records the resolved
  tag so that the difference is at least diagnosable, and the version is surfaced by `doctor`.
- **The security property is weaker than the one ADR-0005 established, and the ADR says so.**
  See Decision 2. Anyone reading only the phrase "checksum-verified" would over-trust the
  channel; that phrase never appears unqualified in the amended specification text.
- **"The plugin never downloads anything" becomes testable.** It stops being a comment and
  becomes an assertion: both entry points run with poisoned `npx`/`npm`/`curl` on `PATH` that
  record their own invocation, and the test fails if any of them ran.
- **Offline and air-gapped use needs a deliberate step.** Previously a vendored platform package
  covered it by accident. Now it is an explicit directory override, documented as such.
- **This does not decide publication.** Whether `@13w/memory` is finally pushed to the public
  registry stays an owner decision, unchanged by this ADR; the repository still has no publish
  step and no version-bump tooling.
- **This does not fix win32.** `D-029` and `D-033` still stand: no Windows binaries exist to
  deliver, so the Windows path is designed here and unproven, and the ADR does not claim
  otherwise.
- **This does not change the ONNX Runtime version.** Decision 4 moves where the library comes
  from, not which one; the pinned table is carried over as-is, including the older pin that
  `darwin-x64` needs because upstream stopped shipping Intel-Mac builds.

## Amendment: Windows binaries (2026-08-26, D-108)

**Windows binaries exist and already ship.** Release `0.0.0` carries all three product binaries
for `x86_64-pc-windows-msvc`; `dist-manifest.json` gives each of them a single `executable`
member and records a dedicated `build:local:x86_64-pc-windows-msvc` build environment. The
Consequences section below says the opposite. It is left standing, because an ADR is evidence of
what was decided and on what basis — including where that basis was wrong — and this amendment is
how the record is corrected.

### Why the error happened

`D-029` and `D-033` were read and their wording carried over without checking how they ended.
`D-033` is **resolved**: the first real `windows-latest` runner found that `win32-x64` did not
compile at all, because `crates/local-rag-hook/src/recall.rs` imported
`std::os::unix::net::UnixStream` unconditionally; the fix `#[cfg(unix)]`-gated the IPC transport
and made the Windows branches a typed immediate refusal. Windows has built and shipped ever since.

The empty `npm/memory-win32-x64/bin/` has a different cause entirely: `cargo-zigbuild` cannot
target Windows, and the by-hand ritual that filled the platform packages ran through it. CI never
used `cargo-zigbuild` — it appears in neither `.github/workflows/release.yml` nor
`dist-workspace.toml`.

### What this invalidates in the text above

Only the Consequences bullet beginning "**This does not fix win32.**" Its factual claim is wrong
and its conclusion is backwards. The corrected statement, and it argues *for* Decision 1 rather
than qualifying it: **the release-asset channel already delivers Windows, and the platform-package
channel structurally could not.** The abandoned channel was not merely unimplemented on Windows —
it was unimplementable there from this project's own build machine.

What is genuinely unfinished on Windows, stated precisely so the gap is not overclaimed in either
direction: the daemon's IPC is a typed refusal rather than an implementation, with full
named-pipe IPC deferred to a future task by the owner's decision inside `D-033`; and `ORT_ASSETS`
carries no `win32` entry, whose recorded reason is again `cargo-zigbuild`. That reason dissolves
under this ADR, so a Windows ONNX Runtime becomes obtainable — an input for the card that moves
ORT into the first-run installer, not a claim made here.

No decision, alternative or consequence other than that one bullet is affected.

## Amendment: attestations are produced, not verified (2026-08-31, D-110)

Decision 2 says, of the digest's limits, that "the mitigation is GitHub artifact attestation,
verified in addition to the digest". It reads as part of the decision, and it is not what shipped.
The record is corrected here rather than left to be discovered by the next person comparing this
text with `npm/memory/`.

**What shipped.** The producing half: `github-attestations = true` in `dist-workspace.toml`, which
regenerates `.github/workflows/release.yml` with `attestations: write` / `id-token: write` and an
`actions/attest` step, and every tagged release now emits attestations.

**What did not, and why it is a decision rather than an omission.** The installer does not verify
them. Doing so needs either the `gh` CLI or a sigstore library inside `npm/memory`, and that is a
direct reversal of the stance group 22 built and then pinned with a test: `archive.js` and
`http.js` use Node built-ins only, and `plugin/test/no-network.test.js` asserts that neither
shipped file can reach the network without an external command. Buying attestation verification
means paying that stance, and the owner's decision of 2026-08-28 was not to.

**What the residual risk actually is, stated so it is not overclaimed in either direction.** It is
exactly what Decision 2 already described *before* naming the mitigation: verification is against a
sidecar published in the same release as the asset, which defends against corruption in transit and
tampering on the wire, and not against a compromised release. The attestations exist and a reader
can check one by hand with `gh attestation verify`, which `cargo-dist` prints into the release body
— so the property is available to a human, just not enforced by the installer.

**What this invalidates in the text above.** One clause of Decision 2: "verified in addition to the
digest" describes an intention, not the as-built. Read it as "produced in addition to the digest,
and verifiable by hand". No other decision, alternative or consequence is affected; `13 §1`'s
as-built note already carries the same statement for the specification side.

## Alternatives rejected

- **Pin the asset to the package version** — `@13w/memory@X.Y.Z` fetching the release tagged
  `X.Y.Z`, with digests baked into the npm tarball at publish time. This is the true analogue of
  ADR-0005's compiled-in pin and would have preserved its standard unchanged. Rejected by the
  owner in favour of `latest`, so that binaries can be rebuilt without a republish. Recorded
  here because it is the option that would have to be revisited if the residual risk in
  Decision 2 ever becomes unacceptable.
- **Let the MCP entry point be the binary itself**, with the client performing the `PATH`
  lookup. It removes Node from the MCP start path entirely, which is the largest single cost
  there. Rejected because it also removes the well-known-directories recovery that exists for
  the GUI-`PATH` case, and turns "not installed" — the state this whole group is trying to make
  legible — into an opaque spawn failure with no actionable message.
- **Keep the platform packages as an offline fallback.** Rejected because it preserves the
  manual build-and-copy pipeline and a second resolution branch permanently, in order to serve a
  case that an explicit directory override serves better and cheaply.
- **Implement the `[FIXED]` channel for real** rather than amending it. This is the option the
  specification currently describes, and it was weighed seriously. It requires version-bump
  tooling, a publish step, and roughly 28–38 MB of ONNX Runtime inside each of five packages —
  none of which exists, and the last of which is the plausible reason the packages were never
  published in the first place.
- **Shell out to the system archiver** so the existing `.tar.xz` release format could be kept.
  Rejected on the project's own precedent: `crates/xtask/src/dist_ort.rs` shells out to `tar`
  and says in its own module documentation that this is acceptable for a manually invoked
  development tool, not for something that must work offline in production. On minimal Linux
  images the extraction simply fails for want of an `xz` binary.
