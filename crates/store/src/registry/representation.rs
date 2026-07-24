//! The representation/model-space registry (spec 03 §2.2, machine in spec 04
//! §3) — group 11, T11-01.
//!
//! This module ships migration **version 6** ([`SCHEMA_V6`]): the
//! `representation` table (the canonical, six-field-unique `RepresentationKey`
//! spec 10 §2 names) and `model_space_representation` (the "normalized
//! membership" the group-11 charter names — normalizing the rev6 inline shape
//! into its own join table, per the DDL's own `[SPEC]` comment). `model_space`
//! itself and its seeded default row already exist ([`super::SCHEMA_V4`],
//! T07-02) — this module adds the missing **build-state machine**
//! ([`ModelSpaceState`], its
//! [`check_transition`](ModelSpaceState::check_transition), and
//! [`transition_model_space`]) over that table's `state` column, mirroring
//! [`GenerationState`](super::GenerationState) exactly (spec 04 §3's diagram is
//! the same shape as §1's Generation machine: `building → projection_ready →
//! active → retiring`, plus `building|projection_ready → failed`).
//!
//! ## Coverage (spec 10 §3)
//!
//! "Coverage = expected/ready set per **required** representation kind …
//! stored advisory JSON, always recomputable from `state.sqlite` ×
//! `embedding_cache`." [`Coverage`]/[`CoverageEntry`] are the data model and
//! on-disk (JSON) encoding; [`recompute_coverage`] is a **pure** function over
//! caller-supplied counts. Real coverage *computation* — walking actual
//! occurrences/memory entries and the not-yet-existing `embedding_cache` to
//! produce real expected/ready/failed numbers — is T11-04's "resumable
//! coverage backfill" card; this module only provides the shape and the
//! completeness gate ([`Coverage::fully_covered`], consulted by
//! [`transition_model_space`] before allowing `building → projection_ready`),
//! the same seam-before-real-data precedent
//! [`switch::VectorSource`](../../../projection/src/switch.rs) used ahead of
//! T11-02's real `embedding_cache`.
//!
//! ## Scope boundary (T11-01)
//!
//! This is a `local-rag-store`-only change. `RepresentationKind` here is a
//! **fresh, crate-local enum** — not reused from
//! `local_rag_projection::contract::RepresentationKind` — because
//! `local-rag-store` has no dependency on `local-rag-projection` (the
//! dependency runs the other way), and every other registry enum in this
//! crate is already crate-local with no cross-crate sharing
//! ([`GenerationState`](super::GenerationState),
//! [`WorktreeState`](super::WorktreeState),
//! [`ProjectionStatus`](super::ProjectionStatus)). `crates/projection`'s
//! `expected::REQUIRED_REPRESENTATION_KINDS` hardcoded pair is **not** wired to
//! this registry by this task — that needs a working multi-model-space switch
//! to actually exercise, which is T11-05's card ("production model-axis uses
//! standard projection switch").

use std::collections::BTreeMap;

use rusqlite::types::Type;
use rusqlite::{Connection, Error, OptionalExtension, Transaction, params};

/// A representation kind (spec 03 §2.2 `representation.kind` CHECK; spec 10
/// §2 lists the four kinds — `structural_description` is post-v0).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RepresentationKind {
    /// Raw code text.
    CodeRaw,
    /// Code text with surrounding context.
    CodeContext,
    /// A structural natural-language description (post-v0).
    StructuralDescription,
    /// A durable memory entry.
    Memory,
}

impl RepresentationKind {
    /// The stored `representation.kind` / `model_space_representation
    /// .representation_kind` value.
    pub fn as_str(self) -> &'static str {
        match self {
            RepresentationKind::CodeRaw => "code_raw",
            RepresentationKind::CodeContext => "code_context",
            RepresentationKind::StructuralDescription => "structural_description",
            RepresentationKind::Memory => "memory",
        }
    }

    /// Parse a stored kind value; `None` for anything the CHECK forbids.
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

/// A vector distance metric (spec 03 §2.2 `representation.distance_metric`
/// CHECK).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DistanceMetric {
    /// Cosine similarity.
    Cosine,
    /// Raw dot product.
    Dot,
    /// Euclidean (L2) distance.
    L2,
}

