//! The Qdrant Edge `ProjectionStore` spike candidate (T10-04, spec 05 §1/§3).
//!
//! The third and last real candidate named in spec 05 §1. Lives in its own
//! workspace member (not a module in `local-rag-spike-harness`, unlike
//! brute-force/usearch) — see `spike/Cargo.toml`'s own header comment for why:
//! the `qdrant-edge` crate republishes the *actual* Qdrant server's WAL/segment
//! storage engine (~80 transitive dependencies), a different risk class from
//! usearch's compact C++ library, and isolating it means a build/platform
//! problem here can never make the already-passing brute-force/usearch
//! candidates uncompilable. This crate depends on `local-rag-spike-harness`
//! (for [`SpikeAdapter`], `oracle`, `conformance::run_conformance`), so the
//! harness cannot depend back — this candidate ships its own `spike` binary
//! rather than adding a 4th match arm to the harness's.
//!
//! ## On-disk layout
//!
//! [`EdgeShard`] owns its own directory structure (`edge_config.json`, `wal/`,
//! `segments/`) entirely — we never touch those files directly. Alongside it:
//! a `head` file, our own independent `key=value` text (same idiom as
//! `brute_force.rs`/`usearch_backend.rs` — deliberately duplicated a third
//! time rather than extracted into a shared helper: G10 deletes the losing
//! candidates' code after T10-05, so investing in an abstraction with a
//! bounded remaining lifetime, at the cost of touching two already-shipped,
//! already-gated files for symmetry alone, is the wrong trade here).
//!
//! **No companion id file** — the inverse of usearch's situation. Qdrant Edge
//! natively supports enumerating all points ([`EdgeShard::scroll`], paginated)
//! and per-point JSON payload storage, which neither usearch nor our own
//! hand-rolled formats have. [`ShardHandle::point_ids`]/[`ShardHandle::point_count`]
//! read directly from Qdrant's own storage (`scroll`/`count`); the reverse
//! mapping (derived key → our original 64-hex-char [`PointId`]) is recovered
//! from each point's payload (`{"point_id": "<hex>"}`), not computed — Qdrant
//! Edge's own on-disk state is the *sole* source of truth here.
//!
//! ## ID mapping, as-built `[SPEC]` (T10-04, spec 05 §3)
//!
//! Qdrant's native point id (`qdrant_edge::PointId`, i.e. `ExtendedPointId`)
//! supports a full 128-bit UUID, not just a 64-bit numeric key like usearch's —
//! [`derive_uuid`] parses a [`PointId`]'s first 32 hex characters (its first 16
//! raw digest bytes) as one big-endian `u128`, matching spec 05 §3's own
//! "...or 16 bytes" clause. **Deliberately no collision guard** (unlike
//! usearch's cheap in-memory hashmap check): checking here would need a real
//! `retrieve()` I/O call per point against a 128-bit keyspace, at the same
//! trust level this codebase already places in unguarded UUIDv7 identity
//! everywhere else (`worktree_id`, `generation_id` — also 128-bit, never
//! collision-guarded). The asymmetry with T10-03 isn't "128 bits is safer
//! alone" — it's that the *cost* of checking is categorically different
//! (persisted I/O vs. a free hashmap lookup). A genuine collision would not be
//! silently invisible even so: it would merge two point ids into one Qdrant
//! point (last-write-wins vector and payload), and `point_count()`/manifest
//! recomputation (sourced from `scroll()`) would then report one fewer point
//! than upserted — a divergence the shared conformance/manifest machinery
//! already knows how to catch.
//!
//! ## Scoring convention, as-built `[SPEC]` (T10-04, spec 05 §1)
//!
//! The shard is built with `Distance::Dot`. Verified directly in the vendored
//! source (`DotProductMetric::postprocess` is a literal identity: `fn
//! postprocess(score) { score }`, and `similarity` is the raw dot product, no
//! sign flip) — **no score transformation is needed at all**, the simplest of
//! the three real candidates (contrast usearch's `score = -distance`). Qdrant's
//! own `search()` results are used verbatim as [`ScoredPoint::score`].
//! `filtered_hnsw_available()` is `true` — and, unlike usearch (a bolted-on
//! separate `filtered_search` method), payload filtering is a first-order
//! parameter on every Qdrant search/scroll/count call, arguably the most
//! "native" filtered-HNSW story of the three candidates.
//!
//! ## Plain until optimized, as-built finding (T10-04, load-bearing for T10-05)
//!
//! A freshly created shard's only segment is Qdrant's "plain" (exact,
//! unindexed, full-scan) appendable form — `ensure_appendable_segment` in the
//! vendored source builds it via `config.plain_segment_config()`, never an
//! HNSW-indexed one directly. Promotion to HNSW-indexed happens only when
//! `EdgeShard::optimize()` runs (see [`ShardHandle::optimize`]'s impl below)
//! **and** the segment already exceeds `qdrant-edge`'s own default 10,000 KB
//! indexing threshold (`DEFAULT_INDEXING_THRESHOLD_KB`, verified in the
//! vendored `shard/optimizers/config.rs`) — dims×4 bytes×points must clear
//! that before there is anything to promote. Neither this spike's shared
//! `measure_metrics` nor a real product `switch()`/rebuild ever calls
//! `optimize()` automatically (spec 05 §9 `[FIXED]`: "triggered by metrics
//! only... never after every reconcile") — so **every recall/latency number
//! this spike measures for this candidate reflects exact, not approximate,
//! search**, unless a manual `optimize()` call is added to the measurement
//! path. This is not an artifact of the spike harness's own limitations: it
//! is exactly how this candidate would behave in the real product
//! architecture too, and arguably a *better* fit for spec 05 §9's own stated
//! principle than usearch (whose HNSW graph is live immediately, even for a
//! handful of points, with no equivalent "stay exact while small" grace
//! period). Measured recall on both the `small` (544 points) and
//! `representative` (50,000 points) matrix datasets is a stable `1.0` for
//! exactly this reason — neither ever gets indexed without an explicit
//! `optimize()` call, so this is **not directly comparable** to usearch's own
//! recall numbers (which reflect a live HNSW graph at every scale) without
//! this context; T10-05 should read it as "exact search, as this backend
//! would actually ship without further optimizer-scheduling work," not as "an
//! inherently more accurate ANN algorithm."
//!
//! ## No external daemon, as-built (T10-04, the card's own emphasis)
//!
//! Every [`EdgeShard`] method this adapter calls (`load`/`update`/`search`/
//! `scroll`/`count`/`flush`) is a plain synchronous `fn` in the vendored
//! source — none `async`. This crate's own `Cargo.toml` has **no** `tokio`
//! dev-dependency, and every test here is a plain `#[test]` with no
//! `#[tokio::test]`/manually-constructed runtime. That is the structural proof
//! this candidate is genuinely embedded/no-daemon: a hidden requirement for a
//! running async reactor or a listening socket would panic immediately on the
//! very first call, not pass silently — see
//! `qdrant_edge_needs_no_tokio_runtime_or_listening_socket` in
//! `tests/qdrant_edge.rs`.
//!
//! ## WAL takes a real exclusive lock, as-built (T10-04, found during testing)
//!
//! Unlike brute-force/usearch (plain files, no locking — multiple handles can
//! open the same shard directory concurrently with undefined but non-erroring
//! results), [`EdgeShard::load`] fails with a `WouldBlock`-shaped error if
//! another live handle already has the shard's `wal/` open. This is a real,
//! useful property, not a limitation to route around: it structurally
//! enforces the single-writer-per-shard invariant spec 02 §5's per-worktree
//! write lock already assumes at the product level — this candidate would
//! need no additional external locking discipline if chosen. The shared
//! `conformance.rs`/`lib.rs::measure_metrics` harness code was already safe
//! (every reopen there happens strictly after the previous handle's own
//! scope ends, or against a distinct directory); only this crate's own
//! `delete_is_idempotent_and_survives_reopen` test needed an explicit scope
//! to drop the first handle before reopening.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use local_rag_core::identity::Uuid as CoreUuid;
use local_rag_projection::{
    DenseQuery, Hash32, PointId, ProjectionError, ProjectionHead, ProjectionPoint, ProjectionStore,
    Result, ScoredPoint, ShardHandle, ShardParams,
};
use local_rag_spike_harness::report::PlatformSupport;
use local_rag_spike_harness::{SpikeAdapter, current_target};

