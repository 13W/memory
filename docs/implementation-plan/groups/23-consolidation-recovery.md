# Group 23 — Consolidation recovery and candidate dedup (post-v0)

The sixth group opened after `T00–T17` closed (`G17: PASS`), by the owner's explicit product
decision of 2026-08-31. The decision itself is recorded in
`docs/adr/0014-consolidation-recovery-and-candidate-dedup.md`, written by card `T23-01` — until
then every reference to that ADR is forward-looking. No gate `G00–G22` is reopened.

**This is the first group opened from the state of a live store rather than from the plan's own
queue,** and that is the reason it exists in this shape: every number in the diagnosis below was
measured on `~/.local/share/local-rag/state.sqlite` while the daemon was running, not derived from
reading code. The precedent paragraph in `TRACEABILITY.md` records what that changes about how a
group may be opened.

Goal: consolidation that can get itself unstuck without rebuilding the binary, a backlog an
operator can see the shape of, and a candidate queue that stops accumulating the same proposal.

References: ADR-0014 (created by `T23-01`); ADR-0006 (the local generator and its bounds);
ADR-0010/ADR-0011 (memory canon, for the entry shapes the router writes); spec 04 §4 (the
consolidation-run machine); spec 08 §3/§4 (candidates, the consolidation loop, and the accepted
cost this group revisits); spec 12 §3; `crates/store/src/memory/consolidation.rs`;
`crates/store/src/memory/runner.rs`; `crates/store/src/memory/stats.rs`;
`crates/memory/src/recall/mod.rs`; `crates/memory/src/router.rs`;
`crates/local-rag/src/daemon/consolidation_trigger.rs`; `crates/local-rag/src/cli/stats.rs`;
`crates/local-rag/src/cli/doctor.rs`; `D-024`, `D-050`, `D-052`, `D-058`, `D-061`, `D-071`,
`D-080`, `D-096` (the observability precedent), `X-005`.

Card format follows `TASK-TEMPLATE.md`; group structure follows groups 18/20/21/22. One task —
one iteration — one commit.

## Diagnosis (context for the executor — read before starting)

Every figure below is reproducible against a live store with `sqlite3 "file:<state.sqlite>?mode=ro"`.
They are recorded here so a later reader can tell what this group was actually looking at.

1. **Consolidation is stopped for two sessions, not globally — and the correction matters,
   because it changes which mechanism is at fault.** Remeasured while writing `T23-01`, and this
   supersedes the first reading recorded in `T23-00`'s evidence, which said "consolidation is
   stopped" on the strength of an instantaneous `throughput_observations_per_min` of `0.0`. That
   was wider than the evidence. Over one session the backlog moved 1386 → 1373, `applied` went
   11515 → 11522, and one session went from 17 un-consolidated observations to 4: **`D-058`'s
   shrink ladder works**, halving an overflowing 20-observation window to 10, and windows of 10
   have been applying every ~25 seconds since.

   What is genuinely stuck is narrower and worse: **two sessions are blocked permanently and hold
   1368 of the 1373 — 99.6 %**. Twenty-five sessions carry a failed run; twenty-one are harmless,
   their cursor already past.

2. **Two of those four are blocked permanently, and it is provable rather than suspected.** Their
   latest run is a `mechanical` dead-letter that is **not** a context overflow (`optimistic
   conflict: expected entry_version 2, found 3`, and `router output did not parse as a JSONL ops
   stream: EOF while parsing a string at line 1 column 4430`). No path forward exists for such a
   row:
   - `stale_runs` (`crates/store/src/memory/consolidation.rs`) deliberately excludes it —
     `mechanical` with a matching fingerprint — and that exclusion is right: it is the guard
     `D-050`/spec 08 §4 added so that
     no failure class retries unboundedly, after one window of three observations was retried 627
     times;
   - `dead_letter_shrink_decision` (same file) requires `last_failure_context_overflow`,
     so `D-058`'s shrink ladder does not apply;
   - the CLI cannot act on it. `doctor` and `stats` report it, `purge` deletes the session's data
     rather than consolidating it, and nothing else touches a run.

