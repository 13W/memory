//! The brute-force `ProjectionStore` spike candidate (T10-02, spec 05 §1).
//!
//! One of the three candidate backends named in spec 05 §1 ("brute-force over
//! `embedding_cache`"). Structurally independent from
//! [`local_rag_projection::FakeProjectionStore`] — that type is pre-T10 product
//! dev scaffolding for groups 08-09, not a spike candidate, and its
//! `BTreeMap<PointId, Vec<f32>>` + hex-text persistence would bias this
//! candidate's own timing numbers (pointer-chasing, hex parsing) in a way that
//! has nothing to do with the algorithm being compared. This module reimplements
//! the same idea (exact linear-scan search) with a layout built to be measured:
//!
//! - **In memory**: a contiguous row-major `Vec<f32>` (`ids[i]` <-> row `i`),
//!   cache-friendlier for a full linear scan than a tree of individually
//!   allocated vectors.
//! - **On disk**: two files per shard directory (spec 05 §2), written strictly
//!   in the order the contract requires (all point mutations before
//!   [`ShardHandle::write_head`], spec 05 §1/§5):
//!   - `head` — the [`ProjectionHead`] as `key=value` lines (same idea as the
//!     fake's own head file, independently serialized here).
//!   - `points.bin` — a binary, fixed-record format (see [`read_points_bin`] /
//!     [`write_points_bin`]) streamed via `BufReader`/`BufWriter` rather than
//!     built up as one in-memory buffer first — at the `large` (500k x 768)
//!     spike matrix dataset that avoids transiently doubling a ~1.5 GiB working
//!     set. `points.bin` is always the largest regular file in a populated
//!     shard directory, which is what lets the spike's shared, backend-agnostic
//!     corruption case (`conformance::run_conformance`, "truncate the largest
//!     regular file") land on real point data for this candidate too.
//!
//! As-built note (T10-02, `[SPEC]`, spec 05 §1): search scores by dot product,
//! "higher is closer" — the same convention [`ScoredPoint`]'s own doc and the
//! fake backend already use. Pinned here explicitly so recall@k comparisons
//! across T10-02/03/04 are against the same similarity metric; not a `[FIXED]`
//! requirement on whichever backend the group ultimately chooses.
//!
//! "Rebuild" has no separate code path here: the spike has no real
//! `embedding_cache` yet (T11-02), so — exactly as `conformance::build` already
//! treats "generate a dataset, then open + upsert + write_head" as standing in
//! for it — a fresh shard build **is** the rebuild path for this candidate's
//! purposes; `measure_metrics`'s open+upsert timing already measures it.

use std::collections::HashMap;
use std::fs;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use local_rag_core::identity::Uuid;
use local_rag_projection::{
    DenseQuery, Hash32, PointId, ProjectionError, ProjectionHead, ProjectionPoint, ProjectionStore,
    Result, ScoredPoint, ShardHandle, ShardParams,
};

use crate::report::PlatformSupport;
use crate::{SpikeAdapter, current_target};

const POINTS_FILE: &str = "points.bin";
const POINTS_TMP_FILE: &str = "points.bin.tmp";
const HEAD_FILE: &str = "head";
const FORMAT_VERSION: u32 = 1;
/// Every point id in this codebase is a BLAKE3 digest rendered as lowercase hex
/// (`crates/core/src/identity/domain.rs::hash` -> `blake3::hash(..).to_hex()`),
/// always exactly 64 ASCII characters — the fixed width `points.bin`'s record
/// layout relies on.
const POINT_ID_HEX_LEN: usize = 64;
/// `version(u32) + dims(u32) + point_count(u64)`.
const HEADER_LEN: usize = 4 + 4 + 8;

/// The brute-force spike candidate, over the harness's [`SpikeAdapter`] seam.
#[derive(Debug, Default, Clone, Copy)]
pub struct BruteForceAdapter;

impl SpikeAdapter for BruteForceAdapter {
    fn name(&self) -> &str {
        "brute-force"
    }

