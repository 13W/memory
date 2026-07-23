//! Seeded, fully deterministic synthetic datasets for the spike (T10-01).
//!
//! A dataset is a set of [`ProjectionPoint`]s plus a set of [`DenseQuery`]s, both
//! derived from a single `u64` seed via a splitmix64 PRNG — **no** wall clock, no
//! entropy, no `rand` crate — so `generate(spec, seed)` is a pure function and
//! byte-for-byte reproducible (the "seeded dataset repeatability" acceptance test).
//!
//! Vectors are synthetic, not real embeddings: the spike compares *backends*, and
//! coupling the harness to an embedding model would (a) pull a model SDK before
//! T11 and (b) make the corpus non-deterministic. Point IDs, however, are produced
//! by the real [`projection_point_id`] so a generated dataset yields a valid
//! [`crate::conformance`] manifest.
//!
//! ## Dataset sizes `[SPEC]`-provisional (anchored on the T00-01 baseline)
//!
//! `dims = 768` and `small = 544` points are the *measured* v1 baseline
//! (`fixtures/search/baseline/baseline.md`: `embeddinggemma:300m` dim 768;
//! `manifest.json → baseline.runs[0].chunks_indexed = 544`), not invented numbers.
//! `representative`/`large` and the registry-scale fan-out are engineering
//! provisional sizes, revisited at T10-05 (O2: collect metrics, do not invent
//! thresholds). All are call parameters, never a hard-coded policy.

use local_rag_core::identity::{Uuid, uuidv7_from};
use local_rag_projection::projection_point_id;
use local_rag_projection::{DenseQuery, PointId, ProjectionPoint, RepresentationKind};

/// The vector dimensionality of every spike dataset (baseline `embeddinggemma:300m`).
pub const DIMS: usize = 768;

/// The default number of neighbours a query asks for.
pub const DEFAULT_K: usize = 10;

/// A named dataset size in the spike matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DatasetSpec {
    /// The dataset's matrix name (`small`/`representative`/`large`/…).
    pub name: &'static str,
    /// How many points to generate.
    pub points: usize,
    /// How many query vectors to generate.
    pub queries: usize,
    /// Vector dimensionality.
    pub dims: usize,
}

/// `small` — the measured v1 baseline corpus size (544 chunks, dim 768).
pub const SMALL: DatasetSpec = DatasetSpec {
    name: "small",
    points: 544,
    queries: 49,
    dims: DIMS,
};

/// `representative` — a mid-size repository, `[SPEC]`-provisional.
pub const REPRESENTATIVE: DatasetSpec = DatasetSpec {
    name: "representative",
    points: 50_000,
    queries: 49,
    dims: DIMS,
};

/// `large` — a stress size for warm-search p95 / RAM, `[SPEC]`-provisional.
pub const LARGE: DatasetSpec = DatasetSpec {
    name: "large",
    points: 500_000,
    queries: 49,
    dims: DIMS,
};

/// `tiny` — a fast preset for the conformance/repeatability *tests* only. It is
/// **not** part of the 14 §7 spike matrix; the matrix is `small`/`representative`/
/// `large`. Kept here so tests never pay the cost of a matrix-size build.
pub const TINY: DatasetSpec = DatasetSpec {
    name: "tiny",
    points: 24,
    queries: 4,
    dims: 8,
};

/// Resolve a matrix name to its [`DatasetSpec`] (`tiny` is intentionally not
/// resolvable — it is test-only).
pub fn spec_by_name(name: &str) -> Option<DatasetSpec> {
    match name {
        "small" => Some(SMALL),
        "representative" => Some(REPRESENTATIVE),
        "large" => Some(LARGE),
        _ => None,
    }
}

/// A generated dataset: the projection tuple it belongs to, its points, and its
/// queries. The tuple lets [`crate::conformance`] build a valid head/manifest.
#[derive(Debug, Clone, PartialEq)]
pub struct SeededDataset {
    /// The dataset's matrix name.
    pub name: String,
    /// Vector dimensionality.
    pub dims: usize,
    /// The seed it was generated from.
    pub seed: u64,
    /// The worktree the (synthetic) projection belongs to.
    pub worktree_id: Uuid,
    /// The generation the manifest binds.
    pub generation_id: Uuid,
    /// The model space the points belong to.
    pub model_space_id: Uuid,
    /// The generated points.
    pub points: Vec<ProjectionPoint>,
    /// The generated query vectors.
    pub queries: Vec<DenseQuery>,
}

/// The fixed synthetic projection tuple every dataset uses (deterministic UUIDs,
/// not path-derived — spec 01 §5). Distinct byte patterns keep the three ids
/// distinguishable in a manifest.
fn fixed_tuple() -> (Uuid, Uuid, Uuid) {
    (
        uuidv7_from(1000, [0x10; 10]),
        uuidv7_from(1000, [0x20; 10]),
        uuidv7_from(1000, [0x30; 10]),
    )
}

