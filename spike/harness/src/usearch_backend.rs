//! The `usearch` `ProjectionStore` spike candidate (T10-03, spec 05 §1/§3).
//!
//! One of the three candidate backends named in spec 05 §1. Named
//! `usearch_backend` rather than `usearch` deliberately: `lib.rs` declares
//! candidate modules at the crate root (`pub mod usearch_backend;`), and a
//! root-level module literally named the same as an external crate shadows
//! that crate's name in every `use` path throughout this crate — naming it
//! distinctly sidesteps that footgun entirely.
//!
//! ## On-disk layout
//!
//! Three files per shard directory (spec 05 §2 explicitly allows
//! backend-defined, multi-file contents):
//!
//! - `usearch.index` — the native serialized HNSW graph + vectors
//!   (`usearch::Index::save`/`Index::restore`), self-describing (dimensions/
//!   metric/quantization/multi are read back from its own header by
//!   `restore`, so a reopen never needs to hand-carry [`IndexOptions`]).
//! - `ids.bin` — this adapter's own self-describing id↔key registry (`u32`
//!   format version + `u64` count header, then fixed 72-byte records: `u64`
//!   key LE ‖ 64 ASCII point-id bytes, sorted ascending bytewise by point id).
//!   This is the **sole source of truth** for [`ShardHandle::point_ids`]/
//!   [`ShardHandle::point_count`] — the `usearch` crate has no key-enumeration
//!   API at all (`neighbors`/`NeighborsCursor` both require already knowing a
//!   key), so a real dense-vector library, unlike the fake/brute-force
//!   candidates, cannot answer "what points do you hold" on its own.
//! - `head` — the [`ProjectionHead`] as independent `key=value` text (same
//!   idiom as [`crate::brute_force`], deliberately duplicated rather than
//!   shared — see that module's own doc comment for the "duplication over
//!   premature abstraction" reasoning; a third occurrence at a future
//!   candidate would be the point to extract a shared helper).
//!
//! Both `usearch.index` and `ids.bin` are written to a `.tmp` sibling then
//! `fs::rename`d into place. This gives `usearch.index` the same external
//! all-or-nothing property `ids.bin`/`points.bin` already have from the
//! `rename` itself, even though `usearch::Index::save` makes no documented
//! atomicity guarantee of its own: a crash mid-save leaves the *previous*
//! `usearch.index` on disk, untouched.
//!
//! ## ID-width mapping, as-built `[SPEC]` (T10-03, spec 05 §3)
//!
//! `usearch`'s native key is a 64-bit `u64` (confirmed directly in the
//! crate's `rust/lib.rs`: `pub type Key = u64;`) — exactly the case spec 05
//! §3 anticipates: "backends needing 64/128-bit IDs derive them from the
//! first 8/16 bytes of the digest." [`derive_key`] parses a [`PointId`]'s
//! first 16 hex characters (its first 8 raw digest bytes) as one big-endian
//! `u64`. A collision — two distinct point ids sharing a derived key — is
//! never silently merged: [`UsearchShard::upsert`] checks both the shard's
//! existing key map and the current batch, and rejects the whole call with
//! `ProjectionError::Backend` before mutating anything if one is found.
//!
//! ## Scoring convention, as-built `[SPEC]` (T10-03, spec 05 §1)
//!
//! The shard is built with `MetricKind::IP`, whose distance is `1 -
//! Σ(a[i]·b[i])` (usearch's own doc comment) — a strictly decreasing function
//! of the raw dot product. `usearch::Index::search`'s `Matches` are already
//! sorted ascending by distance (closest first); negating each distance
//! (`score = -distance`) yields the same descending-by-dot-product ranking
//! [`crate::oracle::exact_top_k`] and [`crate::brute_force`] already use —
//! "higher is closer" ([`ScoredPoint`]'s own doc), enabling a fair recall@k
//! comparison. The absolute score *value* need not match brute-force's raw
//! dot product magnitude: no test compares scores across backends, only
//! point-id sets/order within one backend's own results. `ScalarKind::F32` is
//! used (no lossy quantization), so any recall gap measured is from the
//! approximate graph search itself, not from quantization noise.
//!
//! `filtered_hnsw_available()` is the first candidate to honestly report
//! `true` (spec 05 §1/14 §7): `usearch::Index::filtered_search` is real,
//! predicate-during-traversal filtered-HNSW.

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
use usearch::{Index, IndexOptions, MetricKind, ScalarKind};