impl DistanceMetric {
    /// The stored `representation.distance_metric` value.
    pub fn as_str(self) -> &'static str {
        match self {
            DistanceMetric::Cosine => "cosine",
            DistanceMetric::Dot => "dot",
            DistanceMetric::L2 => "l2",
        }
    }

    /// Parse a stored value; `None` for anything the CHECK forbids.
    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "cosine" => Some(DistanceMetric::Cosine),
            "dot" => Some(DistanceMetric::Dot),
            "l2" => Some(DistanceMetric::L2),
            _ => None,
        }
    }
}

/// The canonical six-field `RepresentationKey` (spec 03 §2.2's `UNIQUE (kind,
/// representation_version, normalization_version, model_id, dimensions,
/// distance_metric)` — "duplicate registrations caused by serialization drift
/// are impossible by constraint", spec 10 §2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepresentationKey {
    /// The representation kind.
    pub kind: RepresentationKind,
    /// The representation format/algorithm version.
    pub representation_version: u32,
    /// The text/content normalization version applied before embedding.
    pub normalization_version: u32,
    /// The embedding model identifier (free-form, not a foreign key).
    pub model_id: String,
    /// The vector dimensionality.
    pub dimensions: u32,
    /// The distance metric this representation's vectors compare under.
    pub distance_metric: DistanceMetric,
}

/// Register `key`, returning its `representation_id`.
///
/// Idempotent and race-free in **one** atomic statement: `INSERT ... ON
/// CONFLICT (the six-field key) DO UPDATE SET representation_id =
/// representation.representation_id RETURNING representation_id`. The no-op
/// `DO UPDATE` (rather than `DO NOTHING`) is deliberate — SQLite's `RETURNING`
/// only fires for rows actually inserted or updated, never for a skipped
/// conflict, so a plain `DO NOTHING` would not hand back the pre-existing id.
/// This is the same `ON CONFLICT` idiom
/// [`observe_repository_path`](super::observe_repository_path) already uses
/// for its own idempotent upsert. A duplicate `key` therefore **converges** on
/// the first-registered id — `representation_id`/`now_ms` are discarded on
/// that path, never creating a second row.
pub fn register_representation(
    tx: &Transaction<'_>,
    representation_id: &str,
    key: &RepresentationKey,
    now_ms: i64,
) -> rusqlite::Result<String> {
    tx.query_row(
        "INSERT INTO representation \
           (representation_id, kind, representation_version, normalization_version, \
            model_id, dimensions, distance_metric, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
         ON CONFLICT (kind, representation_version, normalization_version, model_id, \
                       dimensions, distance_metric) \
         DO UPDATE SET representation_id = representation.representation_id \
         RETURNING representation_id",
        params![
            representation_id,
            key.kind.as_str(),
            key.representation_version,
            key.normalization_version,
            key.model_id,
            key.dimensions,
            key.distance_metric.as_str(),
            now_ms,
        ],
        |r| r.get(0),
    )
}

/// The `RepresentationKey` stored under `representation_id`, if any.
///
/// A stored `kind`/`distance_metric` outside its CHECK domain (corruption)
/// surfaces as [`rusqlite::Error::FromSqlConversionFailure`], never a silent
/// default.
pub fn representation_key(
    conn: &Connection,
    representation_id: &str,
) -> rusqlite::Result<Option<RepresentationKey>> {
    conn.query_row(
        "SELECT kind, representation_version, normalization_version, model_id, \
                dimensions, distance_metric \
         FROM representation WHERE representation_id = ?1",
        params![representation_id],
        |r| {
            let kind_raw: String = r.get(0)?;
            let kind = RepresentationKind::from_db(&kind_raw).ok_or_else(|| {
                Error::FromSqlConversionFailure(
                    0,
                    Type::Text,
                    format!("invalid representation.kind {kind_raw:?}").into(),
                )
            })?;
            let metric_raw: String = r.get(5)?;
            let distance_metric = DistanceMetric::from_db(&metric_raw).ok_or_else(|| {
                Error::FromSqlConversionFailure(
                    5,
                    Type::Text,
                    format!("invalid representation.distance_metric {metric_raw:?}").into(),
                )
            })?;
            Ok(RepresentationKey {
                kind,
                representation_version: r.get(1)?,
                normalization_version: r.get(2)?,
                model_id: r.get(3)?,
                dimensions: r.get(4)?,
                distance_metric,
            })
        },
    )
    .optional()
}