3. **The specification foresaw this and accepted its cost — under an assumption release `0.1.0`
   falsified.** Spec 08 §4 says, verbatim: "Knowingly accepted cost of (3): a genuinely long
   generator outage parks the runs it hits **until the binary is rebuilt** — a daemon restart does
   not revive them — and `open_next_run` blocks that session's whole backlog meanwhile." The
   escape is a rebuild, because a rebuild changes `BUILD_ID` and the fingerprint stops matching.
   A published release has no rebuild: checked on the downloaded `0.1.0`, its `BUILD_ID` is the
   literal `0.1.0`, constant for the life of that release. For anyone running a released binary
   the first mechanical dead-letter parks that session **forever**, with no supported remedy at
   all. The cost was accepted when the only user was a developer with `cargo build` at hand;
   publishing made the assumption false, not the cost larger.

4. **Candidate dedup is delegated entirely to the generator, and the generator cannot do it.**
   9564 pending candidates carry **3294** distinct texts — 66 % duplicates — and the worst single
   text has been proposed **476** times. The `conflicts` column is non-empty on **0 of 9564**.
   The mechanism is not a tuning problem: spec 08 §4 step 3 hands the router a "candidate conflict
   set", and `recall::candidate_conflict_set` builds it from `active_entries_for_scope` — **active
   entries only**. A pending candidate is not an entry, so a proposal identical to 475 pending
   siblings is never shown to the router, cannot be named as a conflict, and is written as a new
   row. There is no deterministic backstop in the store.

5. **The backlog is a single number.** `pending_backlog_total` has no per-session breakdown, so
   "all 1386 are behind four sessions" was only obtainable with hand-written SQL — exactly the gap
   `D-096` closed for indexing, in the same shape.

6. **Two root failures are defects in their own right, and are not the same defect as (2).** The
   `optimistic conflict` was retried ten times against a moving `entry_version` rather than
   re-read; and the router's output was truncated mid-string at 4430 characters, against a fixed
   `MAX_GENERATION_TOKENS = 1024` (`crates/memory/src/router.rs`) that does not scale with the
   window it must describe.

7. **A failed run's trace costs the session forever, and nobody had named it.**
   `latest_non_applied_run` selects `WHERE state != 'applied' ORDER BY created_at DESC LIMIT 1`,
   and nothing ever clears a `failed` row — so the run that overflowed once stays the blocking row
   for the life of the session, and `dead_letter_shrink_decision` keeps computing
   `previous_count / 2` against that same frozen window. A session that overflowed **once**
   therefore opens half-size windows permanently: verified, the recovering session's recent
   windows are all exactly 10 against a configured `consolidation_batch_size` of 20 — twice the
   runs and twice the generator calls, forever. `T23-03` lifts this as a side effect of clearing
   the row, which is a second reason the repair is not only for the permanently blocked.

## Baseline, recorded before any card runs

A group that means to move numbers has to state them first. Measured 2026-08-31:

| Measure | Before |
| --- | --- |
| Backlog | 1386 observations, 100 % behind 4 sessions |
| Throughput | 0.0 observations/min |
| Failed runs | 27, across 25 sessions |
| Pending candidates | 9564, over 3294 distinct texts |
| Candidates with a non-empty `conflicts` | 0 of 9564 |

## T23-00 — Scope registration

- **Depends on:** —.
- **Specification:** `TRACEABILITY.md` (the "new scope → ADR + group" rule); `CLAUDE.md`
  (deviation workflow).
- **Result:** this file, the `## 23` section in `PROGRESS.md`, rows `D-117`…`D-122` in
  `DEVIATIONS.md` (status `open`, corrective cards named), and the sixth precedent paragraph in
  `TRACEABILITY.md`.
