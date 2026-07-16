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
  state.sqlite          # source of truth (+ -wal/-shm)
  cache.sqlite          # rebuildable, independently validated (+ -wal/-shm)
  projection/
    <worktree_id>/      # one dense shard per worktree; layout is backend-defined
  spool/
    <session_id>/
      000001.seg …      # append-only segments (07)
  models/
    <model_id>/         # downloaded weights + manifest.json + .ok marker
  run/
    daemon.sock         # unix domain socket (POSIX); dir mode 0700
  logs/                 # daemon logs (rotated)
  quarantine/           # corrupted shards moved here before rebuild (05 §7)
```

### 2.1 Directory resolution `[SPEC]`

| Item | POSIX | Windows |
| --- | --- | --- |
| `<data_dir>` | `$LOCAL_RAG_HOME`, else `$XDG_DATA_HOME`, else `~/.local/share` | `%LOCALAPPDATA%` |
| `<config_dir>` | `$XDG_CONFIG_HOME/local-rag`, else `~/.config/local-rag` | `%APPDATA%\local-rag` |
| MCP endpoint | `<data_dir>/local-rag/run/daemon.sock` | named pipe `\\.\pipe\local-rag-<sha256(user SID)[..12]>` |

`LOCAL_RAG_HOME` overrides everything (tests, containers). All directories are created `0700`,
files `0600` (POSIX); on Windows, default per-user ACLs of `%LOCALAPPDATA%` apply.

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
languages = ["typescript", "javascript"]   # [OPEN — first-release language set]
max_file_size_kb = 1024
```

### 3.2 Per-repository settings `[SPEC]`

Stored in `state.sqlite` (`repo_settings` table, 03 §2.1), edited via CLI/dashboard-equivalent,
never via files inside the repository (a repo checkout must not be able to change daemon policy).
Keys mirror `[models]`/`[index]` sections.

**Conflict rule for `data_policy`:** effective policy = **most restrictive** of global and
repository value. Order of restrictiveness:
`local_only > metadata_only_remote > allow_remote_with_redaction > allow_remote_full`.
Requests routed while any involved repo demands stricter policy MUST use the stricter policy.

### 3.3 Request context `[FIXED — daemon defines routing]`

Every daemon request (MCP tool call, hook recall RPC) carries an explicit context:
`{session_id, worktree_root?, repo_hint?}`. The daemon resolves `worktree_root` →
`worktree_id`/`repo_id` via the registry (03 §2.1); there is **no ambient current project**.
Requests without a resolvable worktree operate in global scope only.

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
