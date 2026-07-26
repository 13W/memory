//! The two-axis projection deployment state (spec 03 §2.2, machine in spec 04 §2)
//! — group 07, T07-02.
//!
//! This module ships migration **version 4** ([`SCHEMA_V4`]) and the guard layer
//! over `worktree_projection_state`. The table records, per worktree, three
//! `(generation, model_space)` tuples — **active** (what search serves),
//! **projected** (what the shard's `ProjectionHead` reflects), and **target**
//! (the in-flight destination of a switch) — plus the `status` machine
//! `clean → updating → clean` with the `dirty → rebuilding` recovery arm
//! (spec 04 §2).
//!
//! The projection is always an *untrusted cache* (spec 05): the two-axis
//! invariants are **not** DDL constraints but **procedural precondition checks**
//! in this layer (spec 04 preamble: "illegal transitions MUST fail the
//! transaction, never silently coerce"). [`check_transition`] guards the status
//! FSM; [`check_invariants`] guards the row shape (the truth table); both are
//! pure, and [`write_projection_state`] applies them read-then-write in one
//! transaction with **no mutation on rejection**, mirroring the generation
//! machine ([`transition_generation`](super::transition_generation)).
//!
//! ## Scope (T07-02)
//!
//! `SCHEMA_V4` creates `model_space` (the FK target for the model axis) and
//! `worktree_projection_state`, and seeds one default `active` model space
//! (`store_settings.default_model_space_id`, which spec 04 §3 requires be
//! `active`). The full representation registry — the `representation` and
//! `model_space_representation` tables, the canonical six-field RepresentationKey,
//! the coverage data model, and the `model_space` build-state machine — is
//! [`super::representation`] (T11-01, "Representation/model-space registry",
//! version-6 `SCHEMA_V6`). The switch orchestration
//! that sits *between* a write-ahead and a commit (desired-set reconcile against
//! the fake backend) is **T07-03**, composed in `local_rag_projection::switch`
//! over this module's guarded primitives; validate-on-open, `mark_dirty`, and the
//! rebuild transitions are **T07-04**.

use rusqlite::types::Type;
use rusqlite::{Connection, Error, OptionalExtension, Transaction, params};

/// The fixed identity of the default model space seeded by [`SCHEMA_V4`].
///
/// A deterministic, UUIDv7-shaped sentinel (not a random id) so the migration is
/// reproducible and checksum-stable — a migration has no clock or entropy source.
pub const DEFAULT_MODEL_SPACE_ID: &str = "00000000-0000-7000-8000-000000000001";

/// The default model space's `display_name`, matching
/// `local_rag_core::config::ModelsConfig::default_model_space` (`"default"`), the
/// name the config resolves against this registry.
pub const DEFAULT_MODEL_SPACE_NAME: &str = "default";

/// The `projection_schema_version` stamped into rows this build initializes.
///
/// Mirrors `local_rag_projection::PROJECTION_SCHEMA_VERSION` (the two crates are
/// not linked; keep them in sync — validate-on-open (T07-04) compares the stored
/// version against the shard's `ProjectionHead.projection_schema_version`).
pub const PROJECTION_SCHEMA_VERSION: i64 = 1;

