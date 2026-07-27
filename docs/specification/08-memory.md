# 08 — Memory

## 1. Model recap (normative source: 03 §2.5, 04 §5)

- `kind` = origin, immutable; `state` = current confirmedness `[FIXED]`.
- Scope: `global | repository | worktree`, `scope_owner_id NOT NULL` (global singleton UUID)
  `[FIXED]`; uniqueness `(scope_kind, scope_owner_id, canonical_key)` partial on non-null keys.
- Memory is scoped to `repo_id` (or global/worktree); the code index is scoped to
  `worktree_id + generation_id` `[FIXED]` — never mix the two scopes.

## 2. Confidence is a policy score, not an LLM probability `[FIXED]`

```
confidence = clamp01( base(source reliability)
           + w_user   · explicit user decision
           + w_tool   · tool/test evidence
           + w_repeat · independent repetition
           + w_code   · agreement with current code state
           − w_contra · contradictions
           − w_model  · model-claim-only provenance
           − w_stale  · staleness vs last_verified_tree )
```

Weights are constants in the router config, versioned by `router_version` `[SPEC values TBD
with the fixture set]`. `reinforce` may raise confidence; it never edits text (§3).

## 3. Transactional memory operations `[FIXED]`

Operations: `create | reinforce | resolve | supersede | retract | noop`
(+ `edit`, `merge` from review tools). Contract for every operation:

- Single `state.sqlite` tx containing: the entry mutation, `memory_evidence` rows,
  `audit_event` (with `idempotency_key` when router-originated), and — for consolidation —
  the `processing_cursor` advance.
- **Preconditions**: `expected_version` (optimistic concurrency); kind/state machine legality
  (04 §5); scope uniqueness. Violation ⇒ tx aborts with a typed error.
- Response carries the new `entry_version` and `audit_id`.
- **Retry is idempotent**: same `idempotency_key` ⇒ recognized as already applied, returns the
  original result.
- `reinforce`: adds evidence, may raise confidence, never changes text `[FIXED]`.
- `edit`: new `entry_version` via audit; `actor` distinguishes user-edit vs router-edit `[FIXED]`.
- `retract` ≠ delete: entry survives for audit; hard removal exists only as an explicit privacy
  `purge` (12 §5) which also rewrites audit references to tombstones `[SPEC]`.
- `merge_memories`: one tx — survivor absorbs evidence, losers → `superseded` with
  `supersedes_id` → survivor; audit records the merge set.

As-built note (T14-02, `[SPEC]`): `local_rag_store::memory::op` ships exactly `create`/
`reinforce`/`noop` — the shared transactional engine `resolve`/`supersede`/`retract`/`edit`
(T14-03) and `merge_memories` (T14-04) compose on top of. Three implementation details this
section states less precisely than the code fixes:

- **Idempotency-key check runs first**, inside the same transaction, before any other
  precondition (`op::apply_create`/`apply_reinforce` call `audit::find_by_idempotency_key`
  before touching `memory_entry`). A hit reconstructs the response directly from the matching
  `audit_event` row and touches nothing else — this is what makes "returns the original result"
  true by construction rather than by re-deriving equal output on every retry.
- **`create`'s scope-uniqueness precondition is a typed error**, per this section's own wording
  ("Violation ⇒ tx aborts with a typed error") — `op::apply_create` pre-checks `canonical_key`
  with a `SELECT` and returns `MemoryOpError::CanonicalKeyConflict` before ever attempting the
  insert, rather than letting `create_memory_entry`'s raw `UNIQUE` constraint violation bubble up
  unwrapped (that primitive's own doc comment explicitly defers this typed wrapping to this
  task).
- **`noop` writes nothing at all** — no `memory_entry` mutation, no `memory_evidence`, no
  `audit_event`. The op envelope in §4 below lists `target/kind/text/scope/canonical_key/
  confidence inputs` for the op list generally; `noop` needs none of them, unlike every other
  listed op. Recording a `noop` as its own `audit_event` would need some
  `(entity_kind, entity_id, entity_version)` to satisfy `audit_event`'s own `UNIQUE` constraint
  (03 §2.5) — attaching it to the examined entry's current, unchanged version breaks the first
  time two independent consolidation runs both examine that same still-unmodified entry and both
  legitimately decide "no action": the second, equally valid decision would be rejected as a
  duplicate of the first. A zero-write `noop` sidesteps this entirely and needs no
  `idempotency_key` bookkeeping either — redoing nothing on retry is still nothing.
- **`reinforce` always bumps `entry_version`** on a successful apply, even when `confidence` is
  unchanged (evidence-only reinforcement) — every real mutation gets a new version with a
  matching `audit_event`, consistent with "Response carries the new `entry_version`" being
  stated for every operation, not only state transitions. It does not check the entry's current
  `state`/terminality — that guard, if warranted, belongs to T14-03's kind/state-aware lifecycle
  operations, which this task's card does not cover.

