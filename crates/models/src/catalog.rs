//! What a model is made of: files, sizes, digests, and its license (spec 10 §5).
//!
//! Spec 10 §5 `[FIXED policy]` requires a **checksum-verified manifest**, which
//! only means something if the expected digests are known *before* the download —
//! otherwise the installer would be certifying whatever it happened to receive.
//! So the catalog is compiled in: each entry pins the exact `sha256` and byte
//! size of every file, measured from the upstream repository (see ADR-0005 for
//! how they were captured).
//!
//! The catalog is data, not policy: it says which bytes make up a model, while
//! [`crate::install`] decides how they land on disk and
//! `local_rag_embed::require_model_assets` decides when they may be used.

use local_rag_store::RepresentationKey;
use local_rag_store::{DistanceMetric, RepresentationKind};

/// The model ADR-0004 selected as the v0 default.
pub const DEFAULT_MODEL_ID: &str = "embeddinggemma-300m";

/// Where the weights come from — recorded verbatim in `manifest.json`'s
/// `source` field (spec 10 §5) and printed before the first byte is fetched.
pub const DEFAULT_MODEL_SOURCE: &str =
    "https://huggingface.co/onnx-community/embeddinggemma-300m-ONNX";

/// The license the user is accepting by downloading. Not an OSI license, which
/// is exactly why it must be surfaced and persisted (ADR-0004's "the installer
/// inherits a license obligation").
pub const DEFAULT_MODEL_LICENSE: &str = "Gemma Terms of Use";

/// Where that license can be read in full.
pub const DEFAULT_MODEL_LICENSE_URL: &str = "https://ai.google.dev/gemma/terms";

/// The upstream revision the digests below were taken from. Pinning it keeps the
/// URLs immutable — a branch name would let the bytes change under a fixed
/// digest and turn every install into a checksum failure.
pub const DEFAULT_MODEL_REVISION: &str = "5090578d9565bb06545b4552f76e6bc2c93e4a66";

/// One file of a model: where it comes from, how big it is, what it hashes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetFile {
    /// Path relative to the model directory (`models/<model_id>/`).
    pub relative_path: &'static str,
    /// Path relative to the source repository root.
    pub source_path: &'static str,
    /// Exact size in bytes.
    pub size: u64,
    /// Lowercase hex SHA-256 of the file's contents.
    pub sha256: &'static str,
}

/// A model's complete asset set plus the metadata `manifest.json` records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelCatalogEntry {
    /// The `model_id` this installs under — also the directory name and one of
    /// the six fields of the canonical `RepresentationKey` (spec 03 §2.2).
    pub model_id: &'static str,
    /// Human-readable source (repository URL).
    pub source: &'static str,
    /// The pinned upstream revision.
    pub revision: &'static str,
    /// License name.
    pub license: &'static str,
    /// Where the license text lives.
    pub license_url: &'static str,
    /// Vector dimensionality this model produces.
    pub dimensions: u32,
    /// Files to install, in install order.
    pub files: &'static [AssetFile],
}

impl ModelCatalogEntry {
    /// The download URL of `file` in this entry's source repository.
    ///
    /// HuggingFace's `resolve/<revision>/<path>` form, with the revision pinned
    /// so the bytes behind a URL never change.
    pub fn url_for(&self, file: &AssetFile) -> String {
        format!(
            "{}/resolve/{}/{}",
            self.source, self.revision, file.source_path
        )
    }

    /// Total bytes this entry will download (before any reuse).
    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }

    /// The canonical representation key this model's `code_raw` vectors carry
    /// (ADR-0004's decision, byte-for-byte).
    ///
    /// `representation_version = 2` since D-016: the sequence window moved from
    /// 256 to 1024 tokens (`crate::onnx::MAX_SEQUENCE_TOKENS`), which changes the
    /// vectors a long unit gets. The window is not itself a key field, so this
    /// version is what keeps `embedding_cache` from serving 256-token vectors as
    /// though they were 1024-token ones — the field exists precisely to make such
    /// a change addressable instead of silent.
    pub fn representation_key(&self) -> RepresentationKey {
        RepresentationKey {
            kind: RepresentationKind::CodeRaw,
            representation_version: 2,
            normalization_version: 1,
            model_id: self.model_id.to_string(),
            dimensions: self.dimensions,
            distance_metric: DistanceMetric::Cosine,
        }
    }
}

