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
(+ `edit`, `merge`, `confirm`, `reject` from review tools). Contract for every operation:

- Single `state.sqlite` tx containing: the entry mutation, `memory_evidence` rows,
  `audit_event` (with `idempotency_key` when router-originated), and — for consolidation —
  the `processing_cursor` advance.
- **Preconditions**: `expected_version` (optimistic concurrency); kind/state machine legality
  (04 §5); scope uniqueness. Violation ⇒ tx aborts with a typed error.
- Response carries the new `entry_version` and `audit_id`.
- **Retry is idempotent**: same `idempotency_key` ⇒ recognized as already applied, returns the
  original result.
- `reinforce`: adds evidence, may raise confidence, never changes text `[FIXED]`.
- `edit`: new `entry_version` via audit; `actor` distinguishes user-edit vs router-edit vs
  **system-edit** `[FIXED]` (the third actor added by ADR-0011 — a normalization rewrite is an
  audited `edit` like any other, see the amendment note below).
- **Durable memory text is stored in English** `[FIXED, ADR-0011]`. Non-English input is
  translated at the boundary before it is written (08 §5, 11 §2); the author's original is kept as
  provenance and is what `inspect`/`export` show (12 §3). The invariant is **eventually** English:
  when translation fails the entry is written with the author's text and its canon is rewritten
  later by `Actor::System`, because losing a note to a model failure is not an acceptable
  degradation.
- `retract` ≠ delete: entry survives for audit; hard removal exists only as an explicit privacy
  `purge` (12 §5) which also rewrites audit references to tombstones `[SPEC]`.
- `merge_memories`: one tx — survivor absorbs evidence, losers → `superseded` with
  `supersedes_id` → survivor; audit records the merge set.