/// The build-lifecycle state of a model space (spec 03 §2.2 `model_space
/// .state`, machine in spec 04 §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelSpaceState {
    /// Representations registered; embedding backfill in progress (coverage
    /// advisory).
    Building,
    /// All required representation kinds have full coverage; benchmark may
    /// still be pending.
    ProjectionReady,
    /// Eligible to be a `target_model_space_id`; the default space MUST be
    /// `active`.
    Active,
    /// No longer selectable as target; still referenced by worktrees that
    /// have not reopened.
    Retiring,
    /// A build error terminated this model space.
    Failed,
}

impl ModelSpaceState {
    /// The stored `model_space.state` value.
    pub fn as_str(self) -> &'static str {
        match self {
            ModelSpaceState::Building => "building",
            ModelSpaceState::ProjectionReady => "projection_ready",
            ModelSpaceState::Active => "active",
            ModelSpaceState::Retiring => "retiring",
            ModelSpaceState::Failed => "failed",
        }
    }

    /// Parse a stored `model_space.state` value; `None` for anything the
    /// CHECK constraint forbids.
    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "building" => Some(ModelSpaceState::Building),
            "projection_ready" => Some(ModelSpaceState::ProjectionReady),
            "active" => Some(ModelSpaceState::Active),
            "retiring" => Some(ModelSpaceState::Retiring),
            "failed" => Some(ModelSpaceState::Failed),
            _ => None,
        }
    }

    /// Check whether `self → to` is a legal transition (spec 04 §3), returning
    /// a typed [`IllegalModelSpaceTransition`] otherwise. Pure — no I/O.
    ///
    /// Identical shape to
    /// [`GenerationState::check_transition`](super::GenerationState::check_transition):
    /// `building → projection_ready → active → retiring`, plus `building →
    /// failed` and `projection_ready → failed`. `retiring` and `failed` are
    /// terminal (no diagram edge leaves either in spec 04 §3). A
    /// self-transition (`X → X`) is an idempotent no-op and is legal (spec 04
    /// preamble: honor the request rather than coerce it).
    pub fn check_transition(self, to: ModelSpaceState) -> Result<(), IllegalModelSpaceTransition> {
        use ModelSpaceState::{Active, Building, Failed, ProjectionReady, Retiring};
        let legal = match (self, to) {
            (a, b) if a == b => true,
            (Building, ProjectionReady) => true,
            (Building, Failed) => true,
            (ProjectionReady, Active) => true,
            (ProjectionReady, Failed) => true,
            (Active, Retiring) => true,
            _ => false,
        };
        if legal {
            Ok(())
        } else {
            Err(IllegalModelSpaceTransition { from: self, to })
        }
    }
}

/// A rejected model-space state transition (spec 04 §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IllegalModelSpaceTransition {
    /// The current state.
    pub from: ModelSpaceState,
    /// The requested (illegal) target state.
    pub to: ModelSpaceState,
}

impl std::fmt::Display for IllegalModelSpaceTransition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "illegal model space transition {} → {}",
            self.from.as_str(),
            self.to.as_str()
        )
    }
}

impl std::error::Error for IllegalModelSpaceTransition {}

/// Why a [`transition_model_space`] request was rejected at the domain level
/// (as opposed to an infrastructure/SQLite failure, which surfaces as the
/// outer [`rusqlite::Error`] and rolls the transaction back).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelSpaceTransitionError {
    /// No `model_space` row has this id.
    UnknownModelSpace,
    /// The state machine (spec 04 §3) forbids the requested transition.
    Illegal(IllegalModelSpaceTransition),
    /// A required representation kind lacks full coverage; the transition to
    /// `projection_ready` requires `ready >= expected` for every required
    /// kind (spec 04 §3: "all required representation kinds have full
    /// coverage").
    IncompleteCoverage,
}

impl std::fmt::Display for ModelSpaceTransitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ModelSpaceTransitionError::UnknownModelSpace => write!(f, "unknown model space"),
            ModelSpaceTransitionError::Illegal(e) => write!(f, "{e}"),
            ModelSpaceTransitionError::IncompleteCoverage => {
                write!(f, "a required representation kind lacks full coverage")
            }
        }
    }
}