/// Version-4 migration DDL: the projection deployment state and the minimal model
/// registry seed (spec 03 §2.2).
///
/// Creates `model_space` and `worktree_projection_state` (byte-exact reproduction
/// of the §2.2 blocks this task owns), then seeds the default `active` model space
/// and records it under `store_settings.default_model_space_id`. `store_settings`
/// already exists (migration bootstrap). Referenced by [`crate::migrate::ALL`] as
/// migration version 4.
///
/// The seed uses the fixed [`DEFAULT_MODEL_SPACE_ID`] and `created_at`/`updated_at`
/// of `0` (an epoch sentinel for the bootstrap singleton) so the SQL — and thus
/// its checksum — is deterministic.
///
/// **Frozen once shipped.** The migration checksum is the SHA-256 of this text
/// (see [`crate::migrate::Migration::checksum`]); any edit — even whitespace or a
/// comment — trips [`ChecksumDrift`](crate::migrate::MigrationError::ChecksumDrift)
/// on an existing store. Future schema changes are new numbered migrations
/// (the representation registry lands as its own migration in T11-01).
pub(crate) const SCHEMA_V4: &str = "\
CREATE TABLE model_space (
  model_space_id  TEXT PRIMARY KEY,                   -- UUIDv7
  display_name    TEXT NOT NULL UNIQUE,
  state           TEXT NOT NULL CHECK
    (state IN ('building','projection_ready','active','retiring','failed')),
  coverage        TEXT,        -- advisory JSON {kind:{expected,ready,failed}}; recomputable
  benchmark_result TEXT,       -- JSON
  created_at      INTEGER NOT NULL,
  updated_at      INTEGER NOT NULL
);

CREATE TABLE worktree_projection_state (              -- two-axis: generation × model space [FIXED]
  worktree_id                 TEXT PRIMARY KEY REFERENCES worktree(worktree_id),
  active_generation_id        TEXT REFERENCES generation(generation_id),
  active_model_space_id       TEXT REFERENCES model_space(model_space_id),
  projected_generation_id     TEXT REFERENCES generation(generation_id),
  projected_model_space_id    TEXT REFERENCES model_space(model_space_id),
  target_generation_id        TEXT REFERENCES generation(generation_id),
  target_model_space_id       TEXT REFERENCES model_space(model_space_id),
  projection_op_id            TEXT,                   -- UUID of in-flight/last op
  projection_schema_version   INTEGER NOT NULL,
  status                      TEXT NOT NULL CHECK
    (status IN ('clean','updating','dirty','rebuilding')),
  last_error                  TEXT,
  updated_at                  INTEGER NOT NULL
);

INSERT INTO model_space (model_space_id, display_name, state, created_at, updated_at)
  VALUES ('00000000-0000-7000-8000-000000000001', 'default', 'active', 0, 0);
INSERT INTO store_settings (key, value)
  VALUES ('default_model_space_id', '00000000-0000-7000-8000-000000000001');
";

/// The projection `status` (spec 03 §2.2 `worktree_projection_state.status`,
/// machine in spec 04 §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectionStatus {
    /// `active == projected` and no switch is in flight — search serves the
    /// projected tuple.
    Clean,
    /// A switch wrote ahead its `target` tuple and `projection_op_id`; the backend
    /// reconcile is in flight (spec 05 §5 step 2).
    Updating,
    /// Validate-on-open (or a crash mid-switch) found a divergence; the shard must
    /// be rebuilt (spec 05 §6).
    Dirty,
    /// A rebuild of the active tuple is in flight (spec 05 §7).
    Rebuilding,
}

impl ProjectionStatus {
    /// The stored `status` value.
    pub fn as_str(self) -> &'static str {
        match self {
            ProjectionStatus::Clean => "clean",
            ProjectionStatus::Updating => "updating",
            ProjectionStatus::Dirty => "dirty",
            ProjectionStatus::Rebuilding => "rebuilding",
        }
    }

    /// Parse a stored `status` value; `None` for anything the CHECK forbids.
    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "clean" => Some(ProjectionStatus::Clean),
            "updating" => Some(ProjectionStatus::Updating),
            "dirty" => Some(ProjectionStatus::Dirty),
            "rebuilding" => Some(ProjectionStatus::Rebuilding),
            _ => None,
        }
    }

    /// Check whether `self → to` is a legal status transition (spec 04 §2),
    /// returning a typed [`IllegalProjectionTransition`] otherwise. Pure — no I/O.
    ///
    /// The machine is `clean → updating → clean` (write-ahead then commit), with
    /// the recovery arm `clean|updating → dirty → rebuilding → clean` and the
    /// rebuild-failure edge `rebuilding → dirty`. A self-transition (`X → X`) is an
    /// idempotent no-op and is legal (honor rather than coerce, spec 04 preamble).
    /// Everything else — e.g. `clean → rebuilding` (must pass through `dirty`) or
    /// `dirty → clean` (must rebuild first) — is illegal.
    pub fn check_transition(self, to: ProjectionStatus) -> Result<(), IllegalProjectionTransition> {
        use ProjectionStatus::{Clean, Dirty, Rebuilding, Updating};
        let legal = match (self, to) {
            (a, b) if a == b => true,
            (Clean, Updating) => true,
            (Updating, Clean) => true,
            (Clean, Dirty) => true,
            (Updating, Dirty) => true,
            (Dirty, Rebuilding) => true,
            (Rebuilding, Clean) => true,
            (Rebuilding, Dirty) => true,
            _ => false,
        };
        if legal {
            Ok(())
        } else {
            Err(IllegalProjectionTransition { from: self, to })
        }
    }
}

