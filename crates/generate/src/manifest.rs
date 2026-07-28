//! `models/<model_id>/manifest.json` — what was installed and under what
//! terms (spec 10 §5 `[FIXED policy]`, mirrored from `local_rag_models::manifest`
//! for a generation model instead of an embedding one — no `dimensions`
//! field, since a generator has no `RepresentationKey` analog).

use serde::{Deserialize, Serialize};

/// One installed file's record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GeneratorManifestFile {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

/// The installed generator model's manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GeneratorManifest {
    pub model_id: String,
    pub source: String,
    pub revision: String,
    pub license: String,
    pub license_url: String,
    pub context_length: u32,
    pub files: Vec<GeneratorManifestFile>,
}

impl GeneratorManifest {
    /// Serialize for on-disk storage (pretty, so a human can read the terms).
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

    fn sample() -> GeneratorManifest {
        GeneratorManifest {
            model_id: "qwen2.5-0.5b-instruct-gguf-q4km".to_string(),
            source: "https://huggingface.co/Qwen/Qwen2.5-0.5B-Instruct-GGUF".to_string(),
            revision: "main".to_string(),
            license: "Apache-2.0".to_string(),
            license_url: "https://huggingface.co/Qwen/Qwen2.5-0.5B-Instruct-GGUF".to_string(),
            context_length: 32_768,
            files: vec![GeneratorManifestFile {
                path: "model.gguf".to_string(),
                size: 491_400_032,
                sha256: "74".repeat(32),
            }],
        }
    }

    #[test]
    fn round_trips_through_json() {
        let manifest = sample();
        let parsed = GeneratorManifest::from_json(&manifest.to_json()).expect("parse");
        assert_eq!(parsed, manifest);
    }

    #[test]
    fn records_the_four_fields_the_spec_names() {
        let json = sample().to_json();
        for field in ["\"source\"", "\"size\"", "\"sha256\"", "\"license\""] {
            assert!(json.contains(field), "manifest must record {field}: {json}");
        }
        assert!(json.ends_with('\n'));
    }
}
