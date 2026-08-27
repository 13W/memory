# @13w/memory

npm package for `local-rag` — a local, co-located MCP service for Claude Code (persistent
memory, hybrid semantic code search, spool-only observation capture).

Its job is to **obtain and expose** the native binaries, not to contain them. Installing the
package fetches the release assets for your platform from the project's own GitHub releases,
verifies each against the checksum published in the same release, and puts `local-rag`,
`local-rag-proxy`, `local-rag-hook` and `local-rag-tui` side by side in one directory. They stay
side by side deliberately: the proxy finds its daemon by looking next to itself.

The commands then resolve an executable, in this order:

1. `LOCAL_RAG_BIN_DIR`, if you set it — a directory of prebuilt binaries that wins over
   everything and downloads nothing. This is the supported route for an air-gapped or mirrored
   install.
2. A source checkout, when the package is running from inside one (`pnpm link --global` from a
   clone). Then `target/release` is used, and nothing is downloaded — the local build is the
   point.
3. The binaries this package installed.

If none of those holds a complete set, the commands print what is missing and the one command
that fixes it, rather than a stack trace.

Verification is against the checksum sidecar published beside each asset. That defends against
corruption in transit and tampering on the wire; it does not defend against a compromised
release, because whoever can publish an asset can publish its digest.

See `docs/specification/13-distribution-and-migrations.md` and
`docs/adr/0013-binary-delivery-via-release-assets.md` in this repository for the full
specification and the reasoning behind it.