/// A rejected projection status transition (spec 04 §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IllegalProjectionTransition {
    /// The current status.
    pub from: ProjectionStatus,
    /// The requested (illegal) target status.
    pub to: ProjectionStatus,
}

impl std::fmt::Display for IllegalProjectionTransition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "illegal projection status transition {} → {}",
            self.from.as_str(),
            self.to.as_str()
        )
    }
}

impl std::error::Error for IllegalProjectionTransition {}

/// A row of `worktree_projection_state` (spec 03 §2.2). The three axes are the
/// paired `(generation, model_space)` columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionStateRow {
    /// The owning worktree.
    pub worktree_id: String,
    /// The generation search currently serves.
    pub active_generation_id: Option<String>,
    /// The model space search currently serves.
    pub active_model_space_id: Option<String>,
    /// The generation the shard head reflects.
    pub projected_generation_id: Option<String>,
    /// The model space the shard head reflects.
    pub projected_model_space_id: Option<String>,
    /// The in-flight switch's target generation (`updating` only).
    pub target_generation_id: Option<String>,
    /// The in-flight switch's target model space (`updating` only).
    pub target_model_space_id: Option<String>,
    /// The in-flight/last op UUID (matched against `ProjectionHead.projection_op_id`).
    pub projection_op_id: Option<String>,
    /// The projection-head schema version.
    pub projection_schema_version: i64,
    /// The status machine state.
    pub status: ProjectionStatus,
    /// The last error recorded on a failed rebuild, if any.
    pub last_error: Option<String>,
    /// Last-write timestamp (ms).
    pub updated_at: i64,
}

/// A violated two-axis row invariant (spec 04 §2 invariants + the one-axis rule of
/// spec 04 §8 / 05 §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectionInvariantViolation {
    /// `status='clean'` but a `target` column is set (must be NULL when clean).
    TargetSetWhenClean,
    /// `status='clean'` but `active != projected` on some axis.
    ActiveNotProjectedWhenClean,
    /// `status='updating'` but the `target` tuple is not fully set.
    TargetMissingWhenUpdating,
    /// `status='updating'` but `projection_op_id` is NULL.
    OpIdMissingWhenUpdating,
    /// A switch moves **both** axes at once — forbidden (spec 04 §8 / 05 §5
    /// `[FIXED]`: the two axes are never applied simultaneously).
    BothAxesMovedAtOnce,
}

impl std::fmt::Display for ProjectionInvariantViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let msg = match self {
            ProjectionInvariantViolation::TargetSetWhenClean => {
                "clean projection state must have a NULL target tuple"
            }
            ProjectionInvariantViolation::ActiveNotProjectedWhenClean => {
                "clean projection state must have active == projected"
            }
            ProjectionInvariantViolation::TargetMissingWhenUpdating => {
                "updating projection state must have a fully-set target tuple"
            }
            ProjectionInvariantViolation::OpIdMissingWhenUpdating => {
                "updating projection state must have a projection_op_id"
            }
            ProjectionInvariantViolation::BothAxesMovedAtOnce => {
                "a switch may move only one axis (generation XOR model space) at a time"
            }
        };
        f.write_str(msg)
    }
}

