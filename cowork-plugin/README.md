# memory-cowork — the local-rag Memory MCP in Cowork

Packages the local-rag MCP server (`local-rag-proxy` → `local-rag` daemon) plus a skill that
teaches Claude when to use it, so durable project memory and code search work from Claude
Cowork, where a repository's `.mcp.json` is not read and `plugin/` (the Claude Code plugin)
does not start: its `.mcp.json` runs `node`, which is not on the bare PATH Cowork spawns with.

No hooks: observation capture stays with the Claude Code plugin in `plugin/`. This plugin only
serves the MCP tools.

## Install in Cowork

```sh
npm i -g @13w/memory                  # once — the plugin never downloads anything
cd cowork-plugin
zip -r /tmp/memory-cowork.zip .       # '.' — not '*', or .claude-plugin/ and .mcp.json are dropped
```

Cowork tab → **Customize → Plugins** → upload `/tmp/memory-cowork.zip` → restart Claude so the
server starts. Ask "what do we remember about X?" to confirm it answers.

## Point it at a repository

Cowork starts **one** server for the whole app, in the app's own directory, while every Cowork
session works in its own folders. The daemon picks the repository for each call in this
order:

1. **the launch worktree** — the directory the proxy started in, if it is a repository the
   daemon knows. When it resolves, it always wins, and the per-call argument is ignored.
2. **the per-call `worktree` argument** — the absolute path Claude passes on every tool call
   (the skill tells it to pass the session's working folder). It is used only when the launch
   worktree does not resolve. `.mcp.json` sets `LOCAL_RAG_PER_CALL_WORKTREE=1`, which adds
   this optional argument to every tool. Without that variable (plain Claude Code) the proxy
   changes nothing.
3. **global** — neither resolves: memory tools work in `global` scope and code tools report
   no worktree. This is never an error.

A per-call path resolves when it is inside a git checkout the daemon has registered
(`local-rag project add <path>`, or any repository a Claude Code session with `plugin/` has
already used).

The launch worktree comes from the launcher, which `cd`s into a repository first, found in this
order:

1. `LOCAL_RAG_WORKTREE` in the `env` of `.mcp.json`
2. the working directory, when it is inside a git checkout (Claude Code, `claude --plugin-dir`)
3. the single line in `~/.config/local-rag/cowork-root` (`~` allowed)

Installed in Cowork, step 3 is the one that matters:

```sh
mkdir -p ~/.config/local-rag && echo /opt/legatics.com/firefly > ~/.config/local-rag/cowork-root
```

Change the line and restart Claude to switch repositories. A pinned repository wins over every
per-call argument, so leave all three unset if different Cowork sessions work in different
repositories, and let the per-call argument route them.

## Finding the binaries

Same rungs as `plugin/bin/local-rag-mcp-launcher.js`: `LOCAL_RAG_BIN_DIR`, `PATH`, the
directory of `node` if on PATH, `PNPM_HOME`, `/opt/homebrew/bin`, `/usr/local/bin`,
`~/.local/bin`, pnpm, bun, volta, `~/.npm-global/bin` — plus the newest nvm / fnm node `bin`,
because a global install under a version manager lives there and the bare PATH cannot derive
it. A directory counts only when `local-rag` sits beside `local-rag-proxy`.

## Troubleshooting

The launcher never writes to stdout (it belongs to the JSON-RPC stream); diagnostics go to
stderr and appear in the plugin's error output. Reproduce Cowork's environment:

```sh
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}' \
  '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
| (cd / && env -i PATH=/usr/bin:/bin:/usr/sbin:/sbin HOME="$HOME" \
  /bin/sh "$PWD/cowork-plugin/launcher/memory-mcp.sh")
```

Expect 21 tools. `LOCAL_RAG_DEBUG=1` prints every directory searched for the binaries.

- *"the memory server is not installed"* — `npm i -g @13w/memory`, or set `LOCAL_RAG_BIN_DIR`.
- *"no repository configured"* — expected when routing per call; otherwise write
  `~/.config/local-rag/cowork-root`, see above.
- `remember` answers with scope `global` / `degraded` — no launch worktree, and the call's
  `worktree` was missing, not absolute, or not a registered repository
  (`local-rag project add <path>`); or the repository is not indexed yet (`local-rag` indexes
  it in the background once the daemon sees it).
