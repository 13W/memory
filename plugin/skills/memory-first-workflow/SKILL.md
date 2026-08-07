---
description: Routing guide for this workspace's memory and code-search tools (search_code,
  recall, remember, get_file_context, project_overview) — which to use instead of
  Grep/Glob/Read/ls, and how to load them if deferred. Use before searching this repository's
  code, recalling project memory or context, or orienting in an unfamiliar part of the codebase.
---

# memory-first workflow

Tool-routing table for this workspace's `memory` MCP server. If these tools are deferred
(absent from context), load them via tool search first — search for the tool name.

| Instead of | Use | When |
| --- | --- | --- |
| `Grep` / `Glob` | `search_code` | Searching by meaning, or the identifier is unknown |
| `Read` / another search | `get_file_context` | You already have a file's path |
| `ls` / `Glob` series | `project_overview` | Orienting in an unfamiliar repository |
| — | `recall` | Before your first file read, grep, or search this session — never after |
| — | `remember` | The moment something durable surfaces — a decision, a convention, a fact worth keeping |

RECALL → SEARCH_CODE → THINK → ACT → REMEMBER: each arrow is a tool call, not narration.
