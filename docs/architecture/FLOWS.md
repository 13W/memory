# Flows

Every distinct behaviour of local-rag v2: what triggers it, what it does in
order, where it takes a lock or opens a transaction, the exact point after which
its effect survives a kill, and what a caller sees when it fails.

The structural views in this model say who talks to whom. This document and the
dynamic views say *when*, and — more importantly — *what is still true after a
crash*. Read a flow's `DURABILITY` line first; almost every design decision in
this system is downstream of one.

## The six recovery shapes

A hundred flows were catalogued to write this. They resolve into six recovery
shapes, and everything else is a repetition of one of them. This is the real
table of contents.

**1. Re-read from a durable cursor.** The consumer records how far it got, in
the same transaction as the work that got it there. A crash re-reads; dedup
absorbs the overlap. *Spool import, consolidation runs, segment cleanup.*

**2. Detect at open, rebuild from the canonical store.** The artifact carries a
validity proof written strictly last. Any observation contradicting it means
"rebuild" regardless of what the stored status claims, because a cache that
proved itself once is not a cache that is still right. *Dense shards (all twelve
fault rows), the lexical view, `cache.sqlite` as a whole.*

**3. Wait out a lock.** Contention proves a live holder, so the contender waits
rather than reclaiming, and the waiter's budget is sized to sit inside its
caller's. *Store lock acquisition and handover, the per-worktree read lock.*

**4. Classify and back off.** A failure is either reproducible or not, and the
two get different budgets — a fingerprint that a rebuild moves, or an
exponential backoff that escalates into the same dead letter at a cap. No class
retries unboundedly. *Consolidation runs, memory normalization, reconcile
retries, the proxy's connect loop.*

**5. Refuse with a typed code.** The caller is told what happened in a form it
can act on: a degradation carrying its reason, or an error code with
`retryable` set honestly. Nothing degrades silently. *Search, MCP dispatch,
migration refusal, every memory op precondition.*

**6. Ask a human.** Some states cannot be resolved without a decision. These are
the ones worth counting, because each is a place the system stops on its own.
*Parked consolidation runs, purge, vacuum, project enrolment, rollback from a
backup.*

## How to read a flow block

```
ID          the flow's name, matching its dynamic view where one exists
TRIGGER     what starts it
STEPS       ordered, verb first, naming the component that acts
LOCKS+TX    which lock, and where a transaction opens and commits
DURABILITY  the point after which the effect survives a kill
FAILURE     per distinguishable outcome, with the code or degraded flag
SPEC        the normative section (never an as-built note — those supersede)
CODE        the module that executes it
```

A flow with a `view:` entry in the index has a dynamic view of the same name;
open it with `likec4 start docs/architecture`. A flow marked `no view` is
catalogued here deliberately — it is a step inside another flow, or one of a
family whose members differ only in their detection signal.

## Index