    fn platform_support(&self) -> PlatformSupport {
        // Pure `std`, no platform-specific code path — supported everywhere.
        PlatformSupport {
            target: current_target(),
            supported: true,
            reason: None,
        }
    }

    fn filtered_hnsw_available(&self) -> bool {
        false
    }

    fn store(&self) -> Option<Box<dyn ProjectionStore>> {
        Some(Box::new(BruteForceStore::new()))
    }
}

/// The brute-force [`ProjectionStore`]. Stateless; each
/// [`ProjectionStore::open`] yields an independent [`BruteForceShard`].
#[derive(Debug, Default, Clone, Copy)]
pub struct BruteForceStore;

impl BruteForceStore {
    /// Create a brute-force store.
    pub fn new() -> Self {
        Self
    }
}

impl ProjectionStore for BruteForceStore {
    fn open(&self, dir: &Path, params: ShardParams) -> Result<Box<dyn ShardHandle>> {
        Ok(Box::new(BruteForceShard::open(dir, params)?))
    }
}

/// In-memory mirror of a persisted shard: a contiguous, row-major vector table
/// plus an id -> row index for upsert/delete, guarded for `Send + Sync`.
#[derive(Debug)]
struct BruteForceState {
    /// `ids[i]` is the point id occupying row `i` of `vectors`.
    ids: Vec<PointId>,
    /// Row-major: row `i` is `vectors[i*dims..(i+1)*dims]`.
    vectors: Vec<f32>,
    /// Point id -> row index, for O(1) upsert-in-place / delete lookups.
    index: HashMap<PointId, usize>,
    head: Option<ProjectionHead>,
}

/// An opened brute-force shard.
#[derive(Debug)]
pub struct BruteForceShard {
    dir: PathBuf,
    params: ShardParams,
    state: Mutex<BruteForceState>,
}

