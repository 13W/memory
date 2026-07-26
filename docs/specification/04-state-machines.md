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
