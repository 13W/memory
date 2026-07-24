//! The in-process default provider (spec 10 §1: "the local backend is the
//! working default") and the model-asset precondition it will grow into.
//!
//! [`HashingEmbedder`] is a real, fully local, byte-deterministic embedder built
//! on feature hashing: no ONNX/`candle` runtime, no weights, no network, no
//! external daemon. It exists for three reasons, all of them stated rather than
//! implied:
//!
//! 1. spec 10 §1's `[FIXED]` "local backend is the working default" is true from
//!    this task on, with no optional dependency and no download step;
//! 2. it is the **deterministic model fixture** the pool's tests embed with, so
//!    provider-pool behavior is testable without a 100 MB asset;
//! 3. it is explicitly **not** the model ADR-0004 selects. Its bootstrap
//!    `model_id` ([`LOCAL_BOOTSTRAP_MODEL_ID`]) is part of the six-field
//!    [`RepresentationKey`] (spec 03 §2.2), so its vectors can never be mistaken
//!    for the production model's: they hash to a different `representation_id`
//!    and therefore occupy different `embedding_cache` rows and a different
//!    model space.
//!
//! The ONNX-backed provider for ADR-0004's selected model arrives with its
//! weights in **T11-06** (spec 10 §5 owns delivery: checksum-verified manifest,
//! `.part → fsync → rename → .ok`, offline afterwards); see `D-008` in
//! `docs/implementation-plan/DEVIATIONS.md`. [`require_model_assets`] is the
//! precondition that provider will call — it is the `.ok`-marker check, and
//! today it is what makes "assets are missing" a *typed* error instead of a
//! panic or a silent fallback.

use std::path::PathBuf;

use local_rag_core::paths::StoreLayout;
use local_rag_store::{DistanceMetric, RepresentationKey, RepresentationKind};

use crate::contract::{EmbedError, EmbedRequest, Embedder, Vector};

/// The bootstrap local model identifier (`[SPEC]`).
///
/// Deliberately not a real model name: it must never collide with ADR-0004's
/// `model_id`, because `model_id` is one of the six fields that make a
/// [`RepresentationKey`] canonical.
pub const LOCAL_BOOTSTRAP_MODEL_ID: &str = "local-hashing-v1";

/// Dimensionality of the bootstrap representation (`[SPEC]`).
pub const LOCAL_BOOTSTRAP_DIMENSIONS: u32 = 256;

/// Representation-format version of the bootstrap embedder (`[SPEC]`). Any
/// change to the hashing algorithm below must bump this, exactly like
/// `normalization_version` for text normalization.
pub const LOCAL_BOOTSTRAP_REPRESENTATION_VERSION: u32 = 1;

/// The text-normalization version the bootstrap embedder expects its input to
/// already carry — `local_rag_store::code::normalize`'s
/// `normalization_version = 1` (spec 03 §4.2). The embedder does **not**
/// normalize on its own: normalization is a property of the cached subject, not
/// of the provider.
pub const LOCAL_BOOTSTRAP_NORMALIZATION_VERSION: u32 = 1;

/// A deterministic, dependency-free local embedder (feature hashing).
///
/// The vector is built by hashing each token and each adjacent token bigram into
/// a signed bucket, then L2-normalizing. Properties that matter for a fixture:
///
/// * **byte-deterministic** — the same text yields bit-identical `f32`s on every
///   run, thread and platform (integer hashing, one deterministic float
///   division at the end);
/// * **unit length** — every vector, including one for text with no tokens at
///   all, has L2 norm 1, so cosine distance is always defined;
/// * **order-sensitive** — bigrams make `a b` differ from `b a`, which a
///   bag-of-tokens hash would not.
#[derive(Debug, Clone)]
pub struct HashingEmbedder {
    key: RepresentationKey,
}

impl HashingEmbedder {
    /// A bootstrap embedder for `kind` at [`LOCAL_BOOTSTRAP_DIMENSIONS`].
    pub fn new(kind: RepresentationKind) -> Self {
        Self::with_dimensions(kind, LOCAL_BOOTSTRAP_DIMENSIONS)
    }

