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
