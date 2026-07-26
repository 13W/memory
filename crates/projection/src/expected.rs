//! `expected_point_ids(state.sqlite)` — the deterministic expected point set for
//! a `(generation, model_space)` tuple (spec 05 §4) — T07-03.
//!
//! This is deliberately **not** in [`crate::identity`]: that module hashes an
//! already-supplied point set; this one *derives* the set by reading
//! `generation_unit_occurrence` through `local-rag-store`.
//!
//! ## Required representation kinds, as-built `[SPEC]`
//!
//! Spec 05 §4 defines the expected set as "every occurrence of the generation ×
//! every required representation kind of the model space that applies to code
//! (`code_raw`, `code_context`; `structural_description` only when descriptions
//! are enabled post-v0)". The real per-model-space registry —
//! `representation`/`model_space_representation`, the canonical
//! `RepresentationKey`, and required-coverage recomputation — now exists
//! (`local_rag_store::registry::representation`, **T11-01**), but this
//! function does not yet join against it: `structural_description` is excluded
//! from v0 by the spec's own parenthetical (descriptions are post-v0), and
//! today there is exactly one (T07-02-seeded default) model space whose
//! required set *is* `{code_raw, code_context}` — so
//! [`REQUIRED_REPRESENTATION_KINDS`] still hardcodes that pair rather than
//! joining against `model_space_representation`. This is not a narrowing of
//! the spec for v0; wiring this lookup to the real registry needs a working
//! multi-model-space switch to actually exercise, which is **T11-05**'s card
//! ("production model-axis uses standard projection switch") — not bundled
//! into T11-01, whose own card scopes only the registry itself.
use std::fmt;

use local_rag_core::identity::Uuid;
use local_rag_store::rusqlite::{self, Connection};

use crate::contract::{PointId, RepresentationKind};
use crate::identity::projection_point_id;

/// The representation kinds spec 05 §4 says "apply to code" — the filter the
/// model space's own `required` set is intersected with.
///
/// `structural_description` is excluded by the section's own parenthetical
/// ("only when descriptions are enabled post-v0") and `memory` is not code at
/// all, so neither ever produces a projection point in v0. Which of the two
/// remaining kinds are actually expected is **not** fixed here — that comes from
/// `model_space_representation` (T11-05, see [`required_code_kinds`]).
pub const CODE_REPRESENTATION_KINDS: [RepresentationKind; 2] =
    [RepresentationKind::CodeRaw, RepresentationKind::CodeContext];

/// Why an expected point set could not be derived.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExpectedError {
    /// Reading `state.sqlite` failed.
    Sqlite(String),
    /// The model space requires no code representation at all, so the expected
    /// set would be empty for every occurrence.
    ///
    /// Refused rather than returned as an empty set: an empty expectation makes
    /// a switch "successfully" delete every point in the shard (spec 05 §5 step
    /// 3 deletes `existing \ expected`), which is indistinguishable from a
    /// correct wipe. A model space with no code representation is a registry
    /// mistake, and this is where it surfaces.
    NoCodeRepresentation {
        /// The model space that has none.
        model_space_id: String,
    },
}

impl fmt::Display for ExpectedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExpectedError::Sqlite(e) => write!(f, "sqlite error deriving expected points: {e}"),
            ExpectedError::NoCodeRepresentation { model_space_id } => write!(
                f,
                "model space {model_space_id} requires no code representation kind"
            ),
        }
    }
}

impl std::error::Error for ExpectedError {}

impl From<rusqlite::Error> for ExpectedError {
    fn from(e: rusqlite::Error) -> Self {
        ExpectedError::Sqlite(e.to_string())
    }
}

/// The code representation kinds `model_space_id` actually requires (spec 05 §4:
/// "every `required` representation kind of the model space that applies to
/// code") — T11-05.
///
/// The registry join T07-03 deferred: `model_space_representation` decides the
/// set, [`CODE_REPRESENTATION_KINDS`] filters it down to the ones that project
/// code. Ordered, so the expected set is deterministic.
pub fn required_code_kinds(
    conn: &Connection,
    model_space_id: &Uuid,
) -> Result<Vec<RepresentationKind>, ExpectedError> {
    let model_space_id = model_space_id.to_string();
    let kinds: Vec<RepresentationKind> =
        local_rag_store::model_space_required_kinds(conn, &model_space_id)?
            .into_iter()
            .filter_map(crate::vectors::store_kind_to_projection)
            .filter(|kind| CODE_REPRESENTATION_KINDS.contains(kind))
            .collect();
    if kinds.is_empty() {
        return Err(ExpectedError::NoCodeRepresentation { model_space_id });
    }
    Ok(kinds)
}

