//! A persistent, backend-neutral **fake** [`ProjectionStore`] (spec 05 §1).
//!
//! This is not merely a test double: it is the working dense backend for
//! groups 08–09, exercising the crash protocol before the real backend is
//! chosen at the T10/roadmap-step-11 spike (spec 05 §1, 15 roadmap). It stores
//! one shard per directory (`projection/<worktree_id>/`, spec 05 §2) in two
//! plain files written atomically (temp + `rename`), using only `std` — no
//! external dependency, so the pre-T10 dependency guardrail stays trivially
//! green:
//!
//! - `points` — one line per point, `<point_id_hex>\t<vector_le_f32_hex>`,
//!   sorted ascending bytewise by point id (so the file is a deterministic
//!   function of the point *set*, independent of upsert order);
//! - `head` — the [`ProjectionHead`] as `key=value` lines, written **only** by
//!   [`ShardHandle::write_head`], strictly after all point mutations of an op
//!   (spec 05 §1/§5). Absent until the first head is written.
//!
//! `open` loads whatever is on disk **without validating** it (validate-on-open
//! and rebuild are T07-04); malformed bytes surface as [`ProjectionError::Corrupt`],
//! which is the F12 "unopenable shard" signal (spec 05 §10).
//!
//! Under the `failpoints` feature the fake also exposes the fault-injection
//! controls the projection matrix needs (spec 05 §10, 14 §3): named
//! [`fail_point!`](local_rag_test_support::fail_point)s at the op-ordering
//! boundaries, an [`FakeShard::inspect`] view of loaded state, and
//! [`FakeShard::corrupt`] for out-of-band mutations a later open must detect.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use local_rag_core::identity::Uuid;

use crate::contract::{
    DenseQuery, Hash32, PointId, ProjectionError, ProjectionHead, ProjectionPoint, ProjectionStore,
    Result, ScoredPoint, ShardHandle, ShardParams, rank_scored, similarity,
};

const POINTS_FILE: &str = "points";
const HEAD_FILE: &str = "head";

/// A persistent fake [`ProjectionStore`]. Stateless; each [`ProjectionStore::open`]
/// yields an independent [`FakeShard`] over a shard directory.
#[derive(Debug, Default, Clone, Copy)]
pub struct FakeProjectionStore;

impl FakeProjectionStore {
    /// Create a fake store.
    pub fn new() -> Self {
        Self
    }
}

impl ProjectionStore for FakeProjectionStore {
    fn open(&self, dir: &Path, params: ShardParams) -> Result<Box<dyn ShardHandle>> {
        Ok(Box::new(FakeShard::open(dir, params)?))
    }
}

/// In-memory mirror of the persisted shard, guarded for `Send + Sync` (the
/// per-worktree writer already serializes real traffic; the mutex just makes
/// the fake sound to share).
#[derive(Debug)]
struct ShardState {
    points: BTreeMap<PointId, Vec<f32>>,
    head: Option<ProjectionHead>,
}

/// An opened fake shard.
#[derive(Debug)]
pub struct FakeShard {
    dir: PathBuf,
    params: ShardParams,
    state: Mutex<ShardState>,
}