use qdrant_edge::external::uuid::Uuid as QdrantUuid;
use qdrant_edge::{
    CountRequest, DEFAULT_VECTOR_NAME, Distance, EdgeConfig, EdgeShard, EdgeVectorParams,
    NamedQuery, Payload, PointId as QdrantPointId, PointInsertOperations, PointOperations,
    PointStructPersisted, QueryEnum, ScrollRequest, SearchRequest, UpdateOperation,
    VectorStructPersisted, WithPayloadInterface, WithVector,
};

const HEAD_FILE: &str = "head";
/// JSON payload key each point's original [`PointId`] hex string is stored
/// under — the sole reverse-mapping mechanism (see module doc).
const POINT_ID_PAYLOAD_KEY: &str = "point_id";
/// Scroll page size — a pagination/performance constant, not a
/// correctness/quality-relevant tuning value (O2's "no invented thresholds"
/// concern doesn't apply to it).
const SCROLL_PAGE_SIZE: usize = 1000;

/// The Qdrant Edge spike candidate, over the harness's [`SpikeAdapter`] seam.
#[derive(Debug, Default, Clone, Copy)]
pub struct QdrantEdgeAdapter;

impl SpikeAdapter for QdrantEdgeAdapter {
    fn name(&self) -> &str {
        "qdrant-edge"
    }

