//! The in-process ONNX embedding provider (spec 10 §1 `[FIXED]`, D-008).
//!
//! Spec 10 §1 fixes that embeddings run **in-process** via `fastembed` (ONNX
//! Runtime) or `Candle`; ADR-0005 picks ONNX Runtime through `ort` with the
//! `load-dynamic` feature. That feature is the load-bearing detail: nothing is
//! downloaded or linked at build time, and `libonnxruntime` is resolved at
//! runtime, so `cargo xtask ci` keeps running offline — the objection D-008
//! raised against `ort`'s default `download-binaries`.
//!
//! # Assets come through the consumer contract, not from here
//!
//! [`OnnxEmbedder::open`] locates weights **only** via
//! `local_rag_embed::require_model_assets`, which returns the directory when its
//! `.ok` marker is present and `EmbedError::ModelAssetsMissing` otherwise
//! (spec 10 §5). This type therefore performs no filesystem policy of its own
//! and, by construction, no network access: a missing model is a typed error,
//! never an implicit download.
//!
//! # Runtime requirements
//!
//! Loading a session needs the ONNX Runtime shared library on the host. Where it
//! comes from per platform package is T17-03's "ORT bundling before the final CI
//! matrix"; until then `ort` resolves it from `ORT_DYLIB_PATH` or the system
//! loader path, and its absence surfaces as a typed
//! [`OnnxError::Runtime`] rather than a panic.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use local_rag_embed::{EmbedError, EmbedRequest, Embedder, Vector};
use local_rag_store::RepresentationKey;
use ndarray::{Array2, Axis};
use ort::session::Session;
use ort::value::TensorRef;
use tokenizers::Tokenizer;

use crate::catalog::ModelCatalogEntry;

/// Tokens per sequence. Longer inputs are truncated, matching how the ADR-0004
/// measurements were taken (`max_length = 256`), and long code units are already
/// split into `parsed_unit` spans upstream.
pub const MAX_SEQUENCE_TOKENS: usize = 256;

/// Why the ONNX provider could not be created.
#[derive(Debug)]
#[non_exhaustive]
pub enum OnnxError {
    /// The model's assets are not installed (no `.ok` marker) — spec 10 §5.
    Assets(EmbedError),
    /// The ONNX Runtime library or the session could not be loaded.
    Runtime(String),
    /// `tokenizer.json` could not be loaded.
    Tokenizer(String),
}

impl fmt::Display for OnnxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OnnxError::Assets(e) => write!(f, "{e}"),
            OnnxError::Runtime(e) => write!(
                f,
                "ONNX Runtime unavailable ({e}); install the runtime or set ORT_DYLIB_PATH"
            ),
            OnnxError::Tokenizer(e) => write!(f, "could not load the model tokenizer: {e}"),
        }
    }
}

impl std::error::Error for OnnxError {}

impl From<OnnxError> for EmbedError {
    fn from(e: OnnxError) -> Self {
        match e {
            // An asset error is already the typed one the pool understands.
            OnnxError::Assets(inner) => inner,
            // Everything else is a provider that cannot serve at all: permanent
            // for this provider, so the pool falls back rather than retrying.
            other => EmbedError::permanent(other.to_string()),
        }
    }
}

/// An `Embedder` backed by a locally installed ONNX model.
pub struct OnnxEmbedder {
    key: RepresentationKey,
    tokenizer: Tokenizer,
    /// `ort` sessions are not `Sync`; the pool calls `embed` from one worker at
    /// a time, and the mutex makes that structurally true rather than assumed.
    session: Mutex<Session>,
    model_dir: PathBuf,
}

impl OnnxEmbedder {
    /// Open the provider for `entry`, reading assets from the store layout.
    ///
    /// Returns `ModelAssetsMissing` (through [`OnnxError::Assets`]) when the
    /// model has not been installed — without touching the network.
    pub fn open(
        layout: &local_rag_core::paths::StoreLayout,
        entry: &ModelCatalogEntry,
    ) -> Result<Self, OnnxError> {
        let dir = local_rag_embed::require_model_assets(layout, entry.model_id)
            .map_err(OnnxError::Assets)?;
        Self::open_dir(&dir, entry)
    }

