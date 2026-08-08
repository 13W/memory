# ADR-0009: Daemon-managed multi-project indexing

## Status

Accepted — 2026-08-08.

Unlike ADR-0008, this is **not** wholly new scope. A dedicated research pass (this
session) found that daemon-hosted indexing is already `[FIXED]` architecture:
[spec 02 §1](../specification/02-architecture.md)'s topology diagram literally lists
`background workers (reconcile, embedding, consolidation, GC)` under the daemon, and
its prose fixes that "the **daemon** … hosts all SQLite connections, shard handles,
and **workers**." What is genuinely new is the *product surface* on top of it:
an explicit, **persisted, per-project opt-in registry** that survives daemon restarts,
plus the CLI/admin verbs to manage it. That half exists nowhere in `idea.md` rev 6,
`docs/specification/`, or `docs/implementation-plan/` before this ADR.

Three normative artifacts already disclose the implementation gap this ADR closes,
none of which was ever registered as a deviation or assigned an owner:

- **spec 02 §4.3, as-built note (T15-01)**: "no reconcile-watcher or periodic-GC
  scheduling exists yet either (**no card names an owner narrower than 'group 15'**
  for either)."
- **spec 02 §4.4, as-built note (T15-01)**: periodic/startup scheduling of the
  housekeeping sweeps "remains unclaimed by any card narrower than 'group 15,' the
  same gap this section's own as-built notes above already flag for reconcile-watcher
  and GC triggering generally."
- **`crates/store/src/lock/worktree.rs:1-9`**: "The write side of L2 already exists
  *structurally* today … This registry is the actual lock object: adopting it into the
  projection switch is later work (T11-05, group 11); **the reconcile driver's own
  adoption has no dedicated task yet in the current plan.**"