impl FakeShard {
    /// Open (or create) a fake shard at `dir`, returning the concrete handle.
    ///
    /// Loads whatever is on disk **without validating** it (validate-on-open is
    /// T07-04): parse errors become [`ProjectionError::Corrupt`] (spec 05 §10
    /// F12), never a silent default.
    pub fn open(dir: &Path, params: ShardParams) -> Result<Self> {
        fs::create_dir_all(dir).map_err(ProjectionError::Io)?;
        let points = read_points(dir)?;
        let head = read_head(dir)?;
        Ok(Self {
            dir: dir.to_path_buf(),
            params,
            state: Mutex::new(ShardState { points, head }),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ShardState> {
        self.state.lock().expect("fake shard mutex poisoned")
    }
}

impl ShardHandle for FakeShard {
    fn read_head(&self) -> Result<Option<ProjectionHead>> {
        Ok(self.lock().head.clone())
    }

    fn point_ids(&self) -> Result<Box<dyn Iterator<Item = PointId> + '_>> {
        let ids: Vec<PointId> = self.lock().points.keys().cloned().collect();
        Ok(Box::new(ids.into_iter()))
    }

    fn point_count(&self) -> Result<u64> {
        Ok(self.lock().points.len() as u64)
    }

    fn upsert(&self, points: &[ProjectionPoint]) -> Result<()> {
        #[cfg(feature = "failpoints")]
        local_rag_test_support::fail_point!(
            "projection.fake.upsert",
            Err(ProjectionError::Backend("failpoint: upsert".to_string()))
        );

        for point in points {
            if point.vector.len() != self.params.dimensions {
                return Err(ProjectionError::DimensionMismatch {
                    expected: self.params.dimensions,
                    actual: point.vector.len(),
                });
            }
        }
        let mut state = self.lock();
        for point in points {
            // Idempotent by point id: repeated upsert overwrites (spec 05 §3).
            state
                .points
                .insert(point.point_id.clone(), point.vector.clone());
        }
        write_points(&self.dir, &state.points).map_err(ProjectionError::Io)
    }

    fn delete(&self, ids: &[PointId]) -> Result<()> {
        #[cfg(feature = "failpoints")]
        local_rag_test_support::fail_point!(
            "projection.fake.delete",
            Err(ProjectionError::Backend("failpoint: delete".to_string()))
        );

        let mut state = self.lock();
        for id in ids {
            // Idempotent: a missing id is a no-op (spec 05 §3).
            state.points.remove(id);
        }
        write_points(&self.dir, &state.points).map_err(ProjectionError::Io)
    }

    fn write_head(&self, head: &ProjectionHead) -> Result<()> {
        // The head is the LAST write of an op (spec 05 §1/§5). The seam fires
        // *before* the head lands, so an armed error/abort leaves the previously
        // persisted head (or none) on disk — the F3 detection signal.
        #[cfg(feature = "failpoints")]
        local_rag_test_support::fail_point!(
            "projection.fake.write_head",
            Err(ProjectionError::Backend(
                "failpoint: write_head".to_string()
            ))
        );

        write_head_file(&self.dir, head).map_err(ProjectionError::Io)?;
        self.lock().head = Some(head.clone());
        Ok(())
    }

    fn search(&self, q: &DenseQuery) -> Result<Vec<ScoredPoint>> {
        let state = self.lock();
        // Scored through the shared helper (T12-02) so this backend and the
        // production one can never rank the same shard differently.
        let mut scored: Vec<ScoredPoint> = state
            .points
            .iter()
            .map(|(id, vector)| ScoredPoint {
                point_id: id.clone(),
                score: similarity(self.params.distance_metric, &q.vector, vector),
            })
            .collect();
        rank_scored(&mut scored);
        scored.truncate(q.k);
        Ok(scored)
    }

    fn optimize(&self) -> Result<()> {
        // Metrics-driven for a real backend (spec 05 §9); the fake has nothing
        // to compact, so it is a no-op.
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

// ---- Persistence (pure std, atomic temp + rename) --------------------------

fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}

fn write_points(dir: &Path, points: &BTreeMap<PointId, Vec<f32>>) -> io::Result<()> {
    let mut buf = String::new();
    for (id, vector) in points {
        buf.push_str(id.as_str());
        buf.push('\t');
        buf.push_str(&vector_to_hex(vector));
        buf.push('\n');
    }
    atomic_write(&dir.join(POINTS_FILE), buf.as_bytes())
}

fn read_points(dir: &Path) -> Result<BTreeMap<PointId, Vec<f32>>> {
    let content = match fs::read_to_string(dir.join(POINTS_FILE)) {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(ProjectionError::Io(e)),
    };
    let mut points = BTreeMap::new();
    for (lineno, line) in content.lines().enumerate() {
        if line.is_empty() {
            continue;
        }
        let (id, vector_hex) = line.split_once('\t').ok_or_else(|| {
            ProjectionError::Corrupt(format!("points line {} has no tab separator", lineno + 1))
        })?;
        let vector = hex_to_vector(vector_hex).map_err(|why| {
            ProjectionError::Corrupt(format!("points line {}: {why}", lineno + 1))
        })?;
        points.insert(PointId::from_hex(id), vector);
    }
    Ok(points)
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

fn read_head(dir: &Path) -> Result<Option<ProjectionHead>> {
    let content = match fs::read_to_string(dir.join(HEAD_FILE)) {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(ProjectionError::Io(e)),
    };
    let mut fields: BTreeMap<&str, &str> = BTreeMap::new();
    for line in content.lines() {
        if line.is_empty() {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| ProjectionError::Corrupt(format!("head line has no '=': {line:?}")))?;
        fields.insert(key, value);
    }

    let uuid = |key: &str| -> Result<Uuid> {
        let raw = field(&fields, key)?;
        raw.parse::<Uuid>()
            .map_err(|e| ProjectionError::Corrupt(format!("head {key}: {e}")))
    };
    let head = ProjectionHead {
        worktree_id: uuid("worktree_id")?,
        generation_id: uuid("generation_id")?,
        model_space_id: uuid("model_space_id")?,
        projection_op_id: uuid("projection_op_id")?,
        projection_schema_version: field(&fields, "projection_schema_version")?
            .parse::<u32>()
            .map_err(|e| {
                ProjectionError::Corrupt(format!("head projection_schema_version: {e}"))
            })?,
        point_count: field(&fields, "point_count")?
            .parse::<u64>()
            .map_err(|e| ProjectionError::Corrupt(format!("head point_count: {e}")))?,
        manifest_hash: Hash32::from_hex(field(&fields, "manifest_hash")?),
    };
    Ok(Some(head))
}

fn field<'a>(fields: &BTreeMap<&str, &'a str>, key: &str) -> Result<&'a str> {
    fields
        .get(key)
        .copied()
        .ok_or_else(|| ProjectionError::Corrupt(format!("head is missing `{key}`")))
}

fn vector_to_hex(vector: &[f32]) -> String {
    let mut bytes = Vec::with_capacity(vector.len() * 4);
    for value in vector {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    hex_encode(&bytes)
}

fn hex_to_vector(hex: &str) -> std::result::Result<Vec<f32>, String> {
    let bytes = hex_decode(hex)?;
    if !bytes.len().is_multiple_of(4) {
        return Err("vector byte length is not a multiple of 4".to_string());
    }
    Ok(bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect())
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from_digit((b >> 4) as u32, 16).expect("nibble is < 16"));
        out.push(char::from_digit((b & 0x0f) as u32, 16).expect("nibble is < 16"));
    }
    out
}

fn hex_decode(hex: &str) -> std::result::Result<Vec<u8>, String> {
    if !hex.len().is_multiple_of(2) {
        return Err("odd-length hex".to_string());
    }
    hex.as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let hi = hex_val(pair[0]).ok_or_else(|| "non-hex digit".to_string())?;
            let lo = hex_val(pair[1]).ok_or_else(|| "non-hex digit".to_string())?;
            Ok((hi << 4) | lo)
        })
        .collect()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