- `confirm`/`reject`: the `hypothesis` machine's own two transitions (04 §5), `active →
  confirmed` and `active → rejected` `[SPEC]` — see the D-079 note below.

Amendment note (T21-12, `[FIXED]` change under
[ADR-0011](../adr/0011-english-canon-for-durable-memory.md)): the two `[FIXED]` bullets above about
`actor` and about language are this ADR's, and they are deliberately narrow. This section already
said an entry's text may change **only** through `edit`, with a new `entry_version` and an audit
row; English canon does not weaken that contract, it uses it. `Actor::System` already existed in
code (`crates/store/src/memory/audit.rs`) and was simply unnamed here. What is genuinely new is the
language invariant, and it is stated as *eventually* English on purpose — ADR-0011 §Decision 3
explains why refusing a write whose translation failed would be the worse failure. The write path
that performs the boundary translation is `T21-14`'s and lives **above** `crates/store`: a
generative model must not run inside the write transaction (the precedent D-063 set for
subprocesses).

As-built note (D-079, `[SPEC]`): `confirm`/`reject` are listed above with `edit` and `merge`
rather than in the headline six on purpose — like those two they are **review-tool** verbs, not
router ops. This list and 04 §5's transition table had disagreed since idea.md rev 6: the table
declared `hypothesis: active → confirmed | rejected | superseded` while this list held no verb
able to reach the first two states, so the transitions were legal, tested at the guard, and
unreachable by any caller. `op::apply_confirm`/`apply_reject` close that (04 §5's D-079 note has
the full account). Two boundaries this deviation deliberately did **not** cross: §4's router op
envelope is unchanged, so the model still cannot emit `confirm`/`reject` and the
`memory.router.op.*` corpus is unmoved; and `ProposedOperation` is unchanged, so a candidate
cannot propose them either — for the same reason it cannot propose `edit`/`merge`.

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
- **A `create` of text that already exists in the scope is a `reinforce`** (as-built, `D-078`).
  `local_rag_memory::guard::materialize` looks the text up
  (`local_rag_store::active_entry_with_text`: same scope, exact text, non-terminal) and, when it
  finds one, emits `reinforce` on that entry instead of a second copy — carrying the window's
  evidence, and leaving `confidence` untouched, because one window's opinion of a *new* entry is
  no basis for rewriting an accumulated one.

  The rewrite looks the text up in the **store**, not in the plan it is building, so a window that
  proposes one stored text twice produces two reinforces of one entry — the shape §4's collapse
  exists to fold (`T23-05`/`D-121`). `D-078` stops the store accumulating copies across windows;
  the collapse stops one window's plan contradicting itself about an *existing* entry. A window
  proposing one **not-yet-stored** text twice was the same blind spot one op kind later: `D-078`'s
  lookup has nothing to find, so both creates minted their own entry (`T23-07`/`D-127`, measured:
  14 live runs did, leaving 11 active entries over 4 shared texts). `local_rag_memory::plan::
  collapse` now also folds sibling `create` ops that agree on scope and text, the same way it
  already folds sibling `reinforce`/`resolve`/`retract`/`supersede` ops that agree on a target.

  This is a mechanical guard rather than a prompt rule for a measured reason: the router cannot
  see the duplicate. §4 step 3's candidate set is capped, and past the cap the model is blind to
  its own recent output (`D-080`), so it re-derives the same claim every window. On the owner's
  store that produced **136** copies of one sentence — over half of the durable memory. The
  `canonical_key` uniqueness above cannot catch it either: that index is partial on non-null keys
  and the router leaves the key null.

  Exact text, not similarity, and deliberately: "is this the same claim?" asked loosely is a
  judgement that belongs to the model and to review; asked as byte equality it is a fact, and a
  fact is what a guard may act on silently. Near-duplicates are left alone. `propose_candidate`
  is **no longer** fully left alone (`T23-07`, ADR-0014 Decision 2, `D-118`): a proposal
  byte-identical to one already pending, or to an already-active entry, is declined — no
  `pending_memory_candidate` row is written. That check lives one layer down, in
  `local_rag_store::memory::propose_candidate` itself, and this guard's own boundary is unchanged:
  it still never turns a candidate into an automatic write, because declining to enqueue a
  duplicate *request* is not answering it.
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
  operations, which this task's card does not cover. `T23-05` found the consequence and left the
  bullet standing: the optimistic version comparison is, accidentally, the only thing that stops a
  consolidation batch from reinforcing an entry it superseded two ops earlier, which is one of the
  three reasons §4's collapse happens above the store instead of by relaxing that comparison.
  One window's repeated re-observation of one entry is **one** reinforce, not two, so a window
  cannot inflate an entry's version — or its accumulated confidence — by saying the same thing
  twice.

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

As-built note (D-080, `[SPEC]`): step 3 says "recall of **plausibly related** existing entries",
and until D-080 the code did not do that. `local_rag_memory::recall::candidate_conflict_set` built
the honest scope union, sorted it by `memory_id` — UUIDv7, so time-ascending — and truncated to
`MAX_PROMPT_CANDIDATES` (50). Past 50 entries in a scope the model was therefore shown only the
**oldest** ones and never anything recent, including entries the router itself had written a window
earlier: it could not reinforce, supersede or retract its own output, and re-proposed it instead.
That is the mechanism behind D-078's 136 copies of one sentence. Measured on the owner's store the
day D-080 was written: 56 active entries in one repository scope plus 12 global = 68 against a cap
of 50, so the 18 newest were being discarded on every consolidation.

As-built note (`D-095`, `[SPEC]`): the cap `D-080` kept is a count, and step 3's "bounded" is about
size. The two are not the same bound, and on a real store the difference is the whole product: 50
entries averaging 2501 characters are ≈31 892 tokens against a 32 768-token context, so the
conflict set alone consumed 97 % of the window it exists to be compared against, and every
consolidation for that session failed with a deterministic overflow — permanently, since the
failure reproduces exactly on retry. Three sessions were stuck, backlog 1829 and rising, throughput
zero. Stated plainly, the defect was that **the more the product remembered, the less it could
consolidate**. The set is therefore cut by a token budget
(`config.memory.router_conflict_token_budget`, accounted with recall's own `estimate_tokens`) taken
as a **prefix** of `D-080`'s order, so the entries most worth showing survive and the rest are
dropped rather than the prompt failing whole. An entry larger than the entire budget is left out
completely: the router can route with no conflict set, but not with a prompt that does not fit.

The cap stays; **which** entries survive it is now a rule rather than a byproduct of the sort:
lexical matches against the window's own excerpt text first (best match first), then the remaining
entries newest-first to fill the budget — and above the cap they are presented in that same order.
Below the cap nothing changed at all: the union is returned whole in `memory_id` order, so the
function is byte-identical to its pre-D-080 self wherever it already worked. A window with no
excerpt text yields no query terms and falls back to newest-first without issuing SQL.

The presentation half was measured rather than argued. The first shipped attempt kept `memory_id`
order after selecting, on the reasoning that changing membership and order at once would confound
the A/B — and that left the one entry the window was about at position **49 of 50**, where the
model answered `noop` instead of `supersede`. Front-loading the selection changed the answer.
Three runs of the same release binary over the same corpus, differing only in this function:
control (pre-D-080 selection) predicted `create` on the saturating case, `memory_id`-order
selection predicted `noop`, related-first selection predicted `propose_candidate`. None is the
labeled `supersede`, and that is stated rather than smoothed: what D-080 fixes is that the entry
reaches the model at all, which the three different answers prove it now does. Whether this model
then picks the right op on a 50-entry prompt is §7's quality question, not this one's. The failure
mode does improve monotonically across the three — writing a duplicate, doing nothing, asking a
human — but that is one case, and one case is not a quality claim.

This partially revisits `crates/memory/src/recall/mod.rs`'s own "a consolidation window is not a
recall request" reasoning, deliberately and narrowly: ranking enters only as the rule for *choosing
what to drop when the prompt overflows*, never as the shape of what the router receives — it still
gets an unscored set, not a ranked top-K. The machinery is not a second copy of the §6 pipeline
either: only its lexical leg is used, and that leg (`recall::lexical`) is a pure synchronous
function over an already-fetched candidate list, backed by an ephemeral in-memory FTS5 table. No
embedder, no persistence, no cross-database work.

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

As-built note (`T23-04`/`D-120`/`D-125`, `[SPEC]`): step 1's `to_received_seq` is now
`min(cursor + batch, max_seq)` **and** the last observation whose excerpt text fits a character
budget, whichever comes first — always at least one observation, so the floor `D-058`'s ladder
ends at is unchanged. Rows were the wrong unit for the same reason `D-095` found them to be the
wrong unit for the conflict set, and this time it was the larger half of the prompt: measured with
the model's own tokenizer on six windows a running daemon had failed on, the observations cost
17 599 to 23 127 tokens of a 32 768-token context, against 8 791 to 14 159 for the conflict set and
1 213 for the system prompt. An excerpt is capped at 4 KiB by the hook and averages a fifth of
that, so twenty of them span 12 KiB to 80 KiB; `estimate_tokens`' four-characters-per-token
heuristic holds for prose and for memory entries (3.66 measured) and not for excerpts, which are
tool output, JSON and code (2.93 aggregate, 1.87 at the worst).

The budget itself is not configuration: `local_rag_memory::budget::PromptBudget::derive` subtracts
the answer reserve, the one corrective re-prompt's own cost, the system prompt and the conflict
set's promised floor from `GeneratorCatalogEntry::context_length`, and what remains is the window.
`consolidation_batch_size` stays as the upper bound it always was. Which of the two binds is
therefore a property of the model, not of the config — a larger context spends the difference on a
wider window without any value being edited.

Two things follow that step 3 should be read with. The set of existing entries is cut a second
time, by the model's own tokenizer, to exactly what the assembled prompt has room for: the
`router_conflict_token_budget` prefix decides what is *worth* showing, and this decides what
*fits*. The conflict set is the term that yields because it is the only one that may — a window is
a promise to the cursor, which step 4 advances to `to_received_seq` whatever the router read. And
when even an empty conflict set will not fit, the window fails as a deterministic context overflow
**without** a generator call: the tokenizer has already answered, and `D-058`'s ladder narrows the
window on the next tick exactly as it does when `llama.cpp` answers instead.

As-built note (`T23-06`/`D-122`/`D-128`, `[SPEC]`): step 3's answer is bounded by
`PromptBudget::answer_reserve_tokens` (`crates/memory/src/budget.rs::ANSWER_RESERVE_TOKENS`), a flat
measured constant, not a quantity derived from the window. `D-122` originally read as "derive the
answer budget from the window" — measured against real regenerated answers, that premise failed on
both candidates: row count (a one-row window produced *more* operations on average than a
twenty-row one) and window character volume (Pearson r ≈ 0.15, r² ≈ 0.02 over historical
`consolidation_run` data). The measured constant (`6 144` tokens, from 35 real windows regenerated
with a deliberately oversized `max_tokens = 8192` so today's answer is not censored by the very
budget being sized; 30 finished on their own — `p50 = 176, p90 = 2098, p95 = 2420, p99 = 4986`,
mean ≈ 640 — and the other 5 hit the cap) folds into `derive`'s same subtraction chain `T23-04`
built, so the two cards' answers stay in the one place that sums to `context_length` rather than
drifting apart.

Raising the reserve past 1 113 tokens crosses a branch this same arithmetic already had:
`conflict_floor_tokens` stops being the flat `CONFLICT_SET_FLOOR_TOKENS` promise (`D-095`'s
14 000-token floor) and becomes `available / 2` instead, once the answer reserve exceeds
`(context_length − retry_overhead − system) / 2 − CONFLICT_SET_FLOOR_TOKENS` (that boundary, at
`context_length = 32 768`) — registered as `D-128` and closed in the same card, since it is a
documented consequence of the accepted arithmetic rather than a defect to fix. Separately, the
measurement surfaced a real decoding failure mode distinct from answer size: greedy decoding can
degenerate into verbatim repetition of one JSON line for thousands of tokens (`D-130`, open) — no
finite reserve closes that tail, so the constant above is sized from the answers that finished on
their own, not from the ones that hit the cap.

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
it in the prompt, since the same key text can legitimately exist in more than one scope.

As-built note (`T23-07`, `[SPEC]`, ADR-0014 Decision 2): step 3's "recall of plausibly related
existing entries (candidate conflict set)" now also carries the distinct `create`-shaped proposals
already pending review in scopes the window touches — "show the router what it is supposed to
notice," the soft half of the candidate-dedup decision, whose hard half is the deterministic check
named in §3's `D-078` bullet above. A pending row is folded into the same
`local_rag_store::MemoryEntrySummary` shape real entries use, tail-appended after them so a tight
token budget cuts it first, and rendered with **`"memory_id":null`** — never its `candidate_id`,
never an entry's — precisely so it stays what the `canonical_key` sentence above already
establishes the pattern for: something the model can read but never name as a `target_memory_id`.
Deduplicated by the same exact-proposal identity the deterministic check uses
(`local_rag_store::candidate_dedup_key`) before it ever reaches the prompt, so a claim proposed
hundreds of times over (measured live: one claim 475 times) arrives as one row, not hundreds.

Two-tier
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
`Transient` by default, not split per variant — none of the live retry-storm incidents *known at
the time* went through that path (D-069 later found one that did, on the neighbouring
`WriteError::Sqlite` path; see its own note below). `consolidation_run` gains five nullable, unbackfilled
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

As-built note (D-069, `[SPEC]`): D-050's "`Transient` by default" for apply-time failures had a
hole neither D-050 nor D-051 closed, found in live dogfooding: an op's `evidence_observation_ids`
is untrusted model output (12 §4), and both evidence tables are keyed `(owner_id, observation_id)`,
so one `observation_id` the router repeated inside a *single* op's citation list violated a PRIMARY
KEY and rolled the whole window transaction back. That failure arrives as
`RunOutcomeError::Write(WriteError::Sqlite(_))`, not as a `RunnerApplyError`, and was therefore
classified `Transient` — whose backoff caps at 4s and never terminates. One window of **three**
observations was retried 627 times at one full local-model generation per attempt (~6h of GPU in a
day) before it was stopped by hand. Three changes, all in
`local_rag_store::memory`: (1) `runner::apply_run` deduplicates each op's citation list,
order-preserving, on both the `Materialize` and `ProposeCandidate` branches — the boundary where
untrusted output enters the store, deliberately *not* an `INSERT OR IGNORE` inside
`insert_candidate_evidence`/`insert_memory_evidence`, whose "a duplicate surfaces as the natural
PRIMARY KEY error" contract stays right for callers minting unique ids; (2) a SQLite constraint
violation is classified `Mechanical`, since the generated ops and the rolled-back rows are both
unchanged on a retry, so the same build reproduces it exactly — the fingerprint dead-letter now
covers it; (3) `consolidation::record_run_failure` escalates a `Transient` failure that reaches
`TRANSIENT_ATTEMPT_CAP` (8 attempts — the backoff has been pinned at its 4s cap for three of them)
into an ordinary fingerprinted `Mechanical` dead-letter, so no failure class can retry unboundedly
any more. Duplication *across* two ops of one batch is not deduplicated (a semantically different
batch); it is bounded by (2) instead. Knowingly accepted cost of (3): a genuinely long generator
outage parks the runs it hits until the binary is rebuilt — a daemon restart does not revive them
— and `open_next_run` blocks that session's whole backlog meanwhile. D-071 is the observability
half that surfaces such a row in `stats`/`doctor`.

As-built note (`T23-05`/`D-121`, `[SPEC]`): step 3's "ordered ops list" may legitimately name one
entry more than once, and step 4's `expected_version` for **every** op is captured once, by
`local_rag_memory::guard::materialize`, before any op is applied. So a second op on one entry does
not *race* into failing — it is guaranteed to fail, because the first op moves the version the
second is still holding. `D-078`'s rewrite is the common producer: a `create` of text that already
exists becomes a `reinforce` of that entry, so a window proposing one stored text twice yields two
reinforces of one entry carrying one snapshot. Measured on a live store: eight runs failed this way,
**every one with `found = expected + 1` and an op index of at least 3**, and 569 runs proposed one
text more than once inside a single transaction.

The plan is therefore collapsed above the store, by `local_rag_memory::plan::collapse`, before
`run_once` is handed it — the same "dedup at the untrusted-input boundary" move `D-069` made one
level down, and the sentence above about cross-op duplication stays true of `apply_run` itself,
which is unchanged. The rules: ops that version-check an entry group by that entry; `reinforce`
yields to `supersede`/`resolve`/`retract`; among those three the last in plan order wins, plan
order being the only evidence about which the model stated second; citations are unioned in plan
order with the first occurrence winning; a merged `reinforce` keeps the last stated confidence,
which is what `COALESCE(?2, confidence)` would have left; and `noop`/`propose_candidate` never
participate — `noop` carries no `expected_version` to group on. `op_index` numbers the collapsed
plan, and the shift is immaterial for the reason the `idempotency_key` paragraph above already
gives.

As-built note (`T23-07`/`D-127`, `[SPEC]`): `create` was the fourth op this section originally
said never participates, on the reasoning that it "carries no `expected_version`" — true, and
beside the point: two `create`s of one **not-yet-stored** text are the same class of self-
contradiction as two `reinforce`s of one entry, just one op kind earlier, and `D-078`'s store-only
lookup (above) cannot see it for the identical reason it could not see the `reinforce` case before
this module existed. Measured: 14 live runs minted two entries for one text inside one transaction.
`create` now groups by `(scope_kind, scope_owner_id, text)` instead of an entry id — the only
identity it has, since `guard` mints its `memory_id` fresh per op — and folds the same way, keeping
the later op's minted id and the union of citations. `propose_candidate` still does not
participate, but the reason changed: the within-window half of *its* duplicate — two
`propose_candidate`s of one claim in one plan — is absorbed one layer down, inside the same
transaction `commit_apply_run` opens, by `local_rag_store::memory::propose_candidate`'s own
deterministic check (ADR-0014 Decision 2, `D-118`), which sees a sibling's uncommitted insert and
needs no help from a plan-level fold; merging two review requests before the store has even seen
either would still decide something a person has not decided yet.

Why the store was not the place, recorded because the alternative looks obvious: `apply_run`
already reads its own writes (SQLite shows an uncommitted `UPDATE` to later reads on the same
transaction), so a batch-local version map could only *relax* the comparison, and relaxing it is
worse twice over. `memory_evidence` is keyed `(memory_id, observation_id)` and `D-069`'s citation
dedup is per-op, so two reinforces of one entry citing one observation — 70 of 96 within-run op
pairs on the live store share a cited observation — would violate that key, and a constraint
violation is `Mechanical` on the first attempt, i.e. an immediate permanent dead-letter instead of
a retryable one. And `apply_reinforce` deliberately does not check the entry's `kind`/`state`, so
the version comparison is the only thing preventing a batch from reinforcing an entry it has just
superseded.

What this does not fix, stated plainly: a genuine outside writer — a review tool, the normalization
worker, another session's run — moving the version between plan and apply. That stays a conflict,
correctly, and the existing retry converges on it because a retry re-invokes the generator and
`guard` re-reads every version. Convergence there is a property of re-planning, not of waiting.

## 5. Explicit tool-initiated memory (`remember`, review tools)

`remember` (11 §2) is an explicit durable operation: creates an `active` entry, `actor='user'`
when the human confirmed, else `actor='router'`-equivalent trust with `evidence_kind=
model_claim` `[SPEC]`. It passes through the same transactional path as router ops.

## 6. Recall v0 `[FIXED pipeline]`

```
query normalization: script-detect the query; translate to English if it is not
   (ADR-0011 §Decision 2) — free for an English query, degrades to the original
   text plus an explicit marker when translation fails [FIXED, ADR-0011]