    /// Open the provider against an already-resolved asset directory.
    ///
    /// Split out so a caller that has already validated the directory (or a test
    /// working against a fixture) does not repeat the marker check; the
    /// marker-checking entry point is [`OnnxEmbedder::open`].
    pub fn open_dir(dir: &Path, entry: &ModelCatalogEntry) -> Result<Self, OnnxError> {
        let tokenizer = Tokenizer::from_file(dir.join("tokenizer.json"))
            .map_err(|e| OnnxError::Tokenizer(e.to_string()))?;

        let graph = entry
            .files
            .iter()
            .map(|f| f.relative_path)
            .find(|p| p.ends_with(".onnx"))
            .unwrap_or("model_quantized.onnx");

        let session = Session::builder()
            .and_then(|mut b| b.commit_from_file(dir.join(graph)))
            .map_err(|e| OnnxError::Runtime(e.to_string()))?;

        Ok(OnnxEmbedder {
            key: entry.representation_key(),
            tokenizer,
            session: Mutex::new(session),
            model_dir: dir.to_path_buf(),
        })
    }

    /// The directory the assets were loaded from.
    pub fn model_dir(&self) -> &Path {
        &self.model_dir
    }

    /// Tokenize a batch into the `(input_ids, attention_mask, token_type_ids)`
    /// tensors the encoder expects, padded to a common length.
    fn encode(&self, texts: &[String]) -> Result<(Array2<i64>, Array2<i64>), EmbedError> {
        let encodings = self
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(|e| EmbedError::permanent(format!("tokenization failed: {e}")))?;

        let width = encodings
            .iter()
            .map(|e| e.get_ids().len().min(MAX_SEQUENCE_TOKENS))
            .max()
            .unwrap_or(1)
            .max(1);

        let mut ids = Array2::<i64>::zeros((encodings.len(), width));
        let mut mask = Array2::<i64>::zeros((encodings.len(), width));
        for (row, encoding) in encodings.iter().enumerate() {
            for (col, (&id, &attn)) in encoding
                .get_ids()
                .iter()
                .zip(encoding.get_attention_mask())
                .take(width)
                .enumerate()
            {
                ids[[row, col]] = id as i64;
                mask[[row, col]] = attn as i64;
            }
        }
        Ok((ids, mask))
    }
}