// ---- Fault-injection controls (test builds only) ---------------------------

/// A read-only view of a fake shard's *loaded* state, for asserting what
/// survived a crash/corruption (spec 05 §10 detection tests). Available only
/// under the `failpoints` feature.
#[cfg(feature = "failpoints")]
#[derive(Debug, Clone, PartialEq)]
pub struct ShardInspection {
    /// Point IDs currently loaded, sorted ascending bytewise.
    pub point_ids: Vec<PointId>,
    /// The number of loaded points.
    pub point_count: u64,
    /// The loaded head, if any.
    pub head: Option<ProjectionHead>,
}

/// An out-of-band corruption to inject into a shard's persisted files
/// (spec 05 §10). Available only under the `failpoints` feature. Detection of
/// these is validate-on-open's job (T07-04); T07-01 only provides the control.
#[cfg(feature = "failpoints")]
#[derive(Debug, Clone)]
pub enum Corruption {
    /// Delete a point from the persisted set while leaving the head intact
    /// (F6: partial deletion with intact catalog).
    DropPoint(PointId),
    /// Replace one persisted point with a different one, keeping the count the
    /// same (F8: equal count, different ID set).
    SwapPoint {
        /// The point id to remove.
        remove: PointId,
        /// The point to insert in its place.
        insert: ProjectionPoint,
    },
    /// Remove the persisted head entirely (F7: missing head).
    RemoveHead,
    /// Overwrite the head file with arbitrary bytes (F12: unopenable head).
    OverwriteHead(Vec<u8>),
}

#[cfg(feature = "failpoints")]
impl FakeShard {
    /// Snapshot the loaded in-memory state (bypasses no validation because the
    /// fake performs none at open — that is T07-04).
    pub fn inspect(&self) -> ShardInspection {
        let state = self.lock();
        ShardInspection {
            point_ids: state.points.keys().cloned().collect(),
            point_count: state.points.len() as u64,
            head: state.head.clone(),
        }
    }

    /// Apply `corruption` to the shard's persisted files **without** touching
    /// in-memory state, modelling an out-of-band fault. Re-open the shard to
    /// observe it.
    pub fn corrupt(&self, corruption: Corruption) -> Result<()> {
        match corruption {
            Corruption::DropPoint(id) => {
                let mut points = read_points(&self.dir)?;
                points.remove(&id);
                write_points(&self.dir, &points).map_err(ProjectionError::Io)
            }
            Corruption::SwapPoint { remove, insert } => {
                let mut points = read_points(&self.dir)?;
                points.remove(&remove);
                points.insert(insert.point_id, insert.vector);
                write_points(&self.dir, &points).map_err(ProjectionError::Io)
            }
            Corruption::RemoveHead => match fs::remove_file(self.dir.join(HEAD_FILE)) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(ProjectionError::Io(e)),
            },
            Corruption::OverwriteHead(bytes) => {
                atomic_write(&self.dir.join(HEAD_FILE), &bytes).map_err(ProjectionError::Io)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips_vectors() {
        let vector = vec![0.0f32, 1.5, -2.25, f32::MIN_POSITIVE];
        let hex = vector_to_hex(&vector);
        assert_eq!(hex_to_vector(&hex).expect("decode"), vector);
    }

    #[test]
    fn hex_decode_rejects_malformed() {
        assert!(hex_to_vector("abc").is_err(), "odd length");
        assert!(hex_to_vector("zz").is_err(), "non-hex");
        assert!(hex_to_vector("0102").is_err(), "not a multiple of 4 bytes");
    }
}