→ scope resolution: global ∪ repository(worktree→repo) ∪ worktree
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

As-built note (T21-15, `[SPEC]`, [ADR-0011](../adr/0011-english-canon-for-durable-memory.md)): the
query-normalization stage above is implemented, and one detail of *where* is worth pinning because
the obvious reading is wrong. The decision is made **above** this pipeline —
`crates/local-rag/src/daemon/mcp/memory.rs`, under `spawn_blocking`, reusing the same
`normalize_for_write` the write boundary uses (T21-14) — and only the already-decided query travels
down, together with a `query_degraded` marker. `recall()` is synchronous and holds `!Send`
connections, so it cannot be moved off the async worker from inside; and answering "what is this
text in the canon's language" in two places would be two chances to answer it differently.

Both legs then read one string, which is the entire point: the store is English, so a query that
is not reaches only the dense leg. `crates/local-rag/src/daemon/mcp/memory.rs::
query_boundary_tests::a_russian_query_reaches_the_lexical_leg` proves the fix with the dense leg
deliberately unavailable — the entry can only be found by BM25 there, so finding it is proof the
translation reached the lexical leg rather than a multilingual embedder covering for it.

A termless recall never reaches the translator, an already-English query is short-circuited by the
pure detector at no cost, and a refusal searches the author's own words with the marker set rather
than failing (02 §6). The hook's budget moved to accommodate the step — 11 §3.2's own amendment
note has the measurement it was re-derived from.

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
  overhead**, not a real tokenizer. `T23-04` since added one, `Generator::count_prompt_tokens`, for
  the one path that owns a real tokenizer and can afford the call — the consolidation router
  (08 §4); this recall leg is not that path, running on the hot query path with no generator in
  reach, so the heuristic stays here on purpose. `MAX_SEQUENCE_TOKENS` bounds an unrelated ONNX
  subsystem, not this text. This section fixes only the number
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