impl std::error::Error for ProjectionInvariantViolation {}

/// Validate the two-axis invariants of a prospective row (spec 04 §2). Pure — no
/// I/O. `dirty`/`rebuilding` rows carry no tuple invariant: the stored tuple is
/// untrusted until validate-on-open (spec 04 §2, 05 §6).
///
/// The one-axis rule (spec 04 §8 / 05 §5 `[FIXED]`) counts an axis as **moved**
/// only when its `active` value is non-NULL and differs from `target`; initializing
/// a projection from a NULL active axis is bootstrap, not a prohibited
/// simultaneous change.
pub fn check_invariants(row: &ProjectionStateRow) -> Result<(), ProjectionInvariantViolation> {
    match row.status {
        ProjectionStatus::Clean => {
            if row.target_generation_id.is_some() || row.target_model_space_id.is_some() {
                return Err(ProjectionInvariantViolation::TargetSetWhenClean);
            }
            if row.active_generation_id != row.projected_generation_id
                || row.active_model_space_id != row.projected_model_space_id
            {
                return Err(ProjectionInvariantViolation::ActiveNotProjectedWhenClean);
            }
        }
        ProjectionStatus::Updating => {
            if row.target_generation_id.is_none() || row.target_model_space_id.is_none() {
                return Err(ProjectionInvariantViolation::TargetMissingWhenUpdating);
            }
            if row.projection_op_id.is_none() {
                return Err(ProjectionInvariantViolation::OpIdMissingWhenUpdating);
            }
            let generation_moved = row.active_generation_id.is_some()
                && row.active_generation_id != row.target_generation_id;
            let model_moved = row.active_model_space_id.is_some()
                && row.active_model_space_id != row.target_model_space_id;
            if generation_moved && model_moved {
                return Err(ProjectionInvariantViolation::BothAxesMovedAtOnce);
            }
        }
        ProjectionStatus::Dirty | ProjectionStatus::Rebuilding => {}
    }
    Ok(())
}

/// The desired next shape of a `worktree_projection_state` row, applied by
/// [`write_projection_state`]. The write-ahead (T07-03), commit (T07-03), and
/// rebuild (T07-04) operations are expressed as values of this type.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectionStateChange {
    /// The target status.
    pub status_to: Option<ProjectionStatus>,
    /// The active tuple after the change.
    pub active_generation_id: Option<String>,
    /// The active model space after the change.
    pub active_model_space_id: Option<String>,
    /// The projected tuple after the change.
    pub projected_generation_id: Option<String>,
    /// The projected model space after the change.
    pub projected_model_space_id: Option<String>,
    /// The target tuple after the change (NULL clears it).
    pub target_generation_id: Option<String>,
    /// The target model space after the change (NULL clears it).
    pub target_model_space_id: Option<String>,
    /// The op UUID after the change (NULL clears it).
    pub projection_op_id: Option<String>,
    /// The recorded error after the change (NULL clears it).
    pub last_error: Option<String>,
}

/// Why a [`write_projection_state`] request was rejected at the domain level (as
/// opposed to an infrastructure/SQLite failure, which surfaces as the outer
/// [`rusqlite::Error`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectionStateError {
    /// No `worktree_projection_state` row exists for this worktree.
    UnknownWorktree,
    /// The status machine forbids the requested transition.
    Illegal(IllegalProjectionTransition),
    /// The prospective row violates a two-axis invariant.
    Invariant(ProjectionInvariantViolation),
}

