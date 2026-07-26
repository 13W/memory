//! Backend-neutral projection contract (spec 05 §1, `[FIXED abstraction,
//! signatures [SPEC]]`).
//!
//! A dense projection is **one shard per worktree** (spec 05 §2) holding the
//! active-only set of [`ProjectionPoint`]s for a `(generation, model_space)`
//! tuple. The store is always an *untrusted cache* (spec 05, principle): a
//! [`ProjectionHead`] written strictly last, plus validate-on-open (T07-04),
//! makes every divergence detectable — the backend owes only detectability, not
//! a durability barrier.
//!
//! The trait surface below is copied verbatim from spec 05 §1. The concrete
//! shapes of [`ProjectionPoint`], [`ShardParams`], [`DenseQuery`] and
//! [`ScoredPoint`] are `[SPEC]` (not fixed): the as-built decisions made in
//! T07-01 are documented on each type. The backend that implements this trait is
//! chosen at the T10/roadmap-step-11 comparative spike; until then the only
//! implementor is the persistent [`fake`](crate::fake) backend, which is a real
//! working backend for groups 08–09, not merely a test double.

use std::fmt;
use std::path::Path;

use local_rag_core::identity::Uuid;
pub use local_rag_store::DistanceMetric;

/// The projection-head schema version stamped into every [`ProjectionHead`]
/// (spec 05 §1 `projection_schema_version`). Bumping it invalidates persisted
/// heads and forces a rebuild; it is `[SPEC]` and starts at 1.
pub const PROJECTION_SCHEMA_VERSION: u32 = 1;

/// Convenience result alias for projection operations.
pub type Result<T, E = ProjectionError> = std::result::Result<T, E>;

/// A deterministic dense point identity: the 64-character lowercase-hex
/// `projection_point` digest (spec 05 §3, computed by
/// [`crate::identity::projection_point_id`]).
///
/// Ordering is bytewise over the hex string, which for lowercase hex is the
/// "sorted ascending bytewise" order the manifest hash requires (spec 03 §1.2).
/// A backend that needs a 64/128-bit numeric ID derives it from the first
/// 8/16 bytes of the digest (spec 05 §3 `[SPEC]`); the fake keeps the full hex.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PointId(String);

impl PointId {
    /// Wrap an already-computed hex digest (the output of
    /// [`crate::identity::projection_point_id`]).
    pub fn from_hex(hex: impl Into<String>) -> Self {
        Self(hex.into())
    }

    /// The point ID as its hex string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PointId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for PointId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PointId({})", self.0)
    }
}

/// A 32-byte manifest digest as its 64-character lowercase-hex string (spec 03
/// §1.2, stored as `TEXT` like every other domain hash in the codebase).
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Hash32(String);

impl Hash32 {
    /// Wrap an already-computed hex digest (the output of
    /// [`crate::identity::manifest_hash`]).
    pub fn from_hex(hex: impl Into<String>) -> Self {
        Self(hex.into())
    }

    /// The digest as its hex string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Hash32 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for Hash32 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash32({})", self.0)
    }
}

/// A representation kind that can be projected (spec 03 §2.2 `representation`
/// CHECK). Supplies the `representation_kind` field of the point-ID derivation
/// (spec 05 §3); `as_str` returns the exact stored token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RepresentationKind {
    /// Raw code text.
    CodeRaw,
    /// Code text with surrounding context.
    CodeContext,
    /// A structural natural-language description (post-v0; enabled only when
    /// descriptions are on).
    StructuralDescription,
    /// A durable memory entry.
    Memory,
}

impl RepresentationKind {
    /// The stored token (the `representation` CHECK value, spec 03 §2.2).
    pub fn as_str(self) -> &'static str {
        match self {
            RepresentationKind::CodeRaw => "code_raw",
            RepresentationKind::CodeContext => "code_context",
            RepresentationKind::StructuralDescription => "structural_description",
            RepresentationKind::Memory => "memory",
        }
    }

    /// Parse a stored token; `None` if it is not a CHECK-permitted value.
    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "code_raw" => Some(RepresentationKind::CodeRaw),
            "code_context" => Some(RepresentationKind::CodeContext),
            "structural_description" => Some(RepresentationKind::StructuralDescription),
            "memory" => Some(RepresentationKind::Memory),
            _ => None,
        }
    }
}