impl BruteForceShard {
    /// Open (or create) a brute-force shard at `dir`. Loads whatever is on
    /// disk **without validating beyond format self-consistency** (real
    /// validate-on-open is a product-crate concern, T07-04, not part of this
    /// trait): a malformed `points.bin`/`head` surfaces as
    /// [`ProjectionError::Corrupt`] (spec 05 §10 F12), never a silent default.
    pub fn open(dir: &Path, params: ShardParams) -> Result<Self> {
        fs::create_dir_all(dir).map_err(ProjectionError::Io)?;
        let (ids, vectors) = read_points_bin(dir, params.dimensions)?;
        let index = ids.iter().cloned().zip(0..).collect();
        let head = read_head_file(dir)?;
        Ok(Self {
            dir: dir.to_path_buf(),
            params,
            state: Mutex::new(BruteForceState {
                ids,
                vectors,
                index,
                head,
            }),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BruteForceState> {
        self.state.lock().expect("brute-force shard mutex poisoned")
    }
}

impl ShardHandle for BruteForceShard {
    fn read_head(&self) -> Result<Option<ProjectionHead>> {
        Ok(self.lock().head.clone())
    }

    fn point_ids(&self) -> Result<Box<dyn Iterator<Item = PointId> + '_>> {
        let ids: Vec<PointId> = self.lock().ids.clone();
        Ok(Box::new(ids.into_iter()))
    }

    fn point_count(&self) -> Result<u64> {
        Ok(self.lock().ids.len() as u64)
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
        let mut state = self.lock();
        for point in points {
            // Idempotent by point id: repeated upsert overwrites (spec 05 §3).
            match state.index.get(&point.point_id).copied() {
                Some(row) => {
                    state.vectors[row * dims..(row + 1) * dims].copy_from_slice(&point.vector);
                }
                None => {
                    let row = state.ids.len();
                    state.ids.push(point.point_id.clone());
                    state.vectors.extend_from_slice(&point.vector);
                    state.index.insert(point.point_id.clone(), row);
                }
            }
        }
        write_points_bin(&self.dir, dims, &state.ids, &state.vectors)
    }

    fn delete(&self, ids: &[PointId]) -> Result<()> {
        // Idempotent: a missing id is a no-op (spec 05 §3).
        if ids.is_empty() {
            return Ok(());
        }
        let mut state = self.lock();
        let dims = self.params.dimensions;
        let doomed: std::collections::HashSet<&PointId> = ids.iter().collect();

        let mut new_ids = Vec::with_capacity(state.ids.len());
        let mut new_vectors = Vec::with_capacity(state.vectors.len());
        for (row, id) in state.ids.iter().enumerate() {
            if doomed.contains(id) {
                continue;
            }
            new_ids.push(id.clone());
            new_vectors.extend_from_slice(&state.vectors[row * dims..(row + 1) * dims]);
        }
        state.index = new_ids.iter().cloned().zip(0..).collect();
        state.ids = new_ids;
        state.vectors = new_vectors;
        write_points_bin(&self.dir, dims, &state.ids, &state.vectors)
    }

    fn write_head(&self, head: &ProjectionHead) -> Result<()> {
        // Head is the LAST write of an op (spec 05 §1/§5): every upsert/delete
        // above has already landed on disk by the time a caller reaches here.
        write_head_file(&self.dir, head).map_err(ProjectionError::Io)?;
        self.lock().head = Some(head.clone());
        Ok(())
    }

    fn search(&self, q: &DenseQuery) -> Result<Vec<ScoredPoint>> {
        let state = self.lock();
        let dims = self.params.dimensions;
        if q.vector.len() != dims {
            return Err(ProjectionError::DimensionMismatch {
                expected: dims,
                actual: q.vector.len(),
            });
        }
        let mut scored: Vec<ScoredPoint> = state
            .ids
            .iter()
            .enumerate()
            .map(|(row, id)| ScoredPoint {
                point_id: id.clone(),
                score: dot(&q.vector, &state.vectors[row * dims..(row + 1) * dims]),
            })
            .collect();
        // Deterministic: score descending, ties broken by point id ascending
        // (same convention as the oracle, crate::oracle::exact_top_k).
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.point_id.cmp(&b.point_id))
        });
        scored.truncate(q.k);
        Ok(scored)
    }

    fn optimize(&self) -> Result<()> {
        // Metrics-driven for a real backend (spec 05 §9); a wholesale-rewritten
        // flat array has nothing to compact.
        Ok(())
    }

    fn destroy(self: Box<Self>) -> Result<()> {
        match fs::remove_dir_all(&self.dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(ProjectionError::Io(e)),
        }
    }
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

// ---- Persistence: `points.bin` (streamed binary, fixed-size records) ------

/// Write `ids`/`vectors` (row `i` = `vectors[i*dims..(i+1)*dims]`) to
/// `dir`/`points.bin`, sorted ascending bytewise by point id — a deterministic
/// function of the point *set*, independent of upsert order (mirrors the
/// fake's own persisted-file rationale, reimplemented independently here).
/// Streams through a `BufWriter` into a temp file then `rename`s (atomic),
/// never building the whole file in memory first.
fn write_points_bin(dir: &Path, dims: usize, ids: &[PointId], vectors: &[f32]) -> Result<()> {
    for id in ids {
        if id.as_str().len() != POINT_ID_HEX_LEN {
            return Err(ProjectionError::Backend(format!(
                "brute-force point id must be exactly {POINT_ID_HEX_LEN} hex characters, got {} ({id:?})",
                id.as_str().len()
            )));
        }
    }
    let mut order: Vec<usize> = (0..ids.len()).collect();
    order.sort_by(|&a, &b| ids[a].cmp(&ids[b]));

    let tmp = dir.join(POINTS_TMP_FILE);
    write_points_bin_stream(&tmp, dims, ids, vectors, &order).map_err(ProjectionError::Io)?;
    fs::rename(&tmp, dir.join(POINTS_FILE)).map_err(ProjectionError::Io)
}

