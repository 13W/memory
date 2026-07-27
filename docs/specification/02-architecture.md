# 02 — Architecture, Lifecycle, Locking

## 1. Process topology `[FIXED]`

```
Claude Code ──stdio──▶ thin MCP proxy ──UDS/pipe──▶ shared daemon ──▶ state.sqlite
                                                        │             cache.sqlite
Claude hooks ──atomic append──▶ spool/<session>/  ◀──tail/import──┤   projection/ shards
                                                        │             models/
                                                        └──▶ background workers
                                                             (reconcile, embedding,
                                                              consolidation, GC)
```

- The **proxy** is a per-session stdio process spawned by Claude Code's MCP config. It holds no
  state beyond the connection and the handshake result.
- The **daemon** is one per OS user, owns the store exclusively (store lock), and hosts all
  SQLite connections, shard handles, and workers.
- **Hooks** have exactly one ingestion path: durable spool append (07). Hooks additionally have
  an optional **read-only** recall RPC to the daemon (11 §3.2) which is best-effort and
  fail-open; ingestion never depends on the daemon being alive `[FIXED]`, and the recall read
  path never writes `[SPEC]`.

## 2. Store layout

```
<data_dir>/local-rag/
  store.lock            # flock'd JSON: {instance_uuid, pid, daemon_version, started_at}
  migration.lock        # L1 advisory file lock, held only while migrating (§5, 13 §3)
  state.sqlite          # source of truth (+ -wal/-shm)
  cache.sqlite          # rebuildable, independently validated (+ -wal/-shm)
  projection/
    <worktree_id>/      # one dense shard per worktree; layout is backend-defined
      <model_space_id>/ # per model space, so a migration never rewrites the old one in place (T11-05)
  spool/
    <session_id>/
      000001.seg …      # append-only segments (07)
  models/
    <model_id>/         # downloaded weights + manifest.json + .ok marker
  run/
    daemon.sock         # unix domain socket (POSIX); dir mode 0700
  logs/                 # daemon logs (rotated)
  quarantine/           # corrupted shards moved here before rebuild (05 §7)
  backups/              # pre-mutation state.sqlite snapshots: state-<version>-<ts>.sqlite (13 §3)
```

### 2.1 Directory resolution `[SPEC]`

| Item | POSIX | Windows |
| --- | --- | --- |
| `<data_dir>` | `$LOCAL_RAG_HOME`, else `$XDG_DATA_HOME`, else `~/.local/share` | `$LOCAL_RAG_HOME`, else `%LOCALAPPDATA%` |
| `<config_dir>` | `$LOCAL_RAG_HOME/config`, else `$XDG_CONFIG_HOME/local-rag`, else `~/.config/local-rag` | `$LOCAL_RAG_HOME/config`, else `%APPDATA%\local-rag` |
| MCP endpoint | `<data_dir>/local-rag/run/daemon.sock` | named pipe `\\.\pipe\local-rag-<sha256(user SID)[..12]>` |

`LOCAL_RAG_HOME` overrides everything (tests, containers): when it is set, `<data_dir>` is
`$LOCAL_RAG_HOME` and `<config_dir>` is `$LOCAL_RAG_HOME/config` — a sibling of the store root
`$LOCAL_RAG_HOME/local-rag`, so a container is fully self-contained even with `HOME` unset. Per
the XDG Base Directory spec, an empty base-directory variable is treated as unset and a relative
`$XDG_*` value is ignored. All directories are created `0700`, files `0600` (POSIX); on Windows,
default per-user ACLs of `%LOCALAPPDATA%` apply. `sha256(user SID)[..12]` is the first 12
lowercase hex characters of the SHA-256 of the user SID. macOS is a POSIX target and uses the
XDG fallbacks (not `~/Library/Application Support`).

## 3. Configuration model

### 3.1 Global config — `<config_dir>/config.toml` `[SPEC]`