| Flow | Subsystem | View | Spec |
| --- | --- | --- | --- |
| proxy-connect-or-spawn | transport | `proxyHandshake` | 02 §4.2 |
| handshake-hello-welcome | transport | `proxyHandshake` | 02 §4.2, 11 §4 |
| proxy-relay-passthrough | transport | `proxyHandshake` | 11 §1 |
| spool-format-version-check | transport | no view | 11 §4, 07 §3 |
| daemon-startup | lifecycle | `daemonLifecycle` | 02 §4.1 |
| store-lock-acquire | lifecycle | `storeLockHandover` | 02 §4.1, §4.4 |
| store-lock-conditional-release | lifecycle | `storeLockHandover` | 02 §4.3, §4.4 |
| daemon-migration-only-mode | lifecycle | `migrationRun` | 02 §4.1, §6 |
| cache-open-validate-recreate | lifecycle | `daemonLifecycle` | 03 §4.4 |
| daemon-upgrade-shutdown-request | lifecycle | `binaryUpgrade` | 13 §4, 02 §4.2 |
| daemon-drain-shutdown | lifecycle | `daemonLifecycle` | 02 §4.3 |
| daemon-idle-shutdown | lifecycle | `idleShutdown` | 02 §4.3 |
| migration-run | lifecycle | `migrationRun` | 13 §3 |
| migration-refuse-newer-store | lifecycle | `migrationRun` | 13 §3, 02 §6 |
| migration-rollback-restore | lifecycle | no view | 13 §3 |
| v1-to-v2-data-migration | lifecycle | no view — nothing built | 13 §3 `[OPEN]` |
| wal-checkpoint-policy | maintenance | no view | 03 §3 |
| vacuum-reclaim | maintenance | `retentionSweep` | 11 §6, 03 §3 |
| reconcile-trigger-scheduling | indexing | `indexingCycle` | 06 §1 |
| watcher-overflow-strict-rescan | indexing | no view | 06 §1 |
| scan-fast-vs-strict | indexing | `indexingCycle` | 06 §1, §2 |
| reconcile-no-change-short-circuit | indexing | no view | 06 §2 |
| build-generation | indexing | `indexingCycle` | 06 §2, 04 §1 |
| classify-skip-precedence | indexing | `indexingCycle` | 06 §2.2, 12 §2 |
| universal-indexing-path | indexing | no view | 06 §2.1 |
| generation-lifecycle-transitions | indexing | no view | 04 §1 |
| superseded-generation-sweep | indexing | `indexingCycle` | 04 §1, 06 §5 |
| embedding-backfill | indexing | `indexingCycle` | 10 §3, §4 |
| memory-embedding-backfill | indexing | no view | 10 §4 |
| model-space-double-buffer-migration | indexing | `modelSpaceMigration` | 10 §4, 04 §3 |
| model-space-state-machine | indexing | no view | 04 §3 |
| projection-switch-generation-axis | indexing | `projectionSwitch` | 05 §5 |
| projection-switch-model-axis | indexing | `modelSpaceMigration` | 05 §5, 04 §8 |
| expected-point-set-derivation | indexing | no view | 05 §3, §4 |
| dormant-worktree-model-migration | indexing | no view | 05 §8 |
| projection-validate-on-open | indexing | `projectionRebuild` | 05 §6 |
| projection-rebuild | indexing | `projectionRebuild` | 05 §7 |
| shard-manager-acquire-and-evict | indexing | no view | 05 §8, 02 §5 |
| search-path-shard-no-repair | search | `searchDegradation` | 05 §6, 11 §1 |
| projection-fault-matrix (F1–F12) | indexing | table below | 05 §10 |
| orphan-shard-sweep | maintenance | `retentionSweep` | 05 §8 |
| expired-shard-grace-destroy | maintenance | no view | 05 §8, 04 §7 |
| unreferenced-model-space-sweep | maintenance | `modelSpaceMigration` | 05 §8 |
| quarantine-retention-rotation | maintenance | `projectionRebuild` | 05 §7, §8 |
| fts-materialize | indexing | `indexingCycle` | 06 §4 |
| fts-validate-per-search | search | `ftsValidateAndRebuild` | 06 §4 |
| fts-rebuild | indexing | `ftsValidateAndRebuild` | 06 §4 |
| fts-corruption-recovery | indexing | `ftsValidateAndRebuild` | 06 §4, 03 §4.4 |
| fts-tokenizer-version-bump | indexing | no view | 09 §2, 06 §4 |
| search-hybrid | search | `searchRequest` | 09 §1–§4 |
| search-mode-selection | search | `searchDegradation` | 09 §5 |
| search-semantic-refusal | search | `searchDegradation` | 09 §5 |
| search-query-normalization | search | no view | 09 §1, ADR-0011 |
| get-file-context | search | no view | 11 §2 |
| project-overview | search | no view | 11 §2 |
| snippet-cut | search | `searchRequest` | 09 §7, 12 §2 |
| mcp-dispatch | transport | `searchDegradation` | 11 §2, 02 §6 |
| hook-spool-append | observations | `observationCapture` | 07 §1–§3 |
| segment-rotation | observations | no view | 07 §2 |
| source-identity-derivation | observations | no view | 07 §4 |
| hook-not-installed | observations | no view | 11 §3.2, 13 §2 |
| spool-import-session-tail | observations | `spoolImport` | 07 §5 |
| spool-dedup | observations | `spoolImport` | 07 §4, §5 |
| spool-torn-tail-vs-corruption | observations | `spoolImport` | 07 §3, §5 |
| segment-cleanup | observations | `spoolImport` | 07 §5, §6 |
| spool-session-gc | maintenance | no view | 07 §6 |
| payload-ttl-sweep | maintenance | no view | 12 §3 |
| spool-kill-matrix (S1–S8) | observations | table below | 07 §7 |
| consolidation-trigger-tick | memory | `consolidationRun` | 07 §6, 08 §4 |
| startup-consolidation-resume | memory | `daemonLifecycle` | 02 §4.1, 04 §4 |
| consolidation-run | memory | `consolidationRun` | 08 §4 |
| window-and-conflict-set-budgeting | memory | `consolidationFailureLadder` | 08 §4 |
| router-malformed-output-recovery | memory | no view | 08 §4 |
| guard-materialize | memory | `candidateReview` | 08 §3, §4, 12 §4 |
| plan-collapse | memory | `consolidationRun` | 08 §3, §4 |
| consolidation-failure-classification | memory | `consolidationFailureLadder` | 08 §4 |
| consolidation-operator-repair | memory | `operatorRepair` | 11 §6, ADR-0014 |
| transactional-memory-op | memory | `rememberWrite` | 08 §3 |
| entry-lifecycle-state-machines | memory | no view | 04 §5 |
| candidate-lifecycle | memory | `candidateReview` | 04 §6 |
| candidate-dedup | memory | no view — unbuilt | ADR-0014 |
| review-verbs | memory | no view | 08 §3, 11 §2 |
| give-feedback | memory | `giveFeedback` | 11 §2, 07 §1 |
| recall-pipeline | memory | `recallInjection` | 08 §6 |
| recall-additionalcontext-formatting | memory | `recallInjection` | 11 §5, 12 §4 |
| recall-hook-injection | memory | `recallInjection` | 11 §3.2 |
| english-write-boundary | memory | `rememberWrite` | 08 §3, ADR-0011 |
| normalization-worker-tick | memory | `normalizationTick` | 02 §4.3 |
| normalization-apply-ordering | memory | `normalizationTick` | 10 §4, 03 §1.4 |
| retention-mark-and-sweep | maintenance | `retentionSweep` | 06 §5 |
| privacy-inspect-export-purge | maintenance | `purgeMemory` | 12 §3, §5 |
| npm-install-postinstall | delivery | `installAndHeal` | 13 §1, §2 |
| binary-resolution-npm | delivery | `installAndHeal` | 13 §1 |
| plugin-executable-resolution | delivery | no view | 13 §2, 11 §3.2 |
| model-and-runtime-install | delivery | `modelDownload` | 10 §5 |
| managed-project-enrollment | delivery | `projectEnrollment` | 11 §8, ADR-0009 |
| indexing-supervisor-reconcile | indexing | `supervisorReconcile` | 11 §8 |
| doctor | maintenance | `doctorDiagnosis` | 11 §6 |
| stats | maintenance | no view | 11 §2, §6 |
| project-coverage | maintenance | no view | 11 §6, §8 |

## Transport and daemon lifecycle

### proxy-connect-or-spawn · handshake-hello-welcome · proxy-relay-passthrough
**view** `proxyHandshake` · **spec** 02 §4.2, 11 §1, §4

- **TRIGGER** Claude Code launches a proxy for a session.
- **STEPS** Resolve the session parameters once at launch → connect → on failure
  spawn the daemon binary **once** (not per retry), detached, its own process
  group, all stdio null → retry with backoff → HELLO → WELCOME → relay every
  line, wrapping the opaque MCP payload with the request context.
- **LOCKS+TX** None in the proxy; it holds no project state, structurally — the
  crate depends on no store, index or embed crate.
- **DURABILITY** None. Pure connection setup.
- **FAILURE** No connect inside the 20 s budget → `ProxyError` on **stderr**,
  non-zero exit, stdout byte-empty because it is the JSON-RPC stream. A proto
  range mismatch → `INCOMPATIBLE` naming both versions; the two other version
  fields in WELCOME are informational and never reject. Backoff 250 ms doubling,
  4 s cap, 20 s total.
- **CODE** `crates/local-rag-proxy/src/connect.rs`, `crates/protocol/src/handshake.rs`

### daemon-startup · cache-open-validate-recreate
**view** `daemonLifecycle` · **spec** 02 §4.1, 03 §4.4

- **TRIGGER** `local-rag serve`, or the proxy's detached spawn.
- **STEPS** Install the signal handler **before** anything else → acquire the
  store lock → open state, migrate under the migration lock, seed the store
  instance uuid → open and validate the cache → bind, write the readiness
  marker, start workers → resume the pending spool import.
- **LOCKS+TX** L0 for the process lifetime; L1 only while migrating; one short
  transaction for the uuid seed.
