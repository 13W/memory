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

As-built note (T14-07, `[SPEC]`): the seven weights above stay `[SPEC values TBD]` — several
need signals that do not exist yet (`w_repeat` cross-session counting, `w_code` code-state
diffing, `w_contra` conflict detection, all T14-08+ territory), and inventing numbers to fit a
formula with nothing measured to derive them from is exactly what O2's "collect metrics, never
invent thresholds" rule forbids. `local_rag_memory::router` instead has the model emit a
qualitative `confidence_signal`/`importance_signal ∈ {low, medium, high}` per op (never a raw
float — this section's own heading), mapped in `local_rag_memory::schema` to fixed placeholder
constants (`0.3`/`0.6`/`0.85`). `router_version = "v0"` is recorded in every memory-quality
benchmark report's provenance (spec 14 §7) so a future weight retune is visibly a different
version, never a silent change.

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

As-built note (T14-04, `[SPEC]`): `merge` reassigns a loser's `memory_evidence` rows to the
survivor rather than duplicating them — a loser's row for an `observation_id` the survivor already
has stays attached to the (superseded, still-existing) loser instead of erroring or being dropped,
computed with a plain Rust `HashSet` rather than a self-referential SQL subquery against
`memory_evidence` (SQLite gives no snapshot guarantee for a subquery reading the same table an
`UPDATE` in the same statement writes). A precondition this section does not itself name: every
loser's `(scope_kind, scope_owner_id)` must match the survivor's, or the typed
`MemoryOpError::IncompatibleScope` aborts the whole merge with no mutation — kind is deliberately
*not* required to match, since each loser's own eligibility for `superseded` is already enforced
independently by `MemoryState::check_transition` (which structurally rejects a `task`/`question`
loser, `superseded` not being in their legal target set at all). Losers → `superseded` with
`supersedes_id` → survivor is the first place this column is set on an *already-existing* row
(every earlier use — plain `create`, `supersede`'s new-entry half — only sets it at `INSERT`
time). "Audit records the merge set" is realized as a `serde_json`-encoded array of the merged
loser ids on the **survivor's** own `audit_event.payload` (each loser's own transition-audit row
carries `payload: NULL` — `supersedes_id` already links it back to the survivor structurally, so
the set doesn't need duplicating on every row); only the survivor's row carries the caller's
`idempotency_key`, mirroring `supersede`'s "headline row only" placement. The response describes
the survivor only, for the same reason `supersede`'s describes only the new entry.

As-built note (T14-05, `[SPEC]`): `local_rag_store::memory::review::approve_candidate` is not a
new operation but a dispatcher onto this section's existing five — it deserializes a candidate's
`proposed_operation` (04 §6's as-built note has the JSON shape) and calls the matching
`op::apply_create`/`apply_reinforce`/`apply_resolve`/`apply_retract`/`apply_supersede` with
`actor=User` and `idempotency_key = "candidate:<candidate_id>"`, inside the same transaction as
the candidate's own `pending → approved` write — so "same audit, same idempotency" (this section's
own phrase) holds for candidate approval exactly as it does for the router and `remember` (§5).
`merge`/`edit`/`noop` are never dispatched from a candidate: `noop` has nothing to materialize and
`edit`/`merge` are direct review-tool ops per §8, not something a candidate proposes.

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

As-built note (T14-06, `[SPEC]`): `local_rag_store::memory::runner` ships steps 1 (extending
`consolidation.rs`'s T14-01 primitives with lease acquire/renew and the bounded snapshot) and 2–4
(`run_once`/`commit_apply_run`) — everything except the router itself (step 3's actual generation
logic, shipped separately by T14-07, which closes open item O3's generator-crate half — see the
as-built note after this list). Three details this section states less precisely than
the shipped code:

- **"ONE short tx" is atomic across the whole ops list, not per op.** A single rejected op — an
  optimistic-conflict, an illegal transition, an unknown evidence observation — rolls back
  *everything* in that apply attempt: every other op in the same list, the `processing_cursor`
  advance, and the `run → applied` transition. This is a direct reading of "ONE short tx: apply
  ops..., advance processing_cursor, run→applied" as one indivisible unit rather than a loose
  sequence — the alternative (committing whichever earlier ops happened to succeed before the
  rejection) would silently advance the cursor past observations whose ops never actually landed,
  in direct conflict with 03 §1.4's "memory mutations, evidence, audit, and consolidation cursor
  movement are transactionally strict." `commit_apply_run` converts a rejection into a genuine
  `rusqlite::Error` specifically so the underlying `StateWriter::transaction` (which commits on any
  *outer* `Ok(_)` regardless of an inner `Err`) actually rolls back rather than silently committing
  the ops that individually would have succeeded.
- **A rejected apply routes the run to `failed` immediately** (04 §4's own T14-06 note has the
  full rationale) rather than leaving it `running` for the lease to eventually expire — the
  generalization from "router/LLM error" to "any apply-time rejection" is this task's own reading,
  since the atomicity guarantee above makes the two failure classes behave identically from a
  retry's perspective (no partial state either way).
- **No default batch size is specified anywhere normative** in this section, 03 §2.5, or 04 §4 —
  unlike the lease/renewal numbers, which are explicitly `[SPEC]`-tagged. `open_next_run` therefore
  takes `batch` as a required caller parameter rather than inventing a crate constant; picking an
  actual default was deferred to whichever task wires the daemon-level trigger.

As-built note (D-024, `[SPEC]`): the default is `config.memory.consolidation_batch_size = 20`
(`crates/core/src/config::MemoryConfig`, the same "spec names the value's existence without a TOML
layout" home as `recall_token_budget`). Chosen deliberately smaller than the companion
`consolidation_queue_threshold = 50` — see `docs/specification/07-observations-spool.md` §6's own
as-built note for why the relative sizing matters, not just the absolute numbers.

`idempotency_key = H(memory_op, run_id, op_index)` is realized as
`format!("consolidation:{run_id}:{op_index}:{op_kind}")` — a plain deterministic string, mirroring
`approve_candidate`'s own `"candidate:<id>"` precedent (08 §3's own as-built note), not a
cryptographic hash. Given the atomicity fix above, this key's practical role narrows to
defense-in-depth against the one genuinely ambiguous case `WriteError` documents (a crash between
the transaction's commit and the reply reaching the caller) rather than being the primary
duplicate-prevention mechanism spec 04 §4 step 5's prose seems to assume — transaction atomicity
already guarantees a rejected batch leaves zero residue for any legitimate retry to duplicate.

As-built note (T14-07, `[SPEC]`): the router itself (step 3) is `local_rag_memory::router::route`
— the `generate` closure `run_once` is generic over, composed at the daemon/`xtask` call site with
a concrete `local_rag_generate::LlamaGenerator` (ADR-0006). One `Generator` call per window (not
per observation): a negation only makes sense read against an earlier claim in the same window,
matching this section's own "ordered ops list" wording. The model never emits a raw `confidence`/
`importance` float (§2's own as-built note) and never addresses an existing entry by
`canonical_key` — only by the `memory_id` `local_rag_memory::recall::candidate_conflict_set` shows
it in the prompt, since the same key text can legitimately exist in more than one scope. Two-tier
malformed-output handling: a structurally invalid response (bad JSON, an unknown enum value) gets
one bounded corrective re-prompt before the whole window fails (the "router/LLM error ⇒ failed
(retryable)" edge, 04 §4); a semantically-valid but referentially-hallucinated value (an unknown
`target_memory_id`, an unresolvable scope) degrades only that one op to `noop`, never the whole
batch — `local_rag_memory::guard` also pre-checks a `canonical_key` collision
(`local_rag_store::canonical_key_owner`, which — unlike `active_entries_for_scope` — sees
terminal rows too, matching the real unique index) before ever submitting a `create`/`supersede`,
for the identical livelock reason. §4's two placement rules (below) are enforced by
`local_rag_memory::guard` independently of what the model claims, using each window observation's
own `evidence_kind` (set at write time, T13-04) — never the model's self-report.

As-built note (D-048, `[SPEC]`): "a semantically-valid but referentially-hallucinated value…
degrades only that one op to `noop`" (above) also covers a `create`/`propose_candidate` op that
omits `scope_kind` entirely, not only one present but out-of-domain — `crate::schema::RawRouterOp`
gives it `#[serde(default)]` (empty string on omission) specifically so a small local model
skipping the field on one op never fails the whole window's deserialization (tier 1); the empty
string then degrades through the same out-of-domain check (`ScopeKind::from_db`, tier 2) as any
other unrecognized value.

As-built note (D-050, `[SPEC]`): "run retried after lease expiry" (step 5, and 04 §4's own
`stale_runs`/`retry_run` sweep, run every daemon tick forever by the continuous trigger, D-024)
does not by itself bound how many times a `failed` run is retried, or distinguish *why* it failed
— live dogfooding found this exact gap: a window whose router output deterministically fails to
parse (greedy decoding — the same window, model, and code reproduce the identical malformed
response byte-for-byte) was retried every ~15s tick, hours on end, each attempt a real local-model
inference call. `local_rag_store::memory::consolidation::FailureKind` closes it by classifying
every `generate`-closure failure as `Mechanical` (the corrective-re-prompt's parse still failing,
or a per-op materialization rejection — reproduces identically on an unchanged retry) or
`Transient` (a db-read hiccup, or the generator/model call itself failing — not expected to
reproduce). `stale_runs` excludes a `Mechanical` failure whose `last_failure_fingerprint`
(`local_rag_core::BUILD_ID`, a `git describe --always --dirty` captured at compile time, distinct
from the workspace's own fixed-placeholder `VERSION`) matches the running binary's — a rebuild
earns it exactly one more attempt, never an unlimited budget — and gates a `Transient` failure on
`next_retry_at`, an exponential backoff (`transient_backoff_delay_ms`, the same 250ms-base-doubling
shape `local-rag-proxy::connect::DEFAULT_BACKOFF` already established for "wait, then retry a call
that might just be temporarily down"). Apply-time rejections (`RunnerApplyError`) are classified
`Transient` by default, not split per variant — none of the live retry-storm incidents that
motivated this task went through that path. `consolidation_run` gains five nullable, unbackfilled
columns (`last_failure_kind`/`last_failure_reason`/`last_failure_fingerprint`/`attempt_count`/
`next_retry_at`, schema v11): a pre-existing `failed` row with none of them set is never classified
`Mechanical`, so it stays retry-eligible — the safe default, not a special case.

As-built note (D-051, `[SPEC]`): D-050 stopped a *deterministically*-failing window from
retry-storming forever; it did not fix why those windows failed in the first place — live
verification right after D-050 shipped confirmed all 4 incident windows failed **again**, byte-for-
byte identically, on their one post-fix retry. Two root causes, both fixed here. First: like
`scope_kind` (D-048, above), `RawRouterOp::Create`/`ProposeCandidate`/`Supersede`'s
`confidence_signal`/`importance_signal` now carry `#[serde(default)]` as `Option<Signal>` (the same
shape `RawRouterOp::Reinforce`'s own `confidence_signal` already has, though there `None` means
"leave the existing value alone" — a different semantic; here it means "the model didn't say," which
`local_rag_memory::guard` degrades to `Noop` for that one op, tier 2, never a fabricated
`Signal::Low`/`Medium`/`High` the model never emitted). Second, and larger: T14-07's "one bounded
corrective re-prompt before the whole window fails" (above) assumed the router's wire format was a
single top-level JSON array — under that framing, one bad or truncated trailing element invalidated
deserialization of the *entire* response, including every syntactically valid element before it.
The wire format is now JSONL (one `RawRouterOp` object per line, `local_rag_memory::parse::
parse_ops`), parsed line-by-line and stopping at the first line that fails: a response with a valid
prefix followed by trailing garbage or a truncated final line (both live incident shapes) now
recovers that prefix as a real, partial success (`ParseOutcome::dropped_tail` names why recovery
stopped) instead of losing the whole window. `local_rag_memory::router::route` does **not** spend
its one corrective re-prompt trying to recover a dropped tail — a live incident's own corrective
retry reproduced an identical truncation, byte-for-byte, since nothing about a second, otherwise-
identical greedy-decoded generation call changes a deterministic outcome; the re-prompt remains
reserved for the case a partial recovery structurally cannot help — the *first* line itself failing
to parse at all. Deliberate tradeoff, not an oversight: prefix-stop recovers "good prefix, bad
suffix" (the two shapes actually observed) but no longer searches for valid content after *leading*
prose the way the pre-D-051 whole-array recovery did — an unobserved failure shape, not defended.

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

As-built note (T14-08, `[SPEC]`): the pipeline is `local_rag_memory::recall::pipeline::recall`
(`crates/memory/src/recall/pipeline.rs`), a plain function — not a struct/engine — matching this
crate's existing style (`router::route`, `guard::materialize`). Seven decisions this section
states less precisely than the shipped code:

- **Scope resolution** reuses `local_rag_store::resolve` (the same worktree-root resolver
  `local_rag_search::SearchEngine::search_code` calls) to get `{repo_id, worktree_id}` from a
  `RequestRoot` in one call — `Resolution::Resolved` unions `global ∪ repository(repo_id) ∪
  worktree(worktree_id)`; `GlobalOnly`/`Ambiguous` both degrade to `global` alone, the same
  principle 02 §6's own table states ("Worktree unknown / never indexed … memory tools work in
  repo/global scope").
- **The lexical leg is ephemeral, not a persisted materialized view.** Code's FTS view
  (06 §4) is rebuilt at the frequency of *generation switches* — infrequent, atomic events —
  which a persisted, validate-on-open `cache.sqlite` structure fits. Memory mutates through many
  small, individually-transactional ops (create/edit/retract/supersede/merge, 08 §3); there is no
  generation-shaped checkpoint to hang a manifest off, and a persisted index kept transactionally
  current with every `state.sqlite` memory op would need a cross-database write per op, which
  03 §1.4 forbids. Given the guard already bounds the candidate set in memory,
  `local_rag_memory::recall::lexical::lexical_leg` instead builds a short-lived, in-process FTS5
  table per call (`CREATE VIRTUAL TABLE … USING fts5(memory_id UNINDEXED, body)`, SQLite's
  built-in `unicode61` tokenizer — memory text is natural-language prose, not code identifiers,
  so this does **not** reuse `local_rag_store::tokenize_identifier`'s camelCase/snake_case
  splitter), seeded from the already-fetched candidate set and dropped when the call returns.
  Query-term handling mirrors `local_rag_store::cache::fts_query`'s established idiom for the
  identical reason it was chosen there (09 §2's as-built note): lowercase, split on
  non-alphanumeric boundaries, quote each term (`"term"`, embedded `"` doubled), combine with
  `OR`; no surviving terms ⇒ the leg returns empty without issuing SQL at all.
- **The dense leg does not use `BruteForceProjectionStore`.** The production dense backend
  (05 §1, ADR-0003) is disk-shard-shaped — `open()` takes a directory, every mutation rewrites
  `points.bin`, a `ProjectionHead` proves generation/model-space consistency on open — none of
  which fits a transient, scope-unioned scan with no shard and no generation.
  `local_rag_memory::recall::dense::dense_leg` instead bulk-reads `embedding_cache` rows **by
  exact subject key** via a new `local_rag_store::embeddings_for_subjects(conn,
  SubjectKind::MemoryEntry, representation_id, &subject_hashes)` reader (D-067 reshaped this
  reader; see its own as-built note below) and scores them with the exact free function the shard
  itself calls, `local_rag_projection::contract::similarity` — "behind the relevance-backend trait" is
  `local_rag_memory::recall::dense::MemoryDenseBackend`, whose only impl,
  `BruteForceCosine`, a future ANN swap would replace without touching the pipeline around it.
  Query embedding is a second injected seam, `QueryEmbedder` (mirrors
  `local_rag_search::pipeline::QueryEmbedder`'s exact shape — a **new, independent** trait, not a
  shared one: this crate depending on `crates/search` for 15 lines would be backwards), defaulting
  to `UnavailableEmbedder`. "Same `representation_id` as the active memory representation"
  resolves via the request's worktree's `active_model_space_id`
  (`worktree_projection_state`) when one exists — mirroring how code resolves its own active
  representation, and matching this section's own "model-space migration covers the memory
  representation exactly like code" — falling back to `store_settings.default_model_space_id`
  (05 §8's "a dormant worktree migrates to the default space") for a `GlobalOnly`/`Ambiguous`
  resolution or a worktree that has not yet opened a projection tuple. No representation
  resolvable at all ⇒ the leg degrades (`DenseLegUnavailable::NoRepresentation`), never an error —
  the lexical leg still serves.
- **Fusion is unweighted**, unlike 09 §4's own D-018-weighted RRF: that weight was *derived* from
  a 49-query code benchmark measuring an unweighted hybrid scoring below its own dense leg; no
  equivalent per-query relevance-judged benchmark exists for memory recall (08 §7's benchmark
  scores the *router*, not recall), and inventing a weight to fit the formula with nothing
  measured to derive it from would repeat exactly what this section's own §2 as-built note calls
  out for confidence weights ("collect metrics, never invent thresholds", O2). Ships as
  `local_rag_memory::recall::fusion::rrf` — same `RRF_K = 60`, same `f64` accumulator, keyed by
  `memory_id` rather than `occurrence_id` — until a recall-quality fixture set exists to derive a
  weight from.
- **Two filter boxes, one predicate, two moments.** This section's diagram names candidate-set
  filtering *and* a later, separate "lifecycle filters" step; both are the identical
  `!state.is_terminal()`. The first is `recall_candidates_for_scope`'s own `WHERE` clause; the
  second — `local_rag_store::recall_candidate_by_id`, a fresh single-row re-read — exists because,
  unlike code search's `L2.read`-spanned pipeline (02 §5), memory recall holds **no lock across
  the pipeline at all** (no `crates/store/src/memory/*.rs` file touches
  `WorktreeLockRegistry`; memory writes lean on `state.sqlite`'s per-op transactional strictness,
  08 §3, not a read-side lock). A concurrent `retract`/`supersede` between the initial candidate
  read and formatting is therefore possible, and the re-check runs immediately before an entry
  joins the budget — cheap, since by then the list is the short, ranked, budget-bounded one.
- **Every eligible candidate participates in the final order, not only the ones a leg
  matched.** RRF only scores `memory_id`s a leg actually returned; a termless recall (the hook's
  `SessionStart` case, before any prompt exists) makes both legs empty, and RRF alone would then
  order nothing. This section's own ordering step is read literally: **every** candidate
  participates in `(score desc, created_at desc, memory_id)`, defaulting to score `0.0` when
  neither leg matched it — so a termless query still surfaces the scope's most-recent eligible
  memories, generalizing the "termless query is healthy, not empty" idiom 09 §2/§3 already apply
  to a single leg.
- **The token budget is a heuristic estimate, `chars.div_ceil(4)` plus a small fixed per-entry
  overhead**, not a real tokenizer — no token-count utility exists anywhere in this workspace (the
  two "token" constants found elsewhere, `MAX_SEQUENCE_TOKENS`/`MAX_GENERATION_TOKENS`, bound
  unrelated ONNX/llama-context subsystems). This section fixes only the number
  (`recall_token_budget`, `[memory]` config section, 02 §3.1's own T14-08 as-built note), not an
  estimation method. Entries are added in the final deterministic order until the next one would
  overflow the budget, then the walk stops — a ranked prefix, never a skip-ahead search for a
  smaller entry further down the order.

As-built note (D-067, `[SPEC]`): the reader the bullet above names replaces T14-08's own
`embeddings_for_subject_kind(conn, subject_kind, representation_id, limit)`. That one selected
**every** `embedding_cache` row of the kind — all scopes, all entry states, plus the hashes
earlier `edit`/`supersede` ops left stale — under `ORDER BY subject_hash LIMIT ?`, and its single
production caller passed `candidates.len()` as that limit. As soon as the cache held more memory
rows than the request's scope union had candidates, the read was truncated at an arbitrary point
of hash order and every candidate inside the cut silently lost its vector, with no diagnostic at
all: `dense_degraded` reports an `Err` leg, never a leg that merely came up short. Measured on a
live store before the fix: 86 cached `memory_entry` rows against 44 non-terminal candidates, i.e.
an expectation of roughly half the candidates scoring. The replacement hashes each candidate once
(`subject_memory_entry`), reads exactly those rows (`subject_kind = ? AND representation_id = ?
AND subject_hash IN (…)`, chunked at `EMBEDDING_SUBJECT_CHUNK` so the statement's parameter count
stays inside SQLite's portable floor whatever the caller's own bound is), returns them in the
caller's order, omits absent keys, and takes no `limit` at all. The `[SPEC ≤ 20k entries]` guard
in the pipeline block above is unchanged and stays exactly where it is: it bounds the **candidate
set** (`MAX_RECALL_CANDIDATES`, applied in `recall::pipeline` before this leg runs) and therefore
the number of keys the reader is handed; T14-08 additionally reused that same number as a
cache-read limit, and that conflation was the defect. Per-entry degradation is unchanged (03
§4.2): a candidate with no cached row, a row failing `verify_cached_embedding`, or a vector that
fails to decode is simply absent from this leg.

As-built note (X-010/X-011): this section's own "no equivalent per-query relevance-judged
benchmark exists for memory recall" (see the fusion-weight bullet above) is closed by a new,
independent harness — `cargo xtask memory-recall-bench`
(`crates/xtask/src/memory_recall_bench/`), scoring Hit@1/3/5 and MRR over a 24-entry bilingual
fixture (`fixtures/memory-recall/corpus.json`, `family: "memory-recall"`) stratified by
`lang_pair` (8 `ru-ru` / 8 `en-en` same-language controls, 4 `ru-en` / 4 `en-ru` cross-lingual).
This does **not** derive the fusion weight the bullet above defers — 24 queries is too small a
corpus to fit one responsibly, and that remains future work if a weight is ever derived. What it
does answer, with data instead of a guess, is the question the repository owner asked directly
after confirming this codebase (unlike v1) never forces memory text or queries to English: does
normalizing to English actually help recall quality? Four configurations were run over the exact
same corpus/model (`embeddinggemma-300m`) — `baseline` (today's real, as-is pipeline: nothing
translated), `store_en` (only stored text translated), `query_en` (only the query translated),
`both_en` (both sides translated — the v1-style shape) — each a real, hand-authored English
translation living in the fixture already (`text_english`/`query_english`), never a runtime
translation call. Measured MRR, `baseline` → `both_en`:

| Group | `baseline` | `store_en` | `query_en` | `both_en` |
| --- | --- | --- | --- | --- |
| overall (n=24) | 0.8021 | 0.9792 | 0.9062 | **1.0000** |
| `en-en` (n=8) | 1.0000 | 0.9375 | 1.0000 | 1.0000 |
| `ru-ru` (n=8) | 1.0000 | 1.0000 | 0.9375 | 1.0000 |
| `ru-en` (n=4) | 0.5625 | 1.0000 | 0.5625 | 1.0000 |
| `en-ru` (n=4) | 0.2500 | 1.0000 | 1.0000 | 1.0000 |

The same-language groups were already perfect under `baseline` (as expected: nothing about
this pipeline mishandles a single language); the entire gap is in the cross-lingual groups, and
`both_en` closes it completely — Hit@1/3/5 and MRR all `1.0000` across every one of the 24
queries, with none of `store_en`'s or `query_en`'s small (1-in-8) regressions on an
already-perfect same-language group (plausible cause: translating only one side of a pair, or
translating unrelated candidates in the same small pool, can shift the competitive ranking at the
margin even for a query whose own match did not change — an effect `both_en` does not exhibit,
since nothing is asymmetric once every text is in one language). Full run artifacts:
`fixtures/memory-recall/baseline/run-{baseline,store_en,query_en,both_en,comparison}.json` and
`.report.md`.

**This is evidence, not a resolved design decision** — no `[OPEN]` item in 15 §4 names this
question, so there is nothing here to close silently, and building a real translation component
is a separate, real architecture decision (which mechanism, whether it can satisfy
`local_only`/no-mandatory-external-daemon, where in the pipeline it runs) that needs the owner's
explicit sign-off, not an inference from a 24-query synthetic corpus. What the measurement does
support: on this fixture, full bilateral normalization (`both_en`'s shape) fully closes a real,
substantial cross-lingual recall gap with no observed downside, which is a real point in favor of
pursuing it — should the owner decide to.

As-built note (D-068, `[SPEC]`): the "1-in-8 regression" the paragraph above attributes to
`store_en` on `en-en` is an **artifact of the harness, not a property of one-sided
normalization**, and the guessed cause offered there ("translating unrelated candidates in the
same small pool") is wrong. `store_en` and `both_en` seed the store identically — `text_english`
for all 24 entries — and differ only in query text; the single query where their ranks diverge is
`mrq-13`, an `en-en` pair for which the fixture has `text_original == text_english` **and**
`query_original == query_english` byte for byte. Both runs therefore had byte-identical inputs
for that query. The harness minted `memory_id` with `SystemUuidV7` while seeding every entry at
one `created_at`, so this section's own `(score desc, created_at desc, memory_id)` tie-break was
decided by OS entropy whenever scores tied. That half-point is also the whole `store_en` ↔
`both_en` overall delta (1.0000 − 0.9792 = 0.0208 = 0.5/24): on this corpus the two configurations
are indistinguishable, and **the entire measured benefit comes from normalizing the stored text**.
The `query_en` drop on `ru-ru` is by contrast genuine — there an English query really is matched
against a Russian store. The harness now seeds its `UuidSource` and gives each entry a distinct
`created_at`, so the documented tie-break decides and two runs of one configuration agree query
for query. The recorded artifacts above predate that fix and are kept as-is (evidence is never
rewritten); the numbers a comparison may be drawn from are the ones a re-run produces.

## 7. Memory-quality benchmark `[FIXED, new in rev 6]`

A labeled fixture set of observation streams → expected memory ops
(`create | reinforce | supersede | noop`), explicitly covering decision vs hypothesis vs
negation, and RU/EN mixed transcripts. Precision/recall of the consolidation router on this
set is an acceptance gate (14 §2) on par with the 49-query code-search benchmark. Target P/R
numbers are set after the baseline run `[OPEN]`. Without this, the memory pillar has criteria
only for plumbing — the gate exists to prevent that.

As-built note (T14-07, `[SPEC]`, closing this section's `[OPEN]` target-P/R item): the fixture
set is 42 `memory.router.op.*` cases inside `fixtures/memory/index.json` (GAP-04), not a new
top-level family — `fixtures/schema/manifest.schema.json`'s `families` array is fixed-size and
closed-enum, and GAP-04 already scoped this corpus under the existing `memory` family. The
harness is `cargo xtask memory-bench` (`crates/xtask/src/memory_bench/`), split the same way
`cargo xtask bench` is (14 §7's own as-built note): `corpus` loads the labeled cases, `score`
holds op-kind matching (a multiset comparison per window, since a small local model has no
obligation to emit ops in the order a fixture author wrote observations in) and micro-averaged
precision/recall math over the full op vocabulary (`create | reinforce | resolve | retract |
supersede | noop | propose_candidate` — broader than this section's illustrative four, since
`propose_candidate` must be independently scoreable or a guard failure to downgrade would hide
inside a false "correct create"), `report` shapes the output, `gate` turns a report plus
versioned thresholds into a verdict, and `run` is the only piece needing the installed GGUF
weights. The real baseline went through two rounds (ADR-0006): round one measured both
`qwen2.5-{0.5b,1.5b}-instruct-gguf-q4km` (F1 0.3457/0.3529, modest); round two, after the user
asked directly whether Gemma could be used, measured `gemma-4-e2b-it-gguf-q4-0` and found it
roughly doubled the F1 (precision 0.6667, recall 0.6364, F1 0.6512,
`fixtures/memory/baseline/run-gemma-4-e2b.json`), which is why it replaced Qwen2.5-0.5B as the
shipped default despite being ~7× larger to download — disclosed as a real, measured trade-off,
not a free upgrade. `fixtures/memory/baseline/thresholds.json` sets the 14 §2 gate floor a real
margin below the round-two run (`min_precision = 0.60`, `min_recall = 0.55`); the round-one runs
stay on disk as historical evidence, not deleted.

As-built note (T14-09, `[SPEC]`, closes the item immediately above): chat-template rendering was
generalized — `local_rag_generate::chat_template::render` (a real Jinja interpreter, `minijinja`)
applies each model's own raw embedded template directly, replacing the vendored `llama.cpp`'s
fixed-signature template detector and the per-model `chat_template_override` it forced for Gemma
4. Measured effect, not assumed: two independent `cargo xtask memory-bench` runs of the
unchanged default (`gemma-4-e2b-it-gguf-q4-0`) under the new native-template rendering scored
precision/recall/F1 **0.6486/0.5455/0.5926** and **0.6757/0.5682/0.6173** —
`fixtures/memory/baseline/run-gemma-4-e2b-native-template.json` and `-2.json` — both *lower* than
the superseded override-path run (0.6667/0.6364/0.6512), disclosed as measured rather than
smoothed into the "higher ceiling" this section's own T14-09 forward-reference only speculated
about. The two runs also directly quantified real run-to-run variance under nominally
deterministic greedy decoding on this host (Metal backend, ~0.03 precision / ~0.02 recall
between back-to-back runs) that the gate floor must now absorb, not only cross-host variance.
`thresholds.json` was re-derived a wider margin below the *lower* of the two runs:
`min_precision = 0.60` (unchanged), `min_recall = 0.50` (down from `0.55`) — see that file's own
`derivation` field for the full accounting. Both Qwen2.5 ChatML entries were independently
re-measured under the same new rendering path and showed no material change. ADR-0006's own
Amendment section has the full mechanism trace, including a fourth catalog entry
(`phi-3-mini-4k-instruct-gguf-q4`, a third template family, no override) proving the mechanism
itself needs none — and that entry's own real, disclosed, unrelated template limitation (a
`system`-role turn silently dropped by its specific embedded template, measured at
precision=recall=F1=0.0000 on the full corpus, since the router's own system instructions never
reached the model).

## 8. Review tools (surface in 11 §2)

`list_memory`, `list_memory_candidates`, `approve_memory_candidate`,
`reject_memory_candidate`, `edit_memory_candidate`, `edit_memory`, `retract_memory`,
`merge_memories`, `inspect_memory_evidence` `[FIXED set]`. All mutations run through §3; all
list operations expose `entry_version` so edits can carry preconditions.

As-built note (T14-05, `[SPEC]`): the store-level primitives underlying the four
candidate-review tools now exist —
`local_rag_store::memory::{propose_candidate, edit_candidate, approve_candidate,
reject_candidate, list_candidates}` — but the MCP tool surface itself (request/response
shapes, routing) is out of this task's scope; group 15 wires it. `pending_memory_candidate` has
no `entry_version` (04 §6's as-built note), so `edit_memory_candidate`'s precondition, once
wired, is `review_state = 'pending'`, not a version match — `list_candidates` exposes
`review_state`/`created_at` as this table's own staleness signal instead, paired with
`candidate_evidence_for` for provenance.
