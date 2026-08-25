# Draft eval corpus — adherence under context pressure

Status: **draft, not wired up.** These cases are written in the native
`claude plugin eval` format so they run unchanged once that harness is enabled
(it reports `plugin eval is currently in early access` on this machine today).
Until then the same three cases are what the own runner drives over
`claude -p --output-format stream-json --include-hook-events`.

They belong under `plugin/evals/` if and when this becomes a real task; they sit
here while they are research material, so that nothing ships in the plugin
package by accident.

Three cases, chosen because each isolates one mechanism from
`../0001-memory-adoption-under-context-pressure.md` §4:

| case | mechanism | measures |
|---|---|---|
| `recall-before-read` | F1 | does the loop fire at all, in a clean first stretch |
| `memory-necessary-fact` | — | does using memory make the answer *right* (outcome, not process) |
| `post-compaction-rearm` | F2 | does the loop fire again after the frame is replaced |

`post-compaction-rearm` uses `case.yaml`'s `context.history_file`, which replays a
transcript and evaluates the next turn. That is what makes the post-compaction arm
cheap and repeatable: no need to burn 500k tokens forcing a live compaction when a
recorded boundary can be replayed. A live arm using `--autocompact` remains the
cross-check that the replay stays faithful.

Every case needs a seeded store. `context.scaffold_script` must create an isolated
`LOCAL_RAG_HOME`, enrol a fixture worktree, plant the sentinel memory, and start the
daemon — and the runner must pass `--mcp-config` at the built binary plus
`--strict-mcp-config`, because plugin-supplied MCP servers do not start under `-p`
(`PROGRESS.md:1290`).

The ablation arms are switched outside these files: `ENABLE_TOOL_SEARCH`, `--settings`
for the hook set, `--agents` for subagent arms, `--append-system-prompt` for wording
variants.
