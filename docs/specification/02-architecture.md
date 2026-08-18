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

As-built note (T15-01, `[SPEC]`): `store.lock`'s JSON carries the four documented fields plus a
readiness marker written by §4.1 step 4 — `local_rag::daemon::lock::StoreLockInfo { instance_uuid,
pid, daemon_version, started_at, ready: bool, ready_at: Option<i64>, socket_path:
Option<String> }`. `ready`/`ready_at`/`socket_path` are `false`/`None`/`None` from step 1's initial
write and are set only once, by `StoreLockGuard::mark_ready`, through the *same* open, `flock`'d
file handle (truncate-and-rewrite, never close/reopen — a close/reopen would risk a window with no
lock held at all mid-swap).

As-built note (T15-01, `[SPEC]`): the socket at `run/daemon.sock` already answers every connection
with a one-line JSON greeting — `{instance_uuid, daemon_version, mode}` — well before T15-02's real
HELLO/WELCOME protocol exists (11 §1/§4). This is deliberately minimal and provisional: T15-01
needs *something* live on the socket for the store-lock liveness probe (§4.1 step 1) to talk to;
T15-02 replaces only this per-connection handler with a real framed parse, never the listener, the
bind, or the store lock itself.

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

[memory]
recall_token_budget = 1500   # additionalContext token budget (08 §6, 11 §5)
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