    /// A bootstrap embedder for `kind` at an explicit dimensionality.
    ///
    /// `dimensions` is part of the representation key, so two embedders that
    /// differ only here register as two different representations — which is the
    /// point: spec 10 §4 `[FIXED]` forbids reusing a shard layout across
    /// dimensions.
    ///
    /// # Panics
    ///
    /// If `dimensions` is zero — a zero-dimensional representation could never
    /// satisfy `embedding_cache`'s `dimensions * 4 == byte_size` invariant.
    pub fn with_dimensions(kind: RepresentationKind, dimensions: u32) -> Self {
        assert!(dimensions > 0, "dimensions must be non-zero");
        HashingEmbedder {
            key: RepresentationKey {
                kind,
                representation_version: LOCAL_BOOTSTRAP_REPRESENTATION_VERSION,
                normalization_version: LOCAL_BOOTSTRAP_NORMALIZATION_VERSION,
                model_id: LOCAL_BOOTSTRAP_MODEL_ID.to_string(),
                dimensions,
                distance_metric: DistanceMetric::Cosine,
            },
        }
    }

    /// Embed one text (the batch path maps this over its inputs).
    pub fn embed_one(&self, text: &str) -> Vector {
        let dims = self.key.dimensions as usize;
        let mut acc = vec![0.0_f32; dims];
        let mut tokens = 0_usize;
        let mut previous: Option<u64> = None;

        for token in tokenize(text) {
            let h = fnv1a64(token.as_bytes());
            add_feature(&mut acc, h);
            if let Some(prev) = previous {
                // Mix the ordered pair into a distinct bucket so token order
                // changes the vector.
                add_feature(&mut acc, mix(prev, h));
            }
            previous = Some(h);
            tokens += 1;
        }

        if tokens == 0 {
            // No tokens at all (empty text, punctuation only): place a single
            // unit feature derived from the raw bytes, so the vector stays
            // deterministic, non-zero and unit-length.
            let h = fnv1a64(text.as_bytes());
            acc[(h % dims as u64) as usize] = 1.0;
            return Vector::new(acc);
        }

        l2_normalize(&mut acc);
        Vector::new(acc)
    }
}

impl Embedder for HashingEmbedder {
    fn embed(&self, req: EmbedRequest) -> Result<Vec<Vector>, EmbedError> {
        if req.kind != self.key.kind {
            return Err(EmbedError::permanent(format!(
                "provider serves representation kind {}, request asked for {}",
                self.key.kind.as_str(),
                req.kind.as_str()
            )));
        }
        Ok(req.texts.iter().map(|t| self.embed_one(t)).collect())
    }

    fn key(&self) -> RepresentationKey {
        self.key.clone()
    }
}

/// Where a model's assets live: `<store>/models/<model_id>` (spec 02 §2 layout,
/// 10 §5).
pub fn model_assets_dir(layout: &StoreLayout, model_id: &str) -> PathBuf {
    layout.model_dir(model_id)
}

/// The installed-assets precondition (spec 10 §5 `[FIXED policy]`): a model
/// directory counts as usable only once the installer has written its `.ok`
/// marker, because a `.part`/half-renamed download must never be loaded.
///
/// Returns the asset directory, or a typed
/// [`EmbedError::ModelAssetsMissing`] — never a partial success. The installer
/// that creates the marker is T11-06; this is the consumer side of that
/// contract, and it performs no download and no network access of any kind.
pub fn require_model_assets(layout: &StoreLayout, model_id: &str) -> Result<PathBuf, EmbedError> {
    let dir = model_assets_dir(layout, model_id);
    if dir.join(".ok").is_file() {
        Ok(dir)
    } else {
        Err(EmbedError::ModelAssetsMissing {
            model_id: model_id.to_string(),
            expected_path: dir.display().to_string(),
        })
    }
}

/// Split `text` into lowercase identifier-ish tokens.
///
/// Deliberately simple and Unicode-aware only where it is cheap: alphanumeric
/// characters and `_` form tokens, everything else separates. This is part of
/// the bootstrap representation, hence pinned by
/// [`LOCAL_BOOTSTRAP_REPRESENTATION_VERSION`].
fn tokenize(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            current.extend(ch.to_lowercase());
        } else if !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// FNV-1a (64-bit). Stable across platforms and releases by construction — no