    fn platform_support(&self) -> PlatformSupport {
        PlatformSupport {
            target: current_target(),
            supported: true,
            reason: None,
        }
    }

    fn filtered_hnsw_available(&self) -> bool {
        true
    }

    fn reports_recall(&self) -> bool {
        true
    }

    fn store(&self) -> Option<Box<dyn ProjectionStore>> {
        Some(Box::new(QdrantEdgeStore))
    }
}

/// The Qdrant Edge [`ProjectionStore`]. Stateless; each
/// [`ProjectionStore::open`] yields an independent [`QdrantEdgeShard`].
#[derive(Debug, Default, Clone, Copy)]
pub struct QdrantEdgeStore;

impl ProjectionStore for QdrantEdgeStore {
    fn open(&self, dir: &Path, params: ShardParams) -> Result<Box<dyn ShardHandle>> {
        Ok(Box::new(QdrantEdgeShard::open(dir, params)?))
    }
}

/// An opened Qdrant Edge shard.
pub struct QdrantEdgeShard {
    dir: PathBuf,
    params: ShardParams,
    shard: EdgeShard,
    head: Mutex<Option<ProjectionHead>>,
}

impl std::fmt::Debug for QdrantEdgeShard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QdrantEdgeShard")
            .field("dir", &self.dir)
            .field("params", &self.params)
            .finish_non_exhaustive()
    }
}

impl QdrantEdgeShard {
    /// Open (or create) a Qdrant Edge shard at `dir`.
    ///
    /// `EdgeShard::load(dir, Some(config))` handles both the fresh-empty and
    /// already-populated cases uniformly (verified in the vendored source:
    /// `load` creates `wal/`/`segments/` if missing and creates a fresh
    /// appendable segment when none exist yet, exactly like `new` would) — no
    /// existence-check branching is needed, unlike usearch's `open()`. Passing
    /// `Some(config)` on every open also re-validates `params.dimensions`
    /// against whatever was previously persisted, a real, if incidental,
    /// consistency check.
    pub fn open(dir: &Path, params: ShardParams) -> Result<Self> {
        fs::create_dir_all(dir).map_err(ProjectionError::Io)?;

        let vector_params = EdgeVectorParams::builder(params.dimensions, Distance::Dot).build();
        let config = EdgeConfig::builder()
            .vector(DEFAULT_VECTOR_NAME, vector_params)
            .build();

        let shard = EdgeShard::load(dir, Some(config)).map_err(|e| {
            ProjectionError::Corrupt(format!("qdrant-edge shard failed to open: {e}"))
        })?;

        let head = read_head_file(dir)?;

        Ok(Self {
            dir: dir.to_path_buf(),
            params,
            shard,
            head: Mutex::new(head),
        })
    }

    fn lock_head(&self) -> std::sync::MutexGuard<'_, Option<ProjectionHead>> {
        self.head
            .lock()
            .expect("qdrant-edge shard head mutex poisoned")
    }
}

impl ShardHandle for QdrantEdgeShard {
    fn read_head(&self) -> Result<Option<ProjectionHead>> {
        Ok(self.lock_head().clone())
    }