/// A single dense point: its deterministic [`PointId`] and the f32 vector that
/// backs it (spec 05 §1). `[SPEC]` as-built: T07-01 carries only the identity
/// and the vector; vector provenance is `embedding_cache` (spec 05 §5, wired in
/// later groups) and no path/generation/context field ever lives here.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectionPoint {
    /// The deterministic point identity.
    pub point_id: PointId,
    /// The dense vector, as f32 components.
    pub vector: Vec<f32>,
}

/// The commit marker of a projection op (spec 05 §1). It MUST be written by
/// [`ShardHandle::write_head`] strictly after every point mutation of the op;
/// its presence with a matching `projection_op_id` is the proof that all
/// preceding mutations landed. Field layout is copied verbatim from spec 05 §1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionHead {
    /// The worktree that owns the shard.
    pub worktree_id: Uuid,
    /// The projected generation.
    pub generation_id: Uuid,
    /// The projected model space.
    pub model_space_id: Uuid,
    /// The op that produced this head (matched against the write-ahead op id at
    /// validate-on-open; a mismatch ⇒ rebuild, spec 05 §6).
    pub projection_op_id: Uuid,
    /// The projection-head schema version ([`PROJECTION_SCHEMA_VERSION`]).
    pub projection_schema_version: u32,
    /// The number of points the manifest was computed over.
    pub point_count: u64,
    /// `H(projection_manifest, tuple ‖ sorted point ids)` (spec 03 §1.2, 05 §4).
    pub manifest_hash: Hash32,
}

/// A dense nearest-neighbour query (spec 05 §1). `[SPEC]` as-built: T07-01
/// carries the query vector and the number of results wanted; richer filters
/// arrive with the real backend at T10+.
#[derive(Debug, Clone, PartialEq)]
pub struct DenseQuery {
    /// The query vector.
    pub vector: Vec<f32>,
    /// The maximum number of results to return.
    pub k: usize,
}

/// A scored search hit (spec 05 §1). `[SPEC]` as-built: the point identity and
/// its similarity score.
#[derive(Debug, Clone, PartialEq)]
pub struct ScoredPoint {
    /// The matched point.
    pub point_id: PointId,
    /// The similarity score (higher is closer for the fake's dot-product).
    pub score: f32,
}

/// Parameters fixed for a shard's lifetime (spec 05 §1). `[SPEC]` as-built:
/// T07-01 fixed only the vector dimensionality; T12-02 added the distance
/// metric, exactly as that note anticipated ("distance metric and backend
/// tuning arrive with the real backend"). Both are properties of the model
/// space's `code_raw` representation and are resolved together by
/// [`params_for_model_space`](crate::model_switch::params_for_model_space).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShardParams {
    /// The number of f32 components every point vector must have.
    pub dimensions: usize,
    /// How [`ShardHandle::search`] scores a candidate against the query
    /// (spec 09 §3: "distance per `representation.distance_metric`").
    pub distance_metric: DistanceMetric,
}

impl ShardParams {
    /// Params for `dimensions` under [`DistanceMetric::Dot`].
    ///
    /// Dot product is what every caller used before T12-02 (the fake backend
    /// hard-coded it, and the spike's own as-built note pins it), so this
    /// constructor keeps those call sites — bootstrap fallbacks and tests that
    /// have no opinion on the metric — reading as they did. Production params
    /// come from the registry via
    /// [`params_for_model_space`](crate::model_switch::params_for_model_space),
    /// never from here.
    pub fn with_dimensions(dimensions: usize) -> Self {
        Self {
            dimensions,
            distance_metric: DistanceMetric::Dot,
        }
    }
}

/// The similarity of `point` to `query` under `metric`, in the **"higher is
/// closer"** convention [`ScoredPoint::score`] fixes for every backend.
///
/// Shared by every [`ProjectionStore`] implementation in this crate on purpose:
/// two backends that scored the same vectors differently would make a shard's
/// ranking depend on which one opened it, which is exactly the kind of
/// backend-visible behavior spec 05 §1's backend-neutral trait exists to
/// prevent.
///
/// - `dot` — the raw inner product.
/// - `cosine` — the inner product over the product of norms; a zero-norm vector
///   has no direction, so its similarity is `0.0` rather than `NaN`.
/// - `l2` — the **negated** Euclidean distance, so that nearer still sorts
///   first. Scores are therefore `<= 0` for this metric, which is expected: only
///   the ordering is meaningful across metrics, never the absolute value.
pub fn similarity(metric: DistanceMetric, query: &[f32], point: &[f32]) -> f32 {
    match metric {
        DistanceMetric::Dot => dot(query, point),
        DistanceMetric::Cosine => {
            let norms = norm(query) * norm(point);
            if norms == 0.0 {
                0.0
            } else {
                dot(query, point) / norms
            }
        }
        DistanceMetric::L2 => -query
            .iter()
            .zip(point)
            .map(|(a, b)| (a - b) * (a - b))
            .sum::<f32>()
            .sqrt(),
    }
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn norm(v: &[f32]) -> f32 {
    v.iter().map(|x| x * x).sum::<f32>().sqrt()
}

/// Order two scored candidates deterministically: score descending, ties broken
/// by point id ascending (spec 09 §4's tie-break convention, applied inside the
/// leg so a truncation at `k` is reproducible rather than storage-order
/// dependent).
pub fn rank_scored(scored: &mut [ScoredPoint]) {
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.point_id.cmp(&b.point_id))
    });
}

