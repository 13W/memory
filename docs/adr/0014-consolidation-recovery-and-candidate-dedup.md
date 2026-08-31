# ADR-0014: Recovery from a parked consolidation run, and where candidate dedup lives

## Status

Accepted — 2026-08-31.

Opens a new scope by explicit owner decision, and is the first ADR in this project opened from
the state of a **running store** rather than from a reading of the specification. Realized by
group 23 with gate `G23`
([`groups/23-consolidation-recovery.md`](../implementation-plan/groups/23-consolidation-recovery.md)).
No gate `G00`–`G22` is reopened.

It does not itself change any `[FIXED]` text. Following the rule ADR-0013 established, an ADR
makes a change legal; the change is made by cards and registered as a deviation, because the
discrepancy was found in the as-built state rather than invented.

## Context

Every figure here was measured on the owner's live store with a read-only connection while the
daemon was running, on 2026-08-31. They are quoted because the argument depends on them.

### What is actually broken, stated no wider than the evidence

Consolidation is **not** globally stopped, and an earlier draft of this diagnosis said it was.
The correction matters, because it changes which mechanism is at fault. Over a single session the
backlog moved 1386 → 1373, `applied` runs went 11515 → 11522, and one session went from 17
un-consolidated observations to 4. `D-058`'s shrink ladder works: a 20-observation window
overflowed the model's context, halved to 10, and windows of 10 have been applying every ~25
seconds since.

What is genuinely stuck is narrower and worse. **Two sessions are blocked permanently, and they
hold 1368 of the 1373 backlogged observations — 99.6 %.** For both, the latest run is a
`mechanical` dead-letter that is **not** a context overflow: one
`optimistic conflict: expected entry_version 2, found 3`, one
`router output did not parse as a JSONL ops stream: EOF while parsing a string at line 1
column 4430`. Three mechanisms then decline that row, each correct on its own terms:

- `stale_runs` excludes `mechanical` with a matching fingerprint. That is `D-050`'s guard, added
  after one three-observation window was retried 627 times at a full local generation per attempt.
  Removing it is not on the table.
- `dead_letter_shrink_decision` requires `last_failure_context_overflow`, so the ladder above does
  not apply — by design, since halving a window does not fix a stale plan or a truncated answer.
- No command can act on a run. `doctor` and `stats` report it; `purge` deletes the session's data
  rather than consolidating it; nothing else touches one.

### Why that is a deviation and not a cost already accepted

[Spec 08 §4](../specification/08-memory.md) foresaw this and accepted it in as many words:

> Knowingly accepted cost of (3): a genuinely long generator outage parks the runs it hits
> **until the binary is rebuilt** — a daemon restart does not revive them — and `open_next_run`
> blocks that session's whole backlog meanwhile.

The escape is real and it works: a rebuild moves `BUILD_ID`, the fingerprint stops matching, and
`stale_runs` picks the row up again. It was a sound trade while the only user of this software was
the person who could type `cargo build`.

**A published release has no rebuild.** Checked on the downloaded `0.1.0`: its `BUILD_ID` is the
literal `0.1.0`, fixed for the life of that release. So for anyone running a released binary the
first mechanical, non-overflow failure parks that session forever, with no supported remedy at
all. Publishing `0.1.0` falsified the assumption the acceptance rested on without touching the
sentence that states it. Registered as `D-117`.

### A consequence of the same design that nobody had named

`latest_non_applied_run` selects `WHERE state != 'applied' ORDER BY created_at DESC LIMIT 1`, and
a `failed` row is never cleared by anything. So the row that caused one overflow keeps being the
blocking row forever, and `dead_letter_shrink_decision` keeps computing `previous_count / 2`
against that same frozen window. A session that overflowed **once** therefore opens
half-size windows for the rest of its life. Verified: the recovering session's recent windows are
all exactly 10 against a configured `consolidation_batch_size` of 20 — twice the runs and twice
the generator calls, permanently. This is the cost of the failure's *trace*, not of the failure,
and it is the second reason a parked row needs a way to be cleared.

### Why dedup cannot be built on the primitive that exists

Entries already have a deterministic uniqueness mechanism: `canonical_key`, unique per scope,
enforced by the database. Candidates have none, and the obvious move — reuse the key — does not
work. Measured: `canonical_key` is `null` on **all 9605** pending candidates, and all of them are
`op: create`; among active entries only 32 of 209 carry one. That is not an accident of this
store. [Spec 08 §4](../specification/08-memory.md)'s own as-built note says the model "never
addresses an existing entry by `canonical_key` — only by the `memory_id`
`local_rag_memory::recall::candidate_conflict_set` shows it in the prompt". The router is never
shown keys, so it cannot emit them.

And the step that was supposed to catch duplicates cannot see them. Spec 08 §4 step 3 hands the
router a "candidate conflict set"; `recall::candidate_conflict_set` builds it from
`active_entries_for_scope` — **active entries only**. A pending candidate is not an entry, so a
proposal identical to 475 pending siblings is never shown to the router, cannot be named as a
conflict, and is written as a new row. The result is 9605 candidates over 3294 distinct texts, the
worst proposed 476 times, and `conflicts` non-empty on **none** of them. Registered as `D-118`.

## Decision

### 1. A parked run has a supported recovery path, and it is an operator's, not a schedule's

There are two verbs, both explicit, neither automatic: **retry** the parked window, and
**abandon** it — declare the window unconsolidatable and advance the cursor past it. Neither may
be performed by a background loop.

