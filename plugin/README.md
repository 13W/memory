# local-rag (Claude Code plugin)

Registers the `local-rag` MCP server (persistent memory, hybrid code search) and the seven
ingestion hook events (`SessionStart`, `UserPromptSubmit`, `PostToolUse`, `PostToolUseFailure`,
`Stop`, `SubagentStop`, `SessionEnd`) that durably capture observations for local-rag's memory
pipeline.

Both the MCP server and the hook writer are invoked through `npx --package=@13w/local-rag`, which
resolves the platform-specific native binaries published by the `@13w/local-rag` npm package (see
`npm/local-rag/` in this repository). This plugin ships no compiled code itself — only
configuration.

The hook command caches a direct symlink to the resolved native binary under
`${CLAUDE_PLUGIN_DATA}` on its first run, so every subsequent hook invocation execs the binary
directly (no Node/npx startup cost) to stay within spec 13 §1's exec-fast (<50ms cold) budget.

This plugin never writes into the project it is enabled on — hooks durably append to local-rag's
own store (outside the project directory) and never touch `.claude/rules/` or any other
project-local file.

See `docs/specification/11-interfaces.md` §3 and `docs/specification/13-distribution-and-migrations.md`
§1-2 in the local-rag-v2 repository for the full normative behavior.