/// Generate `spec`'s dataset from `seed`. Pure and reproducible.
pub fn generate(spec: &DatasetSpec, seed: u64) -> SeededDataset {
    let (worktree_id, generation_id, model_space_id) = fixed_tuple();
    let mut rng = SplitMix64::new(seed);

    let mut points = Vec::with_capacity(spec.points);
    for index in 0..spec.points {
        // The occurrence id embeds the index in its first 8 bytes, so point ids
        // are unique by construction (before the digest even runs).
        let occurrence_id = occurrence_hex(index as u64, &mut rng);
        let point_id = projection_point_id(
            &worktree_id,
            &occurrence_id,
            &model_space_id,
            RepresentationKind::CodeRaw,
        );
        points.push(ProjectionPoint {
            point_id,
            vector: random_vector(spec.dims, &mut rng),
        });
    }

    let mut queries = Vec::with_capacity(spec.queries);
    for _ in 0..spec.queries {
        queries.push(DenseQuery {
            vector: random_vector(spec.dims, &mut rng),
            k: DEFAULT_K,
        });
    }

    SeededDataset {
        name: spec.name.to_string(),
        dims: spec.dims,
        seed,
        worktree_id,
        generation_id,
        model_space_id,
        points,
        queries,
    }
}

/// The sorted, de-duplicated point-id set of a dataset (the input the manifest
/// hash is computed over, spec 05 §4).
pub fn point_ids(dataset: &SeededDataset) -> Vec<PointId> {
    dataset.points.iter().map(|p| p.point_id.clone()).collect()
}

/// A 64-hex-char synthetic occurrence id whose first 8 bytes are `index` (unique
/// by construction) and whose remaining 24 bytes come from `rng`.
fn occurrence_hex(index: u64, rng: &mut SplitMix64) -> String {
    let mut bytes = [0u8; 32];
    bytes[0..8].copy_from_slice(&index.to_le_bytes());
    bytes[8..16].copy_from_slice(&rng.next_u64().to_le_bytes());
    bytes[16..24].copy_from_slice(&rng.next_u64().to_le_bytes());
    bytes[24..32].copy_from_slice(&rng.next_u64().to_le_bytes());
    hex_encode(&bytes)
}

/// A `dims`-wide vector of f32 components in `[-1, 1)`, drawn from `rng`.
fn random_vector(dims: usize, rng: &mut SplitMix64) -> Vec<f32> {
    (0..dims).map(|_| rng.next_unit_f32()).collect()
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from_digit((b >> 4) as u32, 16).expect("nibble < 16"));
        out.push(char::from_digit((b & 0x0f) as u32, 16).expect("nibble < 16"));
    }
    out
}

/// A splitmix64 PRNG — a well-known, tiny, seedable generator. Deterministic and
/// dependency-free (no `rand`), which is exactly what a reproducible corpus needs.
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A component in `[-1, 1)`, from the top 24 bits (f32 has a 24-bit mantissa).
    fn next_unit_f32(&mut self) -> f32 {
        let bits = (self.next_u64() >> 40) as u32; // 24 bits
        let unit = f64::from(bits) / f64::from(1u32 << 24); // [0, 1)
        (unit * 2.0 - 1.0) as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_is_byte_identical() {
        let a = generate(&TINY, 42);
        let b = generate(&TINY, 42);
        assert_eq!(a, b, "one seed must reproduce the exact same dataset");
    }

    #[test]
    fn different_seeds_diverge() {
        let a = generate(&TINY, 1);
        let b = generate(&TINY, 2);
        assert_ne!(a.points, b.points, "distinct seeds must differ");
    }

    #[test]
    fn spec_shape_is_honored() {
        let d = generate(&TINY, 7);
        assert_eq!(d.points.len(), TINY.points);
        assert_eq!(d.queries.len(), TINY.queries);
        assert!(d.points.iter().all(|p| p.vector.len() == TINY.dims));
        assert!(d.queries.iter().all(|q| q.vector.len() == TINY.dims));
    }

    #[test]
    fn point_ids_are_unique() {
        let d = generate(&TINY, 3);
        let ids = point_ids(&d);
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "no duplicate point ids");
    }

    #[test]
    fn small_preset_matches_the_baseline() {
        // The measured v1 baseline (fixtures/search/baseline), not an invented size.
        assert_eq!(SMALL.points, 544);
        assert_eq!(SMALL.dims, 768);
    }

    #[test]
    fn matrix_names_resolve_but_tiny_does_not() {
        assert_eq!(spec_by_name("small"), Some(SMALL));
        assert_eq!(spec_by_name("representative"), Some(REPRESENTATIVE));
        assert_eq!(spec_by_name("large"), Some(LARGE));
        assert_eq!(spec_by_name("tiny"), None, "tiny is test-only, not matrix");
        assert_eq!(spec_by_name("nope"), None);
    }

    #[test]
    fn unit_f32_stays_in_range() {
        let mut rng = SplitMix64::new(99);
        for _ in 0..10_000 {
            let v = rng.next_unit_f32();
            assert!((-1.0..1.0).contains(&v), "component {v} out of [-1, 1)");
        }
    }
}