As-built note (T14-03, `[SPEC]`): `resolve`/`retract` compose 04 §5's
`MemoryState::check_transition` directly, so an illegal kind/state request (e.g. resolving a
`fact`, retracting a `hypothesis`) surfaces as a typed `MemoryOpError::IllegalTransition` wrapping
that guard's own error, with **no mutation** — the same "typed error, tx aborts" contract this
section already states for scope uniqueness. `supersede` creates the **new** entry first, then
retires the **old** one to `superseded` second (matching 04 §5's own prose order for promotion),
both pre-validated before either write; its response describes the **new** entry only — read as
the "new `entry_version` and `audit_id`" this section promises, since the new entry is the
headline result a promotion produces — while the old entry's transition is a verified side effect,
not a second value in the return type. Only the new entry's `audit_event` row carries a
router-supplied `idempotency_key`; the old entry's transition-audit row does not, so a replay
never needs a second row to collide on the same key. `edit` is the one operation that changes
`text` (structurally — no other op accepts a `text` field) and adds a guard this section does not
itself specify: it rejects editing an entry whose current state is terminal
(`MemoryOpError::EntryTerminal`) — an as-built decision, since this task's card is the one that
owns "kind/state guards" generally and nothing here forces this specific rule.

## 4. Consolidation `[FIXED]`

Trigger: checkpoint on `Stop`, queue-size threshold, best-effort `SessionEnd`, startup catch-up.

```
1. tx: create consolidation_run(pending→running, lease_until), snapshot
       [from_received_seq = cursor+1, to_received_seq = min(cursor+batch, max_seq)]
2. Load envelopes (+ surviving payloads) of the window.
3. ROUTER (LLM) — OUTSIDE any long tx. Input: window observations + recall of plausibly
   related existing entries (candidate conflict set). Output: ordered ops list, each
   op ∈ {create, reinforce, supersede, resolve, retract, noop, propose_candidate}
   with target/kind/text/scope/canonical_key/confidence inputs.
4. ONE short tx: apply ops (idempotency_key = H(memory_op, run_id, op_index)),
   evidence links, audit, advance processing_cursor, run→applied.
5. Crash anywhere ⇒ run retried after lease expiry; step 4 idempotent per op.
```

Router placement rules `[FIXED]`:

- **Auto-save only for explicit durable decisions/instructions.** Questions, brainstorms,
  negations, temporary suggestions → `propose_candidate` (pending review) or `hypothesis`.
  The router prompt + fixtures explicitly distinguish "we decided X" / "what if X?" /
  "do not use X".
- Model-claims are never auto-promoted to facts `[FIXED]` (trust marking, 12 §4).
- Under `data_policy=local_only` the router runs on the **local generator** `[FIXED]` — which
  is precisely why the quality gate (§7) exists.
- Router runs strictly behind the cursor `[FIXED]`.

## 5. Explicit tool-initiated memory (`remember`, review tools)

`remember` (11 §2) is an explicit durable operation: creates an `active` entry, `actor='user'`
when the human confirmed, else `actor='router'`-equivalent trust with `evidence_kind=
model_claim` `[SPEC]`. It passes through the same transactional path as router ops.

## 6. Recall v0 `[FIXED pipeline]`

```
scope resolution: global ∪ repository(worktree→repo) ∪ worktree
→ candidate set: entries in recall-eligible states
   (active; hypothesis/confirmed flagged as hypothesis-confirmed;
    terminal states excluded)
→ relevance:  RRF( FTS over memory text ,
                   brute-force cosine over embedding_cache memory vectors )
              bounded cardinality (guarded [SPEC ≤ 20k entries]); same
              representation_id as the active memory representation; behind the
              relevance-backend trait — ANN replaces brute-force ONLY on
              cardinality/latency metrics, not by default [FIXED]
→ lifecycle filters → token budget [SPEC default 1500 tokens, config]
→ deterministic ordering (score desc, created_at desc, memory_id) [v1 contract]
→ empty result ⇒ empty additionalContext (no text at all) [FIXED]
```

Model-space migration covers the memory representation exactly like code `[FIXED]`.

Full recall (deferred, additive): + tree-validity/provenance → evidence trust → weak
recency/importance → diversity/dedup, top 20–50 `[FIXED deferral]`.

**Recalled memory is untrusted data** — encoding and prompt rules in 12 §4.

## 7. Memory-quality benchmark `[FIXED, new in rev 6]`

A labeled fixture set of observation streams → expected memory ops
(`create | reinforce | supersede | noop`), explicitly covering decision vs hypothesis vs
negation, and RU/EN mixed transcripts. Precision/recall of the consolidation router on this
set is an acceptance gate (14 §2) on par with the 49-query code-search benchmark. Target P/R
numbers are set after the baseline run `[OPEN]`. Without this, the memory pillar has criteria
only for plumbing — the gate exists to prevent that.

## 8. Review tools (surface in 11 §2)

`list_memory`, `list_memory_candidates`, `approve_memory_candidate`,
`reject_memory_candidate`, `edit_memory_candidate`, `edit_memory`, `retract_memory`,
`merge_memories`, `inspect_memory_evidence` `[FIXED set]`. All mutations run through §3; all
list operations expose `entry_version` so edits can carry preconditions.