impl std::fmt::Display for ProjectionStateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProjectionStateError::UnknownWorktree => {
                write!(f, "no projection state for this worktree")
            }
            ProjectionStateError::Illegal(e) => write!(f, "{e}"),
            ProjectionStateError::Invariant(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ProjectionStateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ProjectionStateError::UnknownWorktree => None,
            ProjectionStateError::Illegal(e) => Some(e),
            ProjectionStateError::Invariant(e) => Some(e),
        }
    }
}

/// Map a full `worktree_projection_state` row from a query.
fn row_from_query(r: &rusqlite::Row<'_>) -> rusqlite::Result<ProjectionStateRow> {
    let raw_status: String = r.get(9)?;
    let status = ProjectionStatus::from_db(&raw_status).ok_or_else(|| {
        Error::FromSqlConversionFailure(
            9,
            Type::Text,
            format!("invalid worktree_projection_state.status {raw_status:?}").into(),
        )
    })?;
    Ok(ProjectionStateRow {
        worktree_id: r.get(0)?,
        active_generation_id: r.get(1)?,
        active_model_space_id: r.get(2)?,
        projected_generation_id: r.get(3)?,
        projected_model_space_id: r.get(4)?,
        target_generation_id: r.get(5)?,
        target_model_space_id: r.get(6)?,
        projection_op_id: r.get(7)?,
        projection_schema_version: r.get(8)?,
        status,
        last_error: r.get(10)?,
        updated_at: r.get(11)?,
    })
}

const SELECT_COLUMNS: &str = "worktree_id, active_generation_id, active_model_space_id, \
     projected_generation_id, projected_model_space_id, target_generation_id, \
     target_model_space_id, projection_op_id, projection_schema_version, status, \
     last_error, updated_at";

/// The projection state row for `worktree_id`, if one exists (spec 03 §2.2).
///
/// A stored `status` outside the CHECK domain (corruption) surfaces as
/// [`rusqlite::Error::FromSqlConversionFailure`], never a silent default.
pub fn projection_state(
    conn: &Connection,
    worktree_id: &str,
) -> rusqlite::Result<Option<ProjectionStateRow>> {
    let sql =
        format!("SELECT {SELECT_COLUMNS} FROM worktree_projection_state WHERE worktree_id = ?1");
    conn.query_row(&sql, params![worktree_id], row_from_query)
        .optional()
}

/// Initialize a fresh `clean`, empty projection state row for `worktree_id` (all
/// three tuples NULL, no op in flight). Valid by the clean invariant
/// (`active == projected` trivially, `target` NULL). The worktree must exist (its
/// `worktree_id` foreign key is enforced).
pub fn insert_projection_state(
    tx: &Transaction<'_>,
    worktree_id: &str,
    now_ms: i64,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO worktree_projection_state \
           (worktree_id, projection_schema_version, status, updated_at) \
         VALUES (?1, ?2, 'clean', ?3)",
        params![worktree_id, PROJECTION_SCHEMA_VERSION, now_ms],
    )?;
    Ok(())
}