    fn point_ids(&self) -> Result<Box<dyn Iterator<Item = PointId> + '_>> {
        let mut ids = Vec::new();
        let mut cursor = None;
        loop {
            let request = ScrollRequest {
                offset: cursor,
                limit: Some(SCROLL_PAGE_SIZE),
                filter: None,
                with_payload: Some(WithPayloadInterface::Bool(true)),
                with_vector: WithVector::Bool(false),
                order_by: None,
            };
            let (records, next_offset) = self
                .shard
                .scroll(request)
                .map_err(|e| ProjectionError::Backend(format!("qdrant-edge scroll failed: {e}")))?;
            for record in &records {
                ids.push(point_id_from_payload(record.payload.as_ref())?);
            }
            match next_offset {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        Ok(Box::new(ids.into_iter()))
    }

    fn point_count(&self) -> Result<u64> {
        let count = self
            .shard
            .count(CountRequest {
                filter: None,
                exact: true,
            })
            .map_err(|e| ProjectionError::Backend(format!("qdrant-edge count failed: {e}")))?;
        Ok(count as u64)
    }

    fn upsert(&self, points: &[ProjectionPoint]) -> Result<()> {
        let dims = self.params.dimensions;
        for point in points {
            if point.vector.len() != dims {
                return Err(ProjectionError::DimensionMismatch {
                    expected: dims,
                    actual: point.vector.len(),
                });
            }
        }

        let mut structs = Vec::with_capacity(points.len());
        for point in points {
            structs.push(PointStructPersisted {
                id: QdrantPointId::Uuid(derive_uuid(&point.point_id)?),
                // `Single` maps to the collection's DEFAULT_VECTOR_NAME
                // automatically — no named-vector complexity for our flat
                // single-vector contract.
                vector: VectorStructPersisted::Single(point.vector.clone()),
                payload: Some(payload_with_point_id(&point.point_id)),
            });
        }

        // Qdrant's upsert is natively idempotent-by-id (same id = update in
        // place) — no defensive remove-then-add dance, unlike usearch;
        // verified by `upsert_overwrites_existing_point_id_idempotently`.
        let operation = UpdateOperation::PointOperation(PointOperations::UpsertPoints(
            PointInsertOperations::PointsList(structs),
        ));
        self.shard
            .update(operation)
            .map_err(|e| ProjectionError::Backend(format!("qdrant-edge upsert failed: {e}")))
    }

    fn delete(&self, ids: &[PointId]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let mut qdrant_ids = Vec::with_capacity(ids.len());
        for id in ids {
            qdrant_ids.push(QdrantPointId::Uuid(derive_uuid(id)?));
        }
        let operation =
            UpdateOperation::PointOperation(PointOperations::DeletePoints { ids: qdrant_ids });
        self.shard
            .update(operation)
            .map_err(|e| ProjectionError::Backend(format!("qdrant-edge delete failed: {e}")))
    }

    fn write_head(&self, head: &ProjectionHead) -> Result<()> {
        write_head_file(&self.dir, head).map_err(ProjectionError::Io)?;
        *self.lock_head() = Some(head.clone());
        Ok(())
    }

    fn search(&self, q: &DenseQuery) -> Result<Vec<ScoredPoint>> {
        let dims = self.params.dimensions;
        if q.vector.len() != dims {
            return Err(ProjectionError::DimensionMismatch {
                expected: dims,
                actual: q.vector.len(),
            });
        }

        let request = SearchRequest {
            query: QueryEnum::Nearest(NamedQuery::default_dense(q.vector.clone())),
            filter: None,
            params: None,
            limit: q.k,
            offset: 0,
            with_payload: Some(WithPayloadInterface::Bool(true)),
            with_vector: Some(WithVector::Bool(false)),
            score_threshold: None,
        };
        // `query()` is real but unneeded multi-stage prefetch/fusion/MMR
        // complexity for a plain DenseQuery; `search()` is the direct path we
        // want (module doc: Distance::Dot needs no score transform either).
        #[allow(deprecated)]
        let hits = self
            .shard
            .search(request)
            .map_err(|e| ProjectionError::Backend(format!("qdrant-edge search failed: {e}")))?;

        let mut scored = Vec::with_capacity(hits.len());
        for hit in hits {
            scored.push(ScoredPoint {
                point_id: point_id_from_payload(hit.payload.as_ref())?,
                score: hit.score,
            });
        }
        Ok(scored)
    }

    fn optimize(&self) -> Result<()> {
        // `EdgeShard::optimize()` is real (found in the vendored source's
        // `edge/optimize.rs`, initially missed in this task's own research
        // pass — corrected here): it runs the indexing/merge/vacuum
        // optimizers in-process, synchronously, until no more optimization
        // plans are produced. Concretely, this is what promotes a fresh
        // shard's initial "plain" (exact, unindexed) appendable segment into
        // an HNSW-indexed one — see the module doc's "plain until optimized"
        // section for why this matters for interpreting this candidate's
        // recall numbers.
        self.shard
            .optimize()
            .map_err(|e| ProjectionError::Backend(format!("qdrant-edge optimize failed: {e}")))?;
        Ok(())
    }

    fn destroy(self: Box<Self>) -> Result<()> {
        let dir = self.dir.clone();
        drop(self); // EdgeShard flushes on Drop before we remove its files.
        match fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(ProjectionError::Io(e)),
        }
    }
}

