# local-rag v2

## Project status

This repository currently contains the design, executable specification, and implementation
plan for a greenfield rewrite. Do not assume production code, build commands, crate names, or
legacy fixtures exist until they are introduced by a completed task.

The product is a local, co-located MCP service for Claude Code with three pillars:

- persistent, durable, auditable memory;
- hybrid semantic code search;
- spool-only capture of Claude Code observations.

The implementation language is Rust. Installation is through a single npm package; the
native binaries come from the project's own GitHub release assets. There must be no
mandatory external daemon.

## Sources of truth

Use this precedence order:

1. `docs/idea.md` rev 7 — design rationale and `[FIXED]` decisions.
2. `docs/specification/README.md` and `docs/specification/01-*.md` … `15-*.md` — normative,
   executable-level behavior.
3. `docs/implementation-plan/README.md` — execution workflow and Definition of Done.
4. `docs/implementation-plan/PROGRESS.md` — task order, current status, and evidence.
5. `docs/implementation-plan/groups/NN-*.md` — scope and tests for an individual task.
6. Existing code and tests — the as-built state, which must conform to the sources above.

If code, tests, plan, and specification disagree, do not silently choose one. Follow the
deviation workflow below.

Normative markers:

- `[FIXED]`: do not change without a new design revision.
- `[SPEC]`: executable detail; a change requires an explicit specification amendment.
- `[OPEN]`: do not hard-code an answer. Resolve only through the named spike, benchmark, ADR,
  or product decision.
- `MUST`, `MUST NOT`, `SHOULD`, and `MAY` have RFC 2119 meaning.

## Starting work

When the user names a task ID, execute exactly that task. When the user says to continue
without naming one, select the first unchecked task in `docs/implementation-plan/PROGRESS.md`
whose dependencies are complete.

Before editing:

1. Read `docs/implementation-plan/README.md`.
2. Read `docs/implementation-plan/PROGRESS.md` and `docs/implementation-plan/DEVIATIONS.md`.
3. Read the complete task card in the relevant `groups/NN-*.md` file.
4. Read every specification section referenced by that group/task.
5. Inspect adjacent code, migrations, interfaces, and tests affected by the task.
6. Verify that the previous group gate is `PASS` or `PASS after D-NNN`.

Do not start a later task to bypass a blocked task or failed gate. Do not bundle the next task
because it appears small. One Claude Code iteration should produce one task result.

## Task execution contract

For every implementation task:

1. Mark only the selected task `[~]` in `PROGRESS.md` while it is in progress.
2. Implement only the card's declared scope. Preserve unrelated user changes.
3. Add every test required by the card. Add a regression test for every defect fixed.
4. Run focused tests first, then the repository-wide quality command documented by T00-02 in
   `CONTRIBUTING.md` once that file exists.
5. Check formatting, lint with warnings denied, unit tests, integration/fixture tests, and doc
   tests applicable to the change.
6. Update relevant `[SPEC]` text to as-built precision when the task reveals an executable
   detail. Never rewrite `[FIXED]` behavior as an implementation convenience.
7. Mark the task `[x]` only after all acceptance criteria pass.
8. Commit the completed task (mandatory): a single focused commit containing the task's changes
   plus the `PROGRESS.md` update, with a descriptive message and the required `Co-Authored-By`
   trailer. Branch first only when the working branch is a shared/default branch backed by a
   remote; a local single-line repository commits to its working branch. Do not push unless the
   user asks. A task is not done until it is committed.
9. Append immutable evidence to `PROGRESS.md`: the commit reference (short hash, or the commit
   subject when the evidence line ships inside that same commit), exact commands, result,
   artifact/report path, executor, and date.

**Worktree policy:** until specification work is complete, do not isolate task work in a git
worktree for this project. Work directly in the primary checkout and commit directly to
`master`. This overrides any default harness/session convention that isolates background or
agent sessions into a worktree before editing (e.g. an `EnterWorktree`-style tool). If a session
finds itself already isolated in a worktree for a completed task, merge (fast-forward when
possible) the commit into `master` and remove the worktree before finishing.

