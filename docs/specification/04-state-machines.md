# 04 — State Machines

All transitions execute inside a single `state.sqlite` transaction unless stated otherwise.
Illegal transitions MUST fail the transaction (precondition check), never silently coerce.

## 1. Generation

```
building ──▶ projection_ready ──▶ active ──▶ retiring ──▶ (deleted by GC)
    │                                            ▲
    └──────────▶ failed ─────────────────────────┘ (failed rows GC'd like retiring)
```

| Transition | Trigger | Guard |
| --- | --- | --- |
| ∅ → building | reconcile starts building N+1 | per-worktree write lock held |
| building → projection_ready | membership + occurrences + FTS inputs complete | all `generation_file` rows have `file_revision.source_blob` (structural) |
| projection_ready → active | step 4 of projection switch (05 §5) | same tx that sets `worktree_projection_state` clean |
| active → retiring | the same tx activating N+1 | exactly one `active` per worktree (app invariant, asserted) |
| building/… → failed | error in reconcile/switch | error recorded in `worktree_projection_state.last_error` |
| retiring/failed → deleted | delayed GC | not a pin root (06 §5) |

`retiring` is **never** consulted for routing `[FIXED]` — search resolves only the active tuple.

As-built note (T05-01, `[SPEC]`): the generation lifecycle is `local_rag_store::registry::generation`
(the `generation` table itself ships with `SCHEMA_V2`, T02-03). `allocate_generation` mints the
`∅ → building` row with a per-worktree monotone `generation_number = MAX(number) + 1` over **all** of
the worktree's rows (retiring/failed keep their numbers until GC, so numbers are never reused);
`UNIQUE (worktree_id, generation_number)` (03 §2.1) is the structural backstop, and correctness of
the read-compute-write under concurrency rests on the single global writer (03 §3), not a per-row
lock. `GenerationState::check_transition` is the pure guard and realizes the exact legal set of the
diagram above: `building → projection_ready`, `building → failed`, `projection_ready → active`,
`projection_ready → failed`, `active → retiring`, plus idempotent self-transitions; every other move
is rejected — in particular `active → failed` (the "error in reconcile/switch" edge fires from
`building` on a build error and from `projection_ready` on a switch error, never from the
already-serving `active`, T05-05) and `building → active` (must pass through `projection_ready`).
`transition_generation` mirrors `transition_worktree_state` (04 §7): guarded read-then-write in one
tx, a typed `GenerationTransitionError` (`UnknownGeneration` / `Illegal`) with **no mutation** on
rejection, and a corrupt stored `state` surfacing as `rusqlite::Error::FromSqlConversionFailure`
(never a silent default). The "exactly one `active` per worktree" invariant is **not** a schema
constraint (the schema is frozen and there is no partial unique index); it is upheld *procedurally* by
the projection switch that retires N before promoting N+1 in one tx (05 §5, a later task), and the
`active_generations` reader — which returns only `state = 'active'` rows, so `retiring`/`failed` are
never routed — is what makes the invariant observable/assertable (T05-01 tests the well-sequenced
switch and a negative control). The `projection_ready → active` / `active → retiring` coupling to the
same tx that clears `worktree_projection_state` is the later switch, not `transition_generation`
alone.

## 2. Projection status (`worktree_projection_state.status`)

```
clean ──(step 2: write-ahead)──▶ updating ──(step 4: commit)──▶ clean
  │                                  │
  │                                  └─(any detected divergence at open)─▶ dirty ─▶ rebuilding ─▶ clean
  └──(validate-on-open failure)────────────────────────────────────────────▶ dirty
rebuilding ──(failure)──▶ dirty   (retry with backoff; permanent failure surfaces in last_error)
```

Invariants:

- `status='clean'` ⇒ `active == projected` tuple and `target` is NULL.
- `status='updating'` ⇒ `target` tuple and `projection_op_id` are set.
- Any observation contradicting the above at open ⇒ treat as `dirty` regardless of the stored
  value (the stored status is itself untrusted until validated, 05 §6).
- Only the per-worktree writer mutates this row; readers take snapshots under `L2.read`.

## 3. Model space (registry/build state, not deployment)

```
building ──▶ projection_ready ──▶ active ──▶ retiring
    └──▶ failed
```

- `building`: representations registered, embedding backfill in progress (coverage advisory).
- `projection_ready`: all `required` representation kinds have full coverage for the content
  they are expected to cover; benchmark may still be pending.
- `active`: eligible to be a `target_model_space_id`; the default space
  (`store_settings.default_model_space_id`) MUST be `active`.
- `retiring`: no longer selectable as target; still referenced by worktrees that have not
  reopened. A model space may be deleted only when no `worktree_projection_state` row
  references it in any column and no `embedding_cache` pins remain.