- **DURABILITY** The store is usable at the migration commit; the daemon is
  reachable at the readiness marker.
- **FAILURE** A cache whose store uuid or schema version does not match is
  dropped and recreated — losing it loses nothing. Interrupted projection
  switches are deliberately **not** resumed here; they are detected lazily at
  shard open. A migration refusal binds anyway in migration-only mode.
- **CODE** `crates/local-rag/src/daemon/lifecycle.rs`, `crates/store/src/cache/open.rs`

### store-lock-acquire · store-lock-conditional-release
**view** `storeLockHandover` · **spec** 02 §4.1, §4.3, §4.4

- **TRIGGER** Startup step 1; and the end of every drain.
- **STEPS** `flock(LOCK_EX|LOCK_NB)`. **Success** proves no live holder — the
  kernel releases a flock when its holder dies — so overwrite the record and
  unlink an orphaned socket. **Contention** proves the opposite, so wait up to
  10 s and never reclaim, for any reason. On release, unlink only while the path
  still resolves to the inode the guard holds; if a worker outlived the drain
  budget, leak the guard instead and let process exit drop the lock.
- **LOCKS+TX** L0 only.
- **DURABILITY** Exclusivity is bounded by the **process lifetime**, not by the
  shutdown sequence.
- **FAILURE** Budget exhausted → `STORE_LOCKED`, or `MIGRATION_IN_PROGRESS`
  when the record says not-ready; the owner is named when the record parses and
  `unknown_owner` when it does not. Reclaiming would leave the incumbent's real
  flock in place — it lives on the open file description, not the path —
  producing two daemons each convinced it owns the store. That was measured at
  twelve seconds on a live store before the rule changed.
- **CODE** `crates/local-rag/src/daemon/lock.rs`

### daemon-drain-shutdown
**view** `daemonLifecycle` · **spec** 02 §4.3

- **TRIGGER** SIGTERM, Ctrl-C, a shutdown request, or the idle gate.
- **STEPS** Stop accepting **first** → signal every worker and cancel the
  supervisor before waiting on any of them → wait, bounded → checkpoint both
  databases → release the lock, conditionally.
- **LOCKS+TX** Cancellation lands on await points that are genuine transaction
  boundaries. A cancelled caller never cancels its **queued** transaction: the
  writer thread owns it, the queue is FIFO, so it lands before the checkpoint.
- **DURABILITY** Everything committed before the wait survives; a consolidation
  run that did not reach its apply stays running with a lease and is reclaimed
  at the next startup.
- **FAILURE** The closing truncating checkpoint is deliberately unbounded — it
  is the step that returns disk. Signalling is cheap and waiting is not, which
  is why the order is signal-all-then-wait: before that, two fresh indexing
  cycles were logged 31 and 49 seconds *into* a shutdown that took 110 s. After:
  6.5 s.
- **CODE** `crates/local-rag/src/daemon/lifecycle.rs`

### migration-run · migration-refuse-newer-store · daemon-migration-only-mode
**view** `migrationRun` · **spec** 13 §3, 02 §4.1, §6

- **TRIGGER** Opening a store behind head, or one this binary cannot support.
- **STEPS** Take the migration lock → compare applied versions against frozen
  checksums → back up before any destructive step → apply forward-only, each
  unit idempotent and writing a progress row.
- **LOCKS+TX** L1 for the run; per-step transactions.
- **DURABILITY** The progress row is the resume point — a crash resumes at that
  unit, never re-running a completed one.
- **FAILURE** A newer store or drifted checksums are refused **before any
  write**. The daemon does not abort: it binds and reports `INCOMPATIBLE_STORE`
  with details, so a client gets a diagnosable answer rather than nothing
  reachable. Every tool call short-circuits, including `health` and `stats`;
  `admin/*` answer `available: false` rather than a fabricated number.
  `cache.sqlite` is never migrated — a version bump drops and rebuilds it.
- **CODE** `crates/store/src/migrate/mod.rs`, `crates/local-rag/src/daemon/error.rs`

### daemon-idle-shutdown
**view** `idleShutdown` · **spec** 02 §4.3

- **TRIGGER** The gate evaluating true for the configured window (900 s).
- **STEPS** Read live sessions, unimported spool bytes, running jobs; conjoin.
- **DURABILITY** n/a.
- **FAILURE** None — a false negative only keeps the daemon alive. The
  load-bearing invariant lives in the workers: every job guard is held across
  active work only, so an enrolled but quiet worktree is exactly as
  idle-eligible as none at all. Watching a filesystem is not running.
- **CODE** `crates/local-rag/src/daemon/idle.rs`

## Indexing

### reconcile-trigger-scheduling · scan-fast-vs-strict · build-generation · classify-skip-precedence · superseded-generation-sweep
**view** `indexingCycle` · **spec** 06 §1, §2, §2.2, 04 §1

- **TRIGGER** A filesystem or git-head event (debounced 500 ms, fast scan), or
  startup / periodic (6 h) / manual / watcher overflow, which bypass the
  debounce and force a strict scan.
- **STEPS** Scan gitignore-aware → classify by a first-match-wins precedence
  chain (ignored, huge, lfs, binary, encoding, secret) → reuse an existing file
  revision when content hash and parser fingerprint match, with **no read and no
  parse** → build generation N+1 → move every lower-numbered straggler to failed
  → backfill embeddings → switch → materialize the lexical view → checkpoint.
- **LOCKS+TX** L2 write for the cycle; one transaction **per file** during the
  build; IO and parsing run off the writer thread.
- **DURABILITY** Each file's transaction; the generation is complete at
  `projection_ready`.
- **FAILURE** Any error moves the generation to failed; because it is a disjoint
  row set, no previously built generation is mutated and a retry allocates a
  fresh one. Watcher overflow is a **mandatory** strict rescan — never a resync
  from events. A scan reproducing the last built manifest mints nothing.
- **INVARIANT** Every scanned file is in `generation_file` or `skipped_file`,
  and in exactly one of them. The precedence chain is a chain of preconditions,
  so short-circuiting a cheaper reason can never let a secret-bearing file be
  indexed — and a skipped file gets no source blob and no occurrences, which is
  why a false positive here deletes working source from the index.
- **CODE** `crates/index/src/reconcile/`, `crates/local-rag/src/indexing/mod.rs`

### projection-switch-generation-axis
**view** `projectionSwitch` · **spec** 05 §5, 04 §1, §2

- **TRIGGER** A generation reaching `projection_ready`.
- **STEPS** Commit the intent **before** any backend mutation (status updating,
  target tuple, fresh op id) → reconcile the shard to the desired point set →
  write the head **last** → commit the result, activating N+1 and retiring N in
  one transaction.