impl std::error::Error for ModelSpaceTransitionError {}

/// Create a new `model_space` row, born `building` (spec 03 §2.2, 04 §3).
///
/// `model_space_id` is caller-minted; a duplicate `display_name` surfaces as
/// the natural `UNIQUE` constraint error (no special handling — mirrors
/// [`create_repository`](super::create_repository)).
pub fn create_model_space(
    tx: &Transaction<'_>,
    model_space_id: &str,
    display_name: &str,
    now_ms: i64,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO model_space (model_space_id, display_name, state, created_at, updated_at) \
         VALUES (?1, ?2, 'building', ?3, ?3)",
        params![model_space_id, display_name, now_ms],
    )?;
    Ok(())
}

/// The model space's lifecycle state, if it exists (spec 03 §2.2).
///
/// A stored value outside the CHECK domain (corruption) surfaces as
/// [`rusqlite::Error::FromSqlConversionFailure`], never a silent default.
pub fn model_space_state(
    conn: &Connection,
    model_space_id: &str,
) -> rusqlite::Result<Option<ModelSpaceState>> {
    conn.query_row(
        "SELECT state FROM model_space WHERE model_space_id = ?1",
        params![model_space_id],
        |r| {
            let raw: String = r.get(0)?;
            ModelSpaceState::from_db(&raw).ok_or_else(|| {
                Error::FromSqlConversionFailure(
                    0,
                    Type::Text,
                    format!("invalid model_space.state {raw:?}").into(),
                )
            })
        },
    )
    .optional()
}

/// Whether a model space in `state` may be selected as a
/// `target_model_space_id` (spec 04 §3: "`active`: eligible to be a
/// `target_model_space_id`"). Pure.
///
/// Only `Active` is ever eligible — `retiring` explicitly is not ("no longer
/// selectable as target"), and `building`/`projection_ready`/`failed` cannot
/// be either, since `active` is reachable only through a `projection_ready`
/// that itself required full coverage ([`transition_model_space`]).
pub fn eligible_as_target(state: ModelSpaceState) -> bool {
    matches!(state, ModelSpaceState::Active)
}