Per-worktree activation `[FIXED]`: an offline/dormant worktree migrates to the default space
at its next open (05 §8); there is **no global write barrier**.

As-built note (D-012, `[SPEC]`): "the default space MUST be `active`" is enforced from **both**
sides, because either alone leaves a hole. `set_default_model_space_id` (T11-05) refuses to point
the pointer at a non-active space; `transition_model_space` now additionally refuses to move an
`active` space *out of* `active` while it is the one the pointer names
(`ModelSpaceTransitionError::IsDefaultModelSpace`). Gate G11 found the second half missing: retiring
the default was a legal `active → retiring` edge, and the resulting store had a default no worktree
could migrate to — `dormant_migration_target` (05 §8) returns `None` when the default is itself
unusable, so dormant migration silently stopped instead of failing loudly. Refusing rather than
auto-repointing keeps the choice of default explicit and matches the order 10 §4 already fixes:
step 5 (`default := B`) precedes step 6 (`A → retiring`). The guard is scoped to a departure *from*
`active`, so it can never trap a store whose pointer already names a non-active space: walking that
space back up (`building → projection_ready → active`) stays legal, and self-transitions stay
idempotent no-ops.

The section's deletion rule ("a model space may be deleted only when no `worktree_projection_state`
row references it in any column and no `embedding_cache` pins remain") is **vacuously satisfied
today**: no code deletes `model_space` rows, and no v0 card calls for it. Whichever task introduces
a deletion path owns that precondition. Its filesystem counterpart — reclaiming the shard directory
of a space a worktree no longer references — is D-011 (05 §8).

As-built note (T11-04, `[SPEC]`): the coverage the `projection_ready` precondition reads is
computed by the backfill worker (`local_rag_embed::backfill::run_backfill`, 10 §3/§4 step 2) and
applied by `transition_model_space`, which reads the **stored** `model_space.coverage` — so the
promotion is always a separate `state.sqlite` transaction *after* the vectors are committed to
`cache.sqlite` (03 §1.4 forbids spanning both). "The content they are expected to cover" is fixed
as the retention pin roots (06 §5) unioned across every worktree; see 10 §3's own note. A run that
could not embed some subjects reports them as `failed`, never `ready`, so `Coverage::fully_covered`
stays false and this edge is refused with `IncompleteCoverage` rather than promoting a space whose
vectors are missing.

## 4. Consolidation run

```
pending ──(lease acquired)──▶ running ──(ops applied, cursor advanced)──▶ applied
                                 │
                                 ├─(router/LLM error)──▶ failed (retryable)
                                 └─(crash; lease expires)──▶ retried: new attempt re-enters
                                                            running under a fresh lease
```

- Lease: `lease_until = now + 120 s` `[SPEC]`, renewed every 30 s while the router runs.
- A `running` run with an expired lease is **retryable**: re-execution is idempotent because
  every op carries `idempotency_key = H(memory_op, run_id, op_index)` and the apply-tx skips
  keys already present in `audit_event` `[FIXED idempotency, mechanism [SPEC]]`.
- The LLM router call happens **outside** any long transaction `[FIXED]`; applying
  ops + evidence + audit + `processing_cursor` advance is **one short tx** `[FIXED]`.
- Router runs only on observations **at or below the cursor batch**; never past `to_received_seq`.

As-built note (T14-01, `[SPEC]`): the diagram draws the crash/lease-expiry retry as `running`
re-entering `running`, which the pure transition guard (`local_rag_store::memory::RunState::
check_transition`) gets for free from the project-wide "self-transition is always legal"
convention every state machine in this codebase honors — no separate edge needed. It also labels
`failed` itself "(retryable)" without drawing the edge. Since `idempotency_key = H(memory_op,
run_id, op_index)` requires a *stable* `run_id` across a retry (the bullet above) and nothing in
this section describes minting a replacement run for an in-progress window, `failed → running` is
realized as an explicit legal edge: the same row is retried under a fresh lease. `applied` stays
terminal (no edge leaves it). This task ships only the pure state-legality guard; the lease
`now_ms` comparison that decides *when* a `running` run is eligible for retry is T14-06's runner.

## 5. Memory entry — `kind` is origin (immutable), `state` is confirmedness `[FIXED]`

Common rule: a *confirmed hypothesis* stays `kind=hypothesis, state=confirmed` (recall/router
treat it as high trust); promotion to `fact` happens only via explicit `supersede` — a new
`fact` entry with `supersedes_id` pointing at the hypothesis, which transitions to `superseded`.