- **In scope:** documentation only; every cross-reference must resolve.
- **Not in scope:** ADR-0014 itself — it is `T23-01`; any edit to `[FIXED]` text; any code.
- **Tests:** none. `cargo test -p xtask --test adr_links` is deliberately **not** run: no
  `docs/adr/*.md` is touched, and the ADR that would make it meaningful is created by `T23-01`.
  The `T22-00` evidence line sets that precedent.
- **Acceptance:** the listed files exist and are internally consistent; every reference either
  resolves to an existing file or is explicitly marked forward-looking with the card that creates
  it named; no `T23-01+` card counts as started before this one lands.
- **Evidence:** the `T23-00` row in "Task evidence" (`PROGRESS.md`).

## T23-01 — ADR-0014: recovery from a parked run, and where dedup lives

- **Depends on:** `T23-00`.
- **Specification:** ADR-0005 (form and bar); `TRACEABILITY.md` (the precedent list).
- **Result:** `docs/adr/0014-consolidation-recovery-and-candidate-dedup.md`, in Nygard form, in
  English, plus the sixth precedent paragraph's cross-reference.
- **In scope:** two decisions, each with its price stated. (1) A parked run must have a supported
  recovery path that does not require rebuilding the binary — and the ADR must say what "recovery"
  is allowed to mean, since abandoning a window advances a cursor past observations that were
  never consolidated, which is data loss of a bounded kind and must be recorded as such. (2)
  Candidate dedup is deterministic and lives in the store; the generator's `conflicts` output
  stays a hint rather than the only source. Rejected alternatives recorded, including "let the
  operator edit SQLite" (what has actually happened four times) and "make the fingerprint
  time-based rather than build-based" (revives the retry storm `D-050` stopped).
- **Not in scope:** any code; any `[FIXED]` edit — the ADR makes a change legal, the cards make it.
- **Tests:** `cargo test -p xtask --test adr_links`, whose link check now covers the new file.
- **Acceptance:** both decisions and both prices are named; every relative link resolves; the ADR
  does not silently lower a bar set by ADR-0006 or spec 08 without naming it.
- **Evidence:** the `T23-01` row in "Task evidence".

## T23-02 — The backlog says which sessions it is behind

- **Depends on:** `T23-01`.
- **Specification:** spec 08 §4; spec 11 §6 (`stats`, `doctor`).
- **Result:** `stats` and `doctor` report the backlog per session, together with what is holding
  each one.
- **In scope:** a module that owns the computation, with the commands only formatting it — the
  shape `crates/local-rag/src/cli/coverage.rs` established for `D-096`, for the same reason: two
  callers must not be
  able to disagree about the number.
- **Not in scope:** acting on anything; that is `T23-03`.
- **Tests:** a fixture store with a blocked session and a healthy one; the per-session breakdown
  must sum to `pending_backlog_total`, proved by a mutation that makes it not.
- **Acceptance:** the fact "all N observations are behind M sessions" is obtainable without SQL.
- **Evidence:** the `T23-02` row, with the live figure from the owner's store.

## T23-03 — A supported repair for a parked session

- **Depends on:** `T23-02`.
- **Specification:** ADR-0014 Decision 1; spec 04 §4; spec 08 §4; spec 12 §3 (audit).
- **Result:** a command that clears a session's block without rebuilding the binary.
- **In scope:** both directions ADR-0014 allows — retry a parked run on demand, and declare a
  window unconsolidatable and move the cursor past it — each writing an `audit_event` that says
  which observations were skipped and why. Whatever the repair does, it must not resurrect the
  retry storm `D-050` stopped: an explicit operator action is not a background retry.
- **Not in scope:** the root causes; they are `T23-04`…`T23-06`.
- **Tests:** a parked mechanical non-overflow run is repairable; the audit row names the skipped
  range; the repair is idempotent; and a **regression against the storm**: the repair must not make
  `stale_runs` pick the row up again on its own.
- **Acceptance:** on the owner's live store, the four blocked sessions clear and the backlog moves
  without hand-written SQL. This is the card that unsticks the store — the fix and the remedy are
  one piece of work here, not two.