/// Transition `model_space_id` to state `to`, enforcing the state machine
/// ([`ModelSpaceState::check_transition`]) and — only when `to` is
/// `ProjectionReady` — the coverage-completeness gate (spec 04 §3: "all
/// required representation kinds have full coverage").
///
/// The nested result mirrors [`transition_generation`](super::transition_generation):
///
/// - the outer [`rusqlite::Result`] is `Err` only on a SQLite failure (rolls
///   back, caller may retry);
/// - the inner `Result` is the domain outcome, and **no mutation** happens on
///   any inner `Err` (illegality/incompleteness is detected before any write):
///   - `Err(UnknownModelSpace)` — no such row;
///   - `Err(Illegal(..))` — a forbidden state transition;
///   - `Err(IncompleteCoverage)` — targeting `projection_ready` with an
///     under-covered required kind;
///   - `Ok(())` — the row is updated (or a legal no-op self-transition).
///
/// `required_kinds` is supplied by the caller (from
/// [`model_space_required_kinds`]) rather than re-queried here, so the
/// coverage check and the required-kind lookup stay independently testable.
/// A corrupt stored `state`/`coverage` surfaces as
/// [`rusqlite::Error::FromSqlConversionFailure`], never a silent default.
pub fn transition_model_space(
    tx: &Transaction<'_>,
    model_space_id: &str,
    to: ModelSpaceState,
    required_kinds: &[RepresentationKind],
    now_ms: i64,
) -> rusqlite::Result<Result<(), ModelSpaceTransitionError>> {
    let row: Option<(String, Option<String>)> = tx
        .query_row(
            "SELECT state, coverage FROM model_space WHERE model_space_id = ?1",
            params![model_space_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;

    let Some((raw_state, raw_coverage)) = row else {
        return Ok(Err(ModelSpaceTransitionError::UnknownModelSpace));
    };

    let from = ModelSpaceState::from_db(&raw_state).ok_or_else(|| {
        Error::FromSqlConversionFailure(
            0,
            Type::Text,
            format!("invalid model_space.state {raw_state:?}").into(),
        )
    })?;

    if let Err(illegal) = from.check_transition(to) {
        return Ok(Err(ModelSpaceTransitionError::Illegal(illegal)));
    }

    if to == ModelSpaceState::ProjectionReady {
        let coverage = match raw_coverage {
            Some(text) => Coverage::from_json(&text).map_err(|e| {
                Error::FromSqlConversionFailure(
                    1,
                    Type::Text,
                    format!("invalid model_space.coverage: {e}").into(),
                )
            })?,
            None => Coverage::default(),
        };
        if !coverage.fully_covered(required_kinds) {
            return Ok(Err(ModelSpaceTransitionError::IncompleteCoverage));
        }
    }

    if from != to {
        tx.execute(
            "UPDATE model_space SET state = ?2, updated_at = ?3 WHERE model_space_id = ?1",
            params![model_space_id, to.as_str(), now_ms],
        )?;
    }
    Ok(Ok(()))
}

/// Set (or replace) `model_space_id`'s membership for `kind` — the "normalized
/// membership" join row (spec 03 §2.2 `model_space_representation`,
/// `PRIMARY KEY (model_space_id, representation_kind)`: at most one
/// representation per kind per model space).
///
/// Idempotent upsert (`ON CONFLICT ... DO UPDATE`), matching
/// [`observe_repository_path`](super::observe_repository_path)'s idiom; bumps
/// `model_space.updated_at`. An unknown `model_space_id`/`representation_id`
/// is rejected by the row's foreign keys.
pub fn set_model_space_representation(
    tx: &Transaction<'_>,
    model_space_id: &str,
    kind: RepresentationKind,
    representation_id: &str,
    required: bool,
    now_ms: i64,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO model_space_representation \
           (model_space_id, representation_kind, representation_id, required) \
         VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT (model_space_id, representation_kind) \
         DO UPDATE SET representation_id = ?3, required = ?4",
        params![
            model_space_id,
            kind.as_str(),
            representation_id,
            i64::from(required)
        ],
    )?;
    tx.execute(
        "UPDATE model_space SET updated_at = ?2 WHERE model_space_id = ?1",
        params![model_space_id, now_ms],
    )?;
    Ok(())
}

/// The representation kinds `model_space_id` requires (`required = 1`),
/// ascending (spec 03 §2.2).
///
/// A stored `representation_kind` outside the CHECK domain (corruption)
/// surfaces as [`rusqlite::Error::FromSqlConversionFailure`], never a silent
/// default.
pub fn model_space_required_kinds(
    conn: &Connection,
    model_space_id: &str,
) -> rusqlite::Result<Vec<RepresentationKind>> {
    let mut stmt = conn.prepare(
        "SELECT representation_kind FROM model_space_representation \
         WHERE model_space_id = ?1 AND required = 1 \
         ORDER BY representation_kind",
    )?;
    let rows = stmt.query_map(params![model_space_id], |r| r.get::<_, String>(0))?;
    rows.map(|raw| {
        raw.and_then(|raw| {
            RepresentationKind::from_db(&raw).ok_or_else(|| {
                Error::FromSqlConversionFailure(
                    0,
                    Type::Text,
                    format!("invalid model_space_representation.representation_kind {raw:?}")
                        .into(),
                )
            })
        })
    })
    .collect()
}

/// One representation kind's expected/ready/failed subject counts (spec 10
/// §3's coverage shape).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct CoverageEntry {
    /// Subjects this kind is expected to cover.
    pub expected: u64,
    /// Subjects with a valid `embedding_cache` row.
    pub ready: u64,
    /// Subjects whose embedding attempt failed.
    pub failed: u64,
}

/// A model space's advisory coverage (spec 03 §2.2 `model_space.coverage`,
/// spec 10 §3: "expected/ready set per **required** representation kind").
///
/// Only required kinds are tracked ([`recompute_coverage`] never inserts a
/// non-required kind), matching the spec's own qualifier. Internally keyed by
/// the kind's stored string token (not [`RepresentationKind`] itself) so
/// (de)serialization needs no custom `serde` impl for the enum.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Coverage(BTreeMap<String, CoverageEntry>);