- **LOCKS+TX** L2 write across the backend work; two state transactions
  bracketing it.
- **DURABILITY** The switch is durable at the second commit.
- **FAILURE** A crash between the two leaves either a non-clean status or an
  inconsistent head — both detectable at the next open, both funnelling into one
  rebuild. This is **not** a command log: recovery after an unknown partial
  delta needs no history, because the shard is reconciled to a set rather than
  replayed. Retrying is calling the same switch again.
- **CODE** `crates/projection/src/switch.rs`

### projection-validate-on-open · projection-rebuild · quarantine-retention-rotation
**view** `projectionRebuild` · **spec** 05 §6, §7, §8

- **TRIGGER** Every shard open — daemon start, LRU re-open, post-crash.
- **STEPS** Check status, projected against active, head presence and op id,
  point count, manifest hash → mark dirty with the reason → dirty to rebuilding
  with a fresh op id and the target cleared → destroy, or quarantine when the
  shard could not be opened at all → collect every vector **before** any shard
  write → recreate, head last → clean.
- **LOCKS+TX** L2 write; **three** separately committed transactions, because
  the state machine only allows dirty to rebuilding.
- **DURABILITY** The validation *result* is recorded before the rebuild, so a
  crash during validation re-enters the same path.
- **FAILURE** A missing vector aborts before any shard write, so a shard never
  goes clean with a partial set. The stored status is itself untrusted until
  validated — that is what "the dense projection is always an untrusted cache"
  means operationally. Quarantine keeps at most two per worktree.
- **CODE** `crates/projection/src/{validate,rebuild}.rs`

### model-space-double-buffer-migration · projection-switch-model-axis · unreferenced-model-space-sweep
**view** `modelSpaceMigration` · **spec** 10 §4, 04 §3, §8, 05 §8

- **TRIGGER** Changing the embedding model, dimensions, metric or normalization.
- **STEPS** Register the new space as building → backfill under its
  representations → promote on full coverage → switch **per worktree** on the
  model axis only → repoint the default → retire the old space and sweep its
  now-unreferenced directories.
- **LOCKS+TX** L2 write per worktree, serialized with generation switches by the
  same writer. Both axes never move in one operation; a combined request is two
  sequential switches.
- **DURABILITY** Until the switch commits **for a given worktree, that worktree
  still runs the old space entirely** — literally, not merely recoverably,
  because the old shard lives in its own per-space directory.
- **FAILURE** The last two steps are order-enforced: retiring the old space
  while it is still the default is refused, because a default nothing can
  migrate to silently stops dormant migration. The sweep is race-free without a
  lock because a live directory is one named in **any** of the three state
  columns, and the write-ahead sets the target before the directory exists.
- **CODE** `crates/projection/src/model_switch.rs`, `crates/store/src/housekeeping.rs`

### fts-materialize · fts-validate-per-search · fts-rebuild · fts-corruption-recovery
**view** `ftsValidateAndRebuild` · **spec** 06 §4, 03 §4.3

- **TRIGGER** The materialize step of a cycle; and every search wanting the
  lexical leg.
- **STEPS** Recompute missing normalized text → delete the worktree's stale rows
  → insert the fresh set → write the head **last**. On search: check head
  presence, generation, schema and tokenizer versions, occurrence count.
- **LOCKS+TX** Literally one cache transaction per generation update. The single
  bounded writer serializes concurrent rebuild attempts into individually valid
  commits.
- **DURABILITY** The head's write is the proof; an interrupted materialization
  leaves the previous valid head in place.
- **FAILURE** Invalid head → degrade to dense-only with the reason verbatim, and
  rebuild — synchronously under the size threshold, in the background above it.
  **An empty lexical index is never treated as a correct lexical result.** Both
  predicates read the cache's actual content, not what the canonical store
  expects: sourcing them from the expectation once made a direct corruption
  invisible to both checks.
- **CODE** `crates/store/src/cache/{fts,validate}.rs`

### indexing-supervisor-reconcile
**view** `supervisorReconcile` · **spec** 11 §8, ADR-0009

- **TRIGGER** Startup, an explicit reload, or the backstop poll.
- **STEPS** Read every enrolled row → start one task per enabled row, batched →
  each task forces one immediate startup reconcile, then loops.
- **LOCKS+TX** The job guard is held **only** across the projection call, never
  while watching — the read lock's value here is giving a concurrent search a
  retryable code, not writer exclusion, since the single owning task already
  guarantees one writer.
- **DURABILITY** The enrolment table outlives the process; the live task map is
  never back-filled from it, and they differ deliberately.
- **FAILURE** Notification is a hint, the table is the truth. Each task runs on
  its own OS thread with a local task set, because the backfill holds a read
  connection across awaits and the pipeline future is therefore not `Send` —
  and that arrangement is what still yields a genuine preemptive cancel.
- **CODE** `crates/local-rag/src/daemon/indexing/`

## Search

### search-hybrid · snippet-cut
**view** `searchRequest` · **spec** 09 §1–§4, §7, 06 §3

- **TRIGGER** `search_code`.
- **STEPS** Normalize the query **above** the engine → resolve the worktree
  **before any lock** → take the read lock for the whole pipeline → resolve the
  active tuple under it → run only the legs the mode asked for → fuse by
  reciprocal rank → cut snippets from the stored source bytes → release.
- **LOCKS+TX** The read lock spans everything; the shard map lock is taken only
  for the lookup. Only the *wait* is bounded, never a body already in flight.
- **DURABILITY** Read-only. Nothing on this path ever writes, including repair.
- **FAILURE** See `searchDegradation`. A snippet is cut from the revision's
  stored bytes, never the live file, and a truncation carries a hash over the
  **full** span — hashing what survived would answer a different question.
- **CODE** `crates/search/src/pipeline.rs`, `crates/search/src/snippet.rs`

### search-mode-selection · search-semantic-refusal · search-path-shard-no-repair · mcp-dispatch
**view** `searchDegradation` · **spec** 09 §5, 02 §6, 11 §2