That third pointer is stale: `T11-05` is `[x]` closed and did **not** adopt L2.write —
`crates/projection/src/model_switch.rs:51-54` explicitly re-delegates it ("stays the
caller's job … group 15's wiring"), and group 15's actual caller
(`cli::index::project_generation`) takes no L2 at all. Confirmed by `rg`: the only
production reference to `WorktreeLockRegistry` anywhere is
`crates/local-rag/src/daemon/search.rs:82`, which constructs a **private** instance
used only by the read side. **L2's write side has never existed in production code.**
This ADR registers that gap as `D-043` (`DEVIATIONS.md`) and names
[`groups/20-daemon-managed-indexing.md`](../implementation-plan/groups/20-daemon-managed-indexing.md)
as its corrective owner, per `CLAUDE.md`'s deviation workflow.

Delivery mechanism, as in ADR-0008, is **not** a single `X-NNN` card.
`TASK-TEMPLATE.md`'s own rule — "if the description requires two independent results
… the task must be split" — is violated many times over here; the independent,
separately-testable results are enumerated in §Decision below. The owner therefore
chose the heavier path `TRACEABILITY.md` reserves for exactly this case: a new
numbered group, `20`, gated by `G20`, outside the closed `T00–T17` v0 queue —
the second instance of the precedent ADR-0008 established (`19` was itself a new
group, but registered without its own ADR; this ADR makes the pattern explicit
for scope that also touches `[FIXED]` sections).

Sequencing note: group `18` is still open (`T18-08` `[~]`, `T18-09`/`G18` pending) at
the time of this decision. Group 20 does not depend on `G18` and reopens no gate; it
only *follows* `T18-08`'s already-landed `admin/*` JSON-RPC convention and must not
modify `admin/tail_calls`/`admin/tool_stats` while `G18` is unclosed.

## Context

The owner works on several projects simultaneously and wants the daemon to keep every
one of them indexed by its own background threads — file watchers included — instead
of running one foreground CLI process per project, where "the processes block each
other's database."

The research pass established the actual as-built state:

- **Indexing today bypasses the daemon entirely.** `local-rag index`/`reindex`/`watch`
  (`crates/local-rag/src/cli/{index,watch}.rs`) are standalone foreground processes,
  each opening `StateDb`/`CacheDb` directly (`cli/index.rs:278-289`). This is a
  deliberate, documented `T15-07` decision: `cli/watch.rs:8-17` — "`local_rag_protocol`
  has no verb for 'watch'; the daemon does not spawn a `spawn_watcher`/
  `WorktreeReconciler` anywhere today … Adding a new protocol message would be new,
  unrequested architectural surface."
- **The stated cause of the user's pain is real but mis-diagnosed as "locking."**
  Between separate OS processes there is no coordination at all beyond raw SQLite WAL
  and `busy_timeout=5000` (`cli/mod.rs:23-31`). The typed L2 registry that *would*
  coordinate them exists (`crates/store/src/lock/worktree.rs`, T09-01) but is (a)
  read-only in practice and (b) an **in-process** `tokio::sync::RwLock`, which no
  amount of adoption can extend across process boundaries. So the honest framing is:
  *moving the writers into one process is what makes coordination possible at all* —
  the lock is the beneficiary of that move, not a substitute for it.
- **Every primitive already exists and is tested.** `spawn_watcher` +
  `watch_event_to_trigger` (`crates/index/src/reconcile/watcher.rs`), the pure
  `Debouncer` with `DEBOUNCE_MS = 500`/`PERIODIC_MS = 6h`, and the one-task-per-worktree
  `WorktreeReconciler` (T05-04, spec 06 §1) are complete. `cli::watch::run_watch_loop`
  (`cli/watch.rs:100-190`) is a working reference composition of exactly the loop the
  daemon needs — for exactly one worktree.
- **The daemon already runs independent background tasks correctly.**
  `tokio::runtime::Builder::new_multi_thread` (`main.rs:83-94`); `DaemonHandle::start`
  (`daemon/lifecycle.rs:360-410`) spawns `spawn_spool_resume`/
  `spawn_consolidation_resume`/`spawn_consolidation_trigger`, each accounted by
  `JobRegistry`/`JobGuard` for the idle-shutdown gate. `JobKind` is `#[non_exhaustive]`
  precisely so "a future reconcile/backfill/GC trigger follows the identical path"
  (`daemon/jobs.rs:1-12`).
- **The store is already multi-project.** One `state.sqlite`/`cache.sqlite` per OS
  user; `projection/<worktree_id>/<model_space_id>/` sharded per worktree; identity is
  a stable UUID, never a path; `registry::resolve` already returns
  `Resolved|GlobalOnly|Ambiguous` across many repos/worktrees.
- **Two concrete blockers nobody has hit yet.** (1) `IndexCtx`/`index_worktree`/
  `project_generation` are `pub(crate)` in the **binary** target; `lib.rs` exports only
  `pub mod daemon`, so the daemon cannot call the pipeline at all. (2) The daemon opens
  ONNX only behind the query adapters (`daemon/query_embedder.rs`, two sessions —
  code_raw + memory, D-036) and never exposes the underlying `Arc<dyn Embedder>`, so a
  naive implementation would open **four** ONNX sessions in one process.
- **A concurrency property is untested where it now matters.**
  `crates/projection/tests/switch_concurrency.rs:24-27` states its own premise: "the
  actual property under test — **L2.write serialization** — is what makes every
  switch's step race-free." Concurrent *unserialized* switches on one worktree
  (today's only possible shape, since no production caller takes L2.write) are outside
  every existing test.

Two product decisions were fixed by the owner before this ADR was written:

1. **Registration is explicit and persisted** — the user enrolls a project once, the
   daemon stores the list and brings up a watcher + reconcile task per enrolled project
   at every start.
2. **`index`/`reindex`/`watch` stay exactly as they are** for manual/CI use. The
   daemon-managed mode is **additive**; `T15-07`'s as-built guarantee ("safe to run
   alongside a live `serve`") is preserved, not reversed.

## Decision

**local-rag v2's daemon gains a supervised, per-worktree background indexing subsystem,
driven by a persisted opt-in registry.** Scope, fixed by this ADR:

- **In scope:** a `managed_worktree` table in `state.sqlite` (schema version 10); one
  supervised `tokio` task per enabled managed worktree composing the existing
  `spawn_watcher` + `WorktreeReconciler` + embed/activate/materialize pipeline; the
  first production adoption of `L2.write`; a daemon-wide shared `WorktreeLockRegistry`;
  `admin/*` JSON-RPC verbs for list/reload/manual-trigger; a `local-rag project` CLI
  command family; an advisory (never blocking) double-indexing warning.
- **Explicitly out of scope:** periodic GC/housekeeping scheduling (the *other* half of
  spec 02 §4.3/§4.4's unowned-scheduling note — same shape, different owner, must not be
  smuggled in); daemon config hot-reload; cross-process locking; any change to
  `local-rag index`/`reindex`/`watch` behavior beyond one stderr warning line; new MCP
  tools.

**Registration lives in `state.sqlite`, as a new `managed_worktree` table**, keyed by
`worktree_id` with a foreign key to `worktree`. Alternatives considered and rejected:

- *`config.toml`* — has no writer (`Config::save` is `T18-07`, and belongs to the TUI's
  own screen), is read once with no reload, sits outside the store (so a
  `LOCAL_RAG_HOME`-scoped store would not carry its own project list), and a
  path-keyed list would violate the system-wide invariant "no durable ID is derived
  from a filesystem path."
- *A JSON blob in `store_settings`* — that table is bootstrap-created framework storage
  for singletons (`store_instance_uuid`, `default_model_space_id`); a blob has no FK, no
  per-row query, and one toggle would rewrite the whole value.
- *A `repo_settings` key* — wrong granularity (repository, not worktree), and spec 02
  §3.2 defines that table as the mirror of the global `[models]`/`[index]` config
  sections, not as a work queue.
- *Reusing `worktree.state`* — spec 04 §7's machine (`active|detached|removing`) answers
  "does this path still resolve," an orthogonal axis; conflating it would make "the user
  paused indexing" indistinguishable from "the path vanished" and would require editing
  `[SPEC]` transitions.
- *A file under `run/`* — a second source of truth with no atomicity against
  `register_new_worktree`'s own four-write transaction.

The chosen table is the only option that is transactional with worktree creation
(enrolling a brand-new path is *one* transaction), referentially integral, readable by
all three existing direct-`state.sqlite` clients (CLI, TUI, daemon), and evolvable
through the forward-only checksummed migration framework that already exists.

**Control surface is CLI + `admin/*` JSON-RPC, never MCP tools.** `local-rag project
add|remove|enable|disable|list|status` writes the table directly — the same
architecturally-sanctioned pattern every CLI command already uses, so enrollment works
with the daemon down. A live daemon is then notified best-effort over the existing UDS
transport (`admin/projects_reload`); if that fails, the row is still durable and takes
effect at the next daemon start, and the supervisor re-reads the table on a slow
backstop poll. This is the same discipline spec 06 §1 already fixes for a different
layer: **notify = hint, the table = truth.** New tools are deliberately *not* added to
`mcp::tools::catalog()`: `T19-01` just placed a hard byte budget on that catalog because
its size pushes Claude Code into deferred tool loading, and — independently — enrolling
arbitrary filesystem directories for continuous background indexing is store
administration, not something a model-driven tool surface should be able to do (spec 12).
`daemon/mcp/dispatch.rs:96-103` already establishes `admin/*` as a non-catalog,
non-`tools/list` surface for exactly this class of verb.

**`T11-05` is not a prerequisite — it is a stale pointer.** The L2.write adoption that
`crates/store/src/lock/worktree.rs:6` attributes to it never shipped, and `T11-05` is
closed. Group 20 owns that adoption itself, as a **hard prerequisite card
(`T20-04`) blocking the supervisor cards (`T20-05`/`T20-06`)**. Two properties make it
load-bearing rather than cosmetic here: the daemon's reconcilers and the daemon's
`SearchEngine` must share **one** `WorktreeLockRegistry` instance (today
`build_search_engine` constructs a private one — two registries would make the lock a
no-op), and this is the first configuration in which `spec 02 §5`'s write-path rule and
`spec 02 §6`'s `BUSY_RETRY`-on-`L2.read`-timeout are reachable in production at all.
Group 20 is therefore also the task that spec 02 §5's `[OPEN]` on registry eviction
names ("left for whichever task first owns a long-lived registry instance"), and must
dispose of it explicitly.

**Idle-shutdown semantics are unchanged by default.** A per-worktree task holds a
`JobGuard(JobKind::Reconcile)` only for the duration of an actual scheduled-through-
completed cycle, and holds nothing while merely watching — the exact discipline D-024
already fixed for the consolidation trigger ("its own `JobGuard` is held only for one
tick's active work … so a live-but-idle worker never blocks idle-shutdown"). Consequences,
accepted deliberately: a daemon with enrolled-but-quiet projects still exits after
`idle_shutdown_secs`, and freshness is restored at the next start by the forced
`TriggerKind::Startup` strict reconcile per managed worktree — which is precisely spec
06 §1's own "Startup of a known worktree" trigger, made correct by the `[FIXED]`
principle "watcher = hint, reconcile = truth." Making an enrolled project *pin* the
daemon alive would change a `[FIXED]` clause of spec 02 §4.3 and add a key to spec 02
§3.1 (whose `default_matches_spec_toml` test is pinned); it is therefore **not** decided
here and is carried as an owner-decision card (`T20-10`), the same disposition `T19-06`
uses.

**Double indexing stays "wasteful, never unsafe," and is warned about, never refused.**
The existing basis (`cli/mod.rs:23-31`) holds unchanged, and L2 does not change it in
either direction (an in-process lock cannot see another process). `index`/`reindex`/
`watch` gain one stderr line when the target worktree is daemon-managed and a live
daemon answers the liveness probe, naming `local-rag project reindex` as the deduplicated
path — and then proceed, fail-open, because CI and manual recovery must keep working.
`G20` must additionally *test* the cross-process concurrent case end to end, since
`switch_concurrency.rs`'s own premise excludes it; if it proves unsafe, that is a
deviation to register, not a silent assumption.

## Consequences

- `docs/specification/11-interfaces.md` gains a new **`§8 Daemon-managed indexing`**
  (`[SPEC surface]`, the same forward-sketch convention §6 and §7 used), and §6's CLI
  block gains the `local-rag project …` line. Each `T20-NN` card appends its own
  as-built note there once implemented.
- `docs/specification/02-architecture.md` receives as-built notes closing three of its
  own disclosures: §1 (the `[FIXED]` topology's reconcile worker finally exists),
  §4.1 step 4 ("start workers" — which workers, in what order), §4.3 (the
  reconcile-watcher scheduling owner named by that note's own "no card names an owner"
  sentence, plus the idle-gate accounting), and §5 (L2.write's first production adoption
  and the disposition of its registry-eviction `[OPEN]`). §3.3's `[FIXED — daemon
  defines routing]` gains one clarifying sentence: the managed set is a *background-work
  enrollment list*, never an ambient current project — request routing stays explicit.
- `docs/specification/06-reconcile-and-fts.md` §1 gains an as-built note: the daemon is
  now a second, always-on driver of the same trigger taxonomy, and `local-rag watch`
  remains as the daemon-independent sibling (per the owner's own decision), so the
  T15-07 note there is amended, not deleted.
- `docs/specification/03-data-model.md` §2.1 documents the `managed_worktree` DDL and
  schema version 10.
- `docs/implementation-plan/groups/15-daemon-interfaces-cli.md`'s `T15-07` card gets a
  cross-reference note, in the same style as `T15-07`'s existing "Добавлено D-013"
  block: `watch` is complemented, never replaced.
- `docs/implementation-plan/DEVIATIONS.md` gains `D-043` (spec 02 §1/§5 vs. as-built:
  no daemon-hosted reconcile worker, no production `L2.write`), corrective owner
  `T20-04`/`T20-06`, status `open → resolved` at those cards.
- `docs/implementation-plan/TRACEABILITY.md`: rows `02` and `06` gain `G20` in the
  end-to-end re-verification column; the closing paragraph names ADR-0009 as the second
  instance of the "new numbered group instead of `X-NNN`" precedent.
- `docs/implementation-plan/PROGRESS.md` gains a `## 20 — Daemon-managed indexing`
  section and, once run, a `G20` row in Gate results.
- No `idea.md` revision — the same precedent every post-`G17` decision has followed.
- This ADR implements nothing; `T20-00`–`T20-10` do, and `G20` is where its claims get
  re-verified against shipped code.
