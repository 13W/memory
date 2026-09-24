# ADR-0016: Per-call worktree fallback for a proxy that has no launch worktree

## Status

Accepted — 2026-09-24.

Owner product decision, recorded as the design revision that CLAUDE.md requires before an
implementation departs from `[FIXED]` text. Realized by corrective card `D-137`
([`groups/24-language-expansion.md`](../implementation-plan/groups/24-language-expansion.md),
appendix). It leaves every `[FIXED]` sentence in place and adds an **opt-in exception** to two of
them. With the opt-in off, behaviour is byte-for-byte what those sentences describe.

## Context

Spec 02 §3.3 `[FIXED]` routes each daemon request by an explicit context
`{session_id, worktree_root?, repo_hint?}`. A request without a resolvable worktree runs in global
scope only. The T15-02 as-built note has `local-rag-proxy` resolve that context **once at
launch**: `worktree_root` is `current_dir()`, cloned into every relayed call, because "one proxy
process serves one session". Spec 11 §1 `[FIXED]` calls the proxy a "pass-through for MCP JSON-RPC"
and says its T15-02 note (also `[FIXED]`) "`context` is resolved once at launch and cloned
unchanged into every relayed request".

Claude Code satisfies that premise: it spawns one proxy per session, and the proxy's cwd is inside
the project. **Claude Cowork does not.** It spawns *one* plugin MCP server process for the whole
desktop app, and the cwd is not a repository. Each Cowork session works in its own folders, and
only the model knows those folders, as absolute paths. The `memory-cowork` plugin (`cowork-plugin/`,
commit `fd16781`) can pin one repository through `LOCAL_RAG_WORKTREE` or
`~/.config/local-rag/cowork-root`. Without either, its launcher falls back to `cd $HOME`, and every
call resolves to `GlobalOnly`: `search_code` answers `WORKTREE_NOT_INDEXED`, and `remember`
silently writes machine-wide memory.

## Decision

1. **The launch context keeps priority.** When the proxy's launch `worktree_root` resolves to a
   registered worktree, which is always the case under Claude Code, routing ignores any per-call
   argument.
2. **A per-call root is only a fallback.** `RequestContext` gains an optional
   `worktree_fallback: Option<String>`. The daemon consults it **only** when the launch root
   resolves to `Resolution::GlobalOnly`, and runs it through the same resolution path: git probe,
   toplevel snapping, registry lookup. If the fallback does not resolve either, the request is
   `GlobalOnly` as before. It is never a new error. `Resolution::Ambiguous` is not `GlobalOnly`,
   so the launch context owns routing in that case too. The daemon makes this choice
   (`gitroot::request_root_with_fallback`, called from `mcp::dispatch`) because only the daemon
   can tell whether the launch root resolves. The choice is made per request, with no
   process-global state (spec 02 §3.3 `[FIXED]`).
3. **Opt-in per proxy process.** The proxy does any of this only under
   `LOCAL_RAG_PER_CALL_WORKTREE=1`. With the opt-in, it:
   - adds an optional `worktree` string property to every tool's `inputSchema.properties` in the
     `tools/list` response, never listing it in `required`;
   - removes `worktree` from a `tools/call`'s `params.arguments` before relaying, because the
     daemon's schemas are `additionalProperties: false`, and puts it in **that one request's**
     `context.worktree_fallback`. Only absolute paths are accepted. Anything else is dropped, and
     the proxy says so on stderr, never on stdout.

   Every other request keeps the launch context unchanged. With the variable unset, the proxy
   parses nothing: `tools/list` reaches the client exactly as the daemon sent it, and a `worktree`
   argument reaches the daemon untouched and is rejected there, as before.
4. **The daemon's catalog does not change.** `daemon/mcp/tools.rs` never advertises `worktree`,
   and its byte budget (`MAX_CATALOG_BYTES`) still measures what Claude Code sees.
5. **No `proto` bump.** The field is `#[serde(default, skip_serializing_if = "Option::is_none")]`,
   and `RequestContext` does not deny unknown fields. A context without a fallback serializes to
   the pre-ADR bytes, so an older daemon never sees the key. An older proxy's envelope deserializes
   with `None`. Proxy and daemon ship together and are version-locked by the spec 13 §4 upgrade
   flow in any case.

## Consequences

- The two `[FIXED]` sentences in spec 11 §1 now hold **exactly when the opt-in is off**, which is
  the default and the Claude Code configuration. The opt-in exception is documented in `[SPEC]`
  as-built notes (spec 02 §3.3, spec 11 §1/§4) that cite this ADR. The `[FIXED]` text is not
  rewritten.
- With the opt-in, the proxy rebuilds two kinds of message: a `tools/call` that carries `worktree`,
  and the response to a `tools/list`. It uses `serde_json` without `preserve_order`, so a rebuilt
  message comes out with object keys sorted. That is valid JSON-RPC, and every other message still
  passes through as the original bytes. `preserve_order` stays off because Cargo feature
  unification would switch it on for the daemon build as well.
- The opted-in catalog is about **3.4 KB** larger: 19 tools × one ~177-byte property. Only
  opted-in hosts see it; Claude Code's catalog is untouched.
- The proxy still holds no project state. The only new per-connection state is the list of ids of
  in-flight `tools/list` requests, which dies with the connection.
- The model has to pass the argument. The `memory-cowork` plugin's skill tells it to pass the
  session's working folder on every call, and to report it when a result comes back global or
  degraded.

## Alternatives considered

- **Daemon-side, per-connection catalog.** The proxy signals the opt-in in `HELLO`, and the daemon
  adds and strips `worktree` for that connection. This keeps the proxy a pure pass-through, but it
  puts host-specific catalog variants into the daemon's MCP surface and couples the catalog to
  connection state. Rejected by the owner: the daemon's catalog stays single.
- **Per-call argument with priority over the launch context.** This would let one argument
  redirect a Claude Code session that already has a correct worktree. Rejected: the launch context
  is the stronger signal, and priority is non-negotiable.
- **A new error for an unresolvable fallback.** Rejected: spec 02 §3.3 already defines an
  unresolvable root as `GlobalOnly`, never an error, and a fallback is no different.
- **Bumping `proto`.** Unnecessary for an optional, skipped-when-absent field (Decision 5). It
  would also force the incompatibility path onto every mixed-version pair for no gain.