use crate::report::PlatformSupport;
use crate::{SpikeAdapter, current_target};

const INDEX_FILE: &str = "usearch.index";
const INDEX_TMP_FILE: &str = "usearch.index.tmp";
const IDS_FILE: &str = "ids.bin";
const IDS_TMP_FILE: &str = "ids.bin.tmp";
const HEAD_FILE: &str = "head";
const IDS_FORMAT_VERSION: u32 = 1;
/// See `crate::brute_force`'s identical constant: every point id in this
/// codebase is a BLAKE3 digest rendered as lowercase hex, always exactly 64
/// ASCII characters.
const POINT_ID_HEX_LEN: usize = 64;
/// `key(8) ‖ point_id(64)`.
const IDS_RECORD_LEN: usize = 8 + POINT_ID_HEX_LEN;
/// `version(u32) + count(u64)`.
const IDS_HEADER_LEN: usize = 4 + 8;

/// The `usearch` spike candidate, over the harness's [`SpikeAdapter`] seam.
#[derive(Debug, Default, Clone, Copy)]
pub struct UsearchAdapter;

impl SpikeAdapter for UsearchAdapter {
    fn name(&self) -> &str {
        "usearch"
    }

    fn platform_support(&self) -> PlatformSupport {
        // A build failure on an unsupported target can never be observed by
        // code that never got to run — "build/platform friction" for this
        // candidate lives in the manual win32 build-smoke evidence (PROGRESS.md),
        // never in this runtime self-report.
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
        Some(Box::new(UsearchStore))
    }
}

/// The `usearch` [`ProjectionStore`]. Stateless; each
/// [`ProjectionStore::open`] yields an independent [`UsearchShard`].
#[derive(Debug, Default, Clone, Copy)]
pub struct UsearchStore;

impl ProjectionStore for UsearchStore {
    fn open(&self, dir: &Path, params: ShardParams) -> Result<Box<dyn ShardHandle>> {
        Ok(Box::new(UsearchShard::open(dir, params)?))
    }
}

/// In-memory id↔key registry plus the last-written head, guarded for
/// `Send + Sync` (the underlying `usearch::Index` is `Send + Sync` by the
/// crate's own `unsafe impl`, so only this bookkeeping needs a lock).
struct UsearchState {
    id_to_key: HashMap<PointId, u64>,
    key_to_id: HashMap<u64, PointId>,
    head: Option<ProjectionHead>,
}

/// An opened `usearch` shard.
pub struct UsearchShard {
    dir: PathBuf,
    params: ShardParams,
    index: Index,
    state: Mutex<UsearchState>,
}

impl std::fmt::Debug for UsearchShard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `usearch::Index` does not implement `Debug`; print what we own.
        f.debug_struct("UsearchShard")
            .field("dir", &self.dir)
            .field("params", &self.params)
            .finish_non_exhaustive()
    }
}

