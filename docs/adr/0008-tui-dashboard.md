# ADR-0008: TUI dashboard as a new post-v0 user-facing surface

## Status

Accepted — 2026-08-06.

Introduces new scope, not present anywhere in `idea.md` rev 6, `docs/specification/`,
or `docs/implementation-plan/` prior to this ADR — confirmed by a dedicated research
pass (this session) finding zero `[FIXED]`/`[SPEC]`/`[OPEN]` markers about any
visual UI (TUI or Web) anywhere in the normative documents; the only trace was one
vague phrase in [spec 02 §3.2](../specification/02-architecture.md) ("edited via
CLI/dashboard-equivalent") with no executable content behind it. This is therefore
not a `[OPEN]` question being closed (there was no registered question) and not a
`DEVIATIONS.md` entry (nothing in shipped behavior contradicts a norm) — it is new
product scope, decided by the owner in this session.

Minted post-`G17`, alongside `X-001`/`X-002`/`X-003`. Unlike those three, this ADR's
delivery mechanism is **not** a single `X-NNN` card. `TASK-TEMPLATE.md`'s own rule —
"if the description requires two independent results... the task must be split" —
means a six-screen dashboard with its own new crate, new daemon instrumentation, and
a new config-write primitive cannot honestly be one atomic `X-NNN` result the way
"add an `annotations` block" (`X-003`) or "migrate CLI arg parsing" (`X-002`) were.
The owner explicitly chose, over the lighter alternative (a flat sequence of atomic
`X-NNN` cards), to open a new numbered implementation-plan group —
[`groups/18-tui-dashboard.md`](../implementation-plan/groups/18-tui-dashboard.md),
gated by `G18` — the same shape as groups `00`–`17`, but explicitly outside the
closed `T00–T17` v0 queue. This is the first such group opened after `G17`; this ADR
is the product-decision record `TRACEABILITY.md`'s own closing sentence requires
before deviating from "post-`G17` work is `X-NNN` only."

## Context

The user asked for a comparative investigation (TUI vs. Web UI) for a dashboard
matching v1's feature set (live request log + per-tool stats, memory browser,
project/repository settings, server-wide settings), explicitly excluding v1's
playground (a dynamic-form runner for manually invoking MCP tools). Six research
passes in this session (three comparing TUI vs. Web UI, three establishing concrete
v2 architecture facts, one Plan-agent architectural synthesis verified against the
actual code) established:

- **v1 had only a Web UI** (Angular 21 + Fastify, port 7531, SSE) — no TUI ever
  existed in v1, so there is no "port the TUI" precedent to follow.
- **v2 has zero UI-adjacent dependencies or infrastructure today**: no
  `ratatui`/`crossterm`/`axum`/`tower-http` anywhere in `Cargo.lock`; the daemon
  (`crates/local-rag/src/daemon/`) speaks only Unix-domain-socket JSON-RPC (MCP), no
  HTTP.
- **All existing CLI commands already open `state.sqlite`/`cache.sqlite` directly**,
  bypassing the daemon (WAL + `busy_timeout=5000`, documented in
  `crates/local-rag/src/cli/mod.rs:23-31`), including memory mutations
  (`cli/memory.rs` calls `local_rag_store::memory::apply_*` directly, not through
  MCP) — establishing that a new client process may legally do the same for most
  screens, without requiring a running daemon.
- **The one exception is runtime state that only exists inside a live daemon
  process** — per-tool call counts and a recent-request log (v1's `toolStats`/
  `requestLog`) have no persistent counterpart in `state.sqlite`; nothing in the
  workspace instruments `daemon/mcp/dispatch.rs::route_tools_call` today (no
  `tracing`, no counters, no ring buffer, no `broadcast` channel anywhere in the
  workspace).
- **`crates/core/src/config/mod.rs`'s `Config` has no `Serialize`/`save`** — reading
  `config.toml` is implemented, writing it is not.
- Repository/worktree listing and per-repository settings
  (`crates/store/src/registry/settings.rs`, currently only `data_policy` is
  typed/used) already have a complete, reusable domain layer with no existing CLI
  or MCP editor at all — the TUI would be the first UI over it.