As-built note (T21-09, `[SPEC]`, ADR-0010): the two notes above measured **hand-authored** English —
`store_en`/`query_en`/`both_en` read `text_english`/`query_english` straight out of the fixture. A
fifth configuration, `pipeline_en`, measures **what is actually shipped**: the original text in
`memory_entry`, the real detector and translator (`local_rag_memory::normalize::translate` against
the installed `gemma-4-e2b-it-gguf-q4-0`) writing real `memory_text_normalization` rows, and the
backfill embedding each entry's *effective* text. The corpus, model, and queries are unchanged; the
query side stays original (translating it is `T21-10`).

The translator did its job — **12 translated, 12 passthrough, 0 failed** over the 24 entries — and
the result is that the metric does not move at all:

| Group | `baseline` | `store_en` | `query_en` | `both_en` | **`pipeline_en`** |
| --- | --- | --- | --- | --- | --- |
| overall (n=24) | 0.8021 | 1.0000 | 0.9062 | 1.0000 | **0.8021** |
| `en-en` (n=8) | 1.0000 | 1.0000 | 1.0000 | 1.0000 | **1.0000** |
| `ru-ru` (n=8) | 1.0000 | 1.0000 | 0.9375 | 1.0000 | **1.0000** |
| `ru-en` (n=4) | 0.5625 | 1.0000 | 0.5625 | 1.0000 | **0.5625** |
| `en-ru` (n=4) | 0.2500 | 1.0000 | 1.0000 | 1.0000 | **0.2500** |