fn write_points_bin_stream(
    tmp: &Path,
    dims: usize,
    ids: &[PointId],
    vectors: &[f32],
    order: &[usize],
) -> io::Result<()> {
    let file = fs::File::create(tmp)?;
    let mut w = BufWriter::new(file);
    w.write_all(&FORMAT_VERSION.to_le_bytes())?;
    w.write_all(&(dims as u32).to_le_bytes())?;
    w.write_all(&(ids.len() as u64).to_le_bytes())?;
    for &row in order {
        w.write_all(ids[row].as_str().as_bytes())?;
        for component in &vectors[row * dims..(row + 1) * dims] {
            w.write_all(&component.to_le_bytes())?;
        }
    }
    w.flush()
}

/// Read `dir`/`points.bin` back into `(ids, vectors)` (row-major, matching
/// [`write_points_bin`]'s layout). A missing file is a clean empty shard (no
/// points ever upserted); anything else that fails to parse as this exact
/// format is [`ProjectionError::Corrupt`] — the F12 "unopenable"/detected-
/// divergence signal (spec 05 §10), never a silent partial load.
fn read_points_bin(dir: &Path, dims: usize) -> Result<(Vec<PointId>, Vec<f32>)> {
    let path = dir.join(POINTS_FILE);
    let file = match fs::File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((Vec::new(), Vec::new())),
        Err(e) => return Err(ProjectionError::Io(e)),
    };
    let file_len = file.metadata().map_err(ProjectionError::Io)?.len();
    let mut r = BufReader::new(file);

    let mut header = [0u8; HEADER_LEN];
    r.read_exact(&mut header)
        .map_err(|e| read_err(e, "points.bin header"))?;
    let version = u32::from_le_bytes(header[0..4].try_into().expect("4 bytes"));
    let file_dims = u32::from_le_bytes(header[4..8].try_into().expect("4 bytes")) as usize;
    let point_count = u64::from_le_bytes(header[8..16].try_into().expect("8 bytes"));

    if version != FORMAT_VERSION {
        return Err(ProjectionError::Corrupt(format!(
            "points.bin format version {version} != expected {FORMAT_VERSION}"
        )));
    }
    if file_dims != dims {
        return Err(ProjectionError::Corrupt(format!(
            "points.bin dims {file_dims} != shard dims {dims}"
        )));
    }

    let record_size = (POINT_ID_HEX_LEN + dims * 4) as u64;
    let expected_body = point_count.checked_mul(record_size).ok_or_else(|| {
        ProjectionError::Corrupt("points.bin declared point_count overflows record size".into())
    })?;
    let expected_total = (HEADER_LEN as u64)
        .checked_add(expected_body)
        .ok_or_else(|| ProjectionError::Corrupt("points.bin declared length overflows".into()))?;
    if expected_total != file_len {
        return Err(ProjectionError::Corrupt(format!(
            "points.bin declared length {expected_total} != actual file length {file_len}"
        )));
    }

    let mut ids = Vec::with_capacity(point_count as usize);
    let mut vectors = Vec::with_capacity(point_count as usize * dims);
    let mut id_buf = vec![0u8; POINT_ID_HEX_LEN];
    let mut component_buf = [0u8; 4];
    for _ in 0..point_count {
        r.read_exact(&mut id_buf)
            .map_err(|e| read_err(e, "a point id"))?;
        if !id_buf.iter().all(u8::is_ascii_hexdigit) {
            return Err(ProjectionError::Corrupt(
                "points.bin contains a non-hex point id".to_string(),
            ));
        }
        let id = std::str::from_utf8(&id_buf)
            .map_err(|_| ProjectionError::Corrupt("points.bin point id is not UTF-8".to_string()))?
            .to_string();
        ids.push(PointId::from_hex(id));
        for _ in 0..dims {
            r.read_exact(&mut component_buf)
                .map_err(|e| read_err(e, "a vector component"))?;
            vectors.push(f32::from_le_bytes(component_buf));
        }
    }
    Ok((ids, vectors))
}

