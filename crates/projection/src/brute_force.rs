//! The **production** dense backend: brute-force linear scan (spec 05 §1,
//! [ADR-0003](../../../docs/adr/0003-dense-backend-selection.md)) — T12-02.
//!
//! T10-05 closed open question O1 in favour of brute-force, and ADR-0003's own
//! "Consequences" section assigns this task the job of copying that spike
//! candidate's *design* — contiguous row-major vectors, a streamed fixed-record
//! on-disk format — into the product workspace, without copying the rejected
//! `usearch`/`qdrant-edge` code paths. This module is that copy, adapted to the
//! product's needs (a real [`DistanceMetric`], the shared
//! [`similarity`](crate::contract::similarity) helper). It adds **zero**
//! dependencies: pure `std`, which is one of the reasons the ADR chose it.
//!
//! # Layout
//!
//! - **In memory**: `ids[i]` ↔ row `i` of a contiguous `vectors: Vec<f32>`, plus
//!   a `HashMap<PointId, usize>` for O(1) upsert-in-place/delete lookups. A full
//!   linear scan over one contiguous allocation is cache-friendlier than a tree
//!   of individually allocated vectors — the whole point of the layout, since
//!   *every* search here is a full scan.
//! - **On disk**, two files per shard directory (spec 05 §2), written strictly
//!   in the order the contract requires (all point mutations, then
//!   [`ShardHandle::write_head`] — spec 05 §1/§5):
//!   - `points.bin` — a binary, fixed-record format streamed through
//!     `BufReader`/`BufWriter` rather than materialized as one buffer, so a large
//!     shard never transiently doubles its working set; records are sorted
//!     ascending bytewise by point id, making the file a deterministic function
//!     of the point *set*, independent of upsert order.
//!   - `head` — the [`ProjectionHead`] as `key=value` lines.
//!
//! # Untrusted by construction
//!
//! [`open`](ProjectionStore::open) loads what is on disk **without trusting
//! it**: a wrong format version, a dimension disagreement, a length that
//! contradicts the file's own declared record count, a non-hex point id, or a
//! truncation mid-record all surface as [`ProjectionError::Corrupt`] — the F12
//! "unopenable shard" signal (spec 05 §10) that [`crate::rebuild`] turns into
//! quarantine-then-rebuild. It never silently returns a partial or empty shard.
//! A *missing* `points.bin`, by contrast, is a legitimately empty shard.
//!
//! # Relationship to the fake backend
//!
//! [`FakeProjectionStore`](crate::fake::FakeProjectionStore) stays: it carries
//! the named failpoints and the `inspect`/`corrupt` controls the group-07 fault
//! matrix is built on (spec 05 §10, 14 §3). This backend is the production
//! default; the fake is the fault-injection one. Both score through the same
//! [`similarity`](crate::contract::similarity) helper, so a shard ranks
//! identically whichever opened it.
//!
//! `optimize()` is a documented no-op: a wholesale-rewritten flat array has no
//! segment or graph structure whose fragmentation could accrue, which is why
//! ADR-0003 records that **no** threshold exists to set for this backend
//! (spec 05 §9's "triggered by metrics only" has nothing to trigger).

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use local_rag_core::identity::Uuid;

use crate::contract::{
    DenseQuery, Hash32, PointId, ProjectionError, ProjectionHead, ProjectionPoint, ProjectionStore,
    Result, ScoredPoint, ShardHandle, ShardParams, rank_scored, similarity,
};

const POINTS_FILE: &str = "points.bin";
const POINTS_TMP_FILE: &str = "points.bin.tmp";
const HEAD_FILE: &str = "head";

/// The `points.bin` format version, stamped in its header. A file written by a
/// different version is [`ProjectionError::Corrupt`] rather than best-effort
/// parsed — the same "rebuild on doubt" reflex as every other cache in this
/// codebase.
pub const POINTS_FORMAT_VERSION: u32 = 1;

/// Every point id is a BLAKE3 digest rendered as lowercase hex
/// (`local_rag_core::identity::domain::hash`), always exactly 64 ASCII
/// characters — the fixed record width `points.bin` relies on.
const POINT_ID_HEX_LEN: usize = 64;

/// `version(u32) + dims(u32) + point_count(u64)`.
const HEADER_LEN: usize = 4 + 4 + 8;

/// The production brute-force [`ProjectionStore`]. Stateless; each
/// [`ProjectionStore::open`] yields an independent [`BruteForceShard`].
#[derive(Debug, Default, Clone, Copy)]
pub struct BruteForceProjectionStore;

impl BruteForceProjectionStore {
    /// Create a brute-force store.
    pub fn new() -> Self {
        Self
    }
}

impl ProjectionStore for BruteForceProjectionStore {
    fn open(&self, dir: &Path, params: ShardParams) -> Result<Box<dyn ShardHandle>> {
        Ok(Box::new(BruteForceShard::open(dir, params)?))
    }
}

