//! Deterministic projection identities (spec 05 §3/§4, 03 §1.2).
//!
//! These are pure functions over their inputs — no clock, no entropy, no
//! `state.sqlite` — so they are golden-testable and stable under retry and
//! independent of row/insertion order (spec 03 §1.2 `[FIXED]`). They are built
//! on `local_rag_core::identity::domain`, whose `ProjectionPoint` and
//! `ProjectionManifest` domains are already defined (spec 03 §1.2 table).
//!
//! Deriving the *expected* point set from `state.sqlite`
//! (`expected_point_ids`, spec 05 §4) is deliberately **not** here: it reads the
//! canonical store and belongs to the write-ahead switch (T07-03). T07-01 owns
//! only the identity functions over a supplied set.
//!
//! ## Field encoding, as-built `[SPEC]`
//!
//! Following `core::identity::domain`'s conventions (already-hex identities are
//! hashed as their lowercase ASCII `TEXT` bytes):
//!
//! - `projection_point` fields, in the spec-03 §1.2 order:
//!   `worktree_id` (UUID display), `occurrence_id` (its stored hex), then
//!   `model_space_id` (UUID display) and `representation_kind` (its CHECK
//!   token). Matches spec 05 §3.
//! - `projection_manifest` fields: the identifying tuple
//!   `worktree_id`, `generation_id`, `model_space_id` (each UUID display),
//!   followed by the point IDs **sorted ascending bytewise and de-duplicated**
//!   (the set semantics of spec 05 §4). The op id is deliberately excluded —
//!   it is a per-op nonce checked separately at validate-on-open (spec 05 §6),
//!   not part of the content manifest.

use local_rag_core::identity::Uuid;
use local_rag_core::identity::domain::{self, Domain};

use crate::contract::{
    Hash32, PROJECTION_SCHEMA_VERSION, PointId, ProjectionHead, RepresentationKind,
};

/// `projection_point_id = H(projection_point, worktree_id, occurrence_id,
/// model_space_id, representation_kind)` (spec 05 §3).
///
/// `occurrence_id` is the stored `generation_unit_occurrence.occurrence_id` hex
/// digest (spec 03 §1.2); the two UUIDs are hashed as their canonical display
/// strings, exactly as stored in their `TEXT` columns.
pub fn projection_point_id(
    worktree_id: &Uuid,
    occurrence_id: &str,
    model_space_id: &Uuid,
    representation_kind: RepresentationKind,
) -> PointId {
    let worktree = worktree_id.to_string();
    let model_space = model_space_id.to_string();
    PointId::from_hex(domain::hash(
        Domain::ProjectionPoint,
        &[
            worktree.as_bytes(),
            occurrence_id.as_bytes(),
            model_space.as_bytes(),
            representation_kind.as_str().as_bytes(),
        ],
    ))
}

