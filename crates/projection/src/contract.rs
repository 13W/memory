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
/// T07-01 fixes only the vector dimensionality, which the fake enforces on
/// upsert; distance metric and backend tuning arrive with the real backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShardParams {
    /// The number of f32 components every point vector must have.
    pub dimensions: usize,
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