/// One point the target tuple expects to exist, carrying the fields needed to
/// both identify it ([`PointId`]) and source its vector (`occurrence_id` +
/// `representation_kind`, spec 05 §5 step 3).
#[derive(Debug, Clone, PartialEq)]
pub struct ExpectedPoint {
    /// The deterministic point identity (spec 05 §3).
    pub point_id: PointId,
    /// The occurrence this point projects.
    pub occurrence_id: String,
    /// Which representation of that occurrence.
    pub representation_kind: RepresentationKind,
}

/// The expected point set for `(generation_id, model_space_id)` in `worktree_id`
/// (spec 05 §4): every occurrence of the generation, crossed with the model
/// space's own required code representations ([`required_code_kinds`]).
///
/// A deterministic, pure-of-vectors function of `state.sqlite` — no vector is
/// read or computed here (spec 05 §5 step 1's PREPARE, and the vector lookup
/// itself, are the caller's concern via a `VectorSource`, `crate::switch`).
pub fn expected_points(
    conn: &Connection,
    worktree_id: &Uuid,
    generation_id: &Uuid,
    model_space_id: &Uuid,
) -> Result<Vec<ExpectedPoint>, ExpectedError> {
    let kinds = required_code_kinds(conn, model_space_id)?;
    let occurrence_ids =
        local_rag_store::occurrence_ids_for_generation(conn, &generation_id.to_string())?;
    let mut points = Vec::with_capacity(occurrence_ids.len() * kinds.len());
    for occurrence_id in occurrence_ids {
        for &kind in &kinds {
            let point_id = projection_point_id(worktree_id, &occurrence_id, model_space_id, kind);
            points.push(ExpectedPoint {
                point_id,
                occurrence_id: occurrence_id.clone(),
                representation_kind: kind,
            });
        }
    }
    Ok(points)
}

