# 05 — Projection Protocol (dense shards)

**Principle `[FIXED]`: the dense projection is always an untrusted cache.** Write-ahead makes a
crash before final commit visible; `ProjectionHead` + validate-on-open makes everything else
visible — including loss of backend durability *after* a clean commit. No durable barrier or
distributed-transaction semantics is required from the backend; only **detectability of
divergence**. SQLite `clean` never by itself proves the backend is physically durable — the
proof is re-established at every open.

## 1. `ProjectionStore` trait `[FIXED abstraction, signatures [SPEC]]`

```rust
/// One dense shard = one worktree. Backend chosen at roadmap step 11 [OPEN].
pub trait ProjectionStore: Send + Sync {
    /// Open (or create) the shard directory. MUST be cheap enough to run
    /// validate-on-open on every open. Never trusts on-disk state.
    fn open(&self, dir: &Path, params: ShardParams) -> Result<Box<dyn ShardHandle>>;
}

pub trait ShardHandle: Send + Sync {
    fn read_head(&self) -> Result<Option<ProjectionHead>>;
    /// Iterate point IDs (for manifest verification). MAY be sampled only if the
    /// backend can prove exact count + a strong set digest; default: full scan.
    fn point_ids(&self) -> Result<Box<dyn Iterator<Item = PointId> + '_>>;
    fn point_count(&self) -> Result<u64>;

    fn upsert(&self, points: &[ProjectionPoint]) -> Result<()>;   // idempotent by point id
    fn delete(&self, ids: &[PointId]) -> Result<()>;               // idempotent
    fn write_head(&self, head: &ProjectionHead) -> Result<()>;     // LAST write of any delta/rebuild
    fn search(&self, q: &DenseQuery) -> Result<Vec<ScoredPoint>>;
    fn optimize(&self) -> Result<()>;                              // policy-driven, §9
    fn destroy(self: Box<Self>) -> Result<()>;
}

pub struct ProjectionHead {
    pub worktree_id: Uuid,
    pub generation_id: Uuid,
    pub model_space_id: Uuid,
    pub projection_op_id: Uuid,
    pub projection_schema_version: u32,
    pub point_count: u64,
    pub manifest_hash: Hash32,   // H(projection_manifest, tuple ‖ sorted point ids)
}
```

Candidate backends for the step-11 spike: `qdrant-edge`, `usearch`, brute-force over
`embedding_cache` `[OPEN]`. Per-worktree shard semantics hold for **any** backend `[FIXED]`.
Filtered-HNSW is off the critical path (no tenant filter, no generation filter inside a shard)
but is included in the spike matrix `[FIXED]`.

As-built note (T10-02, `[SPEC]`): the brute-force spike candidate
(`spike/harness/src/brute_force.rs`, isolated from the product's pre-T10
`FakeProjectionStore` dev scaffolding) scores by **dot product**, "higher is closer" —
the same convention `ScoredPoint`'s own doc and the fake backend already use. Pinned
here explicitly as this task's own working similarity metric, for a fair recall@k
comparison across T10-02/03/04; not a `[FIXED]` requirement on whichever backend the
group ultimately chooses.

## 2. Shard model

- **One shard per worktree** `[FIXED]`: pure active-only semantics, isolated rebuild, no
  tenant/generation filters. Cost: a shard manager with LRU eviction (`max_open_shards`);
  co-located usage keeps the active set small.