impl UsearchShard {
    /// Open (or create) a `usearch` shard at `dir`.
    ///
    /// Neither artifact present → a fresh, empty index. Both present →
    /// `Index::restore` (self-describing header) plus `ids.bin`, then a
    /// cross-artifact consistency check (`ids.bin`'s declared count against
    /// `index.size()`) — spec 05's "no durability barrier, only
    /// detectability" principle applied to a genuinely two-artifact backend.
    /// Exactly one present is itself a detected divergence.
    pub fn open(dir: &Path, params: ShardParams) -> Result<Self> {
        fs::create_dir_all(dir).map_err(ProjectionError::Io)?;

        let index_path = dir.join(INDEX_FILE);
        let ids_path = dir.join(IDS_FILE);
        let index_exists = index_path.exists();
        let ids_exists = ids_path.exists();

        let (index, id_to_key, key_to_id) = match (index_exists, ids_exists) {
            (false, false) => {
                let options = IndexOptions {
                    dimensions: params.dimensions,
                    metric: MetricKind::IP,
                    quantization: ScalarKind::F32,
                    multi: false,
                    ..Default::default()
                };
                let index = Index::new(&options).map_err(|e| {
                    ProjectionError::Backend(format!("failed to create a usearch index: {e}"))
                })?;
                (index, HashMap::new(), HashMap::new())
            }
            (true, true) => {
                let index_path_str = path_to_str(&index_path)?;
                let index = Index::restore(index_path_str).map_err(|e| {
                    ProjectionError::Corrupt(format!("usearch index failed to load: {e}"))
                })?;
                let (id_to_key, key_to_id) = read_ids_bin(dir)?;
                if id_to_key.len() != index.size() {
                    return Err(ProjectionError::Corrupt(format!(
                        "usearch shard artifacts diverged: ids.bin declares {} points, \
                         usearch.index has {}",
                        id_to_key.len(),
                        index.size()
                    )));
                }
                (index, id_to_key, key_to_id)
            }
            (true, false) => {
                return Err(ProjectionError::Corrupt(
                    "usearch.index present without its companion ids.bin".to_string(),
                ));
            }
            (false, true) => {
                return Err(ProjectionError::Corrupt(
                    "ids.bin present without its companion usearch.index".to_string(),
                ));
            }
        };

        let head = read_head_file(dir)?;

        Ok(Self {
            dir: dir.to_path_buf(),
            params,
            index,
            state: Mutex::new(UsearchState {
                id_to_key,
                key_to_id,
                head,
            }),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, UsearchState> {
        self.state.lock().expect("usearch shard mutex poisoned")
    }

    /// Persist both mutable artifacts: the native index, then `ids.bin`.
    /// `write_head` (a separate, later call) remains the true last write of
    /// an op (spec 05 §1/§5); the ordering between these two internal
    /// artifacts is this adapter's own choice, not `[FIXED]` — a crash
    /// between them is a legitimate divergence the cross-artifact check on
    /// the next open turns into a detected `Corrupt`, either way round.
    fn persist(&self, state: &UsearchState) -> Result<()> {
        let tmp_index = self.dir.join(INDEX_TMP_FILE);
        let tmp_index_str = path_to_str(&tmp_index)?;
        self.index
            .save(tmp_index_str)
            .map_err(|e| ProjectionError::Backend(format!("usearch save failed: {e}")))?;
        fs::rename(&tmp_index, self.dir.join(INDEX_FILE)).map_err(ProjectionError::Io)?;
        write_ids_bin(&self.dir, &state.id_to_key)
    }
}

impl ShardHandle for UsearchShard {
    fn read_head(&self) -> Result<Option<ProjectionHead>> {
        Ok(self.lock().head.clone())
    }

    fn point_ids(&self) -> Result<Box<dyn Iterator<Item = PointId> + '_>> {
        let ids: Vec<PointId> = self.lock().id_to_key.keys().cloned().collect();
        Ok(Box::new(ids.into_iter()))
    }

    fn point_count(&self) -> Result<u64> {
        Ok(self.lock().id_to_key.len() as u64)
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

        // Pass 1 (no mutation): resolve a key for every point, detecting a
        // collision — against existing shard state OR within this very batch
        // — before any native call runs (mirrors brute_force's "validate
        // everything before mutating anything" discipline).
        let mut resolved: Vec<(&ProjectionPoint, u64, bool)> = Vec::with_capacity(points.len());
        let mut batch_keys: HashMap<u64, PointId> = HashMap::new();
        for point in points {
            if let Some(&existing_key) = state.id_to_key.get(&point.point_id) {
                resolved.push((point, existing_key, false));
                continue;
            }
            let key = derive_key(&point.point_id)?;
            if let Some(owner) = state.key_to_id.get(&key)
                && owner != &point.point_id
            {
                return Err(ProjectionError::Backend(format!(
                    "usearch key collision: derived key {key:#018x} already maps to point id \
                     {owner}, not {}",
                    point.point_id
                )));
            }
            if let Some(owner) = batch_keys.get(&key)
                && owner != &point.point_id
            {
                return Err(ProjectionError::Backend(format!(
                    "usearch key collision within one upsert batch: derived key {key:#018x} \
                     claimed by both {owner} and {}",
                    point.point_id
                )));
            }
            batch_keys.insert(key, point.point_id.clone());
            resolved.push((point, key, true));
        }

        // Pass 2 (mutate): reserve headroom once for however many are
        // genuinely new, then overwrite-in-place (remove+add) or insert.
        let new_count = resolved.iter().filter(|(_, _, is_new)| *is_new).count();
        if new_count > 0 {
            self.index
                .reserve(self.index.size() + new_count)
                .map_err(|e| ProjectionError::Backend(format!("usearch reserve failed: {e}")))?;
        }
        for (point, key, is_new) in &resolved {
            if !is_new {
                // Idempotent-by-id overwrite (spec 05 §3): force a true
                // replacement rather than relying on `add`'s unverified
                // behavior on an already-present key.
                self.index.remove(*key).map_err(|e| {
                    ProjectionError::Backend(format!("usearch remove (overwrite) failed: {e}"))
                })?;
            }
            self.index
                .add(*key, &point.vector)
                .map_err(|e| ProjectionError::Backend(format!("usearch add failed: {e}")))?;
            if *is_new {
                state.id_to_key.insert(point.point_id.clone(), *key);
                state.key_to_id.insert(*key, point.point_id.clone());
            }
        }

        self.persist(&state)
    }

    fn delete(&self, ids: &[PointId]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let mut state = self.lock();
        let mut any_removed = false;
        for id in ids {
            // Idempotent: a missing id is a no-op (spec 05 §3), decided at
            // our own map level rather than relying on usearch's `remove`
            // (also confirmed idempotent: `Ok(0)` on an absent key).
            if let Some(key) = state.id_to_key.remove(id) {
                state.key_to_id.remove(&key);
                self.index
                    .remove(key)
                    .map_err(|e| ProjectionError::Backend(format!("usearch remove failed: {e}")))?;
                any_removed = true;
            }
        }
        if !any_removed {
            return Ok(());
        }
        self.persist(&state)
    }

    fn write_head(&self, head: &ProjectionHead) -> Result<()> {
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
        let matches = self
            .index
            .search(&q.vector, q.k)
            .map_err(|e| ProjectionError::Backend(format!("usearch search failed: {e}")))?;

        let state = self.lock();
        let mut scored = Vec::with_capacity(matches.keys.len());
        for (key, distance) in matches.keys.iter().zip(matches.distances.iter()) {
            let point_id = state.key_to_id.get(key).ok_or_else(|| {
                ProjectionError::Corrupt(format!(
                    "usearch returned key {key:#018x} with no known point id mapping \
                     (index/ids.bin diverged)"
                ))
            })?;
            // MetricKind::IP's distance is `1 - dot(a,b)`, strictly
            // decreasing in the dot product; negating restores "higher is
            // closer" (ScoredPoint's convention, same as the oracle/
            // brute-force). Matches are already ascending by distance
            // (closest first) = descending by this score — no re-sort.
            scored.push(ScoredPoint {
                point_id: point_id.clone(),
                score: -distance,
            });
        }
        Ok(scored)
    }

    fn optimize(&self) -> Result<()> {
        // A real, metrics-driven-in-principle maintenance op (spec 05 §9):
        // "removes links to deleted entries and rebuilds the internal vector
        // storage layout" (usearch's own doc comment). Available, but
        // nothing in this spike harness calls `optimize()` yet — no driver
        // exists pre-group-12/15, same disposition as fake/brute-force, the
        // difference being this candidate actually has real work to do here.
        self.index
            .compact()
            .map_err(|e| ProjectionError::Backend(format!("usearch compact failed: {e}")))
    }

    fn destroy(self: Box<Self>) -> Result<()> {
        match fs::remove_dir_all(&self.dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(ProjectionError::Io(e)),
        }
    }
}

/// Derive a `usearch::Key` (`u64`) from a [`PointId`]'s first 8 raw digest
/// bytes, big-endian (spec 05 §3 `[SPEC]`) — parses the id's first 16 hex
/// characters as one big-endian `u64`. Collision handling across distinct
/// point ids is the caller's job ([`UsearchShard::upsert`]); this is a pure
/// mapping, infallible on any well-formed (64-hex-char) point id.
fn derive_key(point_id: &PointId) -> Result<u64> {
    let hex = point_id.as_str();
    let prefix = hex.get(0..16).ok_or_else(|| {
        ProjectionError::Backend(format!(
            "point id {hex:?} is shorter than the 16 hex characters usearch's key derivation needs"
        ))
    })?;
    u64::from_str_radix(prefix, 16).map_err(|e| {
        ProjectionError::Backend(format!("point id {hex:?} prefix is not valid hex: {e}"))
    })
}

fn path_to_str(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| ProjectionError::Backend("shard path is not valid UTF-8".to_string()))
}

// ---- Persistence: `ids.bin` (streamed binary, fixed-size records) ---------

fn write_ids_bin(dir: &Path, id_to_key: &HashMap<PointId, u64>) -> Result<()> {
    let mut entries: Vec<(&PointId, u64)> = id_to_key.iter().map(|(id, &key)| (id, key)).collect();
    for (id, _) in &entries {
        if id.as_str().len() != POINT_ID_HEX_LEN {
            return Err(ProjectionError::Backend(format!(
                "usearch point id must be exactly {POINT_ID_HEX_LEN} hex characters, got {} \
                 ({id:?})",
                id.as_str().len()
            )));
        }
    }
    entries.sort_by(|a, b| a.0.cmp(b.0));

    let tmp = dir.join(IDS_TMP_FILE);
    write_ids_bin_stream(&tmp, &entries).map_err(ProjectionError::Io)?;
    fs::rename(&tmp, dir.join(IDS_FILE)).map_err(ProjectionError::Io)
}

fn write_ids_bin_stream(tmp: &Path, entries: &[(&PointId, u64)]) -> io::Result<()> {
    let file = fs::File::create(tmp)?;
    let mut w = BufWriter::new(file);
    w.write_all(&IDS_FORMAT_VERSION.to_le_bytes())?;
    w.write_all(&(entries.len() as u64).to_le_bytes())?;
    for (id, key) in entries {
        w.write_all(&key.to_le_bytes())?;
        w.write_all(id.as_str().as_bytes())?;
    }
    w.flush()
}

/// Read `dir`/`ids.bin` back into the id↔key maps. A missing file is a clean
/// empty shard; anything else that fails this exact self-describing format
/// is [`ProjectionError::Corrupt`] — never a silent partial load.
fn read_ids_bin(dir: &Path) -> Result<(HashMap<PointId, u64>, HashMap<u64, PointId>)> {
    let path = dir.join(IDS_FILE);
    let file = match fs::File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Ok((HashMap::new(), HashMap::new()));
        }
        Err(e) => return Err(ProjectionError::Io(e)),
    };
    let file_len = file.metadata().map_err(ProjectionError::Io)?.len();
    let mut r = BufReader::new(file);

    let mut header = [0u8; IDS_HEADER_LEN];
    r.read_exact(&mut header)
        .map_err(|e| read_err(e, "ids.bin header"))?;
    let version = u32::from_le_bytes(header[0..4].try_into().expect("4 bytes"));
    let count = u64::from_le_bytes(header[4..12].try_into().expect("8 bytes"));

    if version != IDS_FORMAT_VERSION {
        return Err(ProjectionError::Corrupt(format!(
            "ids.bin format version {version} != expected {IDS_FORMAT_VERSION}"
        )));
    }

    let record_size = IDS_RECORD_LEN as u64;
    let expected_body = count.checked_mul(record_size).ok_or_else(|| {
        ProjectionError::Corrupt("ids.bin declared count overflows record size".to_string())
    })?;
    let expected_total = (IDS_HEADER_LEN as u64)
        .checked_add(expected_body)
        .ok_or_else(|| ProjectionError::Corrupt("ids.bin declared length overflows".to_string()))?;
    if expected_total != file_len {
        return Err(ProjectionError::Corrupt(format!(
            "ids.bin declared length {expected_total} != actual file length {file_len}"
        )));
    }

    let mut id_to_key = HashMap::with_capacity(count as usize);
    let mut key_to_id = HashMap::with_capacity(count as usize);
    let mut key_buf = [0u8; 8];
    let mut id_buf = vec![0u8; POINT_ID_HEX_LEN];
    for _ in 0..count {
        r.read_exact(&mut key_buf)
            .map_err(|e| read_err(e, "an ids.bin key"))?;
        let key = u64::from_le_bytes(key_buf);
        r.read_exact(&mut id_buf)
            .map_err(|e| read_err(e, "an ids.bin point id"))?;
        if !id_buf.iter().all(u8::is_ascii_hexdigit) {
            return Err(ProjectionError::Corrupt(
                "ids.bin contains a non-hex point id".to_string(),
            ));
        }
        let id_str = std::str::from_utf8(&id_buf)
            .map_err(|_| ProjectionError::Corrupt("ids.bin point id is not UTF-8".to_string()))?
            .to_string();
        let id = PointId::from_hex(id_str);
        id_to_key.insert(id.clone(), key);
        key_to_id.insert(key, id);
    }
    Ok((id_to_key, key_to_id))
}

