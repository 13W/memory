# @13w/memory

Thin npm launcher for `local-rag` — a local, co-located MCP service for Claude Code (persistent
memory, hybrid semantic code search, spool-only observation capture).

This package contains **no native code**. On install, npm pulls in exactly one of the five
platform packages listed in `optionalDependencies` (matched by `os`/`cpu`), and the `local-rag-mcp`
binary this package exposes resolves that platform package at run time and execs the native
`local-rag-proxy` binary from it, forwarding stdio and signals unchanged.

If your platform is unsupported, or the matching platform package failed to install, running
`local-rag-mcp` prints an actionable diagnostic instead of a stack trace.

See `docs/specification/13-distribution-and-migrations.md` in this repository for the full
packaging specification.