- **Evidence:** the `T23-03` row, with before/after backlog from the live store.

## T23-04 — The window is bounded by tokens, not by row count

- **Depends on:** `T23-01`.
- **Specification:** spec 08 §4; `config.memory.router_conflict_token_budget`'s own derivation.
- **Result:** a window that cannot deterministically overflow the model's context.
- **In scope:** the same treatment the conflict set already gets — **`D-095`** capped that at
  12 000 tokens with `estimate_tokens` after measuring the identical failure (this card first
  credited `D-080`, which is the *ordering* fix; corrected while executing `T23-04`) — applied to
  the window itself, which is currently bounded only by `consolidation_batch_size` (20) on the
  assumption of "short excerpts". Removing the class at its source, not shrinking after the
  failure: `D-058`'s ladder stays as the backstop it was designed to be.
- **Not in scope:** changing what a window means, or the trigger's cadence.
- **Tests:** a window of oversized observations is split rather than failed; the arithmetic
  (context − answer reserve − prompt) is asserted, not hard-coded twice.
- **Acceptance:** the `context overflow` failure class stops being reachable by a normal window.
- **Evidence:** the `T23-04` row.

## T23-05 — An optimistic conflict is re-read, not retried

- **Depends on:** `T23-01`.
- **Specification:** spec 08 §4 step 4; spec 04 §4.
- **Result:** a stale plan is refreshed rather than replayed.
- **In scope:** `expected entry_version 2, found 3` is not a transient fault. This card first read
  it as a plan built against a version an outside writer had moved; **measured while executing
  `T23-05`, all eight live rows are self-inflicted** — `found = expected + 1` on every one, op
  index at least 3 on every one, and for one of them the audit trail shows no entry anywhere
  reaching the "found" version during the failure window. The cause is that
  `guard::materialize` captures every op's `expected_version` before any op is applied, so two ops
  naming one entry — which `D-078`'s rewrite produces whenever a window proposes one stored text
  twice — both hold the version the first will move. Also corrected: "ten identical retries cannot
  succeed" is too strong. Seven of the eight runs eventually **applied**, because a re-sampled plan
  happened not to duplicate; the cost is eight burned local generations per occurrence, and it
  became an absolute block exactly once — on the run holding 1081 observations.
- **Not in scope:** the version-check itself, which is correct and stays — and after this card it is
  untouched literally: `crates/store` does not change.
- **Tests:** a routed plan naming one entry twice applies, with both proposals' citations merged;
  the same plan un-collapsed still fails with the live string verbatim; an outside writer between
  plan and apply still conflicts, and only re-planning converges; and the rejected store-side fix's
  own trap is pinned — two ops that pass the version check collide on `memory_evidence`'s primary
  key instead. Each rule proved by a named mutation.
- **Acceptance:** a plan that names one entry twice can no longer reach the store.
- **Evidence:** the `T23-05` row.

## T23-06 — The router's answer budget follows its window

- **Depends on:** `T23-01`.
- **Specification:** spec 08 §4 step 3 (two-tier malformed-output handling); ADR-0006.
- **Result:** the generator is not asked to describe a window in fewer tokens than the answer needs.
- **In scope:** `MAX_GENERATION_TOKENS = 1024` (`crates/memory/src/router.rs`) is a constant
  against a window of up to 20 observations; the live failure truncated a string at 4430
  characters and then failed the whole window after its one corrective re-prompt. This card first
  read the fix as "derive the answer from the window" — **measured while executing `T23-06`, that
  premise does not hold**: window content explains roughly 2 % of the variance in real answer
  size, and a one-observation window has produced more operations on average than a
  twenty-observation one. The fix is a flat reserve measured from real regenerated answers
  (`crates/local-rag/tests/prompt_budget_live.rs::measure_real_answer_tokens`), independent of the
  window and of `consolidation_batch_size`, derived in the same `PromptBudget::derive` function
  `T23-04` built — the interaction stays stated in one place, it is just not the interaction this
  card first expected. The measurement also found a second, independent reason the tail of the
  answer-size distribution is large: greedy decoding can degenerate into verbatim repetition for
  a real window, reproducibly, which no finite reserve fixes (`D-130`) — the reserve is sized from
  the distribution of answers that finish on their own, not from that tail.