- **TRIGGER** Any code tool call.
- **FAILURE, per distinguishable outcome**
  - `mode=semantic` → `UNSUPPORTED_MODE`, refused before resolution and before
    any lock. Deliberately a *recognized* mode, so a caller learns "not yet"
    rather than "unknown".
  - No active tuple → `WORKTREE_NOT_INDEXED`, checked before either leg, which
    is what structurally separates it from index-unavailable.
  - Read-lock wait exhausted → `BUSY_RETRY`, the only code with `retryable`
    true.
  - Lexical head invalid → `degraded: dense_only`, the leg not run at all.
  - Shard unavailable → `degraded: lexical_only`, with no internal retry.
  - Both legs down → `INDEX_UNAVAILABLE` carrying both diagnostics. Never an
    empty success.
  - A path outside the tree or not in the active generation →
    `PATH_NOT_INDEXED`, with details separating "no such path" from "skipped,
    reason=…".
- **NOTE** `degraded` means "less than you asked for", so a single-leg mode that
  served reports nothing degraded; when its one leg cannot serve, the answer is
  a refusal instead. Tool-level failures travel as `isError` **content** rather
  than a JSON-RPC internal error, because the latter is indistinguishable from a
  server bug to the model reading it.
- **CODE** `crates/local-rag/src/daemon/mcp/dispatch.rs`, `crates/protocol/src/error.rs`

## Observations

### hook-spool-append · source-identity-derivation · segment-rotation
**view** `observationCapture` · **spec** 07 §1–§4, 12 §2

- **TRIGGER** One of seven Claude Code events. Delivery is at-least-once.
- **STEPS** Parse → check the deny list **first**, so a denied event's payload is
  never even scanned → redact → **then** cap → compute the source identity
  exactly once, at write time → lock the segment, decide rotation from metadata
  read **after** the lock → append → fdatasync → exit 0.
- **LOCKS+TX** An exclusive file lock per append. Per-session segments plus that
  lock are what eliminate interleaving; append mode alone does not guarantee it
  for large writes.
- **DURABILITY** **The fdatasync is the durable moment.** Everything before it is
  non-durable by definition — which is the whole reason there is no
  acknowledgement protocol.
- **FAILURE** Every step is typed and every failure is a silent fail-open with
  exit 0. Redact-then-cap is ordered so a secret near the boundary cannot
  survive inside a half-truncated value. Best-effort identities are never under
  a unique constraint; the counter that makes a subagent stop stable is replaced
  by write-new plus rename, never truncate-in-place.
- **CODE** `crates/local-rag-hook/src/{main,segment,identity}.rs`

### spool-import-session-tail · spool-dedup · spool-torn-tail-vs-corruption · segment-cleanup
**view** `spoolImport` · **spec** 07 §3, §5, §6

- **TRIGGER** The consolidation tick, or the startup resume pass.
- **STEPS** Read from the durable cursor → verify each frame, length against the
  cap **before** the available bytes → probe the frame's root **before** the
  write transaction opens → insert with dedup → commit → delete segments behind
  the cursor.
- **LOCKS+TX** **One transaction per batch**: envelopes, paths, payloads with a
  TTL, and the cursor advance together.
- **DURABILITY** The cursor advances **only in the same transaction as the
  envelopes**. That single fact is the whole idempotence argument.
- **FAILURE** A torn tail is not an error — the appending hook holds its lock
  until the frame is complete, so no valid frame can follow a torn one. An
  impossible length is corruption: reported, never skipped past, and the cursor
  never advances beyond it. Stable identities dedup exactly; best-effort ones
  within 512 envelopes **or** ten minutes of their own capture time, a union.
- **CODE** `crates/store/src/observation/import.rs`, `crates/store/src/spool.rs`

### give-feedback
**view** `giveFeedback` · **spec** 11 §2, 07 §1

- **TRIGGER** The `give_feedback` tool.
- **STEPS** One envelope insert, with the source event id doubling as the dedup
  key. It never calls the memory op engine.
- **DURABILITY** That insert.
- **FAILURE** A retried identical call reproduces the key and is reported as
  already-recorded — an idempotent success.
- **NOTE** This is the one documented exception to spool-only, which binds
  **hooks**, not daemon-internal writes. It has a real consequence: no spool
  directory represents a session whose envelopes all arrived this way, which is
  why the consolidation tick unions the sessions it sees on disk with the
  sessions that have a backlog in the database.
- **CODE** `crates/local-rag/src/daemon/mcp/memory_write.rs`

## Memory

### consolidation-trigger-tick · consolidation-run · plan-collapse
**view** `consolidationRun` · **spec** 07 §6, 08 §4, 04 §4

- **TRIGGER** A 15 s interval whose first tick fires immediately — and that
  first tick *is* the startup catch-up, deliberately not a second spawned pass,
  because two drivers reading the stale set in the same instant burned two
  attempts and two model calls per restart.
- **STEPS** Recover stale runs **first** → import each session tail → evaluate
  four gates (an unconsolidated stop past the cursor, asked of the database
  rather than of "did this call import it"; a backlog threshold; a session end;
  a 24 h idle checkpoint) → open a run with a lease and a window snapshot →
  route **outside any transaction** → guard, then collapse the plan → apply.
- **LOCKS+TX** The writer queue only, no worktree lock. The apply is **one
  short transaction, atomic across the whole ops list**: one rejected op rolls
  back everything including the cursor advance, because committing the ops that
  happened to succeed would advance the cursor past observations whose ops never
  landed.
- **DURABILITY** Nothing exists until that transaction commits; the router's
  output is never persisted in between.
- **FAILURE** A crash leaves the run running with a lease, reclaimed by the next
  tick. The lease doubles as a compare-and-swap token, fencing a slow-but-alive
  attempt against a legitimate retry under the same run id.
- **NOTE** The plan is collapsed above the store because two ops naming one
  entry do not *race* into failing — they are **guaranteed** to fail, since
  every expected version is captured before any op applies.
- **CODE** `crates/local-rag/src/daemon/consolidation_trigger.rs`, `crates/memory/src/plan.rs`

### window-and-conflict-set-budgeting · consolidation-failure-classification
**view** `consolidationFailureLadder` · **spec** 08 §4

- **TRIGGER** Assembling a prompt; and any failure of a run.
- **STEPS** Derive the budget from the model's context length minus the answer
  reserve, one corrective re-prompt, the system prompt and the conflict set's
  floor → bound the window in **tokens**, not rows → trim the conflict set as a
  prefix, then again with the real tokenizer.
- **FAILURE** Mechanical (reproduces identically) is fingerprinted with the
  running build and excluded while that fingerprint matches — a rebuild earns
  exactly one more attempt. Transient backs off exponentially and escalates into
  the same dead letter at eight attempts, so **no class retries unboundedly**.
  A context overflow is decided by the tokenizer with no model call at all, and
  the next tick halves the window.