/// An unexpected EOF mid-read is a detected format divergence (`Corrupt`), not
/// a bare I/O error — the file was shorter than its own declared shape.
fn read_err(e: io::Error, what: &str) -> ProjectionError {
    if e.kind() == io::ErrorKind::UnexpectedEof {
        ProjectionError::Corrupt(format!("points.bin truncated while reading {what}"))
    } else {
        ProjectionError::Io(e)
    }
}

// ---- Persistence: `head` (plain key=value text, atomic temp+rename) -------

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
    let uuid = |key: &str| -> Result<Uuid> {
        get(key)?
            .parse::<Uuid>()
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
                "local-rag-spike-bruteforce-test-{}-{n}",
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

    fn id(n: u8) -> PointId {
        PointId::from_hex(format!("{:064x}", n))
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
        let store = BruteForceStore::new();

        let points = vec![point(1, 4), point(2, 4), point(3, 4)];
        {
            let shard = store.open(&scratch.path, params).expect("open");
            shard.upsert(&points).expect("upsert");
        }
        // Reopen from scratch: a fresh handle over the same directory.
        let shard = store.open(&scratch.path, params).expect("reopen");
        assert_eq!(shard.point_count().expect("count"), 3);
        let mut ids: Vec<PointId> = shard.point_ids().expect("ids").collect();
        ids.sort();
        let mut expected: Vec<PointId> = points.iter().map(|p| p.point_id.clone()).collect();
        expected.sort();
        assert_eq!(ids, expected);
    }

    #[test]
    fn truncated_points_file_is_reported_corrupt() {
        let scratch = Scratch::new();
        let params = ShardParams::with_dimensions(4);
        let store = BruteForceStore::new();
        {
            let shard = store.open(&scratch.path, params).expect("open");
            shard.upsert(&[point(1, 4)]).expect("upsert");
        }
        let points_path = scratch.path.join(POINTS_FILE);
        let full_len = fs::metadata(&points_path).expect("metadata").len();
        let file = fs::OpenOptions::new()
            .write(true)
            .open(&points_path)
            .expect("open for truncation");
        file.set_len(full_len / 2).expect("truncate");

        match store.open(&scratch.path, params) {
            Err(err) => assert!(matches!(err, ProjectionError::Corrupt(_)), "got {err:?}"),
            Ok(_) => panic!("must detect corruption"),
        }
    }

    #[test]
    fn upsert_rejects_wrong_dimension() {
        let scratch = Scratch::new();
        let params = ShardParams::with_dimensions(4);
        let store = BruteForceStore::new();
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
    fn delete_is_idempotent_and_survives_reopen() {
        let scratch = Scratch::new();
        let params = ShardParams::with_dimensions(4);
        let store = BruteForceStore::new();
        let shard = store.open(&scratch.path, params).expect("open");
        shard.upsert(&[point(1, 4), point(2, 4)]).expect("upsert");

        shard.delete(&[id(1)]).expect("delete");
        // Deleting an id that is already gone is a no-op, not an error.
        shard.delete(&[id(1)]).expect("delete again (idempotent)");
        assert_eq!(shard.point_count().expect("count"), 1);

        let reopened = store.open(&scratch.path, params).expect("reopen");
        let ids: Vec<PointId> = reopened.point_ids().expect("ids").collect();
        assert_eq!(ids, vec![id(2)]);
    }

    #[test]
    fn search_returns_highest_dot_product_first() {
        let scratch = Scratch::new();
        let params = ShardParams::with_dimensions(2);
        let store = BruteForceStore::new();
        let shard = store.open(&scratch.path, params).expect("open");
        shard
            .upsert(&[
                ProjectionPoint {
                    point_id: id(1),
                    vector: vec![1.0, 0.0],
                },
                ProjectionPoint {
                    point_id: id(2),
                    vector: vec![0.0, 1.0],
                },
            ])
            .expect("upsert");

        let hits = shard
            .search(&DenseQuery {
                vector: vec![1.0, 0.0],
                k: 1,
            })
            .expect("search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].point_id, id(1));
    }
}