- **Not in scope:** the corrective re-prompt itself, which stays; `FailureKind` classification of a
  truncated answer, which is `T23-10`'s card.
- **Tests:** a window that legitimately needs a long answer completes under the new reserve, and
  the same plan truncates under the old fixed one (an in-tree control, not a mutation nobody
  re-runs); the derivation is asserted, not hardcoded twice.
- **Acceptance:** truncation stops being a window-failing class — not eliminated outright (a
  single sufficiently verbose operation can still exceed any finite reserve; that residual is
  `T23-10`'s to make recoverable, not this card's to close).
- **Evidence:** the `T23-06` row.

## T23-07 — Deterministic candidate dedup in the store

- **Depends on:** `T23-01`.
- **Specification:** ADR-0014 Decision 2; spec 08 §3/§4 step 3.
- **Result:** a proposal identical to something already pending, or already an entry, does not
  create another row.
- **In scope:** the gap is precise and must be fixed at its cause: `recall::candidate_conflict_set`
  builds from `active_entries_for_scope`, so a pending candidate is structurally invisible to the
  router that is supposed to notice it. Both halves matter — show pending candidates to the router,
  *and* keep a deterministic store-side check so the answer does not depend on a 4B model noticing.
  A second, adjacent defect is folded in rather than deferred: `D-127` (found while executing
  `T23-05`) registered this card by name as its home — `D-078`'s create-vs-reinforce rewrite checks
  the store, never the plan being built, so two `create`s of one not-yet-stored text in one window
  both mint an entry. The store-side check (candidate-vs-candidate, candidate-vs-entry) lives in
  `local_rag_store::memory::propose_candidate`'s own transaction, where it also absorbs the
  within-window candidate-vs-candidate case for free (a later insert sees an earlier one's
  uncommitted write); the create-vs-create case is `local_rag_memory::plan::collapse`'s, which
  gains a real grouping key for `create` (keyed on scope + text, since a fresh create has no
  `memory_id` yet for anything else to name).
- **Not in scope:** near-duplicate or semantic dedup, exact-text first, measured before anything
  fuzzier is proposed; and `approve_candidate`'s own blind spot (it never checks a candidate's text
  against active entries before materializing it, so approving two pre-existing duplicate
  candidates can still mint two entries) — the card's Result sentence is about a *proposal* not
  creating another row, i.e. propose-time, not approve-time; registered as `D-131`, open, rather
  than silently left unrecorded or silently pulled into this card's scope.
- **Tests:** the same proposal twice yields one row; a genuinely different proposal is unaffected;
  the store-side check is proved by a mutation that removes it while the generator still says
  nothing.
- **Acceptance:** on the live store, re-running a window that previously produced a duplicate
  produces none.
- **Evidence:** the `T23-07` row, with the distinct-vs-total figure remeasured.

## T23-08 — The queue can be triaged in bulk

- **Depends on:** `T23-07`.
- **Specification:** spec 08 §3; spec 11 §2/§6.
- **Result:** 9564 candidates can be worked through by something other than one call per candidate.
- **In scope:** grouping identical and near-identical proposals so a decision applies to a group;
  the existing per-candidate review path (`approve`/`reject`/`edit`) stays and remains the only way
  a candidate becomes an entry.
- **Not in scope:** automatic approval. A candidate exists because a human decides; bulk **reject**
  of exact duplicates is a different act from bulk accept, and the card must not blur them.
- **Tests:** a bulk decision touches exactly the group it names; a group of one behaves like the
  existing path.
- **Acceptance:** the owner's queue is reducible without losing a distinct proposal — the
  distinct-text count is the invariant, not the total.