- **MEASURED** A count-based conflict cap once consumed 97 % of a 32 768-token
  context — the more the product remembered, the less it could consolidate. A
  row-based window cost 17 599–23 127 tokens, because excerpts are tool output
  and JSON at about 2.9 characters per token, not prose at 4.
- **CODE** `crates/memory/src/budget.rs`, `crates/store/src/memory/consolidation.rs`

### consolidation-operator-repair
**view** `operatorRepair` · **spec** 11 §6, ADR-0014

- **TRIGGER** An operator acting on a parked run.
- **STEPS** `retry` moves it to running with an already-expired lease — exactly
  the row the sweep selects. `abandon` advances the cursor past the window,
  leaves the run failed because it did fail, and writes an audit row.
- **LOCKS+TX** One transaction; **needs no daemon**, writing straight to the
  store, because a wedged store is likely one whose daemon is unhealthy.
- **FAILURE** `retry` is one attempt **by construction**, not by promise: a
  second failure rewrites the fingerprint and re-parks it. `abandon` is bounded
  data loss and says so — those observations never become memory, though their
  envelopes survive.
- **WHY IT EXISTS** The specification's escape was "until the binary is
  rebuilt", and a published release has no rebuild: its build id is fixed for
  the life of the release. Two sessions held 1368 of 1373 backlogged
  observations, permanently.
- **CODE** `crates/local-rag/src/cli/consolidation.rs`

### transactional-memory-op · english-write-boundary
**view** `rememberWrite` · **spec** 08 §3, ADR-0011

- **TRIGGER** `remember`, `edit_memory`, or any router op.
- **STEPS** Check the idempotency key **first**, inside the transaction, before
  any other precondition → check the version, the kind and state legality, the
  scope → mutate, write evidence, write audit, advance the cursor for
  consolidation.
- **LOCKS+TX** One transaction containing all of it.
- **DURABILITY** That commit.
- **FAILURE** Every precondition is a typed error aborting with **no mutation**.
  `reinforce` may raise confidence but never changes text; `noop` writes nothing
  at all, because attaching an audit row to an unchanged version breaks the
  first time two runs both legitimately decide to do nothing. A translation
  refusal stores the author's text unchanged: the invariant is *eventually*
  English, because losing a note to a model failure is not acceptable.
- **CODE** `crates/store/src/memory/op.rs`, `crates/local-rag/src/daemon/normalization/boundary.rs`

### guard-materialize · candidate-lifecycle
**view** `candidateReview` · **spec** 08 §3, §4, 04 §6, 12 §4

- **TRIGGER** The router proposing an op; a human reviewing a candidate.
- **STEPS** Downgrade a create or supersede of a durable kind whose every
  citation is a model claim → rewrite a create of already-stored text into a
  reinforce → capture every expected version once → on approval, materialize the
  proposed op through the same transactional path, with actor=user, inside the
  same transaction as the state change.
- **FAILURE** The model-claim rule is enforced **twice, independently** —
  proactively in the guard, and as a backstop in the op engine that no future
  generator can bypass. Only the second is what the guarantee rests on. Exact
  text rather than similarity, deliberately: asked loosely it is a judgement,
  asked as byte equality it is a fact, and a fact is what a guard may act on
  silently.
- **MEASURED** Past the conflict cap the model is blind to its own recent
  output and re-derives the same claim every window: 136 copies of one sentence,
  over half the durable memory.
- **CODE** `crates/memory/src/guard.rs`, `crates/store/src/memory/review.rs`

### recall-pipeline · recall-additionalcontext-formatting · recall-hook-injection
**view** `recallInjection` · **spec** 08 §6, 11 §3.2, §5

- **TRIGGER** The recall tool, or a session-start / prompt hook after its own
  append already succeeded.
- **STEPS** Normalize the query above the pipeline → resolve the scope union →
  fuse a per-call lexical index with a dense leg read by exact subject key →
  **re-read each entry's lifecycle state** → cut to the token budget → sanitize,
  escape the closing tag, cap, and compute the length **last**, over the exact
  emitted bytes.
- **LOCKS+TX** **No lock at all across the pipeline** — which is precisely why
  the lifecycle re-read exists: a concurrent retract between the candidate read
  and formatting is possible.
- **DURABILITY** Read-only.
- **FAILURE** An empty result emits **no text at all**. Every eligible candidate
  participates at score zero when neither leg matched, so a termless recall
  still surfaces the most recent memories. The hook path is fail-open by
  construction: unreachable daemon, timeout, error and a degraded response all
  collapse to printing nothing, and it must never start a daemon.
- **CODE** `crates/memory/src/recall/`, `crates/local-rag-hook/src/recall.rs`

### normalization-worker-tick · normalization-apply-ordering
**view** `normalizationTick` · **spec** 02 §4.3, 10 §4, 03 §1.4

- **TRIGGER** A 60 s interval.
- **STEPS** Detect — settle every already-English entry in one transaction,
  spending **zero** inference → yield to consolidation for the shared model, but
  only three ticks running → translate one bounded batch → commit the vector to
  the cache **first**, then install the canon as an audited system edit.
- **LOCKS+TX** Two transactions, one per database, in that order.
- **DURABILITY** **The order is the substitute for atomicity**, since no
  transaction may span both databases.
- **FAILURE** The reverse order would leave a referenced hash with no vector,
  which nothing reclaims and nothing reports; this order leaves at worst an
  unreferenced cache row, which eviction takes. Dead letters are keyed by the
  normalizer version, not the build — changing the normalizer is a decision, a
  rebuild is not. An unavailable model aborts the tick and **blames no entry**.
  Unbounded politeness is starvation, not courtesy: six ticks of zero
  translations against a growing backlog is what added the yield cap.
- **CODE** `crates/local-rag/src/daemon/normalization/`

## Delivery

### npm-install-postinstall · binary-resolution-npm
**view** `installAndHeal` · **spec** 13 §1, §2

- **TRIGGER** Installing the npm package; and every launcher entry point.
- **STEPS** Resolve the platform key → resolve `latest` through the redirect
  that names the concrete tag, once per install → download one archive per
  binary → verify each against its sidecar → install **flat**, into one
  directory, and record the manifest beside them.
- **DURABILITY** The install manifest is the record; its absence means
  "unmanaged", never a fault — a source checkout and a hand-placed override are
  both legitimate.
- **FAILURE** Verification defends against corruption in transit and tampering
  on the wire. It does **not** defend against a compromised release, because
  whoever can publish the asset can publish its digest — stated rather than
  advertised as a property the channel lacks.
