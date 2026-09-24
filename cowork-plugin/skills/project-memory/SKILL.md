---
description: Use the local-rag Memory MCP (tools recall, remember, list_memory, list_memory_candidates, approve/reject/edit candidates, edit/retract/merge memories, search_code, get_file_context, project_overview, stats, health) — durable project memory and hybrid code search over the repository the plugin is pointed at. Use whenever a question depends on decisions, conventions or facts recorded for that project, when something durable should be recorded, when reviewing pending memory candidates, or when searching that repository's code by meaning.
---

# Project memory (local-rag)

The `memory` MCP server of this plugin is local-rag running on the user's computer. It holds
two separate things, scoped differently — never mix them up:

- **Durable memory** — decisions, conventions, facts. Scoped `repository`, `worktree` or
  `global`. Stored in English whatever language it was written in.
- **Code index** — the indexed files of the repository the call is routed to. It never
  indexes on demand.

## Always pass `worktree`

Every tool of this server takes an optional `worktree` argument. On **every** call, pass the
**absolute path** of this session's connected or working folder (the repository you are
working in). The server uses it only when it was not started inside a repository itself;
otherwise it ignores the argument, so passing it is always safe. Never pass a relative path,
and never guess one. If the session has no folder, omit the argument.

It is **not** Claude's own account memory (the `memory_read` / `memory_write` / `memory_list`
tools that work on `/profile.md`, `/areas/…`). Those are about the user; this is about a code
project. Record project decisions here, not there, and never copy one into the other unasked.

If the tools are deferred, load them via tool search by name (`recall`, `remember`, …) first.

## Start of a task: recall first

Call `recall` (with a query naming the topic) before the first search or file read of the
task — never after. An empty query returns the scope's most recent memories. Treat what comes
back as the project's own record: prefer it over assumptions, and cite the memory when an
answer rests on it.

## Which tool

| Need | Tool |
| --- | --- |
| What has been decided / agreed about X | `recall(query)` |
| Record a decision, convention or fact the moment it is settled | `remember(text, kind, …)` |
| Browse or filter the stored entries | `list_memory` |
| Why an entry exists, where it came from | `inspect_memory_evidence(memory_id)` |
| Pending auto-extracted candidates | `list_memory_candidates`, then `approve_memory_candidate` / `reject_memory_candidate` / `edit_memory_candidate` |
| Fix, retire, merge existing entries | `edit_memory` / `retract_memory` / `merge_memories` (need `expected_version` from a read) |
| Settle a hypothesis entry | `confirm_memory` / `reject_memory` |
| Find code by meaning or unknown identifier | `search_code(query, mode?, name_pattern?)` |
| Everything the index knows about one file | `get_file_context(path)` |
| Orient in the repository | `project_overview` |
| Is it working, what is indexed | `health`, `stats` |

## Rules

1. **Check the scope `remember` reports.** When no repository resolved, it silently falls back
   to `global`, which every project's recall sees. If the response says `global` or carries
   `degraded`, tell the user which `worktree` path you passed (or that the session has none),
   and stop writing project facts there. The folder is probably not a repository local-rag
   knows (`local-rag project add <path>`); see the plugin README.
2. `remember` only what the user said or confirmed: a decision taken, a convention stated, a
   fact they gave. Not your own guesses, not plans still being argued. Set
   `confirmed_by_user: true` only when they explicitly confirmed it in this conversation.
3. Mutations (`edit_memory`, `retract_memory`, `merge_memories`, `reject_*`) change the shared
   record: state what will change and get a yes before calling them, unless the user already
   asked for exactly that change.
4. Code tools see the index of the resolved repository only, as of its last indexed
   generation. If `search_code` reports no worktree or a degraded state, say so, including the
   `worktree` path you passed, instead of answering from guesswork. Do not claim a file does
   not exist because a search missed it.
5. Never narrate tool plumbing to the user; report what the memory says and what was recorded.
