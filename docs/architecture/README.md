# Architecture model

An executable LikeC4 model of local-rag v2, meant to be the first thing you open
when planning a feature and the last thing you update before committing it.

It answers three questions the prose cannot answer quickly: where does a new
piece go, what does it have to talk to, and which invariants does it cross.

## Rendering it

```bash
likec4 start docs/architecture      # interactive, hot-reloads on save
likec4 validate docs/architecture   # must exit 0
node docs/architecture/check-refs.mjs
```

The CLI is not vendored here — no `package.json`, no `node_modules`. Install it
once (`pnpm add -g likec4@1.59.2` or `npx likec4@1.59.2 …`); CI pins the version
in its command. If `likec4 export` is ever needed, run it as
`env -u GEMINI_API_KEY likec4 export …`: with an AI provider key in the
environment it installs packages and rewrites a lockfile as a side effect of
reading the model.

## What is where

| File | Holds |
| --- | --- |
| `spec.c4` | The only `specification` block: element kinds, relationship kinds, deployment node kinds, tags |
| `styles.c4` | The `status` style group — how a tag becomes colour on a node |
| `model/context.c4` | Actors and external systems |
| `model/containers.c4` | The system boundary: processes, stores, and the channels between them |
| `model/daemon.c4` | Components inside the daemon |
| `model/delivery.c4` | The npm package and the Claude Code plugin |
| `model/deployment.c4` | One machine, one OS user, one store |
| `views/*.c4` | Fifteen views: landscape, containers, six component slices, six flows, deployment |
| `check-refs.mjs` | Every element traces to a spec section, and every path resolves |

## Conventions

**One vocabulary.** LikeC4 merges every `specification` block in the workspace,
so a second one redeclaring a kind is a duplicate-declaration error rather than
a merge. Everything is declared in `spec.c4`; nothing is declared in a model
file.

**Metadata is the link back to the repository, and it is checked.** Every
element carries `spec` — the normative section that defines it. Add `code` when
a module owns it, and `plan` when a whole plan group does. `check-refs.mjs`
fails the build when a path stops resolving, which is exactly what a rename
does to a diagram. Cite the **section**, never an as-built note: notes supersede
each other (the lock-reclaim rule was rewritten three times), so they are a good
source for a description and a bad anchor for structure. Metadata values are
always quoted, integers included.

**Status tags say what is true, not what is planned.** `#implemented`,
`#partial`, `#planned`, `#deferred`. `#partial` is the load-bearing one: it
marks a place where the specification is normative and the code does less —
`metadata_only_remote` behaving like `local_only`, a TTL sweep nothing
schedules, a Windows pipe whose SID lookup is unimplemented. Never smooth those
over; a planning diagram that hides them is worse than no diagram. Read tier
tags with status, never alone: `#post-v0` means "after the MVP scope froze", and
the TUI and daemon-managed indexing are both post-v0 *and* shipped.

**Composition is nesting, order is a dynamic view.** LikeC4 rejects a
relationship from a parent to its own child, and that is the right rule: the
fact that the fusion stage lives inside the search engine is structure, while
the fact that it runs after both legs is behaviour. Structure goes in `model/`,
behaviour in `views/dynamic.c4`.

**Every deferred element gets one edge** showing where it would attach. A
floating box tells a planner nothing; an arrow into the store or the provider
pool tells them what the seam is.

**Views are slices, not copies.** An element is declared once and appears in as
many views as it belongs to.

**English only**, including descriptions — the repository rule, not a style
preference.

## LikeC4 1.59.2 traps, all of them measured

1. A tag's `color` in `spec.c4` tints the **tag chip**, not the node. Node
   colour comes only from a `global { styleGroup … }` rule, and every view must
   opt in with `global style status` or the rules silently do not apply.
2. The two blocks disagree on colour syntax: `tag` accepts only hex/`rgb()`, a
   `style` block accepts only theme names. Each rejects the other's form.
3. A relationship's colour cannot be driven by a tag. Neither
   `style relation.tag == #x` nor a `where tag == …` clause parses.
4. `include *` is the top level only. Descendants must be asked for
   (`localRag.**`); `include **` is a parse error.
5. A tag view needs two predicates — the tag, and `<-> *` for its neighbours.
   The directional forms (`include -> element.tag == #x`) parse and then resolve
   to nothing.
6. A `deployment view` takes no `global style` line.
7. Dynamic views print "Sequence view does not support nested actors" because
   their participants are components inside the daemon. Validation still passes
   and the flow renders; only the sequence-diagram rendering mode is
   unavailable. Flattening the participants to fix it would cost more than it
   buys.

## When to update it

Update the model **in the same commit** as the change, whenever the change adds,
removes or re-wires:

- a process or a shipped artifact;
- a store, or what a store is authoritative for;
- a background loop, or what triggers one;
- an MCP tool, a hook event, or a CLI verb that changes the interface surface;
- an external dependency or a network call;
- an inter-process channel, a lock, or a transaction boundary.

A change to an implementation detail behind an unchanged boundary does not need
a model change. A change that makes an existing description false always does —
including a change that only moves a file, because `check-refs.mjs` will say so.

When the model and the code disagree and you are not the person fixing it, tag
the element `#partial` and say what is missing in its description. That is a
finding, not a defect in the model.