| kind | states | transitions |
| --- | --- | --- |
| task, question | `active → resolved \| retracted` | resolve on completing evidence or user action |
| hypothesis | `active → confirmed \| rejected \| superseded` | confirm on strong evidence; supersede on promotion |
| fact, decision, convention, procedure | `active → superseded \| retracted` | supersede = replacement entry; retract = withdrawn, kept for audit |

Terminal states (`resolved`, `retracted`, `rejected`, `superseded`) are excluded from recall by
default (08 §6) but remain queryable via review tools. Every transition writes an
`audit_event` with the incremented `entry_version`; optimistic concurrency uses
`expected_version` preconditions (08 §3).

As-built note (T14-01, `[SPEC]`): `memory_entry.state` carries no SQL `CHECK` (03 §2.5) — legality
is entirely a Rust-side guard, `local_rag_store::memory::MemoryState::check_transition(self, kind,
to)`, taking `kind` as well as `to` because the table above defines **three disjoint** machines,
not one shared state set (e.g. `confirmed` is legal only for `hypothesis` — a `fact` requesting it
is `Illegal`, not merely "not yet reached"). A corrupt/unknown stored `state` or `kind` surfaces as
a typed `rusqlite::Error::FromSqlConversionFailure`, mirroring the CHECK-backed machines elsewhere
in this crate. `local_rag_store::memory::transition_memory_entry` performs only the guarded `state`
write; it does **not** touch `entry_version`/`updated_at` — this section's own rule that every
version increment carries a matching `audit_event` means the two must commit together, and
composing that (plus evidence linking, the `expected_version` precondition, and idempotency-key
retry recognition) is T14-02's transactional memory-op engine, not this task's primitive.

As-built note (D-020, `[SPEC]`, found while planning T14-03): the "Common rule" paragraph above
the table narrates promotion acting on an *already-confirmed* hypothesis — "a confirmed
hypothesis stays `kind=hypothesis, state=confirmed`... promotion to `fact` happens only via
explicit `supersede`... which transitions to `superseded`" — but T14-01's shipped
`check_transition` only allowed `active → superseded` for `hypothesis`, leaving `confirmed` a
dead end no test exercised. Fixed by adding exactly `confirmed → superseded`; `confirmed →
rejected`/`retracted` have no textual basis (the table gives `reject` no role once confirmed,
and `retracted` isn't in `hypothesis`'s state set at all) and were deliberately not added. As-built
note (T14-03, `[SPEC]`): `local_rag_store::memory::op::apply_resolve`/`apply_retract` compose
`MemoryState::check_transition` directly (not `transition_memory_entry`) so the same call also
bumps `entry_version` and writes the matching `audit_event` this section requires; unlike the raw
`transition_*` primitives elsewhere in this crate, a legal *self*-transition through the op engine
still bumps the version and writes an audit row (consistent with `apply_reinforce`'s T14-02
precedent — every applied op returns a real `audit_id`). `apply_supersede` creates the new entry
first, then retires the old one to `superseded` second (matching this section's own sentence
order), pre-validating both sides before either write; only the new entry's `audit_event` carries
a router-supplied `idempotency_key`, so a retry never risks two rows colliding on the same key.

## 6. Pending memory candidate

```
pending ──▶ approved (materializes its proposed_operation as a normal memory op)
   │──▶ rejected
   └──▶ expired (retention policy; [SPEC] default 30 days)
```

Approval executes the proposed operation through the same transactional memory-op path as the
router (same audit, same idempotency), with `actor='user'`.

## 7. Worktree

```
active ⇄ detached          # detached: path no longer resolvable; reattach via `repo attach`
active|detached ──▶ removing ──▶ (deleted after shard/spool/GC cleanup with grace period)
```

Move of a directory does **not** create a new worktree: `local-rag repo attach <repo_id>`
re-binds the main worktree; ambiguous linked worktrees require explicit attach (common-dir /
admin-dir fingerprint may serve as a hint, never as the sole ID) `[FIXED]`.

## 8. Cross-machine lock/State interaction summary

| Operation | Locks | State rows touched |
| --- | --- | --- |
| reconcile → switch | L2.write, L4a | generation, generation_file, occurrences, worktree_projection_state |
| model-space switch | L2.write, L4a | worktree_projection_state (model axis) |
| shard open + validate | L2.write (brief) or open-path, L3, L4a (status fix-up) | worktree_projection_state |
| hybrid search | L2.read | none (reads snapshot) |
| consolidation apply | L4a only | memory_*, audit, cursor |
| GC sweep | L4a (batched), L2.write per worktree when touching shards | generation, file_revision, shards |

Generation-switch and model-space-switch are the **same protocol** serialized by the single
per-worktree writer; both axes never change in one operation `[FIXED]` — a combined request is
executed as two sequential switches.