/// Derive a Qdrant point id ([`QdrantUuid`]) from a [`PointId`]'s first 16 raw
/// digest bytes, big-endian (spec 05 §3 `[SPEC]`) — parses the id's first 32
/// hex characters as one big-endian `u128`. See the module doc for why no
/// collision guard is added here (unlike usearch's `derive_key`, T10-03).
fn derive_uuid(point_id: &PointId) -> Result<QdrantUuid> {
    let hex = point_id.as_str();
    let prefix = hex.get(0..32).ok_or_else(|| {
        ProjectionError::Backend(format!(
            "point id {hex:?} is shorter than the 32 hex characters this backend's key derivation needs"
        ))
    })?;
    let value = u128::from_str_radix(prefix, 16).map_err(|e| {
        ProjectionError::Backend(format!("point id {hex:?} prefix is not valid hex: {e}"))
    })?;
    Ok(QdrantUuid::from_bytes(value.to_be_bytes()))
}

/// Build the JSON payload carrying a point's original 64-hex-char [`PointId`]
/// — the sole reverse-mapping mechanism (see module doc).
fn payload_with_point_id(point_id: &PointId) -> Payload {
    let mut map = serde_json::Map::new();
    map.insert(
        POINT_ID_PAYLOAD_KEY.to_string(),
        serde_json::Value::String(point_id.as_str().to_string()),
    );
    Payload::from(map)
}

/// Recover the original [`PointId`] from a returned record/hit's payload.
/// Missing or malformed payload is a genuine detected divergence
/// (`ProjectionError::Corrupt`), never a silent default.
fn point_id_from_payload(payload: Option<&Payload>) -> Result<PointId> {
    let payload = payload.ok_or_else(|| {
        ProjectionError::Corrupt(
            "qdrant-edge point is missing its payload (point_id lost)".to_string(),
        )
    })?;
    let value = payload.0.get(POINT_ID_PAYLOAD_KEY).ok_or_else(|| {
        ProjectionError::Corrupt(format!(
            "qdrant-edge point payload is missing `{POINT_ID_PAYLOAD_KEY}`"
        ))
    })?;
    let hex = value.as_str().ok_or_else(|| {
        ProjectionError::Corrupt(format!(
            "qdrant-edge point payload `{POINT_ID_PAYLOAD_KEY}` is not a string"
        ))
    })?;
    Ok(PointId::from_hex(hex.to_string()))
}

// ---- Persistence: `head` (plain key=value text, atomic temp+rename) -------
//
// Deliberately duplicated a third time from `local_rag_spike_harness::
// brute_force`/`usearch_backend`'s identical-shaped functions — see the
// module doc's "duplication over premature abstraction" reasoning.

fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}

fn write_head_file(dir: &Path, head: &ProjectionHead) -> io::Result<()> {
    let buf = format!(
        "worktree_id={}\n\
         generation_id={}\n\
         model_space_id={}\n\
         projection_op_id={}\n\
         projection_schema_version={}\n\
         point_count={}\n\
         manifest_hash={}\n",
        head.worktree_id,
        head.generation_id,
        head.model_space_id,
        head.projection_op_id,
        head.projection_schema_version,
        head.point_count,
        head.manifest_hash,
    );
    atomic_write(&dir.join(HEAD_FILE), buf.as_bytes())
}