/// Apply `change` to `worktree_id`'s projection state, enforcing the status
/// machine ([`ProjectionStatus::check_transition`]) and the two-axis invariants
/// ([`check_invariants`]) in one transaction (spec 04 §2). Mirrors
/// [`transition_generation`](super::transition_generation)'s nested-result
/// convention.
///
/// - the outer [`rusqlite::Result`] is `Err` only on a SQLite failure (rolls back);
/// - the inner [`Result`] is the domain outcome, and on any `Err` **no mutation**
///   is performed (the illegality/violation is detected before the write, so the
///   enclosing transaction commits a no-op):
///   - `Err(UnknownWorktree)` — no row for this worktree;
///   - `Err(Illegal(..))` — a forbidden status transition;
///   - `Err(Invariant(..))` — the prospective row breaks a two-axis invariant;
///   - `Ok(())` — the row is updated (or a legal no-op self-transition applied).
///
/// A `status_to` of `None` keeps the current status; every tuple/op/error field in
/// `change` is written verbatim (a `None` field clears that column).
pub fn write_projection_state(
    tx: &Transaction<'_>,
    worktree_id: &str,
    change: &ProjectionStateChange,
    now_ms: i64,
) -> rusqlite::Result<Result<(), ProjectionStateError>> {
    let sql =
        format!("SELECT {SELECT_COLUMNS} FROM worktree_projection_state WHERE worktree_id = ?1");
    let current: Option<ProjectionStateRow> = tx
        .query_row(&sql, params![worktree_id], row_from_query)
        .optional()?;

    let Some(current) = current else {
        return Ok(Err(ProjectionStateError::UnknownWorktree));
    };

    let status = change.status_to.unwrap_or(current.status);
    if let Err(illegal) = current.status.check_transition(status) {
        return Ok(Err(ProjectionStateError::Illegal(illegal)));
    }

    let prospective = ProjectionStateRow {
        worktree_id: worktree_id.to_string(),
        active_generation_id: change.active_generation_id.clone(),
        active_model_space_id: change.active_model_space_id.clone(),
        projected_generation_id: change.projected_generation_id.clone(),
        projected_model_space_id: change.projected_model_space_id.clone(),
        target_generation_id: change.target_generation_id.clone(),
        target_model_space_id: change.target_model_space_id.clone(),
        projection_op_id: change.projection_op_id.clone(),
        projection_schema_version: current.projection_schema_version,
        status,
        last_error: change.last_error.clone(),
        updated_at: now_ms,
    };

    if let Err(violation) = check_invariants(&prospective) {
        return Ok(Err(ProjectionStateError::Invariant(violation)));
    }

    tx.execute(
        "UPDATE worktree_projection_state SET \
           active_generation_id = ?2, active_model_space_id = ?3, \
           projected_generation_id = ?4, projected_model_space_id = ?5, \
           target_generation_id = ?6, target_model_space_id = ?7, \
           projection_op_id = ?8, status = ?9, last_error = ?10, updated_at = ?11 \
         WHERE worktree_id = ?1",
        params![
            worktree_id,
            prospective.active_generation_id,
            prospective.active_model_space_id,
            prospective.projected_generation_id,
            prospective.projected_model_space_id,
            prospective.target_generation_id,
            prospective.target_model_space_id,
            prospective.projection_op_id,
            prospective.status.as_str(),
            prospective.last_error,
            now_ms,
        ],
    )?;
    Ok(Ok(()))
}

/// The `store_settings.default_model_space_id` pointer, if set (spec 04 §3). The
/// migration seeds it to [`DEFAULT_MODEL_SPACE_ID`].
pub fn default_model_space_id(conn: &Connection) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT value FROM store_settings WHERE key = 'default_model_space_id'",
        [],
        |r| r.get(0),
    )
    .optional()
}

/// Why moving the default-model-space pointer was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DefaultModelSpaceError {
    /// No `model_space` row with that id exists.
    Unknown {
        /// The id that was offered.
        model_space_id: String,
    },
    /// The space exists but is not `active`.
    NotActive {
        /// The id that was offered.
        model_space_id: String,
        /// The state it is actually in.
        state: super::ModelSpaceState,
    },
}

impl std::fmt::Display for DefaultModelSpaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DefaultModelSpaceError::Unknown { model_space_id } => {
                write!(f, "unknown model space {model_space_id}")
            }
            DefaultModelSpaceError::NotActive {
                model_space_id,
                state,
            } => write!(
                f,
                "model space {model_space_id} is {}, the default must be active",
                state.as_str()
            ),
        }
    }
}

impl std::error::Error for DefaultModelSpaceError {}