/// A projection backend failure.
#[derive(Debug)]
#[non_exhaustive]
pub enum ProjectionError {
    /// A filesystem operation on the shard directory failed.
    Io(std::io::Error),
    /// Persisted shard state could not be parsed — a corrupt or unopenable
    /// shard (spec 05 §10 F12). Validate-on-open (T07-04) turns this into a
    /// quarantine-then-rebuild; here it is only surfaced.
    Corrupt(String),
    /// A point's vector length did not match [`ShardParams::dimensions`].
    DimensionMismatch {
        /// The shard's configured dimensionality.
        expected: usize,
        /// The offending vector's length.
        actual: usize,
    },
    /// A backend operation failed. Also the channel through which the fake's
    /// named failpoints (spec 05 §10) inject a mid-op error.
    Backend(String),
}

impl fmt::Display for ProjectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProjectionError::Io(e) => write!(f, "projection shard filesystem error: {e}"),
            ProjectionError::Corrupt(why) => write!(f, "projection shard is corrupt: {why}"),
            ProjectionError::DimensionMismatch { expected, actual } => write!(
                f,
                "projection point vector has {actual} dimensions, shard expects {expected}"
            ),
            ProjectionError::Backend(why) => write!(f, "projection backend error: {why}"),
        }
    }
}

impl std::error::Error for ProjectionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ProjectionError::Io(e) => Some(e),
            _ => None,
        }
    }
}

/// A dense projection backend. One dense shard = one worktree; the backend is
/// chosen at roadmap step 11 (spec 05 §1). Verbatim signature from spec 05 §1.
pub trait ProjectionStore: Send + Sync {
    /// Open (or create) the shard directory. MUST be cheap enough to run
    /// validate-on-open on every open, and MUST never trust on-disk state
    /// (spec 05 §1 / §6).
    fn open(&self, dir: &Path, params: ShardParams) -> Result<Box<dyn ShardHandle>>;
}

/// A handle to one opened shard. Verbatim signatures from spec 05 §1.
pub trait ShardHandle: Send + Sync {
    /// The last-written head, or `None` if none has been written (a valid,
    /// detectable state — spec 05 §10 F7).
    fn read_head(&self) -> Result<Option<ProjectionHead>>;

    /// Iterate the point IDs (for manifest verification). Default is a full
    /// scan; sampling is allowed only for a backend that can prove an exact
    /// count plus a strong set digest (spec 05 §1).
    fn point_ids(&self) -> Result<Box<dyn Iterator<Item = PointId> + '_>>;

    /// The exact number of points in the shard.
    fn point_count(&self) -> Result<u64>;

    /// Insert or overwrite points. Idempotent by point id (spec 05 §1/§3).
    fn upsert(&self, points: &[ProjectionPoint]) -> Result<()>;

    /// Delete points by id. Idempotent — a missing id is a no-op (spec 05 §1/§3).
    fn delete(&self, ids: &[PointId]) -> Result<()>;

    /// Write the head. MUST be the LAST write of any delta or rebuild
    /// (spec 05 §1/§5).
    fn write_head(&self, head: &ProjectionHead) -> Result<()>;

    /// Dense nearest-neighbour search over the shard.
    fn search(&self, q: &DenseQuery) -> Result<Vec<ScoredPoint>>;

    /// Backend maintenance, triggered by metrics only — never after every
    /// reconcile (spec 05 §9).
    fn optimize(&self) -> Result<()>;

    /// Consume the handle and destroy the shard's on-disk state (spec 05 §7/§8).
    fn destroy(self: Box<Self>) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn representation_kind_round_trips_through_db_token() {
        for kind in [
            RepresentationKind::CodeRaw,
            RepresentationKind::CodeContext,
            RepresentationKind::StructuralDescription,
            RepresentationKind::Memory,
        ] {
            assert_eq!(RepresentationKind::from_db(kind.as_str()), Some(kind));
        }
        assert_eq!(RepresentationKind::from_db("not_a_kind"), None);
    }