As-built note (T14-08, `[SPEC]`): the `[memory]` section is `local_rag_core::config::
MemoryConfig` (`recall_token_budget`, default `1500`) — 08 §6 names "token budget `[SPEC
default 1500 tokens, config]`" without fixing a TOML layout, so this is as-built the same
way `[spool]` was for T13-01.

As-built note (X-004, `[SPEC]`): `daemon.log_level` — declared since T02-05, editable from the
TUI since T18-07 — is consumed for the first time here: `local-rag serve` installs a process-wide
`tracing`/`tracing-subscriber` subscriber (`local_rag::logging::init`, `crates/local-rag/src/
logging.rs`), writing plain (non-ANSI) lines to **stderr**. Priority is `RUST_LOG` (when set and
non-empty) `>` `config.daemon.log_level` `>` `"info"`; an invalid directive on either side falls
back to `"info"` with a `warn!` explaining why (§6 "nothing degrades silently"), never a silent
downgrade or a panic. Only `local-rag serve` installs this subscriber — the rest of the CLI
(`index`/`watch`/…) is unaffected, and the library half of this crate never links
`tracing-subscriber`. Logged events are daemon lifecycle steps (lock acquired, state/cache opened,
listening, `daemon ready`, background jobs spawned/finished, the shutdown reason), one line per
request/notification handled (`daemon/handshake.rs::handle_connection` — method/tool label,
session harness, byte counts, duration, status; `admin/*` at `debug` so a future TUI's ~1s poll
does not flood `info`), and the same warnings a few call sites previously wrote via `eprintln!`
(a stalled/failed spool-resume session, an installed-but-unopenable embedding model). Never a
request or response **payload** — CLAUDE.md: recalled memory and indexed repository content are
untrusted data. `T18-08`'s in-memory ring buffer stays a separate thing for a different consumer
(a TUI dashboard, polled via `admin/tail_calls`/`admin/tool_stats`, 11 §7) — that buffer and this
log stream are independent, neither replaces the other.

As-built note (X-007, `[SPEC]`): the same stream also goes to a **file** under
`StoreLayout::logs_dir`, which X-004 had left reserved and unfilled. That boundary turned out to
be a hole rather than a clean line: `local-rag-proxy` starts the daemon with stderr set to
`Stdio::null()` (the normal MCP setup), so in practice every line X-004 emitted was discarded
exactly when someone would want to read it back — including the indexing-cycle lines X-006 added.

Two sinks, **one filter**: `logging::resolve_filter`'s single directive feeds both layers, so
`RUST_LOG`/`log_level` cannot quiet one while the other stays chatty, and neither sink has a
verbosity setting of its own. Files rotate **daily** and the newest seven are kept
(`tracing-appender`'s `RollingFileAppender`, `Rotation::DAILY`, `max_log_files(7)` — the same one
week `X-001` fixed as this store's retention horizon); names read `daemon.<YYYY-MM-DD>.log`.
`logs_dir` is created by `logging::init` itself through the usual private-`0700` `ensure_dir`,
because `StoreLayout::ensure` runs later, inside `DaemonHandle::start` — leaving it to that call
would cost a brand-new store its first run's log. The appender is used synchronously, without
`tracing_appender::non_blocking`, so no `WorkerGuard` has to outlive `serve()` and the lines
immediately preceding an exit or a crash are never the ones lost.

The file sink is **always on**: no config key gates it (an explicit owner decision), so §3.1's
pinned `SPEC_CONFIG_TOML` and its `default_matches_spec_toml` test are unaffected. A log file that
cannot be opened (permissions, a full disk) is **not** fatal — the daemon warns on stderr and runs
with that sink alone (§6: nothing degrades silently). Privacy is unchanged and applies to both
sinks: metadata only, never a request/response payload or indexed content.

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

As-built note (T15-02, `[SPEC]`): the wire form of this context is
`local_rag_protocol::handshake::RequestContext { session_id, worktree_root: Option<String>,
repo_hint: Option<String> }` — `Serialize + Deserialize`, embedded verbatim in every
`RequestEnvelope` the proxy sends (§4.2). `local-rag-proxy` resolves it exactly once at launch
(`handshake::resolve_session_params`: `session_id` from `$LOCAL_RAG_SESSION_ID` if set and
non-empty, else a fresh UUIDv7; `worktree_root` from `current_dir()`) and clones the same value
into every relayed call for the connection's lifetime — one proxy process serves one session, so
there is nothing per-request to vary it by. `repo_hint` is always `None` from the MCP proxy in v0
(no v0 tool fills it; the field exists for the wire shape and a future T15-07 CLI caller). The
`$LOCAL_RAG_SESSION_ID` env-var source is a deliberately provisional default: the real npm/plugin
launch contract that would set it does not exist yet in this repository (packaging is a later
group).

As-built note (T15-03, `[SPEC]`): the git probe §3.3's own T02-04 note named as "the daemon's job"
is `local_rag::daemon::gitroot` (`crates/local-rag/src/daemon/gitroot.rs`). `request_root` is
total — it never errors — built by shelling out to `git` (the same precedent
`crates/xtask/src/bench/run.rs`'s own `git rev-parse --short HEAD` already sets; no `git2`
dependency) rather than reimplementing git's own repository-layout rules:

- **Command**: `git -C <path> rev-parse --path-format=absolute --show-toplevel --git-dir
  --git-common-dir` (git ≥ 2.31), falling back to `--absolute-git-dir` + `--git-common-dir` for
  older git. Any failure (not a git repository, `git` missing, non-zero exit) is `WorktreeKind::
  NonGit`, not a hard error — a probe failure only becomes `worktree_root: None` if the path
  itself cannot be canonicalized at all (does not exist, inaccessible).
- **`WorktreeKind` discriminator**: `Main` iff the canonicalized `--git-dir` equals the
  canonicalized `--git-common-dir`; otherwise `Linked` (`git worktree add` gives a linked tree a
  `--git-dir` of `<common>/worktrees/<name>` while `--git-common-dir` stays `<common>`) — the
  exact discriminator, not a heuristic, and unit-testable without a real git repository since the
  comparison is pure string equality on already-git-reported paths.
- **Toplevel snapping**: the probed path is snapped to `--show-toplevel`, not used as given —
  `RequestContext.worktree_root` is the proxy's launch `current_dir()`, often a package
  subdirectory rather than the repository root, and `local_rag_store::registry::resolve`'s only
  automatic path matches the *recorded worktree root* exactly.
- **`remote_fingerprint`**: `git config --get remote.origin.url` (absent key is normal, not a
  failure — `None`, not an error) through the already-existing
  `local_rag_core::identity::remote::fingerprint`.
- **`common_dir_fingerprint`**: reuses `local_rag_core::identity::domain::path_fingerprint` on the
  canonicalized common/admin dir — consistent with `path_fingerprint`'s own documented shape
  (`H(path_fingerprint, canonical_path)`, and the common dir is itself a canonical path), not a
  new hash-schema variant for a field this module's own doc already marks "advisory only... never
  stored or queried."
- **Case sensitivity**: `daemon::gitroot::case_sensitivity()` returns the platform default
  (insensitive on macOS/Windows, sensitive elsewhere) — no live filesystem probe (would touch the
  caller's worktree on every request); `pub` specifically so T15-07's indexer calls the same
  function rather than risking a divergent fold.
- **No cache**: the probe runs fresh on every request. `git` shells out are a few milliseconds on
  a warm page cache, MCP tool calls are user-turn-driven (not a hot loop), and a path-keyed cache
  would itself be exactly the ambient per-request state the "no process-global current
  project/worktree/branch" guardrail forbids.

`get_file_context`'s absolute-path handling resolves its input's *parent directory* through the
same real `canonicalize_absolute` the worktree root itself used (so symlinked ancestors — macOS's
`/tmp` → `/private/tmp`, `/var` → `/private/var` are the everyday case — compare correctly)
without ever requiring the queried *file* to still exist, falling back to pure string
normalization only if the parent itself is also gone (11 §2's as-built note has the full
reasoning).

As-built note (T20-06, `[SPEC]`): ADR-0009's daemon-managed indexing supervisor
(`local_rag::daemon::indexing::supervisor`) does not create an ambient current project. The
`managed_worktree` registry (03 §2.1) it reads is a **list of background work** — which worktrees
this daemon process keeps a `spawn_worktree_task` (06 §1) running for — never a routing input:
every request this section governs still resolves `worktree_id`/`repo_id` fresh from its own
explicit `RequestContext`, whether or not that worktree happens to be daemon-managed. A managed
worktree's background task and a request naming that same worktree share no state beyond the
registry/store both read; the supervisor never substitutes for, defaults, or narrows request
routing.

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

As-built note (T15-01, `[SPEC]`): step 1's recovery algorithm, `local_rag::daemon::lock::acquire`,
has two independent branches, not one — the wording "on failure... retry once" describes only the
second:

- **On a successful non-blocking `flock`** (the common post-crash path): POSIX `flock` is
  per-open-file-description and releases automatically when its holder's process exits — a crashed
  prior daemon cannot leave a "stale but still `flock`'d" lock file behind. So reaching this branch
  already proves no live process holds the lock; this instance is the sole legitimate owner, and it
  best-effort removes any orphaned `run/daemon.sock` (§4.4) before binding — the socket file itself
  has no such auto-cleanup and can genuinely outlive a `SIGKILL`ed daemon.
- **On `WouldBlock`**: a real contender exists *at this instant*. The owner's PID and its socket
  greeting (§2's as-built note) must **both** check out — except when the owner's own lock record
  has `ready: false` (still between step 1 and step 4, most commonly a large store's migration
  still running at step 2): in that narrow window there is genuinely no socket to answer yet, so the
  liveness check trusts the PID alone rather than misreading "no listener yet" as "dead" and
  wrongly reclaiming a lock a live, still-starting daemon still holds (its real `flock` is never
  released by such a reclaim — the reclaiming instance would only ever win a fresh lock on the
  *path*, leaving two daemons each convinced they alone own the store). Once `ready: true`, the
  full two-part check applies exactly as written above.

As-built note (D-065, `[SPEC]`, amends the bullet above): reclaiming on `WouldBlock` requires a
lock record that was **read and parsed** and whose named owner was then proven gone. A record that
cannot be read at all — absent, empty, or partial — is `STORE_LOCKED`/`MIGRATION_IN_PROGRESS`,
never a reclaim. This follows from the branch's own premise: `WouldBlock` proves a live process
holds the `flock` at that instant, and a dead one cannot hold it (POSIX releases it on exit), so an
unreadable record can only belong to an owner that has not written it yet or is rewriting it —
never to a crashed owner. Recovery from a crash's torn write is the **successful**-`flock` branch's
job, where the content is simply overwritten. Two supporting details: `acquire` re-reads the record
a bounded number of times (`local_rag::daemon::lock::OWNER_READ_ATTEMPTS` × `OWNER_READ_RETRY_MS`,
8 ms total — chosen, not `[SPEC]`) so the refusal usually still names the owner, and `store.lock`
is rewritten in place over at least its previous length instead of being truncated first, so a
concurrent reader (`read_store_lock_file`, behind `status`/`doctor`) sees the old record or the new
one but never an empty file. Before this rule, two daemons started in the same millisecond both
reported acquiring the lock, and the loser left a record naming itself behind.

As-built note (T15-01, `[SPEC]`): step 2's `store_instance_uuid` (03 §2.1, consumed by step 3's
cache-open) is seeded here, inside the same step — `local_rag_store::registry::
ensure_store_instance_uuid`, a first-writer-wins atomic upsert (`INSERT ... ON CONFLICT DO UPDATE
... RETURNING`) called with a freshly minted candidate UUID immediately after migrations succeed.
T01-05's own as-built note had flagged this seeding as deferred to daemon wiring; this is that
wiring.

As-built note (T15-01, `[SPEC]`): a step 2 failure does not abort startup outright when the cause is
a migration-framework refusal (`local_rag_store::migrate::MigrationError::{IncompatibleStore,
ChecksumDrift, ...}`) — steps 3 and 5 are skipped (no usable `state.sqlite` exists to bind them to),
but step 4 still runs: the socket still binds, the lock is still marked ready, and the daemon
reports itself as `DaemonMode::MigrationOnly` (§6 `MIGRATION_IN_PROGRESS`/`INCOMPATIBLE_STORE`
disambiguated by `details`) rather than leaving nothing reachable at all — see §6's "nothing
degrades silently" `[FIXED]`. Every *other* step-1–4 failure (lock contention, a non-migration state
error, a cache-open failure, a bind failure) remains a genuine startup abort.

As-built note (T20-06, `[SPEC]`, G20). Step 4's "start workers" now includes one more: the
daemon-managed indexing supervisor (`local_rag::daemon::indexing::spawn_supervisor`, ADR-0009)
is constructed immediately after the readiness marker is written (`lock_guard.mark_ready(...)`),
before `McpHandler` starts serving connections — still squarely inside step 4, not a new step —
because `McpHandler` needs a ready `Option<SupervisorClient>` at construction time (T20-07's own
as-built note in 11 §8 explains why: `McpHandler` is built, and begins serving, independently of
`DaemonHandle`'s own remaining construction). The supervisor is `None` in exactly the same
condition `engine`/`memory` are `None` in (`DaemonMode::MigrationOnly` — no usable `state.sqlite`
to read `managed_worktree` from), so a migration-only daemon binds and answers step-4's readiness
marker without ever starting a background indexing worker, consistent with the "nothing degrades
silently" discipline `§6` fixes: the daemon still comes up, `admin/*` verbs answer `available:
false` rather than hanging or crashing. Cold start itself: the supervisor's own `reconcile()`
reads every enrolled row from `managed_worktree` (03 §2.1) and starts one `spawn_worktree_task`
per **enabled** row, batched `MAX_CONCURRENT_STARTUP_RECONCILES` at a time — an internal `[SPEC]`
constant chosen (not derived) the same way `LIVENESS_PROBE_TIMEOUT_MS` is, documented at its own
definition site, not repeated here.

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

As-built note (T15-02, `[SPEC]`): the concrete wire types live in `local_rag_protocol::handshake`
— `Hello`, `Welcome { proto, daemon_version, store_instance_uuid, capabilities[],
mcp_passthrough_version, spool_max_format_version, mode }`, `Incompatible { min_proto, max_proto,
daemon_version }`, `ShutdownRequest { requested_by_proxy_version, reason }`, `RequestContext`
(§3.3), `RequestEnvelope { context, mcp: Box<RawValue> }`, `ResponseEnvelope { mcp: Box<RawValue>
}`, unified under one `Message` enum. Framing is **NDJSON** (one `Message` per `\n`-terminated
line) for both the handshake and the MCP-passthrough phase alike: a UDS/pipe is a reliable
ordered byte stream (its only failure mode is EOF, unlike `local_rag_core::spool`'s durable
on-disk format, which needs CRC/magic against physical corruption), and JSON's own string-escaping
guarantees a raw `\n` never appears inside a line, so splitting on it is safe even for the opaque
passthrough payload. `mcp` is `Box<serde_json::value::RawValue>`, not `Value` — the proxy never
parses or rebuilds MCP JSON-RPC content (preserves big-integer `id` precision, field order, and
avoids a reparse allocation on relay), the literal "thin pass-through" of 11 §1.
`Message`'s `#[serde(tag = "type", content = "data")]` (adjacent tagging), not the default
internal tagging: internally-tagged enums cannot deserialize a variant holding a `RawValue` field
(`serde` buffers into a generic `Content` type first, which errors on `RawValue`'s
`deserialize_any`-only impl — reproduced directly), while adjacent (and external) tagging both
work, since neither needs that buffering step. Constants: `PROTO_VERSION = 1`,
`SUPPORTED_PROTO_RANGE = 1..=1`, `MCP_PASSTHROUGH_VERSION = 1`, `MAX_MESSAGE_BYTES = 8 MiB` — all
`[SPEC]`, picked and documented as chosen (not derived), the same precedent
`LIVENESS_PROBE_TIMEOUT_MS` set. `spool_max_format_version` reuses the existing
`local_rag_core::spool::FORMAT_VERSION` rather than a second constant (11 §4's as-built note).

`INCOMPATIBLE` is triggered **only** by a `proto` range mismatch
(`local_rag_protocol::negotiate_proto`) — `mcp_passthrough_version`/`spool_max_format_version` in
`WELCOME` are informational, not an accept/reject condition; the card's literal
`INCOMPATIBLE{min_proto, max_proto, daemon_version}` carries no third reason. A daemon-version
mismatch with a **compatible** `proto` does not produce `INCOMPATIBLE` at all — it is the upgrade
case below.

Daemon side (`local_rag::daemon::handshake`, replacing T15-01's provisional
`handshake_stub`): `HandshakeContext { instance_uuid, daemon_version, supported_proto, mode,
sessions: SessionRegistry, shutdown_requested: Arc<Notify> }`; `RequestHandler` — a native
`async fn`-in-trait (stable since Rust 1.75, no `async-trait`) implemented by `EchoRequestHandler`,
T15-02's own transport-proving stub (echoes `{context, received}`; real MCP dispatch is T15-03's
"type before backend" seam, the same precedent `ProjectionStore`/`Generator` already set);
`serve_connections` spawns one long-lived task per accepted connection (unlike the stub's
single-write-and-close). Per connection: read HELLO → `negotiate_proto` → WELCOME/INCOMPATIBLE
(closing on INCOMPATIBLE) → on WELCOME, register `SessionRegistry::register` (T15-01 built this
registry anticipating exactly this call site) and hold the guard for the connection's lifetime →
loop reading `Request`→`handler.handle`→`Response`, and `ShutdownRequest` → 
`shutdown_requested.notify_one()`. A `ShutdownRequest` does **not** close the connection — it keeps
looping. The daemon's own `main.rs::run_serve` builds its `tokio::runtime::Runtime` as a local
variable that is dropped only after `DaemonHandle::shutdown()`'s full drain (checkpoint, cache
close, lock release) has already completed; dropping a `Runtime` forcibly drops every still-running
task, including this connection's — closing the socket only *after* the drain finishes is what
lets the requesting proxy's `wait_for_close` observe EOF exactly when it is safe to reconnect, not
before.

Proxy side (`local-rag-proxy::{connect,handshake,relay}`): `connect::connect_or_spawn` tries one
connect, and only on failure spawns the daemon binary **once** (not once per retry — a slow
startup should not race a flood of redundant sibling spawns) via `std::process::Command` with
`process_group(0)` (unix — a signal to the proxy's own group, e.g. terminal Ctrl-C, never reaches
the daemon) and `Stdio::null()` on all three standard streams (otherwise the daemon would inherit
the very stdio channel the proxy uses to speak MCP), then retries with backoff. The backoff formula
is `local-rag-proxy`'s own copy of `crates/embed/src/pool.rs::RetryPolicy`'s *shape* (250 ms base,
doubling, 4 s cap — that module's own doc already cites this section as the numbers' source), not
an import (`local-rag-embed` pulls in `local-rag-store`, and this proxy must hold no project
state). The 20 s figure is a `tokio::time::timeout` racing the *entire* retry loop, not a
per-attempt running-total check, so the cutoff lands at exactly 20 000 ms regardless of where the
next scheduled delay would have landed (verified with a paused clock: 250/500/1000/2000/4000/4000…
ms, cut off at the 20 s boundary). The daemon binary is located next to the running proxy binary
via `current_exe()`'s parent directory (spec 13 §1: all product binaries ship side by side).

The upgrade flow (13 §4) is driven by `handshake::establish_session`'s retry loop, bounded by
`MAX_UPGRADE_ROUNDS = 2` (a defensive cap against a persistently flapping daemon — not a number
this section names): on a compatible `WELCOME` whose `daemon_version` differs from this proxy's
own `local_rag_core::VERSION`, the proxy sends `SHUTDOWN_REQUEST` on the same connection, then
`wait_for_close` (a plain `tokio::time::timeout`, the 30 s of this section) for EOF, then calls
`connect_or_spawn` again — with the old daemon's lock now released, this spawns the current
(now-matching) binary. Exceeding the round budget is `ProxyError::UpgradeLoopExceeded`; a
`wait_for_close` timeout is `ProxyError::UpgradeTimedOut` — both are proxy-local diagnostics (see
below), not synthesized MCP responses.

This task deliberately does **not** synthesize an MCP JSON-RPC initialization error on a
handshake/upgrade failure: doing so would require parsing the client's own incoming `initialize`
request for its `id` first (so the error can be correlated), which needs MCP tool-schema awareness
this task does not have — that is T15-03's domain (11 §2's "isError mapping"). `local-rag-proxy`
instead writes a `ProxyError`'s `Display` text to stderr and exits non-zero; nothing degrades
silently (§6's own invariant), but the *shape* on stdout described by this section's prose (an MCP
initialization error naming both versions) is not yet produced. This boundary is deliberate, not
an oversight, and is owned by T15-03 rather than tracked as a separate deviation, the same
established pattern this project already used for T15-06 (continuing consolidation trigger,
flagged by T14-06's as-built note) and T15-07 (real `code_raw`/`memory` required registration,
flagged at D-013's closure).

An implementation-level finding worth recording here since it is easy to reintroduce: this proxy's
stdin is read via `tokio::io::stdin()`, which is backed by a dedicated OS thread doing a genuine
blocking `read()` — there is no async stdin on unix. When the relay loop returns because the
shutdown signal fired rather than because stdin reached EOF, that thread is left blocked in the
syscall forever; `tokio::runtime::Runtime::drop` blocks its caller until every outstanding
`spawn_blocking` task completes, so simply letting the runtime drop at the end of `main` hangs the
whole process indefinitely in exactly that scenario (reproduced directly with a minimal repro
while building this task, and by the real `local-rag-proxy` binary under a real SIGTERM). Fixed by
calling `std::process::exit` explicitly instead of returning from `main`, bypassing `Runtime::drop`
— nothing past that point needs `Drop`-based cleanup.

### 4.3 Shutdown

Idle shutdown only when **all** hold: no live MCP sessions, no unimported spool bytes, no
running index/consolidation/GC jobs `[FIXED]`. SIGTERM/CTRL-C: stop accepting, cancel
reconciles at the next safe point (state tx boundaries), flush WAL checkpoint, release lock.
Kill at any point is safe by construction (05, 07).

As-built note (T15-01, `[SPEC]`): the idle gate's three inputs are
`local_rag::daemon::idle::IdleGateInputs { live_sessions: usize, pending_spool_bytes: bool,
running_jobs: usize }`, read from a protocol-agnostic `SessionRegistry` (registered by T15-02's
future per-connection HELLO handler; T15-01's own tests register directly), a `JobRegistry`
tracking the startup resume passes below, and
`local_rag_store::store_has_pending_spool_bytes`. `idle_eligible` is a pure `&&` of all three —
a single non-idle input refuses regardless of the other two, per this section's own "**all**".
This task's own scope covered only the two *startup* resume jobs (07 §6, 08 §4); no
reconcile-watcher or periodic-GC scheduling exists yet either (no card names an owner narrower
than "group 15" for either).

As-built note (T20-05/T20-06, `[SPEC]`, G20). The reconcile-watcher scheduling gap the note above
names is now owned: `local_rag::daemon::indexing::worktree_task`'s per-worktree task is the
`running_jobs` input's newest producer, `JobKind::Reconcile` (already declared, T20-05). The same
D-024 discipline the consolidation-trigger note below fixes applies here too — `JobGuard` is
acquired immediately before `write_locked(project_generation)` and dropped at the end of that one
call, never held across the outer `select!`'s own wait — so a daemon with one or more **enrolled
but quiet** managed worktrees (registered, watched, nothing has changed since) is exactly as
idle-eligible as one with none registered at all: watching a filesystem for changes is not
"running," only an active embed/activate/materialize cycle is. `tests/idle_shutdown.rs::
a_registered_but_quiet_managed_worktree_still_allows_idle_shutdown` is the regression test proving
this — a managed enrollment alone must never grow an unwritten fifth condition onto this section's
own "**all**" `[FIXED]` clause. (Whether a managed project should instead *keep* the daemon alive
is `T20-10`, an owner-decision card explicitly blocked pending a product decision — until then this
paragraph describes the only behavior that exists.)

As-built note (D-024, `[SPEC]`): continuous consolidation triggering (checkpoint on `Stop`,
queue-size threshold, best-effort `SessionEnd`) — the quarter this section's own text names but
T15-01 left to a later task, and which T15-06's own card never actually carried — is
`local_rag::daemon::consolidation_trigger::run_consolidation_trigger`, a `tokio::time::interval`-driven
loop (`config.memory.consolidation_poll_interval`-adjacent constant, `[SPEC]` 15s default, see
`main.rs::CONSOLIDATION_POLL_INTERVAL`) spawned alongside the two startup passes in
`DaemonHandle::start` (guarded identically: never in `MigrationOnly`). Each tick: T15-01's own
`resume_stale_consolidation_runs` verbatim (crash recovery first, so a same-tick fresh checkpoint
is never blocked by a leftover non-`applied` row), then per known spool session,
`import_session_tail` followed by `open_next_run`+`run_once` whenever that import just saw a
`Stop`/`SessionEnd` row or `local_rag_store::pending_backlog` has crossed
`config.memory.consolidation_queue_threshold`. Unlike the two startup passes (blind-awaited to
natural completion), this worker never completes on its own — `DaemonHandle` cancels it via a
dedicated `(oneshot::Sender<()>, JoinHandle<()>)` pair, signalled then awaited in `shutdown`,
mirroring `handshake_stop`/`handshake_join`'s existing shape rather than `resume_handles`'s. Its
own `JobGuard` (`JobKind::ConsolidationTrigger`) is held only for one tick's active work, dropped
before the next tick's wait, so a live-but-idle worker never blocks idle-shutdown. A known,
accepted gap: at daemon boot the worker's first tick races T15-01's own startup spool-import pass
for the same session's tail — whichever import call wins observes the checkpoint flag, so a
`Stop` arriving exactly at startup can be missed by the checkpoint path and only picked up once
the queue-size threshold is later crossed; see `consolidation_trigger.rs`'s own module doc.

As-built note (T15-01, `[SPEC]`; updated D-024): the four shutdown steps are, in order: (1) stop
accepting — signal the socket's accept loop to return, then best-effort unlink `run/daemon.sock`;
(2) cancel at the next safe point — the two startup resume passes are blind-awaited to natural
completion, which *is* "let the current job finish, refuse new ones" for them (a `StateWriter`/
`CacheWriter` job's only unit of work is already one SQL transaction — there is no smaller safe
point); the continuous consolidation-trigger worker (D-024) is signalled to stop and then awaited
immediately after, since — unlike the two passes — it never completes on its own; (3) flush WAL
checkpoint — `TRUNCATE` on both
`state.sqlite` and `cache.sqlite` (03 §4's own as-built note), then `CacheDb::close` (D-009's
blocking, join-the-writer-thread variant — exactly the "process going away" case it was built for);
`state.sqlite`'s own writer thread stays detached, safe by construction once step 2 already
guarantees nothing is mid-transaction; (4) release the store lock.

As-built note (T15-01, `[SPEC]`): the OS signal handler (`tokio::signal::unix::signal`) is
installed **before** step 1 of §4.1 even begins, not lazily once the wait loop starts — registration
happens at that call, not at the first read, so a `SIGTERM` delivered at any point during startup
(even before the lock is acquired) is captured and observed on the first wait, never lost to the OS
default terminate-immediately disposition. Installing it later (this task's own first draft) leaves
a real window, between the lock being marked ready and the wait loop actually starting, where a
signal kills the process ungracefully instead of draining it — caught by
`tests/serve_subprocess.rs` flaking under concurrent test-suite load before the fix.

### 4.4 Ownership invariants

One daemon per OS user per store `[FIXED]`. Identity = `instance_uuid` (+ PID as advisory)
`[FIXED]`. Orphan artifacts (stale socket, stale lock, orphan shard temp dirs, spool of dead
sessions) are cleaned at startup and by periodic GC.

As-built note (T15-01, `[SPEC]`): of this list, T15-01 cleans the stale socket and stale lock
classes, both at startup (§4.1's as-built note above) and, for the socket, again as part of an
orderly shutdown (§4.3). Orphan shard temp dirs and dead-session spool cleanup are the three
existing `local_rag_store::housekeeping` sweeps (T06-03, D-007, D-011, T13-05) — implemented and
tested, but periodic/startup *scheduling* of them remains unclaimed by any card narrower than
"group 15," the same gap this section's own as-built notes above already flag for
reconcile-watcher and GC triggering generally.

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

As-built note (T15-01, `[SPEC]`): `L0` is now a real participant, not a `LockLevel`-only
placeholder — `local_rag::daemon::lock::acquire` wraps its whole body in
`checked_scope_sync(LockLevel::L0, ...)`, the same in-place instrumentation `L1`/`L4a`/`L4b`
already carry. `L3` (the shard-manager map) remains unclaimed.

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

As-built note (T20-04, `[SPEC]`): the write side's first production adopter now exists.
`daemon::lifecycle::{StartOptions, DaemonHandle}` carry the daemon's single
`Arc<WorktreeLockRegistry>` (field `locks`) — production callers construct exactly one
(`main.rs::serve`), the same instance `build_search_engine` receives instead of constructing
its own private registry (superseding the T09-03 note above on this point only: the read side's
registry is no longer privately owned by `daemon::search`). `local_rag::indexing::write_locked`
is the typed entry point for the write side — a thin wrapper over `WorktreeLockRegistry::write`
that names the policy once ("the whole `reconcile_once → project_generation` cycle is one
`L2.write`-held unit") rather than leaving it to each future caller's own discipline; T20-05's
per-worktree indexing task is its first real caller. Adopting the registry *inside*
`local_rag_index::reconcile::driver` or the `projection` crate's own `switch` remains explicitly
out of scope, unchanged since the T09-01 note above — both stay the caller's job. The eviction
`[OPEN]` question (T09-01 note, above) is now resolved: **no eviction** — entry count is bounded
by the number of distinct worktrees one daemon process ever touches, and the process itself
exits on idle (§4.3 below), so the bound resets on its own; eviction would additionally need a
refcount against guards a caller might still be holding, complexity this bound never pays for.
CLI (`cli::index`/`cli::watch`) is unaffected — it never took `L0`/`L2` and continues to rely
solely on `state.sqlite`/`cache.sqlite`'s own WAL `busy_timeout` for cross-process safety
(`cli/mod.rs`'s own module doc), since `L2` is an in-process `RwLock`, not a cross-process
primitive (T20-09 covers the resulting double-indexing risk with an advisory warning, not a
shared lock).

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

As-built note (T15-01, `[SPEC]`): two of the T09-03 note's remaining three variants now exist,
added by the daemon-lifecycle code that first detects each condition (`crates/local-rag/src/
daemon/`). `ErrorCode::StoreLocked` (`STORE_LOCKED`) is produced when §4.1 step 1's `acquire`
returns `Locked{owner}`; `details` names the owner's `pid`/`instance_uuid`, `retryable = false`.
`ErrorCode::IncompatibleStore` (`INCOMPATIBLE_STORE`) is produced when step 2's migration fails for
a schema reason (`daemon::error::migration_only_reason`/`error_envelope` map the already-typed
`local_rag_store::migrate::MigrationError` variants, never re-deriving the condition); `details` is
this section's own two examples verbatim — `"store_version {n} > binary_max {m}"` /
`"checksum drift at version {n} ({name})"`. `ErrorCode::MigrationInProgress`
(`MIGRATION_IN_PROGRESS`, `retryable = true`) also now exists, but its call site is not inside the
daemon's own request path (a store legitimately mid-migration at step 2 has not bound its socket
yet, per this section's own step ordering, so no *response* can carry this code during that window)
— it is produced by `local-rag serve`'s own CLI-level startup-failure message when `Locked{owner}`
names an owner whose lock record has `ready: false` (still starting, most commonly still
migrating, see §4.1's as-built note above), naming the code for a human or launcher-script reading
stderr, not for a wire response. Group 15 (T15-02+) still owns wiring these into the real MCP/proxy
transport.

As-built note (T15-03, `[SPEC]`): the real MCP wiring named above (`local_rag::daemon::mcp`, 11
§2) confirms the previous note's own prediction rather than contradicting it — a `DaemonMode::
MigrationOnly` `tools/call` answers `INCOMPATIBLE_STORE`, never `MIGRATION_IN_PROGRESS`, for
exactly the reason already given: `MigrationOnlyReason` is *always* a refusal
(`IncompatibleStore`/`ChecksumDrift`/`Other`), never "currently migrating" — a store genuinely
mid-migration has no bound socket to answer *any* MCP request on. `initialize`/`tools/list`/
`ping`/notifications still work in `MigrationOnly` (they touch no store) so a connected client can
at least receive this diagnosable error, per this section's own `[FIXED]`: "nothing degrades
silently."

Also new: `SearchInfraError` (`local_rag_search::pipeline`, T12-04) — real infrastructure failures
(`state.sqlite`/`cache.sqlite` would not open, a corrupt `worktree_id`, a missing generation row),
distinct from this section's own domain taxonomy. It maps to `ErrorCode::IndexUnavailable`
(`retryable = false`, `details` carrying the error's own `Display` text) rather than a new
taxonomy row: the condition is exactly "the index cannot serve this request", which
`INDEX_UNAVAILABLE` already names, and it is surfaced as `isError` content (11 §2's own
two-channel split), never a bare JSON-RPC `-32603 Internal error` — that code is indistinguishable
from a server bug to the model reading it, where `isError` content lets the model see and react to
the specific taxonomy code.