/// Point `store_settings.default_model_space_id` at `model_space_id`
/// (spec 10 §4 step 5: "`default_model_space := B`") — T11-05.
///
/// Guarded rather than a blind upsert: spec 04 §3 `[FIXED]` requires "the default
/// space (`store_settings.default_model_space_id`) MUST be `active`", and this is
/// the only writer, so the invariant is enforced where it is established. A
/// refusal leaves the row untouched (the read happens before the write, and
/// callers run this inside one transaction).
///
/// The pointer is what every *future* worktree open resolves to: spec 05 §8's
/// dormant-worktree migration targets it, and `crate::subjects` pins its rows
/// against eviction precisely because a worktree that has not opened yet will
/// need them.
pub fn set_default_model_space_id(
    tx: &Transaction<'_>,
    model_space_id: &str,
    now_ms: i64,
) -> rusqlite::Result<Result<(), DefaultModelSpaceError>> {
    let Some(state) = super::representation::model_space_state(tx, model_space_id)? else {
        return Ok(Err(DefaultModelSpaceError::Unknown {
            model_space_id: model_space_id.to_string(),
        }));
    };
    if !super::representation::eligible_as_target(state) {
        return Ok(Err(DefaultModelSpaceError::NotActive {
            model_space_id: model_space_id.to_string(),
            state,
        }));
    }
    let _ = now_ms; // `store_settings` carries no timestamp column (spec 03 §2.1).
    tx.execute(
        "INSERT INTO store_settings (key, value) VALUES ('default_model_space_id', ?1) \
         ON CONFLICT(key) DO UPDATE SET value = ?1",
        params![model_space_id],
    )?;
    Ok(Ok(()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(status: ProjectionStatus) -> ProjectionStateRow {
        ProjectionStateRow {
            worktree_id: "wt".to_string(),
            active_generation_id: None,
            active_model_space_id: None,
            projected_generation_id: None,
            projected_model_space_id: None,
            target_generation_id: None,
            target_model_space_id: None,
            projection_op_id: None,
            projection_schema_version: PROJECTION_SCHEMA_VERSION,
            status,
            last_error: None,
            updated_at: 0,
        }
    }

    #[test]
    fn schema_v4_seeds_the_default_model_space() {
        assert!(SCHEMA_V4.contains(DEFAULT_MODEL_SPACE_ID));
        assert!(SCHEMA_V4.contains("'default'"));
        assert!(SCHEMA_V4.contains("default_model_space_id"));
    }

    #[test]
    fn projection_status_round_trips() {
        for status in [
            ProjectionStatus::Clean,
            ProjectionStatus::Updating,
            ProjectionStatus::Dirty,
            ProjectionStatus::Rebuilding,
        ] {
            assert_eq!(ProjectionStatus::from_db(status.as_str()), Some(status));
        }
        assert_eq!(ProjectionStatus::from_db("bogus"), None);
    }

    #[test]
    fn check_transition_covers_the_whole_matrix() {
        use ProjectionStatus::{Clean, Dirty, Rebuilding, Updating};
        let all = [Clean, Updating, Dirty, Rebuilding];

        let legal = [
            (Clean, Updating),
            (Updating, Clean),
            (Clean, Dirty),
            (Updating, Dirty),
            (Dirty, Rebuilding),
            (Rebuilding, Clean),
            (Rebuilding, Dirty),
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
                    Err(IllegalProjectionTransition { from, to }),
                    "{from:?} → {to:?} illegal",
                );
            }
        }
    }

    #[test]
    fn clean_invariants() {
        // Empty clean row is valid (active == projected trivially, target NULL).
        assert_eq!(check_invariants(&row(ProjectionStatus::Clean)), Ok(()));

        // active == projected, target NULL, all non-NULL: valid.
        let mut r = row(ProjectionStatus::Clean);
        r.active_generation_id = Some("g".into());
        r.projected_generation_id = Some("g".into());
        r.active_model_space_id = Some("m".into());
        r.projected_model_space_id = Some("m".into());
        assert_eq!(check_invariants(&r), Ok(()));

        // A set target is illegal when clean.
        let mut r = row(ProjectionStatus::Clean);
        r.target_generation_id = Some("g".into());
        assert_eq!(
            check_invariants(&r),
            Err(ProjectionInvariantViolation::TargetSetWhenClean)
        );

        // active != projected is illegal when clean.
        let mut r = row(ProjectionStatus::Clean);
        r.active_generation_id = Some("g1".into());
        r.projected_generation_id = Some("g2".into());
        assert_eq!(
            check_invariants(&r),
            Err(ProjectionInvariantViolation::ActiveNotProjectedWhenClean)
        );
    }

    #[test]
    fn updating_invariants() {
        // A fully-set target, op id present, one axis moved: valid.
        let mut r = row(ProjectionStatus::Updating);
        r.active_generation_id = Some("g1".into());
        r.active_model_space_id = Some("m".into());
        r.target_generation_id = Some("g2".into()); // generation axis moves
        r.target_model_space_id = Some("m".into()); // model axis unchanged
        r.projection_op_id = Some("op".into());
        assert_eq!(check_invariants(&r), Ok(()));

        // Missing target tuple.
        let mut r = row(ProjectionStatus::Updating);
        r.projection_op_id = Some("op".into());
        assert_eq!(
            check_invariants(&r),
            Err(ProjectionInvariantViolation::TargetMissingWhenUpdating)
        );

        // Missing op id.
        let mut r = row(ProjectionStatus::Updating);
        r.target_generation_id = Some("g".into());
        r.target_model_space_id = Some("m".into());
        assert_eq!(
            check_invariants(&r),
            Err(ProjectionInvariantViolation::OpIdMissingWhenUpdating)
        );

        // Both axes moved from an established active tuple: rejected.
        let mut r = row(ProjectionStatus::Updating);
        r.active_generation_id = Some("g1".into());
        r.active_model_space_id = Some("m1".into());
        r.target_generation_id = Some("g2".into());
        r.target_model_space_id = Some("m2".into());
        r.projection_op_id = Some("op".into());
        assert_eq!(
            check_invariants(&r),
            Err(ProjectionInvariantViolation::BothAxesMovedAtOnce)
        );

        // Establishing both axes from a NULL active tuple is initialization, not a
        // prohibited simultaneous move.
        let mut r = row(ProjectionStatus::Updating);
        r.target_generation_id = Some("g".into());
        r.target_model_space_id = Some("m".into());
        r.projection_op_id = Some("op".into());
        assert_eq!(check_invariants(&r), Ok(()));
    }

    #[test]
    fn dirty_and_rebuilding_carry_no_tuple_invariant() {
        // A target set while dirty/rebuilding is not a violation (untrusted tuple).
        for status in [ProjectionStatus::Dirty, ProjectionStatus::Rebuilding] {
            let mut r = row(status);
            r.active_generation_id = Some("g1".into());
            r.projected_generation_id = Some("g2".into());
            r.target_generation_id = Some("g3".into());
            assert_eq!(check_invariants(&r), Ok(()), "{status:?}");
        }
    }

    /// A corrupt stored `status` must surface a typed conversion error, not a
    /// silent default.
    #[test]
    fn projection_state_rejects_corrupt_status() {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(
            "CREATE TABLE worktree_projection_state \
               (worktree_id TEXT, active_generation_id TEXT, active_model_space_id TEXT, \
                projected_generation_id TEXT, projected_model_space_id TEXT, \
                target_generation_id TEXT, target_model_space_id TEXT, projection_op_id TEXT, \
                projection_schema_version INTEGER, status TEXT, last_error TEXT, updated_at INTEGER);\n\
             INSERT INTO worktree_projection_state (worktree_id, projection_schema_version, status, updated_at) \
               VALUES ('w', 1, 'zombie', 0);",
        )
        .expect("seed corrupt row");

        let bad = projection_state(&conn, "w");
        assert!(
            matches!(bad, Err(Error::FromSqlConversionFailure(9, Type::Text, _))),
            "corrupt status → typed conversion failure, got {bad:?}",
        );
        assert_eq!(projection_state(&conn, "missing").expect("read"), None);
    }
}