/// In-memory mirror of a persisted shard: a contiguous, row-major vector table
/// plus an id → row index, guarded for `Send + Sync`.
#[derive(Debug)]
struct BruteForceState {
    /// `ids[i]` is the point id occupying row `i` of `vectors`.
    ids: Vec<PointId>,
    /// Row-major: row `i` is `vectors[i*dims..(i+1)*dims]`.
    vectors: Vec<f32>,
    /// Point id → row index.
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
    /// Open (or create) a brute-force shard at `dir` (see the module docs for
    /// what "untrusted" means here).
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
        // Validate the whole batch before mutating anything: a half-applied
        // upsert would leave the in-memory table disagreeing with the file it
        // was loaded from.
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
            // Idempotent by point id: a repeated upsert overwrites in place
            // (spec 05 §3).
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
        let doomed: HashSet<&PointId> = ids.iter().collect();

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
        // The head is the LAST write of an op (spec 05 §1/§5): every upsert/
        // delete above has already landed on disk by the time a caller gets
        // here.
        write_head_file(&self.dir, head).map_err(ProjectionError::Io)?;
        self.lock().head = Some(head.clone());
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
        let state = self.lock();
        let mut scored: Vec<ScoredPoint> = state
            .ids
            .iter()
            .enumerate()
            .map(|(row, id)| ScoredPoint {
                point_id: id.clone(),
                score: similarity(
                    self.params.distance_metric,
                    &q.vector,
                    &state.vectors[row * dims..(row + 1) * dims],
                ),
            })
            .collect();
        rank_scored(&mut scored);
        scored.truncate(q.k);
        Ok(scored)
    }

    fn optimize(&self) -> Result<()> {
        // A documented no-op for this backend (module docs, ADR-0003, spec
        // 05 §9): a flat array has no structure to compact.
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

// ---- Persistence: `points.bin` (streamed binary, fixed-size records) --------

/// Write `ids`/`vectors` (row `i` = `vectors[i*dims..(i+1)*dims]`) to
/// `dir/points.bin`, sorted ascending bytewise by point id, through a temp file
/// plus `rename` (atomic).
fn write_points_bin(dir: &Path, dims: usize, ids: &[PointId], vectors: &[f32]) -> Result<()> {
    for id in ids {
        if id.as_str().len() != POINT_ID_HEX_LEN {
            return Err(ProjectionError::Backend(format!(
                "brute-force point id must be exactly {POINT_ID_HEX_LEN} hex characters, \
                 got {} ({id:?})",
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
    w.write_all(&POINTS_FORMAT_VERSION.to_le_bytes())?;
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

/// Read `dir/points.bin` back into `(ids, vectors)`. A missing file is a clean
/// empty shard; anything that fails to parse as this exact format is
/// [`ProjectionError::Corrupt`].
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

    if version != POINTS_FORMAT_VERSION {
        return Err(ProjectionError::Corrupt(format!(
            "points.bin format version {version} != expected {POINTS_FORMAT_VERSION}"
        )));
    }
    if file_dims != dims {
        return Err(ProjectionError::Corrupt(format!(
            "points.bin dims {file_dims} != shard dims {dims}"
        )));
    }

    // The declared record count and the actual file length must agree exactly.
    // This is what catches a truncation that happens to land on a record
    // boundary, which a per-record read alone would see as a short-but-valid
    // file.
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

// ---- Persistence: `head` (plain key=value text, atomic temp+rename) ---------

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
    use crate::contract::DistanceMetric;
    use local_rag_test_support::TempHome;

    const DIMS: usize = 3;

    fn id(n: u8) -> PointId {
        PointId::from_hex(format!("{n:064x}"))
    }

    fn point(n: u8, vector: Vec<f32>) -> ProjectionPoint {
        ProjectionPoint {
            point_id: id(n),
            vector,
        }
    }

    fn open(home: &TempHome, params: ShardParams) -> BruteForceShard {
        BruteForceShard::open(&home.join("shard"), params).expect("open shard")
    }

    #[test]
    fn points_round_trip_through_the_binary_format() {
        let home = TempHome::new().expect("temp home");
        let params = ShardParams::with_dimensions(DIMS);
        let shard = open(&home, params);
        shard
            .upsert(&[
                point(2, vec![1.0, 2.0, 3.0]),
                point(1, vec![-1.0, 0.5, 0.0]),
            ])
            .expect("upsert");

        // Reopening reads only what was persisted.
        let reopened = open(&home, params);
        assert_eq!(reopened.point_count().expect("count"), 2);
        assert_eq!(
            reopened
                .point_ids()
                .expect("ids")
                .map(|i| i.as_str().to_string())
                .collect::<Vec<_>>(),
            vec![id(1).as_str().to_string(), id(2).as_str().to_string()],
            "records are persisted sorted by point id, whatever the upsert order"
        );
        // Exact f32 components survive the little-endian round trip.
        let hits = reopened
            .search(&DenseQuery {
                vector: vec![1.0, 0.0, 0.0],
                k: 10,
            })
            .expect("search");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].point_id, id(2));
        assert_eq!(hits[0].score, 1.0);
        assert_eq!(hits[1].score, -1.0);
    }

    #[test]
    fn the_persisted_file_is_a_function_of_the_point_set_not_the_order() {
        let a = TempHome::new().expect("temp home");
        let b = TempHome::new().expect("temp home");
        let params = ShardParams::with_dimensions(DIMS);
        let points = [
            point(1, vec![1.0, 0.0, 0.0]),
            point(2, vec![0.0, 1.0, 0.0]),
            point(3, vec![0.0, 0.0, 1.0]),
        ];

        open(&a, params).upsert(&points).expect("upsert in order");
        let reversed: Vec<ProjectionPoint> = points.iter().rev().cloned().collect();
        open(&b, params).upsert(&reversed).expect("upsert reversed");

        assert_eq!(
            fs::read(a.join("shard").join(POINTS_FILE)).expect("read a"),
            fs::read(b.join("shard").join(POINTS_FILE)).expect("read b"),
        );
    }

    #[test]
    fn upsert_is_idempotent_and_overwrites_in_place() {
        let home = TempHome::new().expect("temp home");
        let params = ShardParams::with_dimensions(DIMS);
        let shard = open(&home, params);
        shard.upsert(&[point(1, vec![1.0, 1.0, 1.0])]).expect("one");
        shard.upsert(&[point(1, vec![1.0, 1.0, 1.0])]).expect("two");
        assert_eq!(shard.point_count().expect("count"), 1);

        shard
            .upsert(&[point(1, vec![9.0, 0.0, 0.0])])
            .expect("overwrite");
        assert_eq!(shard.point_count().expect("count"), 1);
        let hits = shard
            .search(&DenseQuery {
                vector: vec![1.0, 0.0, 0.0],
                k: 1,
            })
            .expect("search");
        assert_eq!(hits[0].score, 9.0, "the vector was replaced, not appended");
    }

    #[test]
    fn delete_compacts_rows_and_is_idempotent() {
        let home = TempHome::new().expect("temp home");
        let params = ShardParams::with_dimensions(DIMS);
        let shard = open(&home, params);
        shard
            .upsert(&[
                point(1, vec![1.0, 0.0, 0.0]),
                point(2, vec![0.0, 1.0, 0.0]),
                point(3, vec![0.0, 0.0, 1.0]),
            ])
            .expect("upsert");

        shard.delete(&[id(2)]).expect("delete");
        shard.delete(&[id(2)]).expect("delete again is a no-op");
        shard.delete(&[id(200)]).expect("unknown id is a no-op");
        assert_eq!(shard.point_count().expect("count"), 2);

        // The surviving rows still line up with their own vectors after the
        // row-index rebuild.
        let hits = shard
            .search(&DenseQuery {
                vector: vec![0.0, 0.0, 1.0],
                k: 1,
            })
            .expect("search");
        assert_eq!(hits[0].point_id, id(3));
        assert_eq!(hits[0].score, 1.0);
    }

    #[test]
    fn a_dimension_mismatch_is_refused_before_anything_is_mutated() {
        let home = TempHome::new().expect("temp home");
        let params = ShardParams::with_dimensions(DIMS);
        let shard = open(&home, params);
        let err = shard
            .upsert(&[point(1, vec![1.0, 0.0, 0.0]), point(2, vec![1.0, 0.0])])
            .expect_err("batch must be refused");
        assert!(matches!(
            err,
            ProjectionError::DimensionMismatch {
                expected: 3,
                actual: 2
            }
        ));
        assert_eq!(
            shard.point_count().expect("count"),
            0,
            "the valid point of a rejected batch must not land either"
        );

        let err = shard
            .search(&DenseQuery {
                vector: vec![1.0, 0.0],
                k: 1,
            })
            .expect_err("query vector must match the shard");
        assert!(matches!(err, ProjectionError::DimensionMismatch { .. }));
    }

    /// The metric is honored per shard, not hard-coded: the same points and the
    /// same query rank differently under `dot` and `cosine`.
    #[test]
    fn the_shards_distance_metric_decides_the_ranking() {
        let home = TempHome::new().expect("temp home");
        let points = [
            point(1, vec![0.5, 0.0, 0.0]), // aligned, short
            point(2, vec![3.0, 3.0, 0.0]), // longer, skewed
        ];
        let query = DenseQuery {
            vector: vec![1.0, 0.0, 0.0],
            k: 10,
        };

        let dot_shard = open(&home, ShardParams::with_dimensions(DIMS));
        dot_shard.upsert(&points).expect("upsert");
        assert_eq!(
            dot_shard.search(&query).expect("dot search")[0].point_id,
            id(2)
        );

        // Same directory, same bytes — only the metric differs.
        let cosine_shard = BruteForceShard::open(
            &home.join("shard"),
            ShardParams {
                dimensions: DIMS,
                distance_metric: DistanceMetric::Cosine,
            },
        )
        .expect("reopen with cosine");
        assert_eq!(
            cosine_shard.search(&query).expect("cosine search")[0].point_id,
            id(1)
        );
    }

    #[test]
    fn head_is_absent_until_written_and_survives_reopen() {
        let home = TempHome::new().expect("temp home");
        let params = ShardParams::with_dimensions(DIMS);
        let shard = open(&home, params);
        assert_eq!(shard.read_head().expect("read head"), None);

        let head = ProjectionHead {
            worktree_id: "00000000-0000-7000-8000-000000000001".parse().expect("wt"),
            generation_id: "00000000-0000-7000-8000-000000000002".parse().expect("gen"),
            model_space_id: "00000000-0000-7000-8000-000000000003".parse().expect("ms"),
            projection_op_id: "00000000-0000-7000-8000-000000000004".parse().expect("op"),
            projection_schema_version: 1,
            point_count: 0,
            manifest_hash: Hash32::from_hex("ab".repeat(32)),
        };
        shard.write_head(&head).expect("write head");
        assert_eq!(open(&home, params).read_head().expect("reread"), Some(head));
    }

    #[test]
    fn a_truncated_points_file_is_corrupt_not_a_silently_smaller_shard() {
        let home = TempHome::new().expect("temp home");
        let params = ShardParams::with_dimensions(DIMS);
        open(&home, params)
            .upsert(&[point(1, vec![1.0, 0.0, 0.0]), point(2, vec![0.0, 1.0, 0.0])])
            .expect("upsert");

        let path = home.join("shard").join(POINTS_FILE);
        let bytes = fs::read(&path).expect("read");
        // Cut one whole record: the declared count still says 2.
        fs::write(&path, &bytes[..bytes.len() - (POINT_ID_HEX_LEN + DIMS * 4)]).expect("truncate");

        let err = BruteForceShard::open(&home.join("shard"), params).expect_err("must be corrupt");
        assert!(
            matches!(err, ProjectionError::Corrupt(ref why) if why.contains("length")),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn a_mid_record_truncation_is_corrupt_too() {
        let home = TempHome::new().expect("temp home");
        let params = ShardParams::with_dimensions(DIMS);
        open(&home, params)
            .upsert(&[point(1, vec![1.0, 0.0, 0.0])])
            .expect("upsert");

        let path = home.join("shard").join(POINTS_FILE);
        let bytes = fs::read(&path).expect("read");
        fs::write(&path, &bytes[..bytes.len() - 2]).expect("truncate");

        assert!(matches!(
            BruteForceShard::open(&home.join("shard"), params).expect_err("corrupt"),
            ProjectionError::Corrupt(_)
        ));
    }

    #[test]
    fn a_dimension_change_makes_an_existing_file_corrupt_rather_than_misread() {
        let home = TempHome::new().expect("temp home");
        open(&home, ShardParams::with_dimensions(DIMS))
            .upsert(&[point(1, vec![1.0, 0.0, 0.0])])
            .expect("upsert");

        let err = BruteForceShard::open(&home.join("shard"), ShardParams::with_dimensions(4))
            .expect_err("dims disagreement must be detected");
        assert!(
            matches!(err, ProjectionError::Corrupt(ref why) if why.contains("dims")),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn a_missing_points_file_is_an_empty_shard_not_an_error() {
        let home = TempHome::new().expect("temp home");
        let shard = open(&home, ShardParams::with_dimensions(DIMS));
        assert_eq!(shard.point_count().expect("count"), 0);
        assert!(
            shard
                .search(&DenseQuery {
                    vector: vec![1.0, 0.0, 0.0],
                    k: 5
                })
                .expect("search")
                .is_empty()
        );
    }

    #[test]
    fn optimize_is_a_noop_and_destroy_is_idempotent() {
        let home = TempHome::new().expect("temp home");
        let params = ShardParams::with_dimensions(DIMS);
        let shard = open(&home, params);
        shard
            .upsert(&[point(1, vec![1.0, 0.0, 0.0])])
            .expect("seed");
        shard.optimize().expect("optimize");
        assert_eq!(shard.point_count().expect("count"), 1);

        Box::new(shard).destroy().expect("destroy");
        assert!(!home.join("shard").exists());
        Box::new(open(&home, params))
            .destroy()
            .expect("destroy again");
    }
}