*Why not automatic, stated so the next reader does not "improve" it.* Any timer that eventually
revives a mechanical dead-letter is the retry storm `D-050` stopped, wearing a longer interval.
The distinction that makes an operator action safe is not that it is rarer; it is that a person
decided, once, with the failure in front of them. `admin/reconcile_now` (T20-07) is the precedent
in this codebase for a verb that triggers work the daemon would otherwise schedule, and it is the
shape to follow.

*Advancing a cursor over unapplied observations is not a new capability.* `apply_run` calls
`upsert_processing_cursor` unconditionally at the end, so a window whose ops are all `noop`
already advances the cursor while mutating nothing. The difference is not mechanical, it is
epistemic: a `noop` window was **read and judged**; an abandoned window was not. That is precisely
why `abandon` must write an audit record where `noop` needs none.

*The audit vocabulary grows, and the ADR says so rather than letting a card discover it.*
`audit_event.entity_kind` holds only `memory_entry` today — 758 rows, checked. An abandoned window
is the first entity of another kind, and it sits outside [spec 08 §3](../specification/08-memory.md)'s
`[FIXED]` transaction contract, which is written around an entry mutation that here does not exist.
The card amends that text; this record makes the amendment legal.

*The price, named with its deadline.* `abandon` is bounded data loss: the observation envelopes
are durable and survive, so what is lost is their conversion into memory, not the record that they
happened. But the loss has a clock. `observation_payload` lives under a TTL, and the moment that
sweep actually runs (`T23-09`, `D-123` — today it is scheduled by nothing, which is why those
payloads are still on disk), an abandoned window becomes unrecoverable in fact and not only in
practice. **Order matters, and it is not incidental:** rescuing the backlog comes before enforcing
the TTL, or there is nothing left to rescue.

*A second thing the repair buys.* Clearing a parked row also lifts the permanent halving described
above, because the row that forces `previous_count / 2` stops being the latest non-applied one.

### 2. Candidate dedup is deterministic and lives in the store; the generator's answer is a hint

Two halves, and both are required.

**Show the router what it is supposed to notice.** `candidate_conflict_set` must include pending
candidates, not only active entries. This fixes the cause: today the model is asked to spot a
duplicate it is structurally never shown.

**Keep a deterministic check behind it.** Whether a duplicate is created must not depend on
whether a 4B model noticed. The store decides, on the exact proposal — its op, kind, scope and
text — because that is what the router actually emits, and `canonical_key` demonstrably is not.

*The price.* Exact text only. Two proposals that say the same thing in different words will still
both land, and this record accepts that rather than hiding it: near-duplicate matching needs a
similarity threshold nobody here has measured, and inventing one is how a store starts silently
dropping distinct memories. Exact-text dedup is the floor, it is measurable, and the count of
distinct texts is the invariant a later, fuzzier step would have to beat.

## Consequences

- The two permanently blocked sessions become recoverable, and the operator gets a verb for a
  state that today has none. `T23-03` is both the mechanism fix and the remedy for the live store;
  they are one piece of work here, not two.
- A new `entity_kind` enters `audit_event`, and spec 08 §3's transaction contract gains a case
  that mutates no entry. Anything reading audit rows by kind must tolerate it.
- Abandoning is destructive within a bounded window, and the bound is the payload TTL. The group's
  card order encodes that; `G23` must confirm it held.
- Sessions that once overflowed return to their configured batch size instead of paying double
  generator calls forever.
- The candidate queue stops growing by duplication. It does **not** shrink on its own: 9605 rows
  already exist, and reducing them is `T23-08`, whose invariant is the distinct-text count rather
  than the total.
- **What this ADR does not claim.** The shrink ladder already recovers the context-overflow class
  without help; `T23-04` prevents that class rather than rescuing it, and the record says so to
  keep a later reader from crediting this decision with work `D-058` already did.

## Alternatives rejected

- **Expire the fingerprint by time instead of by build.** The simplest way to make a parked run
  eligible again, and it reintroduces exactly what `D-050` was written to stop: a deterministic
  failure retried forever, at one local generation per attempt. A longer interval changes the
  bill, not the behaviour.
- **Abandon automatically after N attempts.** Silently discards observations on a schedule. The
  whole reason `abandon` is acceptable at all is that a person weighed the loss; a timer cannot.
- **Let the operator edit SQLite.** This is what has actually happened — four times, recorded in
  this project's own memory. It is unauditable, unrepeatable, requires knowing the schema, and is
  not available to any user who did not write the software. That it works for one person is the
  argument against it, not for it.
- **Make the daemon clear parked rows at startup.** Attractive because a restart feels like an
  operator action. It is not one: restarts happen for unrelated reasons, and this would revive a
  deterministic failure on a schedule nobody chose.
- **Use `canonical_key` as the dedup key.** Measured to be `null` on 100 % of proposals, and the
  router is never shown keys, so it cannot start emitting them without a separate change to the
  prompt contract — which would be a larger and less certain intervention than an exact-text check.
- **Semantic or embedding-based dedup now.** The store has no similarity primitive at propose
  time, and the threshold would be unmeasured. Rejected as premature, not as wrong: exact-text
  dedup establishes the baseline a fuzzier method would have to beat.
- **Dedup only at review time.** Would let the queue keep growing and merely hide it behind a
  grouped view. `T23-08` still groups for review, but grouping is a reading aid, not the fix.