/// The q8 (`model_quantized`) operating point ADR-0005 selected: 295 MiB against
/// fp32's 1.15 GiB, at the same 768 dimensions and ~the same CPU latency.
///
/// The graph and its external-data sibling are two files by ONNX's own
/// convention — a graph over 2 GiB stores its tensors alongside — and the
/// tokenizer is required because encoding must match the model's training-time
/// vocabulary exactly.
const EMBEDDINGGEMMA_300M_FILES: &[AssetFile] = &[
    AssetFile {
        relative_path: "model_quantized.onnx",
        source_path: "onnx/model_quantized.onnx",
        size: 567_874,
        sha256: "172efde319fe1542dc41f31be6154910b05b78f7a861c265c4600eec906bd6d8",
    },
    AssetFile {
        relative_path: "model_quantized.onnx_data",
        source_path: "onnx/model_quantized.onnx_data",
        size: 308_890_624,
        sha256: "705626e28e4c23c82ade34566b4197d97f534c12275fa406dfb71e9937d388c0",
    },
    AssetFile {
        relative_path: "tokenizer.json",
        source_path: "tokenizer.json",
        size: 20_323_312,
        sha256: "4dda02faaf32bc91031dc8c88457ac272b00c1016cc679757d1c441b248b9c47",
    },
];

/// The default model's catalog entry (ADR-0004 / ADR-0005).
pub const EMBEDDINGGEMMA_300M: ModelCatalogEntry = ModelCatalogEntry {
    model_id: DEFAULT_MODEL_ID,
    source: DEFAULT_MODEL_SOURCE,
    revision: DEFAULT_MODEL_REVISION,
    license: DEFAULT_MODEL_LICENSE,
    license_url: DEFAULT_MODEL_LICENSE_URL,
    dimensions: 768,
    files: EMBEDDINGGEMMA_300M_FILES,
};

/// Every model this build knows how to install.
pub const CATALOG: &[ModelCatalogEntry] = &[EMBEDDINGGEMMA_300M];

/// Look a model up by id.
pub fn find(model_id: &str) -> Option<&'static ModelCatalogEntry> {
    CATALOG.iter().find(|entry| entry.model_id == model_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_entry_matches_adr_0004() {
        let entry = find(DEFAULT_MODEL_ID).expect("the default model is catalogued");
        assert_eq!(entry.dimensions, 768);
        let key = entry.representation_key();
        assert_eq!(key.model_id, "embeddinggemma-300m");
        assert_eq!(key.dimensions, 768);
        assert_eq!(key.distance_metric, DistanceMetric::Cosine);
        assert_eq!(key.kind, RepresentationKind::CodeRaw);
        assert_eq!(entry.license, "Gemma Terms of Use");
    }

    #[test]
    fn every_file_pins_a_digest_and_a_size() {
        for entry in CATALOG {
            assert!(!entry.files.is_empty(), "{} has no files", entry.model_id);
            for file in entry.files {
                assert_eq!(
                    file.sha256.len(),
                    64,
                    "{}: {} must pin a full sha256",
                    entry.model_id,
                    file.relative_path
                );
                assert!(
                    file.sha256
                        .chars()
                        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
                    "{}: digest must be lowercase hex",
                    file.relative_path
                );
                assert!(file.size > 0, "{}: size must be known", file.relative_path);
                // A relative path must stay inside the model directory.
                assert!(
                    !file.relative_path.contains("..") && !file.relative_path.starts_with('/'),
                    "{}: relative_path escapes the model directory",
                    file.relative_path
                );
            }
        }
    }

    #[test]
    fn urls_pin_the_revision() {
        let entry = &EMBEDDINGGEMMA_300M;
        let url = entry.url_for(&entry.files[0]);
        assert_eq!(
            url,
            "https://huggingface.co/onnx-community/embeddinggemma-300m-ONNX/resolve/\
             5090578d9565bb06545b4552f76e6bc2c93e4a66/onnx/model_quantized.onnx"
        );
        assert!(
            url.starts_with("https://"),
            "weights are fetched over TLS only"
        );
    }

    #[test]
    fn the_asset_budget_matches_the_adr() {
        // ADR-0004 quotes ≈295 MiB for the q8 operating point; hold the catalog
        // to that so a silent switch to fp32 (1.15 GiB) cannot pass unnoticed.
        let mib = EMBEDDINGGEMMA_300M.total_bytes() as f64 / (1024.0 * 1024.0);
        assert!(
            (275.0..=320.0).contains(&mib),
            "unexpected asset budget: {mib:.1} MiB"
        );
    }
}