- Shard directory: `projection/<worktree_id>/`. Contents are backend-defined; the
  `ProjectionHead` must be recoverable from it (backend-native payload or a sidecar file —
  backend adapter's choice, but it MUST be written strictly after all point mutations of an op).

## 3. Deterministic point IDs `[FIXED]`

`projection_point_id = H(projection_point, worktree_id, occurrence_id, model_space_id,
representation_kind)` (03 §1.2). Repeated upsert overwrites; repeated delete is a no-op.
Backends needing 64/128-bit IDs derive them from the first 8/16 bytes of the digest `[SPEC]`.

## 4. Expected point set

`expected_point_ids(tuple)` is a **deterministic pure function of `state.sqlite`**:
for the target `(generation, model_space)` — every occurrence of the generation × every
`required` representation kind of the model space that applies to code
(`code_raw`, `code_context`; `structural_description` only when descriptions are enabled
post-v0). The manifest hash is computed over this set, sorted bytewise.

## 5. Switch algorithm (generation-switch ≡ model-space-switch) `[FIXED]`

Serialized by the per-worktree writer; the two axes are never applied simultaneously.

```
0. Preconditions: target generation is projection_ready (gen axis) /
   target model space is active with full required coverage (model axis).
1. PREPARE   target content in state.sqlite + cache.sqlite:
             vectors come from embedding_cache; unchanged content is NOT re-embedded.
2. WRITE-AHEAD (one SQLite tx, BEFORE any backend mutation):
             status='updating', target tuple set, projection_op_id = new UUID.
3. Acquire per-worktree WRITE lock (if not already held by the reconcile driver).
   DESIRED-SET RECONCILIATION against the shard:
             expected := expected_point_ids(target tuple)
             existing := shard.point_ids()
             shard.upsert(expected \ existing  ∪  changed)
             shard.delete(existing \ expected)
             shard.write_head(head(target tuple, op_id, |expected|, manifest_hash))
   -- not an imperative command replay: recovery after an unknown partial delta
   -- requires no history [FIXED].
4. COMMIT (ONE SQLite tx, AFTER backend):
             active := target; projected := target; target := NULL; status='clean';
             generation axis: N+1 → active, N → retiring.
5. Release lock. Schedule delayed GC of generation N.
```

Crash between 2–4 leaves `status='updating'` (or an inconsistent head) — both are open-time
detectable, both funnel into the single recovery path (§7).

As-built note (T07-03, `[SPEC]`): the switch is `local_rag_projection::switch` (a new dependency
of `local-rag-projection` on `local-rag-store`, foreseen by T07-01/T07-02's own doc comments). Four
mechanics are realized concretely, all pre-scoped to later, already-planned groups rather than new
deviations:

- **§4's "every required representation kind of the model space"** is
  `expected::REQUIRED_REPRESENTATION_KINDS`, a hardcoded `{code_raw, code_context}` pair — the real
  per-model-space registry (`representation`/`model_space_representation`) is T11-01, and until it
  ships v0 has exactly one seeded model space whose required set already *is* this pair (this
  section's own parenthetical excludes `structural_description` pre-v0).
- **Step 1's "vectors come from `embedding_cache`"** is realized through a new
  `switch::VectorSource` seam — `vector(occurrence_id, representation_kind) -> Option<Vec<f32>>` —
  standing in for the not-yet-built `embedding_cache` (T11-02). It has no "compute/embed" method, so
  the switch itself cannot trigger re-embedding; it only reads whatever it is given, and only for
  points missing from the shard.
- **Step 3's `∪ changed`** is realized as empty: `ShardHandle` has no vector-read-back method, so
  "does an already-present point's vector differ from desired" is not observable through the
  contract; since `upsert` is idempotent by id, only `expected \ existing` is upserted. A point
  already present under its deterministic id is trusted as-is; guarding against silent vector drift
  beyond id equality is a validate-on-open concern (T07-04), not this switch's.
- **Step 3's per-worktree WRITE lock** is the caller's responsibility — the lock hierarchy is
  T09-01. `switch()` documents that callers must serialize invocations per worktree themselves.

Retry is realized as simply calling `switch` again with the same target: the write-ahead's
`Updating → Updating` is a legal self-transition (T07-02), and step 3 recomputes `existing :=
shard.point_ids()` fresh from whatever the shard currently holds, so `expected \ existing` only
redoes the missing part — no command-log replay, matching this section's `[FIXED]` principle.
`commit_switch` pre-flights both generation-state moves (target `→ Active`, and — if applicable —
the outgoing generation `→ Retiring`) with the pure `GenerationState::check_transition` *before* any
write in the commit transaction, so a rejected commit (e.g. an unready target) leaves the
transaction untouched rather than partially applying the projection-state row without the
generation moves, or vice versa.

## 6. Validate-on-open `[FIXED]`

Executed on **every** shard open (daemon start, LRU re-open, post-crash), before the shard may
serve any search:

```
status != 'clean'                                   → rebuild
projected tuple != active tuple                     → rebuild
ProjectionHead missing OR op_id mismatch            → rebuild
head tuple != clean tuple in SQLite                 → rebuild
point_count mismatch OR manifest_hash mismatch      → rebuild
```

The `manifest_hash` check is the strong one: identical `point_count` with a differing ID set is
still detected. Verification cost is O(points) hashing; acceptable for local shards. Result of
validation is recorded (`status` → `dirty` before rebuild) so a crash during validation itself
re-enters the same path.

## 7. Rebuild — the single recovery path `[FIXED]`

**Full rebuild is the recovery default; delta is only the normal fast path.** Local rebuild is
cheap by construction: vectors are read from `embedding_cache`, no re-embedding.

```
rebuild(worktree):
  L2.write
  tx: status='rebuilding', projection_op_id = new UUID
  destroy or quarantine shard dir (move to quarantine/ on suspicion of backend corruption)
  create fresh shard; upsert expected set for the ACTIVE tuple; write_head
  tx: projected := active; status='clean'
  release
```

Rebuild MUST be idempotent (deterministic IDs + desired-set semantics). Missing vectors in
`embedding_cache` during rebuild → recompute via the embedding pipeline before head write
(coverage guard); the shard never goes `clean` with a partial expected set.

As-built note (T07-04, `[SPEC]`): validate-on-open and rebuild are
`local_rag_projection::{validate, rebuild}`. `state.sqlite`'s FSM only allows `Dirty → Rebuilding`
directly (spec 04 §2), never `Clean/Updating/Rebuilding → Rebuilding`, so the pseudocode's single
`tx: status='rebuilding', …` line above is realized as **three** separate committed transactions:
`mark_dirty` (whatever the current status, move to `dirty` and record the divergence reason as
`last_error` — legal from every status, so a crash right here still re-enters the same path on the
next open, §6), `begin_rebuild` (`dirty → rebuilding`, a fresh `projection_op_id`, and `target_*` is
cleared — a rebuild always targets the **active** tuple and abandons any in-flight switch rather
than resuming it), and `finish_rebuild` (`rebuilding → clean`, `projected := active`, `last_error`
cleared). No generation-state transition happens in `finish_rebuild`: rebuild never changes *which*
generation is active, only re-syncs the shard to match it. "destroy or quarantine … on suspicion of
backend corruption" is realized as an exact boundary: quarantine (a raw directory rename, since an
unopenable shard yields no `ShardHandle` to call `destroy` on) fires only for spec 05 §10 F12 (the
shard could not be opened at all); every other detected divergence destroys the openable shard via
`ShardHandle::destroy` before recreating it. Quarantine directories are named
`<worktree_id>-<uuid>` with a fresh UUIDv7 suffix (lexicographic sort == chronological order), and a
rotation step deletes the oldest same-worktree entries beyond `QUARANTINE_RETENTION = 2` immediately
after each new quarantine event (§8's "kept ≤ 2 rebuild cycles" — D-004's disposition put this here).
"Missing vectors … recompute via the embedding pipeline" is realized through T07-03's
`VectorSource` seam (still not a real `embedding_cache`, T11-02): a miss surfaces as a typed
`RebuildError::MissingVector` raised **before** any shard write in that attempt (built by collecting
every point's vector into a `Vec` first, only calling `upsert` once the whole set is confirmed
available), so the shard never goes `clean` with a partial expected set — the row is left at
`rebuilding` for the next open to retry, exactly as a crash would leave it (spec 05 §10 F11: "crash
during rebuild → `status='rebuilding'` → rebuild restarts"). A full rebuild is always
destroy/quarantine-then-recreate, never a diff against the shard's existing content (that diff is
`switch`'s fast path, §5) — this also means a rebuild that itself crashed and is retried simply
destroys/quarantines again and recreates from scratch, which is trivially idempotent given
deterministic point IDs and a pure `state.sqlite`-derived expected set.

## 8. Shard lifecycle follows registry lifecycle `[FIXED]`

- attach/move of a worktree: same shard directory (keyed by `worktree_id`), never a second shard.
- remove/detach: grace period `[SPEC: 7 days]`, then destroy.
- Orphan shard directories (no worktree row): GC'd at startup sweep.
- Quarantined shards: kept ≤ 2 rebuild cycles for diagnostics, then deleted `[SPEC]`.
- Bounded concurrent opens and background jobs (`max_open_shards`, one rebuild at a time per
  store by default `[SPEC]`); rebuild cancellation at worktree close/remove.
- Disk budget across shards is a metric with a soft cap; eviction closes cold shards (files
  remain; only handles are evicted).
- **Dormant worktree model migration** `[FIXED]`: opening a worktree whose
  `active_model_space_id` is retiring/absent switches it to the default space via the standard
  switch protocol before serving dense search.

As-built note (D-007, `[SPEC]`): the **grace-destroy** bullet above ("remove/detach: grace
period `[SPEC: 7 days]`, then destroy") is
`local_rag_store::housekeeping::run_expired_shard_sweep` (`crates/store/src/housekeeping.rs`),
sitting beside T06-03's orphan sweep in the same module. Its clock is the
`worktree.state_changed_at` column added by migration 5 (03 §2.1's D-007 note) — the missing
foundation that made deviation D-004 defer this bullet out of T06-03 in the first place; gate
G09 found that the deferral's named owners (groups 07/09) had both passed without it and no
later card claimed it, so it is implemented here rather than deferred a second time.

Shape: a pure predicate (`shard_destroy_due`) over `(state, state_changed_at)` plus an explicit
`now_ms`/`grace_ms` supplied by the caller — no clock reaches the store, so the fake-clock tests
are exact rather than approximate. `SHARD_DESTROY_GRACE_MS` is the section's 7-day default, a
plain constant because no `config.toml` surface for it exists (02 §3.1); whichever task adds one
threads it through the existing `grace_ms` parameter. Both `detached` and `removing` are
eligible (the section says "remove/**detach**"), `active` never is, a stamp in the future (clock
skew) is never due, and the boundary is inclusive so `grace_ms = 0` means "destroy now".

Scope boundary, deliberately narrow: this destroys the **shard directory**, which is all this
section ("Shard lifecycle follows registry lifecycle") governs. Deleting the `worktree` row
itself — 04 §7's "deleted after shard/spool/GC cleanup" — additionally needs spool cleanup
(group 13) and the registry cascade, and stays there; a row lingering in `removing` after its
shard is gone simply makes later sweeps no-ops (the sweep is idempotent). Evicting a still-open
handle for a destroyed shard is `ShardManager::remove` (T09-02), which the daemon wires to this
sweep in group 15 — the same wiring deferral the orphan sweep already carries, not a new one.

As-built note (T09-02, `[SPEC]`): the L3 shard-manager map (spec 02 §5) is
`local_rag_projection::manager::ShardManager` (`crates/projection/src/manager.rs`). Ref-counted
handles are plain `Arc<dyn ShardHandle>` — every method but `destroy` takes `&self` and the trait
is already `Send + Sync`, so sharing is sound; the manager itself never calls `destroy` (spec 05
§8's "eviction closes cold shards, only handles are evicted" — destroying on-disk state stays
`rebuild()`'s job alone, as it already was). "In use" is decided by `Arc::strong_count == 1`
(only the map's own copy remains), read under the L3 mutex (`checked_scope_sync(LockLevel::L3,
…)`, T09-01) — race-free because any concurrent `acquire` for the same key must also take L3.
Concurrent same-key opens single-flight through a `tokio::sync::OnceCell` per entry, held inside
each map slot so L3 itself is taken only for the get-or-insert/evict step, released before any
I/O (this section's own "L3 held only for the map lookup" — spec 02 §5). Eviction walks entries
oldest-first and evicts while unheld, but **stops** at the first entry still in use or still
filling rather than skipping ahead to a different victim — deferred, not substituted. Each cache
miss/reopen fills via the existing `open_and_validate` (T07-04, unchanged) before the manager's
own follow-up `open()`, so "validates every actual open/reopen" holds for free; a `[SPEC]`
signature tightening was needed to make this compile — `VectorSource` usage sites now take
`&(dyn VectorSource + Send + Sync)`, mirroring `UuidSource`'s own T05-03 usage-site tightening,
since a fill's `open_and_validate` call runs inside a `tokio::spawn`ed task (needed so `remove`
can cancel it independently of whichever caller's `acquire` triggered it) and holds that
reference across an `.await`. The "one rebuild at a time per store" default is a
`tokio::sync::Semaphore` with one permit around each fill; cancellation is a per-worktree
`tokio::task::AbortHandle`, cooperative (takes effect at the fill's next `.await`) — safe because
`rebuild()`'s three transactions (`mark_dirty`/`begin_rebuild`/`finish_rebuild`) are each
independently committed on `StateWriter`'s dedicated OS thread (T09-01): an already-enqueued
transaction always runs to completion regardless of the awaiting task's cancellation, so an
aborted fill leaves `worktree_projection_state` exactly where a crash between the same two steps
would — already proven self-healing by group 07's fault matrix. `remove` is a forced,
manager-level API distinct from passive LRU eviction (ignores the in-use deferral, cancels any
in-flight fill, drops the cache entry); it is deliberately **not** wired to the worktree
registry's own removal lifecycle — that needs a `removed_at` migration that does not exist yet
(D-004 deferred grace-destroy to "group 07/09" broadly, not this specific task). Also
deliberately deferred, seam in place: dormant-worktree model migration above (needs the real
model-space registry, T11-01) and adopting this manager into `switch`, the reconcile driver, or a
search executor (T09-03/T09-04, group 12/15) — direct `store.open()` call sites elsewhere
continue to race with this manager's own cache exactly as before this task, closed only *within*
the manager, not store-wide.

## 9. `optimize` policy `[FIXED]`

Triggered by metrics only — deleted/stale ratio, segment count, disk amplification, idle time,
max query-latency impact — never "after every reconcile". Thresholds are backend-specific
outputs of the step-11 spike `[OPEN]`.

## 10. Fault-detection matrix `[FIXED]`

Every case MUST lead to detection at open → `dirty` → rebuild. The suite proves exactly two
properties: **(a)** any divergence is detected at open; **(b)** rebuild is correct and
idempotent. (This intentionally compresses rev 5's "recover from every intermediate state".)

| # | Injected fault | Expected detection signal |
| --- | --- | --- |
| F1 | kill between write-ahead and first backend op | `status='updating'` |
| F2 | kill mid-upsert batch | `status='updating'` (+ head op_id stale) |
| F3 | kill after all point ops, before `write_head` | head op_id ≠ `projection_op_id` |
| F4 | kill after `write_head`, before SQLite commit | `status='updating'`, head tuple = target ≠ active |
| F5 | shard WAL loss/truncation *after* clean commit | manifest/point_count mismatch |
| F6 | partial point deletion with intact catalog | manifest_hash mismatch |
| F7 | missing head / stale head from previous op | head missing / op_id mismatch |
| F8 | equal point_count, different ID set | manifest_hash mismatch |
| F9 | failed final upsert/delete reported as ok by backend | manifest verification at next open |
| F10 | backend flush/sync failure swallowed | same as F5 at next open |
| F11 | crash during rebuild | `status='rebuilding'` → rebuild restarts |
| F12 | corruption making shard unopenable | open error → quarantine → rebuild |

Corruption cases are **detection tests**, not recovery-variety tests. See 14 §3 for harness.

As-built note (T07-05, `[SPEC]`): all 12 rows are executable, named tests, verified against
`fixtures/fault/matrix.json` by `crates/projection/tests/fault_matrix_coverage.rs`'s
mechanically-checked cross-reference (the "reusable artifact" this row's obligation asked for —
the declarative fixture itself is unchanged, its schema locks `status` to `"declarative"`). F1
(`switch_faults.rs`, T07-03) and F11/F12 (`rebuild_faults.rs`/`rebuild.rs`, T07-04) were already
covered; F1's test was extended to also confirm `open_and_validate` recovers (it reports
`NoActiveTuple` here specifically, since F1 is the bootstrap case — no switch has ever committed,
so there is no active tuple yet for `rebuild` to target; idempotent recovery is retrying `switch`
itself). F2–F10 are new, in `crates/projection/tests/fault_matrix.rs`:

- F2/F3/F4 run a *second* `switch()` (after a first one committed a real head, so "stale" is
  observable) failing at three distinct points. F4 needed a new production seam —
  `projection.switch.before_commit` (`crates/projection/src/switch.rs`), fired right before the
  final `state.sqlite` commit — since none of the fake shard's own seams (T07-01) fire between a
  landed shard write and that commit.
- F5–F10 corrupt an already-`clean` shard out of band (the existing `Corruption` API, T07-01) and
  prove `open_and_validate` catches it at the *next* open; `switch()` is not involved. F10 is
  literally F5's test under a different narrative (the row's own text: "same as F5 at next open").
  F9's narrative point is *when* the divergence surfaces: the preceding `switch()` call returns
  `Ok` with no error at all — only the separate, later `open_and_validate` call reveals it.
- F6 honesty note: "partial point deletion with **intact catalog**" implies a backend whose
  reported count stays stale relative to its actual data — a real-backend nuance the fake does not
  model (`FakeShard::point_count` always reflects exactly what is loaded, so dropping a point
  changes the count too). `validate` checks point count before manifest, so F6's test observes
  `PointCountMismatch` rather than `ManifestMismatch` specifically; both are correct detections of
  the same divergence. F8's test is what isolates `ManifestMismatch` alone (same count, different
  IDs), matching the pure unit-test coverage already in `validate.rs` (T07-04).

Every test in `fault_matrix.rs` arms, or is vulnerable to, the process-global failpoint registry
(`local_rag_test_support::failpoint`), so all nine serialize on a `tokio::sync::Mutex` (an
async-aware guard held across `.await`, unlike the `std::sync::Mutex` idiom `fake_faults.rs` uses
for its synchronous tests) — omitting this for even one test reproduces exactly the class of
cross-test interference D-005 found and fixed in T07-03.