impl Embedder for OnnxEmbedder {
    fn embed(&self, req: EmbedRequest) -> Result<Vec<Vector>, EmbedError> {
        if req.kind != self.key.kind {
            return Err(EmbedError::permanent(format!(
                "provider serves representation kind {}, request asked for {}",
                self.key.kind.as_str(),
                req.kind.as_str()
            )));
        }
        if req.texts.is_empty() {
            return Ok(Vec::new());
        }

        let (ids, mask) = self.encode(&req.texts)?;
        let mut session = self
            .session
            .lock()
            .map_err(|_| EmbedError::permanent("onnx session mutex poisoned"))?;

        let mut inputs: Vec<(&str, ort::session::SessionInputValue<'_>)> = Vec::new();
        let ids_tensor = TensorRef::from_array_view(&ids)
            .map_err(|e| EmbedError::permanent(format!("input_ids tensor: {e}")))?;
        let mask_tensor = TensorRef::from_array_view(&mask)
            .map_err(|e| EmbedError::permanent(format!("attention_mask tensor: {e}")))?;
        inputs.push(("input_ids", ids_tensor.into()));
        inputs.push(("attention_mask", mask_tensor.into()));

        let outputs = session
            .run(inputs)
            .map_err(|e| EmbedError::retryable(format!("onnx inference failed: {e}")))?;

        // Prefer a pooled output when the graph provides one; otherwise mean-pool
        // the token states under the attention mask.
        let (_, value) = outputs
            .iter()
            .next()
            .ok_or_else(|| EmbedError::permanent("onnx session produced no output"))?;
        let (shape, data) = value
            .try_extract_tensor::<f32>()
            .map_err(|e| EmbedError::permanent(format!("output tensor: {e}")))?;

        let vectors = pool(shape, data, &mask, self.key.dimensions as usize)?;
        Ok(vectors)
    }

    fn key(&self) -> RepresentationKey {
        self.key.clone()
    }
}

/// Reduce an encoder output to one unit-length vector per input.
///
/// A `[batch, dims]` output is already pooled; a `[batch, tokens, dims]` one is
/// mean-pooled over the tokens the attention mask keeps — masked positions carry
/// padding, and averaging them in would make a vector depend on its batch-mates'
/// lengths.
fn pool(
    shape: &[i64],
    data: &[f32],
    mask: &Array2<i64>,
    dimensions: usize,
) -> Result<Vec<Vector>, EmbedError> {
    let batch = mask.len_of(Axis(0));
    let mut out = Vec::with_capacity(batch);

    match shape.len() {
        2 => {
            let dims = shape[1] as usize;
            if dims != dimensions {
                return Err(dimension_error(dimensions, dims));
            }
            for row in 0..batch {
                let start = row * dims;
                out.push(normalized(&data[start..start + dims]));
            }
        }
        3 => {
            let tokens = shape[1] as usize;
            let dims = shape[2] as usize;
            if dims != dimensions {
                return Err(dimension_error(dimensions, dims));
            }
            for row in 0..batch {
                let mut acc = vec![0.0_f32; dims];
                let mut kept = 0.0_f32;
                for token in 0..tokens {
                    if mask[[row, token]] == 0 {
                        continue;
                    }
                    let start = (row * tokens + token) * dims;
                    for (slot, value) in acc.iter_mut().zip(&data[start..start + dims]) {
                        *slot += *value;
                    }
                    kept += 1.0;
                }
                if kept > 0.0 {
                    for slot in acc.iter_mut() {
                        *slot /= kept;
                    }
                }
                out.push(normalized(&acc));
            }
        }
        other => {
            return Err(EmbedError::permanent(format!(
                "unexpected onnx output rank {other}"
            )));
        }
    }
    Ok(out)
}

fn dimension_error(expected: usize, actual: usize) -> EmbedError {
    EmbedError::permanent(format!(
        "model produced {actual}-dimensional vectors, the representation declares {expected}"
    ))
}

/// L2-normalize, so cosine distance (ADR-0004's metric) is a dot product.
fn normalized(values: &[f32]) -> Vector {
    let norm = values.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm > 0.0 {
        Vector::new(values.iter().map(|v| v / norm).collect())
    } else {
        Vector::new(values.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mask(rows: usize, cols: usize, keep: usize) -> Array2<i64> {
        let mut m = Array2::<i64>::zeros((rows, cols));
        for row in 0..rows {
            for col in 0..keep {
                m[[row, col]] = 1;
            }
        }
        m
    }

    #[test]
    fn a_pooled_output_is_normalized_as_is() {
        let m = mask(1, 4, 4);
        let out = pool(&[1, 3], &[3.0, 0.0, 4.0], &m, 3).expect("pool");
        assert_eq!(out.len(), 1);
        // 3-4-5 triangle: the unit vector is exactly (0.6, 0, 0.8).
        assert!((out[0].as_slice()[0] - 0.6).abs() < 1e-6);
        assert!((out[0].as_slice()[2] - 0.8).abs() < 1e-6);
    }

    #[test]
    fn token_states_are_mean_pooled_under_the_mask() {
        // Two tokens kept, one masked: the masked token's huge values must not
        // reach the result.
        let m = mask(1, 3, 2);
        let data = vec![
            1.0, 0.0, // token 0 (kept)
            3.0, 0.0, // token 1 (kept)
            100.0, 100.0, // token 2 (masked)
        ];
        let out = pool(&[1, 3, 2], &data, &m, 2).expect("pool");
        // mean = (2, 0) → normalized = (1, 0)
        assert!((out[0].as_slice()[0] - 1.0).abs() < 1e-6, "{:?}", out[0]);
        assert!(out[0].as_slice()[1].abs() < 1e-6);
    }

    #[test]
    fn a_width_mismatch_is_a_typed_error_not_a_silent_truncation() {
        let m = mask(1, 2, 2);
        let err = pool(&[1, 5], &[0.0; 5], &m, 768).expect_err("mismatch");
        assert!(err.to_string().contains("768"), "{err}");
        assert!(
            !err.is_retryable(),
            "a wrong-width model will not fix itself"
        );
    }

    #[test]
    fn an_all_masked_row_stays_finite() {
        let m = mask(1, 2, 0);
        let out = pool(&[1, 2, 2], &[1.0, 2.0, 3.0, 4.0], &m, 2).expect("pool");
        assert!(out[0].as_slice().iter().all(|v| v.is_finite()));
    }
}
