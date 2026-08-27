//! `models/<model_id>/manifest.json` — what was installed and under what terms.
//!
//! Spec 10 §5 `[FIXED policy]`: *"`models/<model_id>/manifest.json` records
//! source, size, sha256, license."* Those four fields are the whole normative
//! requirement; the rest of the shape below exists so the record is
//! self-describing rather than only meaningful next to the binary that wrote it.
//!
//! The manifest is a **record**, not a source of truth for validation: the
//! digests the installer verifies against are the ones compiled into
//! [`crate::catalog`], so a tampered manifest cannot talk the installer into
//! accepting different bytes. Its job is disclosure — most importantly the
//! license, which is what makes shipping a non-OSI default acceptable when no
//! weights are redistributed (ADR-0004).

use serde::{Deserialize, Serialize};

/// One installed file's record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestFile {
    /// Path relative to the model directory.
    pub path: String,
    /// Size in bytes (spec 10 §5's `size`).
    pub size: u64,
    /// Lowercase hex SHA-256 (spec 10 §5's `sha256`).
    pub sha256: String,
}

/// The installed model's manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelManifest {
    /// The id this is installed under.
    pub model_id: String,
    /// Where the weights came from (spec 10 §5's `source`).
    pub source: String,
    /// The upstream revision the files were fetched at.
    pub revision: String,
    /// License name (spec 10 §5's `license`) — persisted so the terms travel
    /// with the bytes.
    pub license: String,
    /// Where the license text can be read.
    pub license_url: String,
    /// Vector dimensionality the model produces.
    pub dimensions: u32,
    /// Every installed file.
    pub files: Vec<ManifestFile>,
}

impl ModelManifest {
    /// Serialize for on-disk storage (pretty, so a human can read the terms).
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("manifest holds only plain owned data") + "\n"
    }

    /// Parse a stored manifest.
    pub fn from_json(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }
}

/// What an installed ONNX Runtime records about itself (T22-15).
///
/// Deliberately its own type rather than a [`ModelManifest`] with odd fields:
/// a runtime has no dimensions, no license URL and no revision, and a manifest
/// whose half the fields are placeholders is a manifest nobody can trust.
///
/// The two digests are both here because both were checked, and a reader
/// should be able to tell which is which: `archive_sha256` certifies the bytes
/// that were downloaded, `sha256` certifies the file that was installed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrtManifest {
    /// The platform key this runtime was installed for.
    pub platform: String,
    /// The upstream ONNX Runtime release tag.
    pub version: String,
    /// The release-asset URL it came from.
    pub source: String,
    /// SHA-256 of the downloaded archive.
    pub archive_sha256: String,
    /// The installed library's file name.
    pub file: String,
    /// The installed library's size in bytes.
    pub size: u64,
    /// The installed library's SHA-256.
    pub sha256: String,
}

impl OrtManifest {
    /// Serialize for on-disk storage.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("manifest holds only plain owned data") + "\n"
    }

    /// Parse a stored manifest.
    pub fn from_json(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ModelManifest {
        ModelManifest {
            model_id: "embeddinggemma-300m".to_string(),
            source: "https://huggingface.co/onnx-community/embeddinggemma-300m-ONNX".to_string(),
            revision: "abc123".to_string(),
            license: "Gemma Terms of Use".to_string(),
            license_url: "https://ai.google.dev/gemma/terms".to_string(),
            dimensions: 768,
            files: vec![ManifestFile {
                path: "model_quantized.onnx".to_string(),
                size: 567_874,
                sha256: "17".repeat(32),
            }],
        }
    }

    #[test]
    fn round_trips_through_json() {
        let manifest = sample();
        let parsed = ModelManifest::from_json(&manifest.to_json()).expect("parse");
        assert_eq!(parsed, manifest);
    }

    #[test]
    fn records_the_four_fields_the_spec_names() {
        // spec 10 §5: source, size, sha256, license.
        let json = sample().to_json();
        for field in ["\"source\"", "\"size\"", "\"sha256\"", "\"license\""] {
            assert!(json.contains(field), "manifest must record {field}: {json}");
        }
        assert!(json.ends_with('\n'), "files end with a newline");
    }
}
