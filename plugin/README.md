# memory (Claude Code plugin)

Registers the `memory` MCP server (persistent memory, hybrid code search), the seven
ingestion hook events (`SessionStart`, `UserPromptSubmit`, `PostToolUse`, `PostToolUseFailure`,
`Stop`, `SubagentStop`, `SessionEnd`) that durably capture observations for local-rag's memory
pipeline, and a skill (`skills/memory-first-workflow/`) that routes to the right tool instead of
a built-in equivalent.

Neither channel downloads anything, and neither needs Node on the fast path. Both **resolve an
executable by name**, in this order: `LOCAL_RAG_BIN_DIR` if you set it, then every entry of `PATH`
in order, then a short list of well-known global-bin directories. That last rung is there for one
real case — a GUI-launched client inherits launchd's `PATH`, not your shell's, so a perfectly good
global install would otherwise be invisible. The native binaries themselves come from the
separately-published `@13w/memory` package (`npm i -g @13w/memory`, or see `npm/memory/` in this
repository); nothing is ever bundled here.

The plugin ships two small files to do it: `bin/local-rag-mcp-launcher.js` (the `.mcp.json` entry
point) and `bin/local-rag-resolve-hook.sh`, a POSIX shell script on built-ins alone that the seven
hook commands invoke. The hook path is measured whole — the shell fork, the directory walk, the
`exec` and the native binary's own run — at p50 ≈ 13 ms / p95 ≈ 14 ms on a hit and ≈ 10 ms on a
complete miss, inside spec 13 §1's exec-fast (<50 ms cold) budget. The MCP server cannot skip Node
the same way, because `.mcp.json`'s `command` is a single statically-configured process with no
shell fallback chaining; its launcher-only overhead is p50 ≈ 40 ms / p95 ≈ 44 ms, inside its own
p95 < 100 ms budget.

If the server is not installed, the plugin says so instead of failing silently: `SessionStart`
states it through `additionalContext`, the other six hook events stay quiet, every one of them
still exits 0, and the MCP launcher writes the one command that fixes it to stderr while leaving
stdout — the JSON-RPC stream — byte-empty.

`skills/memory-first-workflow/SKILL.md` (T19-04) is auto-discovered from its default-location
directory — no `plugin.json` entry names it, the same convention `hooks/hooks.json`/`.mcp.json`
already use. Its `description` stays in context for every session with this plugin enabled, so
Claude can route to `search_code`/`get_file_context`/`project_overview`/`recall`/`remember`
instead of a built-in equivalent without the model having to invoke the skill explicitly; the
skill can still be invoked by name (`/memory:memory-first-workflow`) if needed. This is a
caller-facing channel only — it never touches any project's own `CLAUDE.md`/`AGENTS.md`.

This plugin never writes into the project it is enabled on — hooks durably append to local-rag's
own store (outside the project directory) and never touch `.claude/rules/` or any other
project-local file.

See `docs/specification/11-interfaces.md` §3 and `docs/specification/13-distribution-and-migrations.md`
§1-2 in this repository for the full normative behavior.