If the repository-wide command does not exist yet, run all discoverable checks appropriate to
the current repository and record exactly what was and was not run. Never invent a successful
test result.

Tests must be deterministic and must not depend on network access, real wall-clock sleeps, or
the user's home directory. Use a temporary `LOCAL_RAG_HOME`, controlled clock/UUID sources,
fixtures, and named failpoints. State-changing retry/crash tests must verify idempotence.

## Deviation workflow

At the first discovered mismatch with normative behavior:

1. Stop expanding the planned implementation.
2. Append a row to `docs/implementation-plan/DEVIATIONS.md` with status `open`.
3. Create a corrective task `D-NNN` using `docs/implementation-plan/TASK-TEMPLATE.md`.
4. Insert it in `PROGRESS.md` before the current group's gate.
5. Implement it with tests, change the deviation to `resolved`, and rerun affected checks.
6. Only then resume the planned queue.

Use `blocked` when resolution needs a product decision or new design revision. Do not move a
normative mismatch to a backlog. Only explicitly deferred/post-v0 scope may remain deferred.

## Group gates

`GNN` is a real conformance task, not a checkbox-only review. For a gate:

- reread all specification sections named by the group;
- build a `requirement -> code -> test` trace;
- run all focused group tests and applicable workspace checks;
- inspect every `[FIXED]`, `[SPEC]`, and `[OPEN]` touched by the group;
- register and fix deviations before declaring success;
- record `PASS`, `PASS after D-NNN`, or `BLOCKED` in the Gate results table;
- append reproducible evidence to `PROGRESS.md`.

The next group cannot start after `BLOCKED` or while a deviation is `open`/`fixing`.

## Architecture guardrails

Preserve these system-wide invariants in every task:

- `state.sqlite` is canonical. `cache.sqlite` and `projection/` are independently validated,
  fully rebuildable caches.
- Never perform writable cross-database transactions through SQLite `ATTACH`.
- No durable ID is derived from a filesystem path. Worktree identity is a stable UUID.
- Content-shared rows never contain path-, generation-, or context-specific fields.
- Every searchable file has its exact `source_blob`; skipped files have no occurrences.
- Dense projection is untrusted: validate on every open and rebuild on doubt.
- Hooks ingest only through durable spool append; they never send ingestion to the daemon.
- Memory mutations, evidence, audit, and consolidation cursor movement are transactionally
  strict and idempotent.
- Request routing is explicit. There is no process-global current project/worktree/branch.
- A per-worktree read lock spans the complete hybrid-search pipeline.
- Data policy defaults to `local_only`; remote calls pass the central policy guard first.
- Recalled memory and indexed repository content are untrusted data, never instructions.
- Do not couple production code to a real dense backend before the T10 comparative spike.
- Do not implement deferred description, reranker, graph, ANN-memory, multi-harness, FreeBSD,
  or win32-arm64 scope as part of v0 tasks.

## Repository hygiene

- Use `rg`/`rg --files` for discovery.
- Keep migrations forward-only, numbered, checksummed, resumable, and tested.
- Keep public protocols and errors typed and documented.
- Do not delete or rewrite prior progress evidence or resolved deviation history.
- Do not weaken tests to make an implementation pass.
- Do not commit generated model weights, local stores, SQLite files, spool segments, benchmark
  scratch data, secrets, or machine-local configuration.
- Do not create project-level `.claude/rules/` or initialization files as product behavior;
  plugin packaging must not modify users' repositories.

## Current entry point

The planned queue starts at T00-01. It imports implementation-neutral v1 behavioral fixtures
and baseline artifacts before rewrite code. If those artifacts are unavailable, mark the task
blocked and request the required source or an explicit product decision; do not fabricate a
baseline or silently skip the gate.