/// Sort point IDs ascending bytewise and de-duplicate them (set semantics of
/// spec 05 §4). The manifest and point count are both computed over this set,
/// so a caller that supplies the same ids in any order — or with duplicates —
/// gets the same result.
fn sorted_unique(point_ids: &[PointId]) -> Vec<&str> {
    let mut ids: Vec<&str> = point_ids.iter().map(PointId::as_str).collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// `manifest_hash = H(projection_manifest, tuple ‖ sorted point ids)`
/// (spec 03 §1.2, 05 §4). Invariant to the order and multiplicity of
/// `point_ids` (see [`sorted_unique`]).
pub fn manifest_hash(
    worktree_id: &Uuid,
    generation_id: &Uuid,
    model_space_id: &Uuid,
    point_ids: &[PointId],
) -> Hash32 {
    let worktree = worktree_id.to_string();
    let generation = generation_id.to_string();
    let model_space = model_space_id.to_string();
    let ids = sorted_unique(point_ids);

    let mut fields: Vec<&[u8]> = Vec::with_capacity(3 + ids.len());
    fields.push(worktree.as_bytes());
    fields.push(generation.as_bytes());
    fields.push(model_space.as_bytes());
    for id in &ids {
        fields.push(id.as_bytes());
    }
    Hash32::from_hex(domain::hash(Domain::ProjectionManifest, &fields))
}

/// Build the [`ProjectionHead`] for the tuple, op, and point set (spec 05 §5
/// step 3: `head(target tuple, op_id, |expected|, manifest_hash)`).
///
/// `point_count` and `manifest_hash` are derived from the same de-duplicated
/// set, so a head is a deterministic function of `(tuple, op_id, point set)`.
pub fn head(
    worktree_id: Uuid,
    generation_id: Uuid,
    model_space_id: Uuid,
    projection_op_id: Uuid,
    point_ids: &[PointId],
) -> ProjectionHead {
    let manifest_hash = manifest_hash(&worktree_id, &generation_id, &model_space_id, point_ids);
    let point_count = sorted_unique(point_ids).len() as u64;
    ProjectionHead {
        worktree_id,
        generation_id,
        model_space_id,
        projection_op_id,
        projection_schema_version: PROJECTION_SCHEMA_VERSION,
        point_count,
        manifest_hash,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fixed inputs, shared by the golden tests. The UUIDs are the canonical
    // display strings of hand-picked byte patterns; the occurrence id is a
    // stand-in 64-char hex digest.
    fn wt() -> Uuid {
        "01234567-89ab-7122-b344-5566778899aa".parse().unwrap()
    }
    fn gen_id() -> Uuid {
        "0000000a-0000-7000-8000-00000000000b".parse().unwrap()
    }
    fn ms() -> Uuid {
        "0000000c-0000-7000-8000-00000000000d".parse().unwrap()
    }
    const OCC: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[test]
    fn projection_point_id_is_exact_golden() {
        let id = projection_point_id(&wt(), OCC, &ms(), RepresentationKind::CodeRaw);
        // Cross-check: the point id IS the ProjectionPoint domain hash of the
        // spec-ordered fields — an independent recomputation, not a self-check.
        let expected = domain::hash(
            Domain::ProjectionPoint,
            &[
                wt().to_string().as_bytes(),
                OCC.as_bytes(),
                ms().to_string().as_bytes(),
                b"code_raw",
            ],
        );
        assert_eq!(id.as_str(), expected);
        // Locked digest — guards against drift in field order/encoding.
        assert_eq!(
            id.as_str(),
            "2f3497a56cf075858460c77c6689c8b331087d27f9d332c2aabf60fab3aa55f0",
        );
    }

    #[test]
    fn representation_kind_changes_the_point_id() {
        let raw = projection_point_id(&wt(), OCC, &ms(), RepresentationKind::CodeRaw);
        let ctx = projection_point_id(&wt(), OCC, &ms(), RepresentationKind::CodeContext);
        assert_ne!(raw, ctx);
    }

    #[test]
    fn manifest_hash_is_exact_golden() {
        let ids = [
            PointId::from_hex("0a"),
            PointId::from_hex("0b"),
            PointId::from_hex("0c"),
        ];
        let manifest = manifest_hash(&wt(), &gen_id(), &ms(), &ids);
        let expected = domain::hash(
            Domain::ProjectionManifest,
            &[
                wt().to_string().as_bytes(),
                gen_id().to_string().as_bytes(),
                ms().to_string().as_bytes(),
                b"0a",
                b"0b",
                b"0c",
            ],
        );
        assert_eq!(manifest.as_str(), expected);
        assert_eq!(
            manifest.as_str(),
            "b428ec08124ee923139fe203e0cb399381a346409bb900878e7fd9e0778826a1",
        );
    }

    #[test]
    fn manifest_is_independent_of_order_and_duplicates() {
        let sorted = [
            PointId::from_hex("0a"),
            PointId::from_hex("0b"),
            PointId::from_hex("0c"),
        ];
        let shuffled = [
            PointId::from_hex("0c"),
            PointId::from_hex("0a"),
            PointId::from_hex("0b"),
            PointId::from_hex("0a"), // duplicate — set semantics collapse it
        ];
        assert_eq!(
            manifest_hash(&wt(), &gen_id(), &ms(), &sorted),
            manifest_hash(&wt(), &gen_id(), &ms(), &shuffled),
        );
    }

    #[test]
    fn manifest_binds_the_tuple() {
        let ids = [PointId::from_hex("0a")];
        let base = manifest_hash(&wt(), &gen_id(), &ms(), &ids);
        // A different generation in the tuple ⇒ a different manifest.
        let other_gen = manifest_hash(&wt(), &ms(), &ms(), &ids);
        assert_ne!(base, other_gen);
    }

    #[test]
    fn head_derives_count_and_manifest_from_the_set() {
        let op = "0000000e-0000-7000-8000-00000000000f".parse().unwrap();
        let ids = [
            PointId::from_hex("0b"),
            PointId::from_hex("0a"),
            PointId::from_hex("0a"), // duplicate
        ];
        let h = head(wt(), gen_id(), ms(), op, &ids);
        assert_eq!(h.point_count, 2, "duplicate collapses");
        assert_eq!(h.projection_schema_version, PROJECTION_SCHEMA_VERSION);
        assert_eq!(
            h.manifest_hash,
            manifest_hash(&wt(), &gen_id(), &ms(), &ids),
        );
    }
}
