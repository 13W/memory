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

As-built note (T14-06, `[SPEC]`): the runner (`local_rag_store::memory::runner`) composes T14-01's
pure guard into two decisions this section's prose leaves open.

- **Lease fencing needs no new column.** `StateWriter::transaction` commits whenever its closure
  returns `Ok(_)` at the *outer* `rusqlite::Result` level, so a naive "loop over ops in one tx,
  return `Ok(Err(reason))` on a mid-batch rejection" would let an earlier op's mutation commit even
  though the run never reaches `applied` — the apply-tx's first action is therefore to re-read the
  run's current `(state, lease_until)` and require `state == running && lease_until == expected`
  (the value *this* attempt acquired or last renewed) before touching any op, refusing with zero
  mutation otherwise. This closes the real race a bare "stable `run_id` across a retry" leaves open:
  a slow-but-alive attempt whose lease has expired while mid-flight, racing a legitimate retry that
  re-acquired a fresh lease under the same `run_id` — without fencing, the stale attempt could apply
  under the fresh attempt's `idempotency_key` space. `lease_until` doubles as its own
  compare-and-swap token, mirroring the `expected_version` optimistic-concurrency idiom 08 §3
  already uses for `memory_entry` rows — no schema change. `open_next_run`'s own existence-check and
  insert already happen inside one transaction, so two callers opening for the same `session_id` are
  fully serialized by the single-writer queue with no separate TOCTOU window.
- **Any apply-time rejection routes straight to `failed`, not only a router/LLM error.** Since the
  apply-tx above is genuinely all-or-nothing, no partial-apply state is ever persisted between
  attempts — so a retry re-invokes the router from scratch regardless of *why* the previous attempt
  didn't reach `applied`. Marking `failed` immediately (rather than leaving the row `running` for a
  lease timeout to eventually rediscover the identical rejection) costs nothing under that
  invariant, and lets the next attempt's router see current state instead of reproducing a
  router/user race for up to the full lease duration. Consequently the startup/checkpoint retry
  sweep selects `failed` rows *and* lease-expired `running` rows, not lease-expiry alone.

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

As-built note (D-079, `[SPEC]`, found while clearing the duplicates of D-078): this table has
declared `hypothesis: active → confirmed | rejected` since idea.md rev 6, and T14-01's
`check_transition` has accepted both edges since — but until D-079 **no operation could reach
them**. The op surface was `create`/`reinforce`/`resolve`/`retract`/`supersede`/`edit`/`merge`/
`noop`; the generic `transition_memory_entry` primitive was only ever called with `Resolved` in
production, and the sole place anything reached `confirmed` at all was a raw-primitive helper
inside one test. A hypothesis could therefore only be born and then absorbed by a `merge` —
"confirm on strong evidence", the whole reason the `hypothesis` kind exists, belonged to nobody.
Closed by `local_rag_store::memory::op::apply_confirm`/`apply_reject`, thin wrappers over the same
`apply_state_transition` `apply_resolve`/`apply_retract` use, so version bump and `audit_event`
commit together exactly as this section requires; surfaced as MCP `confirm_memory`/`reject_memory`
and CLI `local-rag memory confirm`/`refute` (11 §6 — `memory reject` was already taken by
candidate review, a different table). The two edges D-020 deliberately left out stay out:
`confirmed → rejected` is still illegal, so `reject` is not an "undo confirm" and the only exit
from `confirmed` remains `supersede`. The router's op vocabulary is **unchanged** — these are
review-tool verbs like `edit`/`merge`, and "when is evidence strong" is a product decision this
deviation did not settle.

## 6. Pending memory candidate

```
pending ──▶ approved (materializes its proposed_operation as a normal memory op)
   │──▶ rejected
   └──▶ expired (retention policy; [SPEC] default 30 days)
```

Approval executes the proposed operation through the same transactional memory-op path as the
router (same audit, same idempotency), with `actor='user'`.

As-built note (T14-05, `[SPEC]`): `pending_memory_candidate.proposed_operation` (03 §2.5's "JSON:
op + target + text + …") is a tagged JSON enum, `local_rag_store::memory::ProposedOperation`
(`#[serde(tag = "op", rename_all = "snake_case")]`), restricted to the five router ops that are
ever *materializable* — `create`/`reinforce`/`resolve`/`retract`/`supersede`. `noop` (08 §4) writes
nothing and `propose_candidate` cannot nest itself, so neither is ever a proposal shape; `edit`/
`merge` are direct review-tool ops (08 §5, 11 §2's table), never candidate-proposed. `kind`/
`scope_kind` travel as plain strings inside the JSON, parsed via the existing
`MemoryKind::from_db`/`ScopeKind::from_db` at `approve_candidate` time — deliberately not adding
`serde` derives to those T14-01 types. `memory_id`(s) are minted by the proposer and embedded in
the payload at propose time, the same "caller mints the id, never inside the write path"
discipline every other memory op already follows.

`approve_candidate` derives the materializing op's evidence from `candidate_evidence`'s FK to
`observation_envelope` rather than storing it a second time: `candidate_evidence` carries no
`evidence_kind`/`session_id` of its own (unlike `memory_evidence`), so each linked observation's
own `evidence_kind`/`session_id` (already durable from T13-04) becomes that observation's
`EvidenceInput` — the same "FK provenance, not embedded snapshots" principle 03 §2.5 states for
`candidate_evidence` generalized to what it feeds.

An already-`approved` candidate short-circuits `approve_candidate` to `AlreadyApproved` before any
JSON parsing or op-engine call — the state machine's own self-transition-is-legal convention is
the primary double-approval guarantee, cheaper than a `find_by_idempotency_key` round-trip. As
defense-in-depth for a crash mid-transaction, the dispatched op still carries a deterministic
`idempotency_key` (`"candidate:<candidate_id>"`), so a retry that reaches the op-engine layer
resolves through 08 §3's replay mechanism regardless. `reject`/`expire` on an already-terminal
candidate stay ordinary illegal-transition rejections; only `approve → approve` gets this
treatment, since it is the only case 04 §6's card names.

`pending_memory_candidate` carries no `entry_version`/`updated_at` (unlike `memory_entry`), and 11
§2's own `edit_memory_candidate(id, patch)` signature (contrast `edit_memory(id, patch,
expected_version)`) confirms this is intentional. `edit_candidate`'s "conflicting edit" check is
therefore state-based, not version-based: legal only while `review_state = 'pending'`; editing an
already-approved/rejected/expired candidate is rejected with no mutation.

Expiry (this section's `[SPEC]` default 30 days) is a batch sweep,
`local_rag_store::housekeeping::run_candidate_expiry_sweep` (`CANDIDATE_EXPIRY_MS`), mirroring
this crate's other GC-style sweeps (07 §6's spool session GC, 05 §8's shard sweeps) rather than
living per-domain — the first sweep in that module with no filesystem component, since a stale
`pending` row is pure DB state. It tolerates, as a retained row rather than a sweep failure, a
candidate a concurrent `approve`/`reject` already moved out of `pending` between the sweep's read
pass and its own write attempt.

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