fn read_head_file(dir: &Path) -> Result<Option<ProjectionHead>> {
    let content = match fs::read_to_string(dir.join(HEAD_FILE)) {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(ProjectionError::Io(e)),
    };
    let mut fields: HashMap<&str, &str> = HashMap::new();
    for line in content.lines() {
        if line.is_empty() {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| ProjectionError::Corrupt(format!("head line has no '=': {line:?}")))?;
        fields.insert(key, value);
    }
    let get = |key: &str| -> Result<&str> {
        fields
            .get(key)
            .copied()
            .ok_or_else(|| ProjectionError::Corrupt(format!("head is missing `{key}`")))
    };
    let uuid = |key: &str| -> Result<CoreUuid> {
        get(key)?
            .parse::<CoreUuid>()
            .map_err(|e| ProjectionError::Corrupt(format!("head {key}: {e}")))
    };

    Ok(Some(ProjectionHead {
        worktree_id: uuid("worktree_id")?,
        generation_id: uuid("generation_id")?,
        model_space_id: uuid("model_space_id")?,
        projection_op_id: uuid("projection_op_id")?,
        projection_schema_version: get("projection_schema_version")?.parse::<u32>().map_err(
            |e| ProjectionError::Corrupt(format!("head projection_schema_version: {e}")),
        )?,
        point_count: get("point_count")?
            .parse::<u64>()
            .map_err(|e| ProjectionError::Corrupt(format!("head point_count: {e}")))?,
        manifest_hash: Hash32::from_hex(get("manifest_hash")?),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Scratch {
        path: PathBuf,
    }

    impl Scratch {
        fn new() -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "local-rag-spike-qdrant-edge-test-{}-{n}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("create scratch");
            Self { path }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    /// A syntactically valid 64-hex-char point id with `n` encoded in the
    /// first 32 characters — `derive_uuid` only reads the first 32 hex chars,
    /// so distinct `n` values must differ *there*, not merely in a trailing
    /// digit of a `{:064x}`-padded whole id (which would put all small `n`
    /// behind 60+ leading zeros, well outside the derivation window, and
    /// silently collide every test fixture onto the same uuid).
    fn id(n: u8) -> PointId {
        PointId::from_hex(format!("{n:032x}{n:032x}"))
    }

    fn point(n: u8, dims: usize) -> ProjectionPoint {
        ProjectionPoint {
            point_id: id(n),
            vector: (0..dims).map(|i| (n as f32) + i as f32 * 0.1).collect(),
        }
    }

    #[test]
    fn persistence_round_trips_points_and_head() {
        let scratch = Scratch::new();
        let params = ShardParams::with_dimensions(4);
        let store = QdrantEdgeStore;

        let points = vec![point(1, 4), point(2, 4), point(3, 4)];
        {
            let shard = store.open(&scratch.path, params).expect("open");
            shard.upsert(&points).expect("upsert");
        }
        let shard = store.open(&scratch.path, params).expect("reopen");
        assert_eq!(shard.point_count().expect("count"), 3);
        let mut ids: Vec<PointId> = shard.point_ids().expect("ids").collect();
        ids.sort();
        let mut expected: Vec<PointId> = points.iter().map(|p| p.point_id.clone()).collect();
        expected.sort();
        assert_eq!(ids, expected);
    }

    #[test]
    fn upsert_overwrites_existing_point_id_idempotently() {
        let scratch = Scratch::new();
        let params = ShardParams::with_dimensions(2);
        let store = QdrantEdgeStore;
        let shard = store.open(&scratch.path, params).expect("open");

        let pid = id(1);
        shard
            .upsert(&[ProjectionPoint {
                point_id: pid.clone(),
                vector: vec![1.0, 0.0],
            }])
            .expect("first upsert");
        shard
            .upsert(&[ProjectionPoint {
                point_id: pid.clone(),
                vector: vec![0.0, 1.0],
            }])
            .expect("second upsert (overwrite)");

        assert_eq!(shard.point_count().expect("count"), 1);
        let hits = shard
            .search(&DenseQuery {
                vector: vec![0.0, 1.0],
                k: 1,
            })
            .expect("search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].point_id, pid);
    }

    #[test]
    fn delete_is_idempotent_and_survives_reopen() {
        let scratch = Scratch::new();
        let params = ShardParams::with_dimensions(4);
        let store = QdrantEdgeStore;
        // Scoped: the WAL takes a real exclusive file lock (unlike brute-force/
        // usearch, which are plain files with no such lock) — a second `open`
        // on the same directory while this handle is still alive fails with
        // "Can't init WAL: Kind(WouldBlock)". This is a real, useful property,
        // not a limitation to work around: it structurally enforces the
        // single-writer-per-shard invariant spec 05 §2 already assumes.
        {
            let shard = store.open(&scratch.path, params).expect("open");
            shard.upsert(&[point(1, 4), point(2, 4)]).expect("upsert");

            shard.delete(&[id(1)]).expect("delete");
            shard.delete(&[id(1)]).expect("delete again (idempotent)");
            assert_eq!(shard.point_count().expect("count"), 1);
        }

        let reopened = store.open(&scratch.path, params).expect("reopen");
        let ids: Vec<PointId> = reopened.point_ids().expect("ids").collect();
        assert_eq!(ids, vec![id(2)]);
    }

    #[test]
    fn upsert_rejects_wrong_dimension() {
        let scratch = Scratch::new();
        let params = ShardParams::with_dimensions(4);
        let store = QdrantEdgeStore;
        let shard = store.open(&scratch.path, params).expect("open");

        let bad = ProjectionPoint {
            point_id: id(1),
            vector: vec![0.0, 1.0],
        };
        let err = shard.upsert(&[bad]).expect_err("must reject");
        assert!(
            matches!(
                err,
                ProjectionError::DimensionMismatch {
                    expected: 4,
                    actual: 2
                }
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn search_rejects_wrong_dimension() {
        let scratch = Scratch::new();
        let params = ShardParams::with_dimensions(4);
        let store = QdrantEdgeStore;
        let shard = store.open(&scratch.path, params).expect("open");
        shard.upsert(&[point(1, 4)]).expect("upsert");

        let err = shard
            .search(&DenseQuery {
                vector: vec![0.0, 1.0],
                k: 1,
            })
            .expect_err("must reject");
        assert!(
            matches!(
                err,
                ProjectionError::DimensionMismatch {
                    expected: 4,
                    actual: 2
                }
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn derive_uuid_is_deterministic_and_derived_from_first_16_digest_bytes() {
        let a = derive_uuid(&id(1)).expect("derive");
        let b = derive_uuid(&id(1)).expect("derive");
        assert_eq!(a, b, "same point id must always derive the same uuid");

        let hex = id(1);
        let expected_prefix = &hex.as_str()[0..32];
        assert_eq!(a.simple().to_string(), expected_prefix);
    }

    #[test]
    fn payload_round_trips_the_original_point_id() {
        let pid = id(7);
        let payload = payload_with_point_id(&pid);
        let recovered = point_id_from_payload(Some(&payload)).expect("recover");
        assert_eq!(recovered, pid);
    }

    #[test]
    fn point_ids_paginates_past_a_single_scroll_batch() {
        let scratch = Scratch::new();
        let dims = 4;
        let params = ShardParams::with_dimensions(dims);
        let store = QdrantEdgeStore;
        let shard = store.open(&scratch.path, params).expect("open");

        let count = SCROLL_PAGE_SIZE + 10;
        let points: Vec<ProjectionPoint> = (0..count)
            .map(|i| ProjectionPoint {
                // `i` must land within the first 32 hex chars (see `id`'s own
                // doc comment above) or every point silently collides onto
                // the same derived uuid.
                point_id: PointId::from_hex(format!("{i:032x}{i:032x}")),
                vector: vec![i as f32, 0.0, 0.0, 0.0],
            })
            .collect();
        shard.upsert(&points).expect("upsert");

        assert_eq!(shard.point_count().expect("count"), count as u64);
        let ids: Vec<PointId> = shard.point_ids().expect("ids").collect();
        assert_eq!(
            ids.len(),
            count,
            "pagination must return every point, not just page one"
        );
    }

    #[test]
    fn optimize_returns_ok() {
        let scratch = Scratch::new();
        let params = ShardParams::with_dimensions(4);
        let store = QdrantEdgeStore;
        let shard = store.open(&scratch.path, params).expect("open");
        shard.upsert(&[point(1, 4)]).expect("upsert");
        shard.optimize().expect("optimize");
    }

    /// `EdgeShard::optimize()` is real (see the module doc's "plain until
    /// optimized" section): below `qdrant-edge`'s own default 10,000 KB
    /// indexing threshold, a segment stays in its initial "plain" (exact,
    /// unindexed) appendable form even after `optimize()` runs — nothing to
    /// promote yet. This test crosses that threshold (dims×4 bytes×points
    /// clearly above 10 MiB) to prove `optimize()` safely handles a segment
    /// large enough to actually trigger the indexing/merge optimizers,
    /// without corrupting data: point count and search correctness must
    /// survive it.
    #[test]
    fn optimize_handles_a_segment_above_the_indexing_threshold() {
        let scratch = Scratch::new();
        let dims = 768;
        let params = ShardParams::with_dimensions(dims);
        let store = QdrantEdgeStore;
        let shard = store.open(&scratch.path, params).expect("open");

        // dims(768) * 4 bytes * 4000 points ≈ 12.3 MiB > the 10,000 KB default
        // indexing threshold. A simple splitmix64-style generator (not a
        // structured/modular pattern) avoids accidental near-duplicate
        // vectors, which would make "is the exact point its own top hit"
        // meaningless once approximate (post-optimize) search is in play.
        let mut state = 0x9E3779B97F4A7C15u64;
        let mut rnd = move || {
            state = state.wrapping_add(0x9E3779B97F4A7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
            z ^ (z >> 31)
        };
        let points: Vec<ProjectionPoint> = (0..4000u32)
            .map(|i| ProjectionPoint {
                point_id: PointId::from_hex(format!("{i:032x}{i:032x}")),
                vector: (0..dims)
                    .map(|_| ((rnd() >> 40) as f64 / (1u64 << 24) as f64 * 2.0 - 1.0) as f32)
                    .collect(),
            })
            .collect();
        shard.upsert(&points).expect("upsert");

        // Point of this test: optimize() (which now really runs the
        // indexing/merge/vacuum optimizers, see the module doc) must not
        // corrupt data on a segment large enough to actually trigger them —
        // not to prove approximate-search recall, which the dedicated recall
        // test covers over the harness's own real corpus.
        shard
            .optimize()
            .expect("optimize above the indexing threshold");

        assert_eq!(shard.point_count().expect("count"), points.len() as u64);
        let valid_ids: std::collections::HashSet<PointId> =
            points.iter().map(|p| p.point_id.clone()).collect();
        let hits = shard
            .search(&DenseQuery {
                vector: points[0].vector.clone(),
                k: 5,
            })
            .expect("search after optimize");
        assert!(
            !hits.is_empty(),
            "search must still return results after optimize"
        );
        assert!(
            hits.iter().all(|h| valid_ids.contains(&h.point_id)),
            "every hit must be a real point from this shard, not corrupted/foreign data: {hits:?}"
        );
    }

    /// Find a file anywhere under `dir` whose name contains `needle` (used
    /// only by the test below to locate Qdrant Edge's internal id-tracker
    /// file, whose exact path is an implementation detail we don't otherwise
    /// depend on).
    fn find_file_containing(dir: &Path, needle: &str) -> Option<PathBuf> {
        let mut stack = vec![dir.to_path_buf()];
        while let Some(current) = stack.pop() {
            for entry in fs::read_dir(&current).ok()?.flatten() {
                let file_type = entry.file_type().ok()?;
                if file_type.is_dir() {
                    stack.push(entry.path());
                    continue;
                }
                if entry.file_name().to_string_lossy().contains(needle) {
                    return Some(entry.path());
                }
            }
        }
        None
    }

    /// T10-04 finding, load-bearing for T10-05 (see the module doc's "WAL
    /// takes a real exclusive lock" section neighbor and spec 14 §7's
    /// as-built note): the shared conformance suite's generic "truncate the
    /// largest regular file" technique does not exercise a detectable
    /// divergence for this candidate — Qdrant Edge's vector/payload/WAL
    /// storage uses fixed-capacity preallocated files (32 MiB by default)
    /// that are transparently re-extended/tolerated regardless of truncation
    /// depth (empirically verified: even truncating the largest such file to
    /// 0 bytes still reopens with the original point count intact), because
    /// point identity/count tracking lives in a *separate*, small
    /// `mutable_id_tracker.*` file, not interleaved with vector bytes the way
    /// brute-force's `points.bin`/usearch's `usearch.index` are.
    ///
    /// Directly corrupting that id-tracker file *does* surface a real
    /// divergence — but as an **uncaught panic** from inside the vendored
    /// `qdrant-edge` 0.7.2 crate itself (`"can never have more versions than
    /// internal point mappings"`), not a clean, catchable `Result::Err`. This
    /// violates `conformance.rs`'s own stated invariant ("never panics on a
    /// case failure") at the process level for this one candidate — a
    /// genuine robustness gap in the third-party library (still young:
    /// first published March 2026), not a defect in this adapter, and not a
    /// `DEVIATIONS.md`-worthy mismatch with this project's own normative
    /// behavior (no backend is chosen yet, spec 05 §1 O1 is still open; this
    /// is exactly the kind of comparative data point T10-05 needs). Proven
    /// here via `catch_unwind` so it stays a deterministic, permanently
    /// documented property rather than a one-off discovery.
    #[test]
    fn corrupting_the_id_tracker_panics_instead_of_erroring_cleanly() {
        let scratch = Scratch::new();
        let params = ShardParams::with_dimensions(8);
        {
            let shard = QdrantEdgeShard::open(&scratch.path, params).expect("open");
            shard.upsert(&[point(1, 8), point(2, 8)]).expect("upsert");
        }

        let mappings = find_file_containing(&scratch.path, "mappings")
            .expect("qdrant-edge must persist an id-tracker mappings file");
        let len = fs::metadata(&mappings).expect("metadata").len();
        let file = fs::OpenOptions::new()
            .write(true)
            .open(&mappings)
            .expect("open for truncation");
        file.set_len(len / 2).expect("truncate");
        drop(file);

        let dir = scratch.path.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            QdrantEdgeShard::open(&dir, params)
        }));
        assert!(
            result.is_err(),
            "expected corrupting the id-tracker to panic (a documented qdrant-edge behavior); \
             it returned {result:?} instead — if this now returns a clean Result::Err, the \
             library has improved and this test (and its doc comment) should be updated"
        );
    }
}