impl Coverage {
    /// `kind`'s coverage entry, if tracked.
    pub fn get(&self, kind: RepresentationKind) -> Option<CoverageEntry> {
        self.0.get(kind.as_str()).copied()
    }

    /// Whether every kind in `required` is tracked with `ready >= expected`
    /// (spec 04 §3's `projection_ready` precondition).
    pub fn fully_covered(&self, required: &[RepresentationKind]) -> bool {
        required
            .iter()
            .all(|kind| self.get(*kind).is_some_and(|e| e.ready >= e.expected))
    }

    /// Serialize to the `model_space.coverage` advisory JSON text.
    pub fn to_json(&self) -> String {
        serde_json::to_string(&self.0).expect("Coverage holds only plain owned data; infallible")
    }

    /// Parse a stored `model_space.coverage` value.
    pub fn from_json(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str::<BTreeMap<String, CoverageEntry>>(text).map(Coverage)
    }
}

/// Recompute `Coverage` from caller-supplied per-kind counts, one entry per
/// `required` kind (spec 10 §3: coverage is tracked **per required
/// representation kind** — a non-required kind never appears).
///
/// Pure — no I/O. A `required` kind absent from `counts` gets the zero
/// [`CoverageEntry`] (not tracked yet, i.e. `expected=ready=failed=0`), so an
/// omission never silently reads as "fully covered". Real per-subject counting
/// against `embedding_cache` is T11-04's backfill worker; this is the shape
/// and the seam it recomputes through.
pub fn recompute_coverage(
    required: &[RepresentationKind],
    counts: &BTreeMap<RepresentationKind, CoverageEntry>,
) -> Coverage {
    let mut map = BTreeMap::new();
    for kind in required {
        let entry = counts.get(kind).copied().unwrap_or_default();
        map.insert(kind.as_str().to_string(), entry);
    }
    Coverage(map)
}

/// Write `coverage` to `model_space_id`'s advisory `coverage` column, bumping
/// `updated_at`.
pub fn write_model_space_coverage(
    tx: &Transaction<'_>,
    model_space_id: &str,
    coverage: &Coverage,
    now_ms: i64,
) -> rusqlite::Result<()> {
    tx.execute(
        "UPDATE model_space SET coverage = ?2, updated_at = ?3 WHERE model_space_id = ?1",
        params![model_space_id, coverage.to_json(), now_ms],
    )?;
    Ok(())
}

/// Version-6 migration DDL: the representation registry (spec 03 §2.2).
///
/// Byte-exact reproduction of the two `state.sqlite` §2.2 blocks this task
/// owns — `representation` (the canonical six-field `RepresentationKey`) and
/// `model_space_representation` (normalized membership). `model_space` and
/// `worktree_projection_state` already exist ([`super::SCHEMA_V4`], T07-02).
/// Referenced by [`crate::migrate::ALL`] as migration version 6.
///
/// **Frozen once shipped.** Like every prior `SCHEMA_V*`, the checksum is the
/// SHA-256 of this text (see [`crate::migrate::Migration::checksum`]); any
/// edit — even whitespace or a comment — changes the checksum and trips
/// [`ChecksumDrift`](crate::migrate::MigrationError::ChecksumDrift) on an
/// existing store. Future schema changes are new numbered migrations.
pub(crate) const SCHEMA_V6: &str = "\
CREATE TABLE representation (
  representation_id       TEXT PRIMARY KEY,           -- UUIDv7
  kind                    TEXT NOT NULL CHECK
    (kind IN ('code_raw','code_context','structural_description','memory')),
  representation_version  INTEGER NOT NULL,
  normalization_version   INTEGER NOT NULL,
  model_id                TEXT NOT NULL,
  dimensions              INTEGER NOT NULL,
  distance_metric         TEXT NOT NULL CHECK (distance_metric IN ('cosine','dot','l2')),
  created_at              INTEGER NOT NULL,
  UNIQUE (kind, representation_version, normalization_version,
          model_id, dimensions, distance_metric)      -- canonical RepresentationKey
);

CREATE TABLE model_space_representation (             -- [SPEC] normalization of rev6 shape
  model_space_id       TEXT NOT NULL REFERENCES model_space(model_space_id),
  representation_kind  TEXT NOT NULL,
  representation_id    TEXT NOT NULL REFERENCES representation(representation_id),
  required             INTEGER NOT NULL DEFAULT 1 CHECK (required IN (0,1)),
  PRIMARY KEY (model_space_id, representation_kind)
);
";

