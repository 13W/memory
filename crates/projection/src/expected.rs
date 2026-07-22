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
//! `representation`/`model_space_representation`, the canonical RepresentationKey,
//! and required-coverage recomputation — is **T11-01** and does not exist yet
//! (T07-02 seeded only one seam-only `model_space` row). `structural_description`
//! is excluded from v0 by the spec's own parenthetical (descriptions are
//! post-v0), and until T11-01 ships there is exactly one (seeded default) model
//! space whose required set *is* `{code_raw, code_context}` — so
//! [`REQUIRED_REPRESENTATION_KINDS`] hardcodes that pair rather than joining
//! against a registry that isn't built yet. This is not a narrowing of the spec
//! for v0; T11-01 replaces the constant with a real per-model-space lookup.
use local_rag_core::identity::Uuid;
use local_rag_store::rusqlite::{self, Connection};

use crate::contract::{PointId, RepresentationKind};
use crate::identity::projection_point_id;

/// The representation kinds every occurrence needs a projection point for in v0
/// (spec 05 §4). See the module doc for why this is fixed rather than looked up.
pub const REQUIRED_REPRESENTATION_KINDS: [RepresentationKind; 2] =
    [RepresentationKind::CodeRaw, RepresentationKind::CodeContext];

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
/// (spec 05 §4): every occurrence of the generation, crossed with
/// [`REQUIRED_REPRESENTATION_KINDS`].
///
/// A deterministic, pure-of-vectors function of `state.sqlite` — no vector is
/// read or computed here (spec 05 §5 step 1's PREPARE, and the vector lookup
/// itself, are the caller's concern via a `VectorSource`, `crate::switch`).
pub fn expected_points(
    conn: &Connection,
    worktree_id: &Uuid,
    generation_id: &Uuid,
    model_space_id: &Uuid,
) -> rusqlite::Result<Vec<ExpectedPoint>> {
    let occurrence_ids =
        local_rag_store::occurrence_ids_for_generation(conn, &generation_id.to_string())?;
    let mut points = Vec::with_capacity(occurrence_ids.len() * REQUIRED_REPRESENTATION_KINDS.len());
    for occurrence_id in occurrence_ids {
        for &kind in &REQUIRED_REPRESENTATION_KINDS {
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
) -> rusqlite::Result<Vec<PointId>> {
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

    /// A minimal in-memory `generation_unit_occurrence` table — this module only
    /// reads it, so no other schema is needed.
    fn seed(conn: &Connection, rows: &[(&str, &str)]) {
        conn.execute_batch(
            "CREATE TABLE generation_unit_occurrence \
               (occurrence_id TEXT, generation_id TEXT, normalized_path TEXT, unit_id TEXT);\n\
             CREATE INDEX occurrence_by_gen ON generation_unit_occurrence(generation_id);",
        )
        .expect("seed schema");
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
