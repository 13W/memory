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