#[cfg(test)]
mod tests {
    use super::*;

    fn key(model_id: &str, dimensions: u32) -> RepresentationKey {
        RepresentationKey {
            kind: RepresentationKind::CodeRaw,
            representation_version: 1,
            normalization_version: 1,
            model_id: model_id.to_string(),
            dimensions,
            distance_metric: DistanceMetric::Dot,
        }
    }

    #[test]
    fn representation_kind_round_trips() {
        for kind in [
            RepresentationKind::CodeRaw,
            RepresentationKind::CodeContext,
            RepresentationKind::StructuralDescription,
            RepresentationKind::Memory,
        ] {
            assert_eq!(RepresentationKind::from_db(kind.as_str()), Some(kind));
        }
        assert_eq!(RepresentationKind::from_db("bogus"), None);
    }

    #[test]
    fn distance_metric_round_trips() {
        for metric in [
            DistanceMetric::Cosine,
            DistanceMetric::Dot,
            DistanceMetric::L2,
        ] {
            assert_eq!(DistanceMetric::from_db(metric.as_str()), Some(metric));
        }
        assert_eq!(DistanceMetric::from_db("bogus"), None);
    }

    #[test]
    fn model_space_state_round_trips() {
        for state in [
            ModelSpaceState::Building,
            ModelSpaceState::ProjectionReady,
            ModelSpaceState::Active,
            ModelSpaceState::Retiring,
            ModelSpaceState::Failed,
        ] {
            assert_eq!(ModelSpaceState::from_db(state.as_str()), Some(state));
        }
        assert_eq!(ModelSpaceState::from_db("bogus"), None);
    }

    /// The transition matrix is byte-for-byte the same shape as
    /// `GenerationState`'s (spec 04 §3 mirrors §1).
    #[test]
    fn check_transition_covers_the_whole_matrix() {
        use ModelSpaceState::{Active, Building, Failed, ProjectionReady, Retiring};
        let all = [Building, ProjectionReady, Active, Retiring, Failed];

        let legal = [
            (Building, ProjectionReady),
            (Building, Failed),
            (ProjectionReady, Active),
            (ProjectionReady, Failed),
            (Active, Retiring),
        ];
        for (from, to) in legal {
            assert_eq!(from.check_transition(to), Ok(()), "{from:?} → {to:?} legal");
        }
        for s in all {
            assert_eq!(s.check_transition(s), Ok(()), "{s:?} → {s:?} idempotent");
        }
        for from in all {
            for to in all {
                if from == to || legal.contains(&(from, to)) {
                    continue;
                }
                assert_eq!(
                    from.check_transition(to),
                    Err(IllegalModelSpaceTransition { from, to }),
                    "{from:?} → {to:?} illegal",
                );
            }
        }
    }

    /// Only `Active` is ever eligible as a switch target — "retiring cannot
    /// become target" and every non-active state alike.
    #[test]
    fn eligible_as_target_is_active_only() {
        for state in [
            ModelSpaceState::Building,
            ModelSpaceState::ProjectionReady,
            ModelSpaceState::Retiring,
            ModelSpaceState::Failed,
        ] {
            assert!(!eligible_as_target(state), "{state:?} not eligible");
        }
        assert!(eligible_as_target(ModelSpaceState::Active));
    }

