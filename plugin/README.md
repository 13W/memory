# memory (Claude Code plugin)

Registers the `memory` MCP server (persistent memory, hybrid code search) and the seven
ingestion hook events (`SessionStart`, `UserPromptSubmit`, `PostToolUse`, `PostToolUseFailure`,
`Stop`, `SubagentStop`, `SessionEnd`) that durably capture observations for local-rag's memory
pipeline.

Both the hook writer and the MCP server resolve, in order: a locally installed `@13w/memory`
npm package, a cached native binary path under `${CLAUDE_PLUGIN_DATA}`, and — only as a last
resort — `npx --yes --package=@13w/memory`, which resolves the platform-specific native binaries
published by the `@13w/memory` npm package (see `npm/memory/` in this repository). This plugin
ships one small launcher script (`bin/local-rag-mcp-launcher.js`, the `.mcp.json` entry point) plus
the hook shell fallback — every native binary itself still comes from the separately-published
`@13w/memory` package, never bundled here.

The hook command execs a cached direct symlink to the resolved native binary under
`${CLAUDE_PLUGIN_DATA}` once any run has populated it, so every subsequent hook invocation skips
Node/npx entirely, staying within spec 13 §1's exec-fast (<50ms cold) budget (measured p50 ≈ 5ms /
p95 ≈ 6ms). The MCP server cannot skip Node the same way — `.mcp.json`'s `command` is a single
statically-configured process with no shell fallback chaining, unlike `hooks.json` — but its own
launcher-only overhead on the cached path stays under a p95 < 100ms budget (T19-03,
`docs/specification/13-distribution-and-migrations.md` §1/§2), one to two orders of magnitude
better than an uncached `npx` hit against the registry.

This plugin never writes into the project it is enabled on — hooks durably append to local-rag's
own store (outside the project directory) and never touch `.claude/rules/` or any other
project-local file.

See `docs/specification/11-interfaces.md` §3 and `docs/specification/13-distribution-and-migrations.md`
§1-2 in this repository for the full normative behavior.