- **RESOLUTION LADDER** An explicit override (terminal — a miss is an error, not
  a fall-through) → a source checkout (also terminal, erroring with a build
  command) → the package's own directory → the per-user cache. A rung yields a
  **directory** and counts only when it holds every required binary: resolving
  per binary could return a proxy from one rung and a daemon from another, and
  nothing downstream would notice until the versions disagreed.
- **CODE** `npm/memory/src/{install,locate,shim}.js`

### model-and-runtime-install
**view** `modelDownload` · **spec** 10 §5

- **TRIGGER** `local-rag init --download-models`.
- **STEPS** Per file: stream to a partial name while hashing → verify size and
  digest against the catalog compiled into the binary → rename → fsync the
  directory. Then the manifest, then the ready marker **last**.
- **DURABILITY** Everything before the marker is by construction
  indistinguishable from "not installed".
- **FAILURE** Resumability is rehashing, not a journal: a rerun re-derives what
  is missing, a leftover partial file is overwritten rather than trusted, and a
  rerun after a complete install is a no-op. The three artifacts are mutually
  independent; the runtime is fetched before the embedder's early-returning
  disk check, so a machine with no weights still gets a runtime — and that is
  the machine most likely running this for the first time. The manifest is
  disclosure, never the authority a download is checked against.
- **CODE** `crates/models/src/install.rs`, `crates/generate/src/install.rs`

### managed-project-enrollment
**view** `projectEnrollment` · **spec** 11 §8, ADR-0009

- **TRIGGER** `local-rag project add|remove|enable|disable`.
- **STEPS** Resolve the path → write the enrolment straight to the store in one
  transaction → notify a live daemon best-effort, result ignored.
- **DURABILITY** That commit. The durable effect never touches a socket.
- **FAILURE** A brand-new path is registered **and** enrolled in the same
  transaction, closing the window two transactions would leave — a worktree
  registered but never marked managed. `remove` on an unenrolled path is an
  idempotent success; `enable`/`disable` on one is a typed refusal. Enrolment is
  keyed by worktree id, never by a path.
- **CODE** `crates/local-rag/src/cli/project.rs`

### plugin-executable-resolution
**no view** · **spec** 13 §2, 11 §3.2

- **TRIGGER** Claude Code launching the plugin's MCP server or a hook.
- **STEPS** Resolve an **executable**, not a package: an explicit override →
  every PATH entry in order → the directory beside node → package-manager homes
  exported into the environment, each with its `bin` child → well-known global
  bin directories. Both resolvers are held byte-identical by a parity test.
- **FAILURE, and the two contracts differ deliberately** — **MCP**: stdout stays
  byte-empty because it is the JSON-RPC stream, the diagnostic goes to stderr
  naming both the install command and the override, and the process exits
  non-zero so the client shows a *failed* server rather than a silent one.
  **Hooks**: exit 0 always; session start speaks through additional context, the
  other six stay silent.
- **NOTE** The plugin never downloads anything, ever — the same rule as "the
  recall RPC never spawns a daemon": it does work that is already possible, or
  it does nothing.
- **CODE** `plugin/bin/local-rag-mcp-launcher.js`, `plugin/bin/local-rag-resolve-hook.sh`

## Maintenance and privacy

### retention-mark-and-sweep · orphan-shard-sweep · vacuum-reclaim
**view** `retentionSweep` · **spec** 06 §5, 03 §3, 11 §6

- **TRIGGER** A job at daemon startup, or `local-rag gc`.
- **STEPS** Mark the pin roots → take the unpinned candidates → delete in a
  fixed order, children before parents → at most 500 rows per transaction.
- **LOCKS+TX** One committed transaction per batch, through the single writer;
  the dry run goes through the read-only entry point so a long plan does not
  lock out the daemon's writers.
- **DURABILITY** Per batch. **Resumable without a progress table**: the sets are
  recomputed from the live database on every call, deletions are monotone, and
  re-running converges.
- **FAILURE** A failed sweep is a warning, never fatal, and the job is never
  awaited before readiness — a first sweep over a backlog is long, and blocking
  readiness would exhaust the proxy's connect budget on exactly the stores that
  most need collecting. Shard and lexical rows are not touched here; they vanish
  through desired-set reconciliation. Freeing rows and returning disk are
  separate steps, and `vacuum` refuses while a daemon holds the store.
- **CODE** `crates/store/src/retention.rs`, `crates/local-rag/src/cli/{gc,vacuum}.rs`

### privacy-inspect-export-purge
**view** `purgeMemory` · **spec** 12 §3, §5

- **TRIGGER** `local-rag inspect | export | purge`.
- **STEPS** Inspect and export share one type, so export is never poorer than
  inspect, and both show the author's original text beside the English canon.
  Purge: delete the **vector first**, in its own transaction → delete the
  normalization row explicitly → delete the entry and its evidence, relinking
  descendants → rewrite the audit trail, nulling payloads and appending a
  terminal purge row.
- **LOCKS+TX** `purge --all` is **one** transaction, not batched like the
  retention sweep: a partially completed purge is worse than a slow one for an
  all-or-nothing operation.
- **DURABILITY** The cache-first order inverts this system's usual state-first
  rule for a specific reason — the cache key is derived from the text being
  deleted, so after the state commit nothing could ever derive that key again.
  This order leaves at worst a recomputable vector; the reverse leaves one
  nothing can find.
- **FAILURE** Every mode needs an explicit selector **and** a confirmation;
  a single memory additionally needs the version the operator just inspected.
  The normalization row is deleted explicitly because a cascade cannot be
  counted.
- **CODE** `crates/store/src/privacy/`, `crates/local-rag/src/cli/purge.rs`

### doctor
**view** `doctorDiagnosis` · **spec** 11 §6

- **TRIGGER** `local-rag doctor`.
- **STEPS** A **fixed call order**, because every ordinary constructor here
  repairs something as a side effect of opening: read the lock as a plain file →
  stat permissions before anything can re-assert them → check versions over a
  raw read-only connection → check the cache binding the same way → only then
  open anything real, and run the three orphan sweeps dry.
- **DURABILITY** None — categorically read-only. There is no repair flag,
  deliberately: a combined flag would erase the diagnose/repair line.
- **FAILURE** Only stuck generations, stuck consolidation runs and dead-lettered
  normalization make the report unclean. Never indexed, not enrolled, no
  generator installed, a worker switched off — all informational. A bootstrap
  state and a user's choice are not faults, and a `doctor` that failed on every
  fresh machine is one nobody reads.
- **CODE** `crates/local-rag/src/cli/doctor.rs`

## The two fault matrices