    #[test]
    fn coverage_per_required_kind() {
        let mut counts = BTreeMap::new();
        counts.insert(
            RepresentationKind::CodeRaw,
            CoverageEntry {
                expected: 10,
                ready: 7,
                failed: 1,
            },
        );
        counts.insert(
            RepresentationKind::Memory,
            CoverageEntry {
                expected: 4,
                ready: 4,
                failed: 0,
            },
        );
        // CodeContext is present in `counts` but NOT required: must not appear.
        counts.insert(
            RepresentationKind::CodeContext,
            CoverageEntry {
                expected: 99,
                ready: 99,
                failed: 0,
            },
        );

        let required = [RepresentationKind::CodeRaw, RepresentationKind::Memory];
        let coverage = recompute_coverage(&required, &counts);

        assert_eq!(
            coverage.get(RepresentationKind::CodeRaw),
            Some(CoverageEntry {
                expected: 10,
                ready: 7,
                failed: 1,
            })
        );
        assert_eq!(
            coverage.get(RepresentationKind::Memory),
            Some(CoverageEntry {
                expected: 4,
                ready: 4,
                failed: 0,
            })
        );
        assert_eq!(
            coverage.get(RepresentationKind::CodeContext),
            None,
            "non-required kind is never tracked, even if counts supplied it"
        );
        assert!(
            !coverage.fully_covered(&required),
            "code_raw is under-ready"
        );

        // A required kind entirely absent from `counts` still appears, as a
        // zero entry (never silently "fully covered").
        let required_all = [
            RepresentationKind::CodeRaw,
            RepresentationKind::Memory,
            RepresentationKind::StructuralDescription,
        ];
        let coverage_all = recompute_coverage(&required_all, &counts);
        assert_eq!(
            coverage_all.get(RepresentationKind::StructuralDescription),
            Some(CoverageEntry::default())
        );
        assert!(!coverage_all.fully_covered(&required_all));
    }

    #[test]
    fn coverage_json_round_trips() {
        let mut counts = BTreeMap::new();
        counts.insert(
            RepresentationKind::CodeRaw,
            CoverageEntry {
                expected: 5,
                ready: 5,
                failed: 0,
            },
        );
        let required = [RepresentationKind::CodeRaw];
        let coverage = recompute_coverage(&required, &counts);
        assert!(coverage.fully_covered(&required));

        let json = coverage.to_json();
        let parsed = Coverage::from_json(&json).expect("parse");
        assert_eq!(parsed, coverage);
    }

    #[test]
    fn representation_key_uniqueness_and_convergence() {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(SCHEMA_V6_TEST_TABLE)
            .expect("create representation table");

        let tx = conn.unchecked_transaction().expect("tx");
        let a =
            register_representation(&tx, "id-a", &key("model-a", 768), 1000).expect("register a");
        assert_eq!(a, "id-a");

        // A distinct six-field key creates a distinct row (six-field uniqueness).
        let b =
            register_representation(&tx, "id-b", &key("model-b", 768), 1000).expect("register b");
        assert_eq!(b, "id-b");
        assert_ne!(a, b);

        // The SAME six-field key, registered again under a different candidate
        // id, converges on the first-registered id — no second row.
        let a_again =
            register_representation(&tx, "id-a-duplicate-attempt", &key("model-a", 768), 2000)
                .expect("register a again");
        assert_eq!(a_again, "id-a", "duplicate serialization converges");

        let count: i64 = tx
            .query_row("SELECT COUNT(*) FROM representation", [], |r| r.get(0))
            .expect("count");
        assert_eq!(count, 2, "exactly two rows: a and b, no duplicate for a");
    }

    /// Minimal ad-hoc `representation` table for unit tests that only need
    /// `register_representation`'s SQL, not the full migrated schema (the
    /// production DDL lives in [`SCHEMA_V6`]; this mirrors it exactly for the
    /// columns exercised here).
    const SCHEMA_V6_TEST_TABLE: &str = "\
CREATE TABLE representation (
  representation_id TEXT PRIMARY KEY,
  kind TEXT NOT NULL,
  representation_version INTEGER NOT NULL,
  normalization_version INTEGER NOT NULL,
  model_id TEXT NOT NULL,
  dimensions INTEGER NOT NULL,
  distance_metric TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  UNIQUE (kind, representation_version, normalization_version, model_id, dimensions, distance_metric)
);
";

    /// A corrupt stored `model_space.state` must surface a typed conversion
    /// error, not a silent default.
    #[test]
    fn model_space_state_rejects_corrupt_enum() {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(
            "CREATE TABLE model_space (model_space_id TEXT, state TEXT);\n\
             INSERT INTO model_space VALUES ('m', 'zombie');",
        )
        .expect("seed corrupt row");

        let bad = model_space_state(&conn, "m");
        assert!(
            matches!(bad, Err(Error::FromSqlConversionFailure(0, Type::Text, _))),
            "corrupt state → typed conversion failure, got {bad:?}",
        );
        assert_eq!(model_space_state(&conn, "missing").expect("read"), None);
    }
}