/// An unexpected EOF mid-read is a detected format divergence (`Corrupt`),
/// not a bare I/O error — the file was shorter than its own declared shape.
fn read_err(e: io::Error, what: &str) -> ProjectionError {
    if e.kind() == io::ErrorKind::UnexpectedEof {
        ProjectionError::Corrupt(format!("ids.bin truncated while reading {what}"))
    } else {
        ProjectionError::Io(e)
    }
}

// ---- Persistence: `head` (plain key=value text, atomic temp+rename) -------
//
// Deliberately duplicated from `crate::brute_force`'s identical-shaped
// functions rather than shared — see that module's doc comment.

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
                "local-rag-spike-usearch-test-{}-{n}",
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

    /// A syntactically valid 64-hex-char point id built from a distinguishing
    /// `tag` byte repeated in the tail, so two ids can share a chosen prefix
    /// while remaining distinct overall (for the collision test).
    fn id_with_prefix(prefix_hex16: &str, tag: u8) -> PointId {
        assert_eq!(prefix_hex16.len(), 16, "prefix must be 16 hex chars");
        let tail = format!("{tag:02x}").repeat(24);
        PointId::from_hex(format!("{prefix_hex16}{tail}"))
    }

    fn id(n: u8) -> PointId {
        id_with_prefix(&format!("{n:016x}"), n)
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
        let params = ShardParams { dimensions: 4 };
        let store = UsearchStore;

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
        let params = ShardParams { dimensions: 2 };
        let store = UsearchStore;
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
    fn key_collision_between_distinct_point_ids_is_reported_as_backend_error() {
        let scratch = Scratch::new();
        let params = ShardParams { dimensions: 2 };
        let store = UsearchStore;
        let shard = store.open(&scratch.path, params).expect("open");

        let prefix = "0123456789abcdef";
        let a = id_with_prefix(prefix, 0xaa);
        let b = id_with_prefix(prefix, 0xbb);
        assert_ne!(
            a, b,
            "test fixture must produce distinct ids sharing a key prefix"
        );

        shard
            .upsert(&[ProjectionPoint {
                point_id: a,
                vector: vec![1.0, 0.0],
            }])
            .expect("first point establishes the key");

        let err = shard
            .upsert(&[ProjectionPoint {
                point_id: b,
                vector: vec![0.0, 1.0],
            }])
            .expect_err("a different point id sharing the derived key must be rejected");
        assert!(matches!(err, ProjectionError::Backend(_)), "got {err:?}");
        assert_eq!(
            shard.point_count().expect("count"),
            1,
            "the colliding point must not apply"
        );
    }

    #[test]
    fn delete_is_idempotent_and_survives_reopen() {
        let scratch = Scratch::new();
        let params = ShardParams { dimensions: 4 };
        let store = UsearchStore;
        let shard = store.open(&scratch.path, params).expect("open");
        shard.upsert(&[point(1, 4), point(2, 4)]).expect("upsert");

        shard.delete(&[id(1)]).expect("delete");
        shard.delete(&[id(1)]).expect("delete again (idempotent)");
        assert_eq!(shard.point_count().expect("count"), 1);

        let reopened = store.open(&scratch.path, params).expect("reopen");
        let ids: Vec<PointId> = reopened.point_ids().expect("ids").collect();
        assert_eq!(ids, vec![id(2)]);
    }

    #[test]
    fn upsert_rejects_wrong_dimension() {
        let scratch = Scratch::new();
        let params = ShardParams { dimensions: 4 };
        let store = UsearchStore;
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
        let params = ShardParams { dimensions: 4 };
        let store = UsearchStore;
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
    fn missing_companion_artifact_is_reported_corrupt() {
        let scratch = Scratch::new();
        let params = ShardParams { dimensions: 4 };
        let store = UsearchStore;
        {
            let shard = store.open(&scratch.path, params).expect("open");
            shard.upsert(&[point(1, 4)]).expect("upsert");
        }

        fs::remove_file(scratch.path.join(IDS_FILE)).expect("remove ids.bin");
        match store.open(&scratch.path, params) {
            Err(err) => assert!(matches!(err, ProjectionError::Corrupt(_)), "got {err:?}"),
            Ok(_) => panic!("usearch.index without ids.bin must be detected"),
        }

        // Rebuild, then remove the index side instead.
        let scratch2 = Scratch::new();
        {
            let shard = store.open(&scratch2.path, params).expect("open");
            shard.upsert(&[point(1, 4)]).expect("upsert");
        }
        fs::remove_file(scratch2.path.join(INDEX_FILE)).expect("remove usearch.index");
        match store.open(&scratch2.path, params) {
            Err(err) => assert!(matches!(err, ProjectionError::Corrupt(_)), "got {err:?}"),
            Ok(_) => panic!("ids.bin without usearch.index must be detected"),
        }
    }

    #[test]
    fn ids_bin_and_index_count_mismatch_is_reported_corrupt() {
        let scratch = Scratch::new();
        let params = ShardParams { dimensions: 4 };
        let store = UsearchStore;
        {
            let shard = store.open(&scratch.path, params).expect("open");
            shard.upsert(&[point(1, 4), point(2, 4)]).expect("upsert");
        }

        // Hand-write ids.bin declaring only 1 point while usearch.index still
        // has 2 — a divergence no single artifact's own self-check can catch.
        let mut only_one = HashMap::new();
        only_one.insert(id(1), derive_key(&id(1)).expect("derive"));
        write_ids_bin(&scratch.path, &only_one).expect("rewrite ids.bin");

        match store.open(&scratch.path, params) {
            Err(err) => assert!(matches!(err, ProjectionError::Corrupt(_)), "got {err:?}"),
            Ok(_) => panic!("count divergence must be detected"),
        }
    }

    #[test]
    fn truncated_index_file_is_reported_corrupt() {
        let scratch = Scratch::new();
        let params = ShardParams { dimensions: 4 };
        let store = UsearchStore;
        {
            let shard = store.open(&scratch.path, params).expect("open");
            shard
                .upsert(&[point(1, 4), point(2, 4), point(3, 4)])
                .expect("upsert");
        }

        let index_path = scratch.path.join(INDEX_FILE);
        let full_len = fs::metadata(&index_path).expect("metadata").len();
        let file = fs::OpenOptions::new()
            .write(true)
            .open(&index_path)
            .expect("open for truncation");
        file.set_len(full_len / 2).expect("truncate");

        match store.open(&scratch.path, params) {
            Err(err) => assert!(matches!(err, ProjectionError::Corrupt(_)), "got {err:?}"),
            Ok(_) => panic!("truncated usearch.index must be detected, not silently loaded"),
        }
    }

    #[test]
    fn optimize_returns_ok() {
        let scratch = Scratch::new();
        let params = ShardParams { dimensions: 4 };
        let store = UsearchStore;
        let shard = store.open(&scratch.path, params).expect("open");
        shard.upsert(&[point(1, 4)]).expect("upsert");
        shard.optimize().expect("optimize");
    }
}