/// `RandomState`, no address-dependent seed.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for b in bytes {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

/// Combine two feature hashes into a distinct bigram hash.
fn mix(a: u64, b: u64) -> u64 {
    let mut h = a ^ 0x9e37_79b9_7f4a_7c15;
    h = h.wrapping_mul(0x100_0000_01b3);
    h ^= b.rotate_left(31);
    h.wrapping_mul(0x100_0000_01b3)
}

/// Add one signed unit feature to its bucket.
fn add_feature(acc: &mut [f32], h: u64) {
    let idx = (h % acc.len() as u64) as usize;
    let sign = if h & (1 << 63) == 0 { 1.0 } else { -1.0 };
    acc[idx] += sign;
}

/// Scale to unit L2 norm in place (no-op for an all-zero accumulator, which the
/// caller has already excluded).
fn l2_normalize(acc: &mut [f32]) {
    let norm = acc.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in acc.iter_mut() {
            *x /= norm;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(v: &Vector) -> f32 {
        v.as_slice().iter().map(|x| x * x).sum::<f32>().sqrt()
    }

    #[test]
    fn key_is_the_bootstrap_representation() {
        let key = HashingEmbedder::new(RepresentationKind::CodeRaw).key();
        assert_eq!(key.kind, RepresentationKind::CodeRaw);
        assert_eq!(key.model_id, LOCAL_BOOTSTRAP_MODEL_ID);
        assert_eq!(key.dimensions, LOCAL_BOOTSTRAP_DIMENSIONS);
        assert_eq!(key.distance_metric, DistanceMetric::Cosine);
        assert_eq!(
            key.representation_version,
            LOCAL_BOOTSTRAP_REPRESENTATION_VERSION
        );
        assert_eq!(
            key.normalization_version,
            LOCAL_BOOTSTRAP_NORMALIZATION_VERSION
        );
    }

    #[test]
    fn every_vector_is_unit_length_including_tokenless_text() {
        let e = HashingEmbedder::new(RepresentationKind::CodeRaw);
        for text in ["fn main() {}", "", "   ", "!!! ??? ---", "üñïçø∂é"] {
            let v = e.embed_one(text);
            assert_eq!(v.dimensions(), LOCAL_BOOTSTRAP_DIMENSIONS as usize);
            assert!(
                (norm(&v) - 1.0).abs() < 1e-5,
                "text {text:?} produced norm {}",
                norm(&v)
            );
        }
    }

    #[test]
    fn token_order_changes_the_vector() {
        let e = HashingEmbedder::new(RepresentationKind::CodeRaw);
        assert_ne!(e.embed_one("alpha beta"), e.embed_one("beta alpha"));
    }

    #[test]
    fn tokenizer_folds_case_and_keeps_underscores() {
        assert_eq!(
            tokenize("let Foo_Bar = compute(x); // done"),
            vec!["let", "foo_bar", "compute", "x", "done"]
        );
        assert!(tokenize("   -- ++  ").is_empty());
    }

    #[test]
    fn a_mismatched_kind_is_a_permanent_error_not_a_wrong_vector() {
        let e = HashingEmbedder::new(RepresentationKind::CodeRaw);
        let err = e
            .embed(EmbedRequest::new(
                RepresentationKind::Memory,
                vec!["x".into()],
            ))
            .expect_err("kind mismatch must fail");
        assert!(!err.is_retryable(), "{err}");
        assert!(err.to_string().contains("code_raw"), "{err}");
    }

    #[test]
    fn hashes_are_pinned_values_not_platform_defaults() {
        // Golden FNV-1a values: a drift here would silently invalidate every
        // cached vector under the bootstrap representation.
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x85944171f73967e8);
    }

    #[test]
    fn dimensions_participate_in_the_key() {
        let a = HashingEmbedder::with_dimensions(RepresentationKind::CodeRaw, 64);
        let b = HashingEmbedder::new(RepresentationKind::CodeRaw);
        assert_ne!(a.key(), b.key());
        assert_eq!(a.embed_one("fn main() {}").dimensions(), 64);
    }
}
