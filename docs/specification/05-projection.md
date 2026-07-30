# 05 — Projection Protocol (dense shards)

**Principle `[FIXED]`: the dense projection is always an untrusted cache.** Write-ahead makes a
crash before final commit visible; `ProjectionHead` + validate-on-open makes everything else
visible — including loss of backend durability *after* a clean commit. No durable barrier or
distributed-transaction semantics is required from the backend; only **detectability of
divergence**. SQLite `clean` never by itself proves the backend is physically durable — the
proof is re-established at every open.

## 1. `ProjectionStore` trait `[FIXED abstraction, signatures [SPEC]]`

```rust
/// One dense shard = one worktree. Backend: brute-force linear scan over
/// `embedding_cache` (ADR-0003, T10-05, closes O1).
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

Candidate backends evaluated by the step-11 spike: `qdrant-edge`, `usearch`, brute-force over
`embedding_cache`. **Resolved (T10-05, ADR-0003): brute-force.** Per-worktree shard semantics
hold for **any** backend `[FIXED]`. Filtered-HNSW is off the critical path (no tenant filter, no
generation filter inside a shard) but is included in the spike matrix `[FIXED]`.

As-built note (T10-05, `[SPEC]`, closes O1): brute-force linear scan is the v0 dense backend —
[ADR-0003](../adr/0003-dense-backend-selection.md) has the full comparison. Decisive factors: it
is the only candidate with zero new dependencies, recall that is exact by construction at any
scale (no tuning-vs-recall tradeoff, unlike usearch's measured recall collapse from 0.98 @544
points to 0.09 @50,000 on this spike's synthetic corpus), sub-millisecond `open`/`close`/
`registry_startup` at every scale tested (this section's own "cheap enough for every open"
requirement — Qdrant Edge measured 68–75 ms `open_ms` and 573–624 ms `registry_startup_ms`, two to
four orders of magnitude worse), a clean corruption-detection story with no special-case blind
spots (unlike Qdrant Edge, whose vendored 0.7.2 crate panics — an uncaught `Result`-bypassing
panic, not a catchable error — on a corrupted structural id-tracker file), and no native-toolchain
requirement at all (both real ANN candidates failed win32 build-smoke in-sandbox; brute-force is
pure `std`). `usearch`'s recall risk is explicitly **not** proven irrelevant to real embeddings
(synthetic i.i.d. vectors lack the cluster structure real code/text embeddings have) — it is
simply unquantified until T11 exists, and was not worth betting the v0 backend on. `optimize()`
is a documented no-op for this backend (§9's as-built note) — a flat array has no segment structure
to compact. T12-02 integrates this candidate's design into the product workspace.

As-built note (T12-02, `[SPEC]`): the production implementation is
`local_rag_projection::BruteForceProjectionStore` (`crates/projection/src/brute_force.rs`) —
still zero new dependencies (pure `std`), which the `Cargo.lock` diff proves rather than
asserts. It carries the spike candidate's design over: a contiguous row-major `Vec<f32>` with a
`point_id → row` index in memory, and on disk two files per shard directory — `points.bin` (a
streamed, fixed-record binary format, records sorted ascending bytewise by point id so the file
is a deterministic function of the point *set*, independent of upsert order) plus the
`key=value` `head`, written strictly last. `open` trusts nothing: a wrong `POINTS_FORMAT_VERSION`,
a dimension disagreement, a declared record count that contradicts the actual file length, a
non-hex point id, or a truncation mid-record all surface as `ProjectionError::Corrupt` — the F12
signal `crate::rebuild` turns into quarantine-then-rebuild — while a *missing* `points.bin` is a
legitimately empty shard. The declared-length check is what catches a truncation that happens to
land on a record boundary, which per-record reads alone would see as short-but-valid.

`ShardParams` gained `distance_metric` (§9 §3's "distance per `representation.distance_metric`"),
resolved together with `dimensions` from the model space's `code_raw` `RepresentationKey` by
`params_for_model_space`, which now delegates to the extracted
`code_raw_representation_key` — the same lookup the search pipeline's dense leg uses to embed the
query, so shard and query can never disagree about model, width or metric. Both backends score
through one shared helper (`similarity`) and rank through one shared comparator (`rank_scored`),
so a shard cannot rank differently depending on which store opened it.

`FakeProjectionStore` is **kept**, not replaced: it carries the named failpoints and the
`inspect`/`corrupt` controls the group-07 fault matrix is built on (§10, 14 §3), and group 07's
evidence stands unrewritten. The division is explicit — brute-force is the production backend,
the fake is the fault-injection one — and `crates/projection/tests/backend_contract.rs` asserts
the whole `ProjectionStore`/`ShardHandle` contract against **both**, so this section's
"backend-neutral" claim is now a test rather than a convention.

As-built note (T10-02, `[SPEC]`): the brute-force spike candidate
(`spike/harness/src/brute_force.rs`, isolated from the product's pre-T10
`FakeProjectionStore` dev scaffolding) scores by **dot product**, "higher is closer" —
the same convention `ScoredPoint`'s own doc and the fake backend already use. Pinned
here explicitly as this task's own working similarity metric, for a fair recall@k
comparison across T10-02/03/04; not a `[FIXED]` requirement on whichever backend the
group ultimately chooses.

As-built note (T10-03, `[SPEC]`): the `usearch` spike candidate
(`spike/harness/src/usearch_backend.rs`) is built with `MetricKind::IP`, whose native
distance is `1 - Σ(a[i]·b[i])` — a strictly decreasing function of the raw dot product.
The adapter converts `score = -distance` on every search result (`usearch`'s own
`Matches` are already sorted ascending by distance, so no re-sort is needed), matching
the same "higher is closer" convention T10-02 pinned. `filtered_hnsw_available()`
reports `true` — the first candidate to do so honestly (`usearch::Index::filtered_search`
is real, predicate-during-traversal filtered-HNSW). `ScalarKind::F32` is used (no lossy
quantization), so any recall gap is attributable to the approximate graph search itself.
`connectivity`/`expansion_add`/`expansion_search` are left at usearch's own `0` sentinel
(verified directly in the vendored `include/usearch/index_dense.hpp`: `0` is replaced by
the library's internal defaults — 16/128/64 respectively — during construction, not
literal zero-connectivity); no tuning constants are invented (O2).

As-built note (T10-04, `[SPEC]`): the Qdrant Edge spike candidate
(`spike/qdrant-edge/src/lib.rs`) is built with `Distance::Dot`. Verified directly in the
vendored source: `DotProductMetric::postprocess` is a literal identity (`fn
postprocess(score) { score }`) and `similarity` is the raw, unsigned dot product — **no
score transformation is needed at all**, the simplest of the three real candidates
(contrast usearch's `score = -distance`). `filtered_hnsw_available()` reports `true`;
unlike usearch's separate `filtered_search` method, payload filtering is a first-order
parameter on every Qdrant search/scroll/count call — arguably the most "native"
filtered-HNSW story of the three. `ScalarKind`-equivalent quantization is left disabled
(no `quantization_config`), same no-invented-tuning-constants discipline as usearch's
HNSW sentinel. Found during T10-04 testing, load-bearing for T10-05 (spec 14 §7 carries
the full as-built note): this candidate's on-disk corruption-detection story differs
qualitatively from the other two — point identity/count tracking lives in a small,
separate structural file, decoupled from the (fixed-capacity, preallocated) vector
storage files the shared conformance suite's generic corruption case targets, and
directly corrupting that structural file surfaces an uncaught panic inside the vendored
`qdrant-edge` 0.7.2 crate rather than a clean, catchable error.

## 2. Shard model

- **One shard per worktree** `[FIXED]`: pure active-only semantics, isolated rebuild, no
  tenant/generation filters. Cost: a shard manager with LRU eviction (`max_open_shards`);
  co-located usage keeps the active set small.
- Shard directory: `projection/<worktree_id>/`. Contents are backend-defined; the
  `ProjectionHead` must be recoverable from it (backend-native payload or a sidecar file —
  backend adapter's choice, but it MUST be written strictly after all point mutations of an op).

As-built note (T11-05, `[SPEC]`): the worktree's shard directory is a **root**, and its
backend-defined contents are split one level deeper, per model space:
`projection/<worktree_id>/<model_space_id>/` (`StoreLayout::projection_shard_space`). "One shard per
worktree" is unchanged — the root is still keyed by `worktree_id` alone, so §8's "attach/move …
same shard directory, never a second shard" holds and both housekeeping sweeps still operate on the
root and remove it recursively. What the split buys is two `[FIXED]` requirements of 10 §4 at once:
a model space whose `representation.dimensions` differ opens its own directory with its own
`ShardParams` (never an impossible in-place widening), and the outgoing space's shard stays
untouched for the whole migration, which is what makes "until step 4 commits for a worktree, that
worktree still runs A entirely" literally true rather than merely recoverable.

## 3. Deterministic point IDs `[FIXED]`

`projection_point_id = H(projection_point, worktree_id, occurrence_id, model_space_id,
representation_kind)` (03 §1.2). Repeated upsert overwrites; repeated delete is a no-op.
Backends needing 64/128-bit IDs derive them from the first 8/16 bytes of the digest `[SPEC]`.

As-built note (T10-03, `[SPEC]`): `usearch` is the concrete case this sentence anticipated —
its native key (`usearch::Key`) is `u64`. `spike/harness/src/usearch_backend.rs::derive_key`
parses a `PointId`'s first 16 hex characters (its first 8 raw digest bytes) as one
big-endian `u64`. A collision (two distinct point ids sharing a derived key) is never
silently merged: the adapter checks both its persisted key map and the current upsert
batch, and rejects the call with a typed error before mutating anything if one is found.

As-built note (T10-04, `[SPEC]`): Qdrant Edge is the concrete case for this sentence's
"...or 16 bytes" clause — its native id (`qdrant_edge::PointId`, i.e. `ExtendedPointId`)
supports a full 128-bit UUID, not just a 64-bit numeric key. `spike/qdrant-edge/src/
lib.rs::derive_uuid` parses a `PointId`'s first 32 hex characters (its first 16 raw digest
bytes) as one big-endian `u128`. Deliberately **no explicit collision guard** here (unlike
usearch's cheap in-memory hashmap check): checking would need a real backend I/O call per
point against a 128-bit keyspace, at the same trust level this codebase already places in
unguarded UUIDv7 identity elsewhere (`worktree_id`, `generation_id` — also 128-bit, never
collision-guarded). The asymmetry with T10-03 isn't "128 bits is safer alone" — the *cost*
of checking is categorically different (persisted I/O vs. a free hashmap lookup). A
genuine collision would not be silently invisible even so: it would merge two point ids
into one Qdrant point, and `point_count()`/manifest recomputation would report one fewer
point than upserted.

## 4. Expected point set

`expected_point_ids(tuple)` is a **deterministic pure function of `state.sqlite`**:
for the target `(generation, model_space)` — every occurrence of the generation × every
`required` representation kind of the model space that applies to code
(`code_raw`, `code_context`; `structural_description` only when descriptions are enabled
post-v0). The manifest hash is computed over this set, sorted bytewise.

As-built note (T11-05, `[SPEC]`): the "every `required` representation kind of the model space"
half is now a real `model_space_representation` join
(`local_rag_projection::expected::required_code_kinds`), replacing T07-03's hardcoded pair;
`CODE_REPRESENTATION_KINDS` survives only as this section's own "applies to code" filter over the
registry's answer. A model space that requires **no** code kind is a typed refusal
(`ExpectedError::NoCodeRepresentation`), not an empty expected set: an empty expectation would make
§5 step 3's `delete(existing \ expected)` silently wipe the shard, which is indistinguishable from
a correct empty projection.

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
  `expected::REQUIRED_REPRESENTATION_KINDS`, a hardcoded `{code_raw, code_context}` pair. The real
  per-model-space registry (`representation`/`model_space_representation`, the canonical six-field
  `RepresentationKey`, the `model_space` build-state machine, and the coverage data model) now exists
  (`local_rag_store::registry::representation`, T11-01) — but `expected_point_ids` does not yet join
  against it: today there is exactly one (T07-02-seeded) model space whose required set already *is*
  this pair (this section's own parenthetical excludes `structural_description` pre-v0), and wiring
  the lookup needs a working multi-model-space switch to actually exercise, which is T11-05's card
  ("production model-axis uses standard projection switch") — named explicitly, not a silent gap.
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

As-built note (T11-01, `[SPEC]`): the representation/model-space registry named above as a T07-03
gap is now `local_rag_store::registry::representation` (migration 6, `SCHEMA_V6`) —
`representation`/`model_space_representation` (spec 03 §2.2), a fresh crate-local
`RepresentationKind` (`local-rag-store` has no dependency on `local-rag-projection`, so this is not
the same Rust type as `crate::contract::RepresentationKind` above), the `ModelSpaceState` build
machine (spec 04 §3, identical transition shape to `GenerationState`), and the `Coverage` advisory
data model (`CoverageEntry{expected,ready,failed}` per required kind, `recompute_coverage` pure over
caller-supplied counts, `Coverage::fully_covered` gating `transition_model_space`'s
`building → projection_ready` edge). `model_space` itself and its seeded default row already existed
(`SCHEMA_V4`, T07-02); this task added only the missing transition guard over it. Real per-subject
coverage counting (walking occurrences/memory entries and `embedding_cache`) is T11-04's "resumable
coverage backfill" card — this registry provides the shape and the completeness gate only. This task
did **not** wire `expected::REQUIRED_REPRESENTATION_KINDS` or `switch::VectorSource` to the new
registry (see the bullet above) — that stays T11-05's, once a real multi-model-space switch exists to
exercise it.

As-built note (T11-05, `[SPEC]`): both wirings above are now done, and step 0's model-axis
preconditions have an owner. `local_rag_projection::model_switch::switch_model_space` is the
production entry point: it checks the target space is `active` (`eligible_as_target`) and that its
**stored** coverage is complete for its own required kinds, then calls `switch()` with the
worktree's *current* generation, so exactly one axis moves. Re-checking coverage here is not
redundant with `transition_model_space`'s gate: a space reaches `active` once, while the content it
must cover keeps growing with every new generation, and a switch started on stale coverage would
fail after the write-ahead already committed. Step 1's vectors come from
`local_rag_projection::vectors::CacheVectorSource`, which resolves `occurrence_id → blob_id →
H(subject/content_blob) → embedding_cache` and treats a row failing `verify_cached_embedding` as
absent — the caller's `MissingVector` is §7's coverage guard, and repairing the row is the backfill
worker's job (T11-04), never this reader's. Taking `L2.write` around the call remains the caller's
responsibility (group 15's wiring, 02 §5's own T09-01 note).

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

As-built note (T15-03, `[SPEC]`): the daemon's MCP code-query path (11 §2) runs this validation
unchanged on every shard open — but does so through a `VectorSource` that never answers
(`NoRebuildVectorSource`, `crates/local-rag/src/daemon/search.rs`, always `None`). The production
`VectorSource` (`local_rag_projection::CacheVectorSource`, T11-05, used by the switch/rebuild
paths below) cannot serve this call site: it borrows `&StateDb`/`&CacheDb` and is scoped to one
`(generation_id, model_space_id)` tuple *at construction time*, while the daemon's `ShardManager`
is a single, daemon-lifetime `Arc` built once at startup, long before any request names a
generation. The practical effect: a shard a real indexing run already filled opens and serves
normally (validation still catches drift); one that would need §7's rebuild degrades to
`lexical_only` instead — this card's own "no synchronous indexing call" requirement, made
structural rather than a discipline the MCP handler has to remember. Repair stays exclusively
T15-07's (CLI indexing) and T11-04's (backfill worker) job.

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

As-built note (T15-07, `[SPEC]`): `local-rag rebuild --dense` (11 §6) needed a rebuild entry point
independent of what §6's own validate-on-open predicates say — an operator forcing a rebuild is not
reacting to a detected divergence. `RebuildCause` gained a third variant, `Forced`, alongside
`Unopenable`/`Divergent`; it is handled exactly like `Divergent` in the quarantine-vs-destroy branch
(destroy the openable shard, no quarantine — an operator-requested rebuild is not a corruption
suspicion). The new `local_rag_projection::force_rebuild` mirrors `open_and_validate`'s own
structure (read `projection_state`, `Ok(None)` when there is no active tuple to rebuild from) but
skips `validate` entirely: a shard that opens fine is rebuilt anyway (`RebuildCause::Forced`), and
one that does not open falls back to the same `Unopenable` recovery `open_and_validate` already
uses — a forced rebuild must not fail outright just because the existing shard happens to be
unopenable. It shares the private `rebuild()` core with `open_and_validate`, so every invariant this
section already establishes (three separate transactions, quarantine rotation, `MissingVector`
raised before any shard write, idempotent restart) applies to a forced rebuild unchanged.

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

As-built note (T11-05, `[SPEC]`): implemented as
`local_rag_projection::model_switch::migrate_dormant_on_open`, called by `ShardManager`'s fill
**before** `open_and_validate` — i.e. before anything decides whether the shard may serve, which is
what "before serving dense search" asks for. "Retiring/absent" is read as: no active space at all, a
space the registry no longer knows, or one in `retiring`/`failed` (a `failed` space is likewise
never a legal target, so leaving a worktree on one would strand it). A worktree on a *healthy* space
is left alone even when a newer default exists — moving it is an explicit migration (10 §4 step 4),
not something an open performs silently. The shard's directory and `ShardParams` are then resolved
from whatever space is active after that migration, so a worktree that just moved to a
different-dimension space opens the right shard on the same call.

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

As-built note (D-011, `[SPEC]`): the per-model-space split T11-05 introduced (§2) needs a **third**
sweep, `local_rag_store::housekeeping::run_unreferenced_space_sweep`, beside the orphan (T06-03) and
grace-destroy (D-007) ones. After a worktree migrates A → B (10 §4 steps 4–6), `projection/<wt>/<A>/`
is dead weight neither sibling can see: the worktree is alive, so its root is not orphaned, and it is
`active`, so the root never expires. On the *generation* axis the equivalent stale data is reclaimed
inside the switch itself (§5 step 3's `delete(existing \ expected)` runs against the same directory);
the model axis has no such step by construction, which is exactly what makes the outgoing buffer
survivable during the switch — so the reclamation has to be a sweep. Gate G11 found the requirement
had no owning card: D-004's deferral chain and the sweeps predate the split.

Liveness is read as spec 04 §3's own phrase, per worktree: a space directory is live while the
worktree's `worktree_projection_state` row names it in **any** column
(`local_rag_store::referenced_model_space_ids`). Reading all three columns — not just `active` — is
what makes the sweep race-free against a switch in flight with no lock at all: §5 commits the
write-ahead (which sets `target_model_space_id`) *before* any backend mutation, so a target
directory is referenced from before it exists. Conservative in the same two ways as its siblings: a
root with **no** projection-state row is skipped wholesale (that root belongs to the orphan sweep),
and only directories are candidates. It never removes the worktree's shard *root* — "keyed by
`worktree_id`" above is untouched.

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
max query-latency impact — never "after every reconcile". Thresholds are backend-specific.

As-built note (T10-05, `[SPEC]`, closes O1's `optimize`-threshold half): the chosen v0 backend
(brute-force, ADR-0003) needs **no thresholds at all** — its `optimize()` is a documented no-op,
since a wholesale-rewritten flat array has no segment/graph structure whose fragmentation could
accrue. Recorded explicitly rather than left silently unresolved (O2: never invent a threshold
that isn't needed).

As-built note (T12-02, `[SPEC]`): the production `BruteForceProjectionStore::optimize` ships as
exactly that no-op, and `backend_contract.rs::optimize_never_changes_what_the_shard_holds` pins
the property every backend owes regardless of thresholds — calling it never alters the point set
or the head.

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

As-built note (T10-03, `[SPEC]`): the T10 dense-backend spike's shared conformance suite
(`spike/harness/src/conformance.rs`, run identically against fake/brute-force/`usearch`)
structurally cannot reach the full F1–F12 matrix, since it drives a bare `ShardHandle`
directly and never a `switch()`/`state.sqlite` cycle. Disposition: **F5–F8, F12** are
exactly what the suite's reopen/head/manifest/corruption cases exercise generically for
any candidate (equal-count-different-set is a pure manifest-hash property test; on-disk
corruption is a backend-agnostic largest-file truncation). **F1–F4, F9–F11** are
write-ahead-switch-driver or `state.sqlite`-registry-level concerns entirely out of a bare
`ShardHandle`'s reach — already exhaustively covered above at product-crate scope
(T07-05) against the fake backend, and not re-tested per spike candidate. This mirrors
T10-02's identical disposition for its own "crash/reopen cases" test bullet.

As-built note (T10-04, `[SPEC]`): Qdrant Edge is the one candidate where the shared
suite's F5/F8/F12-shaped corruption case (largest-file truncation) does **not** reach a
detectable divergence at `TINY` scale — its vector/payload/WAL storage uses fixed-capacity
preallocated files (verified empirically: even truncating the largest such file to 0 bytes
still reopens with the original point count intact), because point identity/count tracking
lives in a separate, small structural file the largest-file heuristic never targets (unlike
brute-force/usearch, where the largest file *is* the identity-bearing one). A dedicated
candidate-specific test (`spike/qdrant-edge/src/lib.rs::
corrupting_the_id_tracker_panics_instead_of_erroring_cleanly`) directly corrupts that
structural file instead and finds a genuine, separate robustness gap in the vendored
`qdrant-edge` 0.7.2 crate: an uncaught panic, not a clean `Result::Err` — a real finding for
T10-05, not a defect in this adapter or a `DEVIATIONS.md`-worthy mismatch with this
project's own normative behavior (no backend is chosen yet).