`pipeline_en` is `baseline` query for query. The card predicted that such a divergence would mean
"the embedder is being fed the wrong text"; the run proves it is not. This task added the
instrumentation that settles it — every run now records, per query, whether the dense leg degraded
and at what rank **that leg alone** placed the expected entry (`## Legs` in each report):

> dense leg ranked the expected entry #1 — **24/24**, in *every one* of the five configurations,
> `baseline` included; dense leg degraded — 0/24.

So the multilingual embedder already solves this corpus's cross-lingual problem on its own, with no
normalization whatsoever, and all 24 vectors are found by hash. The cross-lingual gap the earlier
notes measured is created **at fusion**: RRF ranks an entry that both legs surfaced above an entry
only the dense leg found, so a dense-rank-1 answer that BM25 never saw lands outside the top 5.
`store_en`/`both_en` close that gap by changing what the **lexical** leg reads (English stored text
matching an English query), not by improving retrieval quality. The shipped design deliberately does
the opposite — `memory_entry.text` stays canonical (§3 `[FIXED]`) and BM25 stays raw-against-raw
(ADR-0010) — so it moves only the leg that was already perfect.

This is registered as **`D-075`, status `blocked`**: whether to change fusion, to feed the lexical
leg normalized text (a revision of ADR-0010), to pursue the query side (`T21-10`), or to accept the
current state is an owner decision, not something this measurement may settle on its own. Full run
artifacts: `fixtures/memory-recall/baseline/run-{baseline,store_en,query_en,both_en,pipeline_en,
comparison}.json` and `.report.md`, all five regenerated together on the D-068-fixed harness (the
older recorded numbers predate that fix, as the note above says).

A harness defect found and fixed in the same task: every configuration used **one** reused temp
directory, deleted and recreated per configuration while the previous one's `StateDb`/`CacheDb`
writer threads could still hold its files. On this five-configuration sweep that raced and killed
the run outright with a bare SQLite `disk I/O error` on the second configuration. Each configuration
now gets its own directory and the cache is closed explicitly before the next one starts.

## 7. Memory-quality benchmark `[FIXED, new in rev 6]`

A labeled fixture set of observation streams → expected memory ops
(`create | reinforce | supersede | noop`), explicitly covering decision vs hypothesis vs
negation, and RU/EN mixed transcripts. Precision/recall of the consolidation router on this
set is an acceptance gate (14 §2) on par with the 49-query code-search benchmark. Target P/R
numbers are set after the baseline run `[OPEN]`. Without this, the memory pillar has criteria
only for plumbing — the gate exists to prevent that.

As-built note (T14-07, `[SPEC]`, closing this section's `[OPEN]` target-P/R item): the fixture
set is 42 `memory.router.op.*` cases inside `fixtures/memory/index.json` (GAP-04) — 43 since
`D-080` added `supersede-existing-past-the-cap-en`, whose reason 14 §7's own note carries — not a new
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