    #[test]
    fn point_id_orders_bytewise() {
        let mut ids = [
            PointId::from_hex("ff"),
            PointId::from_hex("00"),
            PointId::from_hex("a0"),
        ];
        ids.sort();
        assert_eq!(
            ids.iter().map(PointId::as_str).collect::<Vec<_>>(),
            ["00", "a0", "ff"],
        );
    }

    #[test]
    fn dot_similarity_is_the_raw_inner_product() {
        assert_eq!(
            similarity(DistanceMetric::Dot, &[1.0, 2.0, 3.0], &[4.0, 5.0, 6.0]),
            32.0
        );
    }

    /// Mismatched lengths zip to the shorter side rather than panicking — the
    /// backends reject a dimension mismatch before scoring
    /// (`ProjectionError::DimensionMismatch`), so this is only a total-function
    /// guarantee, never a path real data reaches.
    #[test]
    fn similarity_zips_to_the_shorter_slice() {
        assert_eq!(
            similarity(DistanceMetric::Dot, &[1.0, 2.0, 3.0], &[1.0, 1.0]),
            3.0
        );
    }

    /// The metric is not decoration: the same pair of candidates ranks
    /// differently under `dot` and `cosine` when their magnitudes differ.
    #[test]
    fn cosine_ignores_magnitude_where_dot_does_not() {
        let query = [1.0, 0.0];
        let aligned_short = [0.5, 0.0];
        let skewed_long = [3.0, 3.0];

        assert!(
            similarity(DistanceMetric::Dot, &query, &skewed_long)
                > similarity(DistanceMetric::Dot, &query, &aligned_short),
            "dot rewards the longer vector"
        );
        assert!(
            similarity(DistanceMetric::Cosine, &query, &aligned_short)
                > similarity(DistanceMetric::Cosine, &query, &skewed_long),
            "cosine rewards the better-aligned one"
        );
        assert!(
            (similarity(DistanceMetric::Cosine, &query, &aligned_short) - 1.0).abs() < 1e-6,
            "a perfectly aligned vector has cosine 1.0 regardless of length"
        );
    }

    /// A zero vector has no direction — `0.0`, never `NaN` (which would poison
    /// the whole sort through `partial_cmp`).
    #[test]
    fn cosine_of_a_zero_vector_is_zero_not_nan() {
        let s = similarity(DistanceMetric::Cosine, &[0.0, 0.0], &[1.0, 1.0]);
        assert_eq!(s, 0.0);
        assert!(!s.is_nan());
    }

    /// L2 is negated so that "higher is closer" still holds.
    #[test]
    fn l2_similarity_is_negated_distance() {
        let exact = similarity(DistanceMetric::L2, &[1.0, 2.0], &[1.0, 2.0]);
        let near = similarity(DistanceMetric::L2, &[1.0, 2.0], &[1.0, 3.0]);
        let far = similarity(DistanceMetric::L2, &[1.0, 2.0], &[5.0, 9.0]);
        assert_eq!(exact, 0.0);
        assert_eq!(near, -1.0);
        assert!(exact > near && near > far);
    }

    #[test]
    fn ranking_is_score_desc_then_point_id_asc() {
        let mut scored = vec![
            ScoredPoint {
                point_id: PointId::from_hex("bb"),
                score: 1.0,
            },
            ScoredPoint {
                point_id: PointId::from_hex("aa"),
                score: 1.0,
            },
            ScoredPoint {
                point_id: PointId::from_hex("cc"),
                score: 2.0,
            },
        ];
        rank_scored(&mut scored);
        assert_eq!(
            scored
                .iter()
                .map(|s| s.point_id.as_str())
                .collect::<Vec<_>>(),
            ["cc", "aa", "bb"]
        );
    }

    #[test]
    fn with_dimensions_defaults_to_dot() {
        let params = ShardParams::with_dimensions(768);
        assert_eq!(params.dimensions, 768);
        assert_eq!(params.distance_metric, DistanceMetric::Dot);
    }

    #[test]
    fn error_display_and_source() {
        let io = ProjectionError::Io(std::io::Error::other("disk"));
        assert!(std::error::Error::source(&io).is_some());
        let dim = ProjectionError::DimensionMismatch {
            expected: 4,
            actual: 3,
        };
        assert!(std::error::Error::source(&dim).is_none());
        assert!(dim.to_string().contains("expects 4"));
    }
}