Twenty named scenarios that are deliberately **not** twenty views. Their
recovery is one path each; only the detection signal differs, and twenty
near-identical diagrams would hide exactly the point the matrices exist to make.

### Projection faults (F1–F12) — spec 05 §10

Every row funnels into: detect at open → mark dirty → rebuild. The rebuild is
three separately committed transactions, because the state machine only allows
dirty to rebuilding, and a crash between any two re-enters the same path.

| Row | Fault | Detection signal |
| --- | --- | --- |
| F1 | Kill between the write-ahead and the first backend op | status is updating; no active tuple exists yet, so recovery is retrying the switch |
| F2 | Kill mid-upsert | status updating, plus a stale head op id |
| F3 | Kill after all point ops, before the head write | head op id ≠ the recorded op id |
| F4 | Kill after the head write, before the state commit | head tuple is the target, not the active one |
| F5, F10 | Shard write-ahead loss; a swallowed flush failure | point count or manifest mismatch |
| F6 | Partial point deletion, catalog intact | manifest mismatch |
| F8 | Equal point count, different id set | manifest mismatch — the reason the manifest hash exists |
| F9 | A final op the backend reported as succeeded and did not perform | manifest verification at the **next** open; the switch itself returned success |
| F7 | Missing head, or one left from a previous op | head missing, or op id mismatch |
| F11 | Crash during a rebuild | status rebuilding — the rebuild simply restarts |
| F12 | On-disk corruption making the shard unopenable | the open itself fails → **quarantine** by rename, then rebuild |

F5, F6, F8, F9 and F10 share one recovery exactly; they are listed separately
only because each pins a different detection signal.

### Spool kills (S1–S8) — spec 07 §7

The matrix exists to pin one line: durability begins at fdatasync, and the
cursor moves only together with the envelopes it accounts for.

| Row | Kill point | Outcome |
| --- | --- | --- |
| S1 | Hook killed mid-write | The event is not durable. The importer stops at the torn frame and the next hook appends after it; the segment stays valid |
| S2 | Hook killed after fdatasync, before exit | Durable, imported exactly once — a process kill is not power loss |
| S3 | Daemon killed after reading frames, before commit | The cursor never moved; re-import, dedup absorbs it |
| S4 | Daemon killed after commit, before cleanup | The rescan skips everything at or below the committed offset |
| S5 | Daemon killed mid-cleanup | Delete-after-commit only, so the segment set stays consistent |
| S6 | A duplicate event with a stable identity | Exactly one envelope, by unique constraint |
| S7 | A duplicate best-effort event | One envelope inside the window, two outside it — and consolidation idempotence absorbs the second |
| S8 | A crash anywhere | **No event with a stable identity is ever lost after a successful append.** Not a test of its own: the conjunction of S1–S7 |

Stated limit, not glossed: genuine byte-level torn writes are not independently
reproduced anywhere — there is no portable way to force a short write below the
frame cap without kernel fault injection. Truncation of any length is covered by
direct byte manipulation instead.

## No recovery path today

Twelve places where the system stops and cannot restart itself. Each is real,
each has an owner or an explicit decision, and none is hidden in a happy path.

1. **Duplicate candidates and duplicate entries.** The router is structurally
   unable to notice a duplicate it is never shown — the conflict set is built
   from active entries, so a proposal identical to hundreds of pending siblings
   is invisible. Measured: 9564 candidates over 3294 distinct texts, the worst
   proposed 476 times, conflicts empty on all of them. *(T23-07, T23-08)*
2. **The payload TTL sweep is scheduled by nothing.** Implemented, exported,
   tested, and reachable only by a human typing `gc`. Measured: 45651 of 46737
   payload rows already past expiry, the oldest by three weeks — a `[FIXED]`
   privacy requirement enforced by hand. Four sibling sweeps share the gap.
   *(T23-09, and it must land after the backlog rescue, or it deletes exactly
   what the rescue exists to consolidate.)*
3. **The router's answer budget is a constant** that does not follow the window
   it must describe. A truncated answer is indistinguishable from genuine
   malformation at the point the handler decides, so it consumes the one
   corrective re-prompt and then the window. *(T23-06)*
4. **A truncated generation is classified as reproducible** and is not — proven
   by a live retry that applied, against the card's own written prediction.
   *(T23-10)*
5. **The proxy upgrade EOF.** A CI failure in 0.016 s with the proxy closing
   stdout without writing a byte; mechanism not established, only the
   diagnostics improved. This is the flow that took the owner's live MCP down.
6. **No v1 memory importer.** Clean-start only, with no manual path either;
   whether GA ships one is an open question, not an oversight.
7. **Permanent window halving.** Nothing clears a failed run, so it stays the
   latest non-applied row and the shrink decision keeps halving against it — a
   session that overflowed once opens half-size windows forever, at twice the
   model calls. Only the operator verbs lift it, as a side effect.
8. **The one-generator-pool rule has no mechanism.** A second pool in one
   process fails and leaves that consumer silently empty for the daemon's whole
   uptime. A fourth careless consumer reintroduces it.
9. **A corrupt subagent counter drops one observation** — by design, because
   reissuing an occurrence already used by stored history would collide against
   permanent data. Correct, and still an unrecoverable loss.
10. **The hook's append budget is measured, never enforced** — killing mid-write
    would risk an inconsistent lock and file state.
11. **One deviation number cites two rows.** The clean repair means editing
    committed evidence, which this repository forbids.
12. **Byte-level torn writes are untested** (see above).

## Detected only by a human

Eleven conditions with no automatic signal — someone has to read `doctor`,
`stats`, a log line, or the size of a file.

- A parked consolidation run, and which sessions it blocks.
- The permanent window halving — visible only as a run count.
- Candidate-queue duplication.
- Expired payload rows — no report at all; found by direct SQL.
- The pin-set ratchet — visible only as cycle duration climbing, 88 s to 25 min.
- WAL growth — visible only as the file, or as a full disk. It once reached
  324 GB against a 41 GB database.
- The free-page ratio after a sweep; nothing reclaims while a daemon holds the
  store.
- Orphan shard directories, expired shards, unreferenced model-space
  directories — `doctor` reports them dry; only `gc` acts.
- A lock record naming a dead pid after a leaked-guard shutdown — a deliberate,
  documented trade.
- Normalization starvation before the yield cap — visible only as repeated
  zero-translation ticks in the worker's own log.
- A silently frozen index. Three paths once skipped a whole cycle writing
  nothing anywhere a human could see; ten hours of it looked like nothing from
  outside. All three log now — and they still only log.