```toml
schema_version = 1

[daemon]
idle_shutdown_secs   = 900        # only when no sessions, no pending spool, no jobs
max_open_shards      = 8          # shard-manager LRU size
log_level            = "info"

[storage]
embedding_cache_budget_mb = 2048  # LRU eviction target for cache.sqlite vectors
payload_ttl_hours         = 72    # observation_payload TTL
retired_generations_keep  = 2     # K   [OPEN — final number]
retired_generations_ttl_h = 168   # T   [OPEN — final number]

[models]
default_model_space = "default"   # name resolved against state.sqlite registry
data_policy = "local_only"        # local_only | metadata_only_remote |
                                  # allow_remote_with_redaction | allow_remote_full  [FIXED default]

[index]
languages = ["typescript", "javascript", "rust"]   # ADR-0001 (closes O4)
max_file_size_kb = 1024

[spool]
deny_paths = []   # configurable deny-list (12 §2); matching events captured envelope-only
deny_tools = []
```

As-built note (T02-05, `[SPEC]`): the global config is parsed by
`local_rag_core::config` (`Config::load(<config_dir>)` reads only
`<config_dir>/config.toml`; `Config::parse_toml` for text). Validation policy for the cases this
section left implicit: a **missing file** yields the full defaults above (config is optional); an
**unknown/unsupported `schema_version`** is a typed `ConfigError::UnsupportedSchemaVersion`
(this binary supports `1`); an **invalid `data_policy`** value is a typed
`ConfigError::InvalidDataPolicy` — never silently downgraded to the default (§6 "nothing degrades
silently" `[FIXED]`); **unknown TOML keys are ignored** (lenient/forward-compatible), and missing
keys default per section. The `[OPEN]` numbers (`storage.retired_generations_keep`/`_ttl_h`) are
parsed as the provisional defaults shown here — T02-05 does not close those open questions.
`index.languages` is now the closed set fixed by ADR-0001 (O4), not a provisional placeholder.
`Config::load` takes only the resolved `<config_dir>`; there is no API that
consults a worktree or repository tree, which is the structural form of §3.2's "never via files
inside the repository".

As-built note (T13-01, `[SPEC]`): the `[spool]` section is `local_rag_core::config::SpoolConfig`
(`deny_paths`/`deny_tools`, both empty by default — opt-in exclusion, no built-in entries since
12 §2 does not mandate any). `deny_paths` matches **component-wise** against an observation's
normalized path(s): an entry is a directory-prefix match (`secrets` matches `secrets/api.key`,
never `not-secrets/x.txt`), not a substring match. `deny_tools` matches by exact tool-name
equality. **Global-only for v0**: unlike `data_policy` (§3.2), this section is not mirrored into
`repo_settings` — 12 §2 asks for "a configurable deny-list", not per-repository granularity, so
extending the generic repo-settings bridge here would be scope beyond what either section
requires. The section's consumer, `local_rag_hook::payload::prepare_payload`, is documented at
07 §2's as-built note.

### 3.2 Per-repository settings `[SPEC]`

Stored in `state.sqlite` (`repo_settings` table, 03 §2.1), edited via CLI/dashboard-equivalent,
never via files inside the repository (a repo checkout must not be able to change daemon policy).
Keys mirror `[models]`/`[index]` sections.

**Conflict rule for `data_policy`:** effective policy = **most restrictive** of global and
repository value. Order of restrictiveness:
`local_only > metadata_only_remote > allow_remote_with_redaction > allow_remote_full`.
Requests routed while any involved repo demands stricter policy MUST use the stricter policy.

As-built note (T02-05, `[SPEC]`): the `repo_settings` reads/writes and the merge live in
`local_rag_store::registry::settings` (see 03 §2.1 T02-05 note for the storage detail).
`DataPolicy::most_restrictive` (`local_rag_core::config`) picks the lower-ranked (stricter) of two
policies; `effective_data_policy(global, conn, repo_ids)` folds it over the global value and every
involved repository's stored policy. The fold is commutative/associative, so the effective policy
is deterministic regardless of the order repositories are visited, and a repository can only
tighten — never relax — the global policy. A repository with no `data_policy` setting does not
change the effective value. The enforcement point that consumes this — the central remote-policy
guard in the provider pool (§6 `POLICY_BLOCKED_REMOTE`; 10 §1; 12 §1) — is a later group
(T11/T16); T02-05 supplies the effective-policy computation it calls.

### 3.3 Request context `[FIXED — daemon defines routing]`

Every daemon request (MCP tool call, hook recall RPC) carries an explicit context:
`{session_id, worktree_root?, repo_hint?}`. The daemon resolves `worktree_root` →
`worktree_id`/`repo_id` via the registry (03 §2.1); there is **no ambient current project**.
Requests without a resolvable worktree operate in global scope only.

As-built note (T02-04, `[SPEC]`): the context maps to
`local_rag_store::registry::RequestRoot { worktree_root: Option<WorktreeRootFacts>, repo_hint:
Option<repo_id> }`; `session_id` is routing/telemetry only and is not part of identity
resolution. `worktree_root = None` **or** an unresolvable root resolves to
`Resolution::GlobalOnly` — never an error. `repo_hint` is a `repo_id` used solely to break a tie
between reattach candidates (never a lookup key into identity, 01 §5); a repo-level hint cannot
disambiguate two linked worktrees of one repository (that needs an explicit worktree-level
`attach`, 04 §7). The daemon (T15) supplies already-canonicalized, git-probed
`WorktreeRootFacts` (`kind`, advisory `common_dir_fingerprint`/`remote_fingerprint`) because
`local-rag-store` carries no git/network dependency (architecture guardrail until T10); the
resolver is a pure registry lookup over those facts.

## 4. Daemon lifecycle `[FIXED, mechanics [SPEC]]`

### 4.1 Startup

1. Acquire `store.lock` via `flock(LOCK_EX | LOCK_NB)`. On failure: read lock JSON, verify the
   owning process exists **and** its instance UUID matches a live handshake on the socket
   (PID alone is not identity — PID reuse). If the owner is dead/stale: remove stale socket +
   lock, retry once.
2. Open `state.sqlite`; run migration framework (13 §3) under the migration lock.
3. Open/validate `cache.sqlite` (03 §4.4): wrong `store_instance_uuid` or unsupported cache
   schema version → recreate empty cache.
4. Bind endpoint (socket/pipe), write readiness marker into `store.lock` JSON, start workers.
5. Resume: pending spool import (07 §6), crashed consolidation runs with expired leases (08 §4),
   interrupted projection switches are *not* resumed — they are detected lazily at shard open (05).

### 4.2 Proxy → daemon handshake

```
proxy  → HELLO {proto: 1, proxy_version, session_id, worktree_root, harness: "claude-code"}
daemon → WELCOME {proto: 1, daemon_version, store_instance_uuid, capabilities[]}
       | INCOMPATIBLE {min_proto, max_proto, daemon_version}
```

- Proxy behavior on missing daemon: attempt connect; on failure spawn the platform daemon
  binary detached, then retry connect with backoff (250 ms × 2ⁿ, cap 4 s, total 20 s) `[SPEC]`.
- On `INCOMPATIBLE`: proxy reports an MCP initialization error naming both versions; it never
  degrades silently.
- Binary upgrade while an old daemon holds the migration/store lock: new proxy sends
  `SHUTDOWN_REQUEST`; old daemon finishes in-flight jobs, releases, exits; new daemon starts
  `[SPEC]`. If the old daemon does not exit within 30 s the proxy reports the conflict instead
  of force-killing.

### 4.3 Shutdown

Idle shutdown only when **all** hold: no live MCP sessions, no unimported spool bytes, no
running index/consolidation/GC jobs `[FIXED]`. SIGTERM/CTRL-C: stop accepting, cancel
reconciles at the next safe point (state tx boundaries), flush WAL checkpoint, release lock.
Kill at any point is safe by construction (05, 07).

### 4.4 Ownership invariants

One daemon per OS user per store `[FIXED]`. Identity = `instance_uuid` (+ PID as advisory)
`[FIXED]`. Orphan artifacts (stale socket, stale lock, orphan shard temp dirs, spool of dead
sessions) are cleaned at startup and by periodic GC.

## 5. Concurrency & lock order `[SPEC]`

Lock levels; a task may only acquire a lock with a **higher level number** than any lock it
holds (strict ordering, no exceptions):

| # | Lock | Kind | Protects |
| --- | --- | --- | --- |
| L0 | `store.lock` | OS file lock | whole store, one daemon |
| L1 | migration lock | file `migration.lock` | schema migrations, exclusive with normal operation |
| L2 | per-worktree RwLock | async RwLock | index/projection consistency of one worktree |
| L3 | shard-manager map | mutex | open-shard LRU map only (handle lookup/insert/evict) |
| L4a | `state.sqlite` write queue | bounded mpsc → single writer task | the single physical SQLite writer |
| L4b | `cache.sqlite` write queue | bounded mpsc → single writer task | same, for cache |

Rules:

- **Search (read) path:** `L2.read` → read snapshot of `worktree_projection_state` (read-only
  connection, no queue) → FTS query (cache read conn) → dense query on a **ref-counted shard
  handle** (L3 held only for the map lookup, released before the query) → RRF → release `L2.read`.
  The read lock spans the *entire* pipeline `[FIXED]` (no generation mixing between legs).
- **Write path (reconcile/switch/rebuild):** `L2.write` → compute → L4a tx (write-ahead) →
  backend ops → L4a tx (commit). Only one writer per worktree ever exists `[FIXED]`; the two
  switch axes (generation, model space) are serialized by this same lock `[FIXED]`.
- L4 queues are **leaves**: while executing inside the writer task, no other lock may be taken.
- Write queues are **bounded** `[FIXED]`; producers await backpressure. Queue depth is a metric.
- `busy_timeout` is a backstop, not the design: within the daemon all writes go through the
  queues; direct write connections outside the queues are forbidden.

As-built note (T09-01, `[SPEC]`): the hierarchy's typed primitive is `local_rag_store::lock`
(`crates/store/src/lock/`). `LockLevel` has seven variants (`L0`…`L4b`); ordering always goes
through `LockLevel::rank()`, never a derived `Ord` on the enum, because `L2Read`/`L2Write` and
`L4a`/`L4b` **share a rank** — the table's own numbering treats each pair as siblings of one
numbered level, not as two independently orderable levels (nesting one under the other is exactly
as forbidden as nesting a level under itself). Order enforcement is `debug`/`cargo test`-only (via
`debug_assert!`, compiled to nothing in `--release`, matching this crate's four other
cost-boundary `debug_assert!` sites) against an ambient `tokio::task_local!` — **not**
`thread_local!`: `L2.read` is meant to span an entire async pipeline across possible OS-thread
migration on a multi-threaded runtime, and `task_local!`'s storage lives inside the polled future
itself, swapped into a thread-local only for the duration of one poll. Because `task_local!`
offers only scoped mutation (`scope`/`sync_scope`, no "set now, clear via `Drop`" API), the public
acquisition shape is a **scoped closure/future** (`checked_scope_sync`/`checked_scope_async`,
"run this critical section for me"), not an RAII guard returned to the caller — matching this
crate's existing `StateWriter::transaction<F, R>(&self, f: F) -> Result<R, WriteError>` idiom.
`checked_scope_sync` needs no running Tokio runtime (`LocalKey::sync_scope` is a plain synchronous
swap), so the same mechanism serves `L1` (`MigrationLock::acquire`, never `.await`s) and each
write-queue job dispatch (a plain `std::thread`) as well as the fully-async `L2`.

`L2` is realized by `WorktreeLockRegistry` (`lock::worktree`): one `tokio::sync::RwLock<()>` per
`worktree_id`, created on first use and kept for the registry's lifetime (`worktree_id`s are
UUIDv7s, never reused, so a stale entry is at worst a few dozen bytes — eviction is `[OPEN]`,
left for whichever task first owns a long-lived registry instance). `L1`
(`crate::migrate::run`) and `L4a`/`L4b` (`StateWriter`/`CacheWriter`) are instrumented **in
place** to actually participate in the order check — not a documentation-only mapping — per this
section's "no exceptions." The write-queue instrumentation is literal: each job dispatch is
wrapped in `checked_scope_sync(L4a|L4b, || job(&mut conn))`, which marks the writer thread as
already holding the hierarchy's topmost rank for the job's duration — this is what turns "L4
queues are leaves" from a construction accident into an enforced invariant, since *any* further
acquisition attempted from inside a queued job fails the strictly-greater check and panics (proven
by `crates/store/tests/lock.rs::{state,cache}_writer_job_cannot_acquire_another_lock`: the panic
tears down the writer thread, observed by the caller as `WriterGone` on both the panicking call —
its `oneshot` reply sender is dropped mid-unwind before replying — and any subsequent call, proving
the thread actually died rather than hanging). `L0` (`store.lock`) and `L3` (the shard-manager
map) ship as `LockLevel` variants only — no real synchronization primitive exists yet (`L0` is
T15's daemon lifecycle; `L3` is T09-02's shard LRU manager) — the same "type before backend"
precedent as T07-01's `ProjectionStore` trait; whichever task adds the real primitive calls the
already-public `checked_scope_sync`/`checked_scope_async` around it, unchanged. Adopting
`WorktreeLockRegistry` into the reconcile driver (`crates/index::reconcile::driver`, today's
*structural*, lock-object-free realization of `L2`'s write side) or the projection switch is
explicitly **not** this task — T09-04 and group 15 own that wiring; the read side is adopted by
T09-03 (below).

As-built note (T09-03, `[SPEC]`): `local_rag_search::SearchEngine::search_code`
(`crates/search/src/pipeline.rs`) is the first caller of `WorktreeLockRegistry::read` — via a new
sibling entry point, `WorktreeLockRegistry::read_bounded(worktree_id, wait, body)`
(`crates/store/src/lock/worktree.rs`), added rather than wrapping `read` in a bare
`tokio::time::timeout`: only the *wait for the guard* is bounded, so a `body` already in flight
when the deadline would fire is never cancelled mid-pipeline — a plain `timeout` around the whole
call would bound the pipeline's own execution time instead, which is a different (and wrong)
thing to bound per this section's own "search waits on L2.read (bounded)" wording (§6). A timeout
returns the typed `lock::ReadTimedOut`, mapped by the caller to `BUSY_RETRY` (§6, below). The
wait budget is a plain `Duration` parameter (`local_rag_search::DEFAULT_L2_READ_WAIT_BUDGET =
2000ms`, uncalibrated — no `config.toml` field exists for it per §3.1) supplied by the caller, not
read from global config by the lock itself. `L3` is adopted the same call: `ShardManager::acquire`
is invoked once per search, inside the held `L2.read`, exactly as this section's read-path rule
requires ("L3 held only for the map lookup, released before the query"; unchanged from T09-02).

## 6. Degraded modes & error taxonomy `[SPEC]`

Degradation is always **explicit** in responses; nothing degrades silently `[FIXED]`.

| Condition | Behavior | Flag / error |
| --- | --- | --- |
| FTS head missing/mismatched (06 §4) | serve dense-only, schedule FTS rebuild — or block until rebuilt if dense also unavailable | `degraded: "dense_only"` |
| Dense shard invalid → rebuilding | serve lexical-only during rebuild | `degraded: "lexical_only"` |
| Both legs unavailable | error | `INDEX_UNAVAILABLE` |
| Worktree unknown / never indexed | code tools error; memory tools work in repo/global scope | `WORKTREE_NOT_INDEXED` |
| Generation switch in flight | search waits on `L2.read` (bounded); timeout → | `BUSY_RETRY` |
| `data_policy` forbids a remote call | operation refused, local fallback if defined | `POLICY_BLOCKED_REMOTE` |
| Migration required / running | daemon serves only health/status | `MIGRATION_IN_PROGRESS` |
| Store locked by other instance | startup aborts with owner info | `STORE_LOCKED` |

Canonical error envelope on the daemon protocol:
`{code, message, retryable: bool, details?}`. MCP tools map `code` into `isError` content with
the same code string; hooks map any error to fail-open (empty output, exit 0).

Diagnostics: every degraded search response includes the validation reason
(e.g. `fts_head: tokenizer_version mismatch (3 != 4)`) so acceptance tests can assert *why*.

Migration-store faults map into this taxonomy at the protocol boundary (T15): a store
newer than the binary, a migration checksum drift, or rewritten migration history all
surface as `INCOMPATIBLE_STORE`, disambiguated by a `details` field (e.g.
`store_version 3 > binary_max 2`, or `checksum drift at version 1`); a migration in
progress surfaces as `MIGRATION_IN_PROGRESS`. The runner's own typed errors (13 §3) are
finer-grained than the wire codes.

As-built note (T12-04, `[SPEC]`): `ErrorCode::PathNotIndexed` (`PATH_NOT_INDEXED`) now exists,
produced by `get_file_context` when the requested path is not part of the active generation. Like
`UNSUPPORTED_MODE` it is a tool-contract condition rather than a store/degradation one, so it has
no row in the table above; `retryable = false`, and `details` separates "no such path in the
active generation" from "skipped, reason=…" (06 §2.2) — two genuinely different answers to
"why can't I see my file?".

As-built note (T12-03, `[SPEC]`): `ErrorCode::UnsupportedMode` (`UNSUPPORTED_MODE`) now exists —
again added by the task that first detects the condition. It is produced when `search_code` is
asked for `mode="semantic"`, the description leg spec 09 §5 defers past v0 `[FIXED]`; the request
is refused before worktree resolution or any lock, and `retryable = false` (no retry can make a
post-v0 leg available). This row is not in the table above because it is a *tool-contract*
condition (spec 09 §5 / 11 §2) rather than a store/degradation one; it uses the same canonical
envelope.

The envelope's `details` is still a freeform `Option<String>`, but the search **response** now has
a wired JSON serialization (`local_rag_protocol::SearchResponse`, spec 09 §7's shape) — the T09-03
note below predates that and is superseded on this point only. Group 15 still owns transport,
handshake and MCP framing.

As-built note (T11-03, `[SPEC]`): `ErrorCode::PolicyBlockedRemote` (`POLICY_BLOCKED_REMOTE`) now
exists — added by the task that first detects the condition, per the T09-03 note below. It is
produced by the central remote-policy guard in the embedding provider pool
(`local_rag_embed::policy`, 10 §1 / 12 §1) when the effective `data_policy` leaves no selectable
provider because every candidate is remote. `retryable = false` (the same request under the same
policy is refused identically) and `details` names the refused providers, so the diagnostic states
*which* selection was blocked. The "local fallback if defined" column is satisfied structurally:
the guard filters candidates **before** selection, so a local provider present in the pool is
simply chosen and no refusal is raised at all.

As-built note (T09-03, `[SPEC]`): the canonical envelope's first concrete shape is
`local_rag_protocol::{ErrorCode, ErrorEnvelope, DegradedMode}` (`crates/protocol/src/error.rs`) —
`protocol` rather than `search`, since this vocabulary is shared by every daemon subsystem (memory
tools too), not code-search-specific, and `protocol` already depends on nothing but `core`. Only
three `ErrorCode` variants exist so far — `IndexUnavailable`, `WorktreeNotIndexed`, `BusyRetry`,
the ones T09-03's search skeleton actually produces; `POLICY_BLOCKED_REMOTE`,
`MIGRATION_IN_PROGRESS`, `STORE_LOCKED`, `INCOMPATIBLE_STORE` are left undefined until the task
that actually detects each condition adds it (`#[non_exhaustive]` keeps this open without a
breaking change). `retryable` is `true` only for `BusyRetry`. `details` stays a freeform
`Option<String>` — no JSON (de)serialization is wired yet (`protocol` has no `serde` dependency),
that remains group 15. `local_rag_search::SearchEngine::run_locked`
(`crates/search/src/pipeline.rs`) implements this table's first three rows precisely: no active
`worktree_projection_state` tuple ⇒ `WORKTREE_NOT_INDEXED` (checked before either leg is
attempted, structurally distinguishing it from `INDEX_UNAVAILABLE`, which is only reachable once
an active tuple exists but both legs fail against it); FTS invalid (`open_and_validate_fts`,
`ValidationDepth::Cheap`, 06 §4) with dense healthy ⇒ `degraded: "dense_only"`; the converse ⇒
`degraded: "lexical_only"`; both bad ⇒ `INDEX_UNAVAILABLE` with the two legs' diagnostic strings
joined into `details`. `Resolution::Ambiguous` (02 §3.3 — a worktree resolvable only via an
explicit `attach()`) is folded into `WORKTREE_NOT_INDEXED` for now: no dedicated code exists for it
in this table, and the wire-facing outcome ("code search cannot proceed for this request") is the
same either way — flagged as a judgment call, not a new taxonomy row.