A prior turn in this session already produced a comparative recommendation (TUI over
Web UI: no new HTTP attack surface, no new external dependencies for most screens,
fits the terminal-native persona of local-rag's actual users — developers already
running Claude Code from a terminal) and the owner accepted it before asking for this
implementation plan.

## Decision

**local-rag v2 gains a TUI dashboard** (`ratatui` + `crossterm`, new crate
`crates/local-rag-tui`, new binary `local-rag-tui` distributed alongside
`local-rag`/`local-rag-proxy`/`local-rag-hook`) as a fourth user-facing surface,
alongside the MCP tool surface, hooks, and the CLI. Scope, fixed by this ADR:

- **In scope:** server status, live request log + per-tool call stats, memory
  browser (list/approve/reject/edit/retract/merge/evidence), repository & worktree
  browser, per-repository settings editor, global server-settings (`config.toml`)
  editor.
- **Explicitly out of scope:** a playground (v1's dynamic-form manual MCP tool
  runner) — the owner's own instruction excluded it; nothing in `groups/
  18-tui-dashboard.md` builds it, and adding it later would need its own product
  decision, not an implicit extension of this one.
- **Registration mechanism:** a new numbered implementation-plan group (`18`) with
  its own gate (`G18`), not a sequence of `X-NNN` cards — this ADR's own §Status
  explains why, and is the explicit product decision `TRACEABILITY.md`'s "only at
  an explicit product decision" bar requires for taking this heavier path instead of
  the lighter one already used by `X-001`–`X-003`.
- **Architecture, fixed by this ADR** (detailed task-by-task in
  `groups/18-tui-dashboard.md`):
  - A separate crate, not a `local-rag` subcommand — `crates/local-rag/src/lib.rs`
    exports only `pub mod daemon`; the CLI's own helpers are private to the binary
    target, and `ratatui`/`crossterm` have no reason to sit in the same binary that
    hosts the daemon.
  - Most screens read/write `state.sqlite`/`cache.sqlite` directly, the same
    architecturally-sanctioned pattern every CLI command already uses — no daemon
    required for repository/worktree/memory/repo-settings screens.
  - Live per-tool stats and the request log are the one exception: they can only
    come from a running daemon process. This requires new daemon-side
    instrumentation (an in-memory ring buffer + per-tool counters, exposed through
    two new JSON-RPC methods on the existing UDS transport — `admin/tail_calls`,
    `admin/tool_stats` — polled, not pushed, since `local_rag_protocol::handshake`
    is deliberately request/response-only).
  - The global-settings screen requires a new `Config::save`/TOML-serialization
    primitive that does not exist today.

## Consequences

- `docs/specification/11-interfaces.md` gains a new `§7 TUI dashboard`
  (`[SPEC surface]`, the same forward-sketch convention §6 CLI originally used
  before any CLI task shipped an as-built note) — not retrospective as-built prose,
  since no code exists yet; each `T18-NN` card appends its own as-built note there
  once implemented, the same convention `T15-07`/`T15-08`/`T16-02`/`T16-03`/`X-002`
  already established for §6.
- `docs/implementation-plan/TRACEABILITY.md`'s closing sentence on post-`G17` work
  ("для них создаются отдельные `X-NNN` только при явном продуктовом решении") is
  amended to name this ADR as the concrete precedent for the heavier alternative —
  a new numbered group — when a post-`G17` addition is not a single atomic result.
- `docs/implementation-plan/PROGRESS.md` gains a new `## 18 — TUI dashboard`
  section (task checklist, same shape as `## 00`–`## 17`, explicitly labeled
  post-v0/ADR-0008) and, once `G18` runs, a row in the Gate results table.
- No `idea.md` revision. Every prior post-`G17` product decision (`X-001`/ADR-0007,
  `X-002`, `X-003`) left `idea.md` rev 6 untouched, routing decisions through an ADR
  and spec as-built notes instead — this ADR follows the same precedent rather than
  reopening the rev-6 design document for a scope it never named.
- This ADR does not itself implement anything — `groups/18-tui-dashboard.md`'s
  `T18-01`–`T18-09` cards are the implementation; `G18` is where this ADR's
  architecture claims get re-verified against shipped code, the same discipline
  `G00`–`G17` already applied to their own groups.