- **Evidence:** the `T23-08` row.

## T23-09 — The payload TTL sweep is actually scheduled

- **Depends on:** `T23-03`. **The order is load-bearing and is not a preference:** this sweep
  deletes exactly the payloads of the observations `T23-03` exists to rescue. Run it first and
  there is nothing left to consolidate.
- **Specification:** spec 12 §3 `[FIXED]` — "`observation_payload` under real TTL
  (`payload_ttl_hours`), enforced by a sweeper"; `D-066`'s precedent for a sweep with no caller.
- **Result:** the TTL is enforced by something other than a human remembering to type a command.
- **In scope:** giving `run_payload_ttl_sweep` (T13-05) a scheduler. `crates/local-rag/src/daemon/gc.rs`
  already runs the generation-retention sweep at daemon startup for `D-066` and carries the
  reasoning for why that trigger was chosen; this is the same shape and the same place.
- **Not in scope:** changing `payload_ttl_hours`, the sweep's own logic, or the other sweeps that
  `local-rag gc` runs — whether they too want a schedule is a separate question this card must not
  answer by accident.
- **Tests:** an overdue payload is removed without a manual command; an envelope whose payload was
  removed is untouched (envelope survival past payload expiry is structural, T13-05's own words);
  a mutation proving the schedule fires rather than the test calling the sweep itself.
- **Acceptance:** on the owner's store the overdue count falls from its measured 45651 without
  `local-rag gc` being typed, **after** `T23-03` has rescued the backlog.
- **Evidence:** the `T23-09` row, with the overdue count before and after.

## T23-10 — A truncated generation is not a mechanical failure

- **Depends on:** `T23-03` (which is how it was found) and `T23-06` (whose budget fix makes the
  case rarer; this one makes it recoverable when it still happens).
- **Specification:** `D-124`; spec 04 §4; spec 08 §4's two-tier malformed-output handling;
  `FailureKind`'s own contract.
- **Result:** a failure that is a property of one sampling stops being parked as if it were a
  property of the code.
- **In scope:** the classification. `Mechanical` claims a failure "reproduces identically on an
  unchanged rebuild", which licenses `stale_runs` to abandon it until the binary changes. A
  router answer truncated at `MAX_GENERATION_TOKENS` does not reproduce identically — proved live:
  a single retry of a session parked for a day applied, and its backlog moved 287 → 277. `Transient`
  already carries a bounded backoff and the `TRANSIENT_ATTEMPT_CAP` escalation, so moving this class
  there does not reintroduce `D-050`'s storm.
- **Not in scope:** the answer budget (`T23-06`), and any other failure that genuinely is a code
  defect — a schema violation still reproduces identically and must stay `Mechanical`.
- **Tests:** a parse failure of generated output classifies as recoverable; a SQLite constraint
  violation still classifies as mechanical; the attempt cap still escalates, so nothing retries
  unboundedly. Proved by mutation both ways.
- **Acceptance:** a session parked on a truncated answer recovers without an operator command.
- **Evidence:** the `T23-10` row.

## G23 — Consolidation and candidate review gate

- **Depends on:** every card of the group.
- **Specification:** spec 04 §4; spec 08 §3/§4; spec 11 §6; spec 12 §3; ADR-0014.
- **Result:** `PASS`, `PASS after D-NNN` or `BLOCKED` in the Gate results table.
- **In scope:** reread the named sections; build the requirement → code → test trace; check every
  `[FIXED]`/`[SPEC]`/`[OPEN]` the group touched — in particular spec 08 §4's accepted-cost
  paragraph, which this group's premise contradicts and which must end up saying what is true;
  confirm `D-117`…`D-124` are all `resolved`; run both JS suites and `cargo xtask ci`; and
  **remeasure the baseline table above on the live store**, since a group opened from a live store
  is answerable to it.
- **Not in scope:** reopening `G00`–`G22`.
- **Acceptance:** the trace is reproducible and the evidence is appended to `PROGRESS.md`.