/// The spec-named bare point-id projection of [`expected_points`] (spec 05 §4's
/// `expected_point_ids`). Reusable by T07-04's validate-on-open, which only
/// needs the id set, not the occurrence/kind provenance.
pub fn expected_point_ids(
    conn: &Connection,
    worktree_id: &Uuid,
    generation_id: &Uuid,
    model_space_id: &Uuid,
) -> Result<Vec<PointId>, ExpectedError> {
    Ok(
        expected_points(conn, worktree_id, generation_id, model_space_id)?
            .into_iter()
            .map(|p| p.point_id)
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wt() -> Uuid {
        "01234567-89ab-7122-b344-5566778899aa".parse().unwrap()
    }
    fn gen_a() -> Uuid {
        "0000000a-0000-7000-8000-00000000000b".parse().unwrap()
    }
    fn gen_b() -> Uuid {
        "0000000a-0000-7000-8000-00000000000c".parse().unwrap()
    }
    fn ms_a() -> Uuid {
        "0000000c-0000-7000-8000-00000000000d".parse().unwrap()
    }
    fn ms_b() -> Uuid {
        "0000000c-0000-7000-8000-00000000000e".parse().unwrap()
    }

    /// A minimal in-memory schema: the occurrences this module reads, plus the
    /// `model_space_representation` rows the required-kind lookup now joins
    /// against (T11-05). Both model spaces require the same code pair, so the
    /// point-set assertions below are unchanged from T07-03's.
    fn seed(conn: &Connection, rows: &[(&str, &str)]) {
        conn.execute_batch(
            "CREATE TABLE generation_unit_occurrence \
               (occurrence_id TEXT, generation_id TEXT, normalized_path TEXT, unit_id TEXT);\n\
             CREATE INDEX occurrence_by_gen ON generation_unit_occurrence(generation_id);\n\
             CREATE TABLE model_space_representation \
               (model_space_id TEXT, representation_kind TEXT, representation_id TEXT, \
                required INTEGER NOT NULL DEFAULT 1, updated_at INTEGER NOT NULL DEFAULT 0);",
        )
        .expect("seed schema");
        for space in [ms_a(), ms_b()] {
            for kind in ["code_raw", "code_context"] {
                conn.execute(
                    "INSERT INTO model_space_representation \
                       (model_space_id, representation_kind, representation_id, required) \
                     VALUES (?1, ?2, 'repr-' || ?2, 1)",
                    rusqlite::params![space.to_string(), kind],
                )
                .expect("seed required kind");
            }
        }
        for (occ, generation_id) in rows {
            conn.execute(
                "INSERT INTO generation_unit_occurrence \
                   (occurrence_id, generation_id, normalized_path, unit_id) \
                 VALUES (?1, ?2, 'p', 'u')",
                rusqlite::params![occ, generation_id],
            )
            .expect("seed row");
        }
    }

    #[test]
    fn empty_generation_has_no_expected_points() {
        let conn = Connection::open_in_memory().expect("db");
        seed(&conn, &[]);
        let points = expected_points(&conn, &wt(), &gen_a(), &ms_a()).expect("read");
        assert!(points.is_empty());
    }

    #[test]
    fn crosses_every_occurrence_with_both_required_kinds() {
        let conn = Connection::open_in_memory().expect("db");
        let (ga, gb) = (gen_a().to_string(), gen_b().to_string());
        // Two occurrences under the target generation, one under another —
        // scoping (only `gen_a`'s row count) and the cross-product both matter.
        seed(&conn, &[("occ-1", &ga), ("occ-2", &ga), ("occ-other", &gb)]);

        let points = expected_points(&conn, &wt(), &gen_a(), &ms_a()).expect("read");
        assert_eq!(points.len(), 4, "2 occurrences x 2 required kinds");
        let ids: std::collections::BTreeSet<&str> =
            points.iter().map(|p| p.point_id.as_str()).collect();
        assert_eq!(ids.len(), 4, "all 4 point ids distinct");
    }

    #[test]
    fn changing_model_space_changes_every_point_id() {
        let conn = Connection::open_in_memory().expect("db");
        let gen_str = gen_a().to_string();
        seed(&conn, &[("occ-1", &gen_str)]);

        let under_a = expected_points(&conn, &wt(), &gen_a(), &ms_a()).expect("read a");
        let under_b = expected_points(&conn, &wt(), &gen_a(), &ms_b()).expect("read b");
        assert_eq!(under_a.len(), under_b.len());
        for (a, b) in under_a.iter().zip(under_b.iter()) {
            assert_ne!(a.point_id, b.point_id);
        }
    }

    #[test]
    fn changing_generation_changes_every_point_id() {
        // A real `occurrence_id` (spec 03 §1.2) is `H(generation_id,
        // normalized_path, unit_id)` — it embeds `generation_id`, so even
        // byte-identical file content re-indexed into a new generation gets a
        // fresh occurrence id, and thus a fully disjoint point set. `expected_points`
        // itself just reads whatever occurrence_id string is stored (scoped by the
        // `generation_id` query param); this test seeds via the real derivation
        // (`local_rag_store::occurrence_id`) so the disjointness reflects actual
        // system behavior, not an artifact of the test's synthetic strings.
        let conn = Connection::open_in_memory().expect("db");
        let (ga, gb) = (gen_a().to_string(), gen_b().to_string());
        let occ_a = local_rag_store::occurrence_id(&ga, "same/path.rs", "unit-1");
        let occ_b = local_rag_store::occurrence_id(&gb, "same/path.rs", "unit-1");
        assert_ne!(occ_a, occ_b, "occurrence_id embeds generation_id");
        seed(&conn, &[(occ_a.as_str(), &ga), (occ_b.as_str(), &gb)]);

        let under_a = expected_points(&conn, &wt(), &gen_a(), &ms_a()).expect("read a");
        let under_b = expected_points(&conn, &wt(), &gen_b(), &ms_a()).expect("read b");
        assert_eq!(under_a.len(), 2);
        assert_eq!(under_b.len(), 2);
        let ids_a: std::collections::BTreeSet<&str> =
            under_a.iter().map(|p| p.point_id.as_str()).collect();
        let ids_b: std::collections::BTreeSet<&str> =
            under_b.iter().map(|p| p.point_id.as_str()).collect();
        assert!(ids_a.is_disjoint(&ids_b));
    }

    #[test]
    fn expected_point_ids_matches_expected_points() {
        let conn = Connection::open_in_memory().expect("db");
        let gen_str = gen_a().to_string();
        seed(&conn, &[("occ-1", &gen_str), ("occ-2", &gen_str)]);

        let points = expected_points(&conn, &wt(), &gen_a(), &ms_a()).expect("points");
        let ids = expected_point_ids(&conn, &wt(), &gen_a(), &ms_a()).expect("ids");
        assert_eq!(
            ids,
            points.into_iter().map(|p| p.point_id).collect::<Vec<_>>()
        );
    }
}
