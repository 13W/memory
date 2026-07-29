//! [`LlamaGenerator`]: the `Generator` provider for ADR-0006's local runtime
//! (llama.cpp via `llama-cpp-2`).
//!
//! Mirrors `local_rag_models::onnx::OnnxEmbedder`'s shape: resolves weights
//! only through the installed-assets precondition (never downloads), loads
//! once, and exposes a synchronous `Generator::generate` matching the
//! `[FIXED]` trait (spec 10 §1).
//!
//! # Determinism (spec 08 §7's benchmark needs reproducible runs)
//!
//! Only [`Sampling::Greedy`] is implemented — a single `LlamaSampler::greedy()`
//! stage with no `dist()`/temperature stage ahead of it, so token selection is
//! pure argmax over the model's logits with no RNG involved anywhere in the
//! chain. [`Sampling::Temperature`] returns [`LlamaError::UnsupportedSampling`]:
//! nothing in this task's own design calls for sampled decoding (see
//! `crates/embed/src/contract.rs`'s module doc), so implementing it here would
//! be unexercised code.
//!
//! # No grammar-constrained decoding in v0 (as-built scope decision)
//!
//! `llama-cpp-2`'s `LlamaSampler::grammar` takes a raw GBNF string, not a JSON
//! Schema — the crate does not expose llama.cpp's own JSON-schema-to-grammar
//! conversion, and hand-authoring/maintaining a full converter is unjustified
//! scope for this task (spec 10 §1 leaves `GenRequest::json_schema`'s exact
//! consumption "advisory": *"a runtime that doesn't [support it] ignores
//! it"*). [`LlamaGenerator`] therefore ignores `json_schema` for v0; output
//! reliability comes from the prompt (`local_rag_memory::prompt`) plus the
//! router's own two-tier malformed-output handling
//! (`local_rag_memory::parse`), not from constrained sampling. A future
//! revision can add a hand-authored GBNF grammar for this task's own fixed op
//! schema without changing the `Generator` contract at all.
//!
//! # The prompt uses the model's own embedded chat template
//!
//! [`GenMessage`]s are rendered against the model's own raw, embedded Jinja
//! chat template (`tokenizer.chat_template` GGUF metadata, read via
//! [`LlamaModel::chat_template`]) through [`crate::chat_template::render`] —
//! a real Jinja interpreter (`minijinja`), not
//! [`LlamaModel::apply_chat_template`]. That method calls the vendored
//! `llama.cpp`'s `llama_chat_apply_template`, which internally uses
//! `llm_chat_detect_template` — a fixed-signature heuristic matcher, not a
//! Jinja interpreter — and does not recognize every model's embedded
//! template text (ADR-0006 needed a one-off `chat_template_override` for
//! Gemma 4 specifically because of this; T14-09 replaced that per-model
//! patch with this general mechanism, `crate::chat_template`'s own module
//! doc has the full trace). An earlier version of this file hand-rolled
//! Qwen2.5-Instruct's ChatML template (`<|im_start|>...`) directly; that
//! happened to work for the Qwen catalog entries but would have silently
//! produced garbled prompts for a model expecting a different template (e.g.
//! Gemma's `<start_of_turn>user\n...<end_of_turn>\n`) — discovered when a
//! second model family was added to the catalog (ADR-0006). A GGUF with no
//! embedded template at all surfaces as a typed `LlamaError::Runtime` rather
//! than silently guessing one, per [`LlamaModel::chat_template`]'s own
//! `MissingTemplate` error.

use std::num::NonZeroU32;
use std::path::{Path, PathBuf};

use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{AddBos, LlamaModel};
use llama_cpp_2::sampling::LlamaSampler;
use llama_cpp_2::token::LlamaToken;

use local_rag_core::paths::StoreLayout;
use local_rag_embed::{
    FinishReason, GenError, GenMessage, GenRequest, GenResponse, Generator, Sampling,
};

use crate::catalog::GeneratorCatalogEntry;
use crate::chat_template::{self, ChatTemplateError};
use crate::install::OK_MARKER;

/// Why loading or running the local generator failed.
#[derive(Debug)]
#[non_exhaustive]
pub enum LlamaError {
    /// The model's assets are not installed yet (spec 10 §5) — the same
    /// `.ok`-marker precondition `local_rag_embed::require_model_assets`
    /// checks for embeddings, applied here for generation.
    AssetsMissing {
        model_id: String,
        expected_path: String,
    },
    /// The prompt plus `max_tokens` exceed the model's context window.
    ContextOverflow {
        requested_tokens: usize,
        max_context_tokens: usize,
    },
    /// [`Sampling::Temperature`] was requested — not implemented (see the
    /// module doc's determinism section).
    UnsupportedSampling,
    /// Rendering the model's chat template (`crate::chat_template::render`)
    /// failed — a bad template, a message sequence the template itself
    /// rejects, or a template bug. Never retryable: waiting will not fix a
    /// template.
    ChatTemplate(ChatTemplateError),
    /// Any other llama.cpp-side failure (backend init, model/context load,
    /// tokenize, batch, decode, detokenize). `llama-cpp-2` gives each of
    /// these its own distinct error type; this crate does not need to branch
    /// on which one occurred, only report the underlying message.
    Runtime(String),
}

impl std::fmt::Display for LlamaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LlamaError::AssetsMissing {
                model_id,
                expected_path,
            } => write!(
                f,
                "model assets for {model_id} are not installed at {expected_path}"
            ),
            LlamaError::ContextOverflow {
                requested_tokens,
                max_context_tokens,
            } => write!(
                f,
                "request needs {requested_tokens} tokens, model context is {max_context_tokens}"
            ),
            LlamaError::UnsupportedSampling => {
                write!(f, "only greedy sampling is implemented")
            }
            LlamaError::ChatTemplate(e) => write!(f, "{e}"),
            LlamaError::Runtime(message) => write!(f, "llama.cpp error: {message}"),
        }
    }
}

impl std::error::Error for LlamaError {}

/// Converting a load/run failure into the `[FIXED]` pool-facing error type
/// (mirrors `impl From<OnnxError> for EmbedError`).
impl From<LlamaError> for GenError {
    fn from(e: LlamaError) -> Self {
        match e {
            LlamaError::AssetsMissing {
                model_id,
                expected_path,
            } => GenError::ModelAssetsMissing {
                model_id,
                expected_path,
            },
            LlamaError::ContextOverflow {
                requested_tokens,
                max_context_tokens,
            } => GenError::ContextOverflow {
                requested_tokens,
                max_context_tokens,
            },
            LlamaError::UnsupportedSampling => GenError::permanent("unsupported sampling mode"),
            LlamaError::ChatTemplate(e) => GenError::permanent(e.to_string()),
            LlamaError::Runtime(message) => GenError::permanent(message),
        }
    }
}

fn runtime_err(e: impl std::fmt::Display) -> LlamaError {
    LlamaError::Runtime(e.to_string())
}

/// A local generator backed by llama.cpp (ADR-0006).
pub struct LlamaGenerator {
    backend: LlamaBackend,
    model: LlamaModel,
    context_length: u32,
    /// See [`GeneratorCatalogEntry::raw_chat_template_override`]'s doc.
    raw_chat_template_override: Option<&'static str>,
}

impl std::fmt::Debug for LlamaGenerator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlamaGenerator")
            .field("context_length", &self.context_length)
            .field(
                "raw_chat_template_override",
                &self.raw_chat_template_override.is_some(),
            )
            .finish_non_exhaustive()
    }
}

impl LlamaGenerator {
    /// Open `entry`'s installed weights under `layout`. Resolves weights
    /// **only** through the `.ok`-marker precondition — never downloads, and
    /// performs no network access of any kind.
    pub fn open(layout: &StoreLayout, entry: &GeneratorCatalogEntry) -> Result<Self, LlamaError> {
        let dir = layout.model_dir(entry.model_id);
        if !dir.join(OK_MARKER).is_file() {
            return Err(LlamaError::AssetsMissing {
                model_id: entry.model_id.to_string(),
                expected_path: dir.display().to_string(),
            });
        }
        let model_path = dir.join(entry.gguf_file().relative_path);
        Self::load_with_template(
            &model_path,
            entry.context_length,
            entry.raw_chat_template_override,
        )
    }

    /// Load a GGUF file directly (tests / non-catalog use), using its own
    /// embedded chat template. Equivalent to
    /// [`Self::load_with_template`]`(model_path, context_length, None)`.
    pub fn load(model_path: &Path, context_length: u32) -> Result<Self, LlamaError> {
        Self::load_with_template(model_path, context_length, None)
    }

    /// Load a GGUF file directly, with an optional literal Jinja
    /// chat-template source overriding the GGUF's own embedded metadata (see
    /// [`GeneratorCatalogEntry::raw_chat_template_override`]).
    pub fn load_with_template(
        model_path: &Path,
        context_length: u32,
        raw_chat_template_override: Option<&'static str>,
    ) -> Result<Self, LlamaError> {
        let mut backend = LlamaBackend::init().map_err(runtime_err)?;
        backend.void_logs();

        let model_params = LlamaModelParams::default();
        let model =
            LlamaModel::load_from_file(&backend, model_path, &model_params).map_err(runtime_err)?;

        Ok(LlamaGenerator {
            backend,
            model,
            context_length,
            raw_chat_template_override,
        })
    }

    /// Convert a special token (BOS/EOS) to the exact string a chat template
    /// expects in its `bos_token`/`eos_token` context variables — never
    /// hardcoded, always read from the model's own vocabulary.
    fn token_text(&self, token: LlamaToken) -> Result<String, LlamaError> {
        let mut decoder = encoding_rs::UTF_8.new_decoder();
        self.model
            .token_to_piece(token, &mut decoder, true, None)
            .map_err(runtime_err)
    }

    /// Render `messages` through the model's own embedded chat template (see
    /// the module doc) using a real Jinja interpreter
    /// (`crate::chat_template::render`), not the vendored `llama.cpp`'s
    /// fixed-signature template detector. Pure rendering logic lives in
    /// `crate::chat_template` and is unit-tested there against real template
    /// strings; this method only supplies what needs a loaded model: the raw
    /// template source and the vocabulary's own BOS/EOS strings.
    fn build_prompt(&self, messages: &[GenMessage]) -> Result<String, LlamaError> {
        let template_source = match self.raw_chat_template_override {
            Some(text) => text.to_string(),
            None => self
                .model
                .chat_template(None)
                .map_err(runtime_err)?
                .to_string()
                .map_err(runtime_err)?,
        };
        let bos = self.token_text(self.model.token_bos())?;
        let eos = self.token_text(self.model.token_eos())?;

        let rendered = chat_template::render(&template_source, messages, &bos, &eos, true)
            .map_err(LlamaError::ChatTemplate)?;
        Ok(chat_template::strip_leading_bos(&rendered, &bos).to_string())
    }

    fn generate_greedy(&self, prompt: &str, max_tokens: u32) -> Result<GenResponse, LlamaError> {
        let tokens = self
            .model
            .str_to_token(prompt, AddBos::Always)
            .map_err(runtime_err)?;

        let requested = tokens.len() + max_tokens as usize;
        if requested > self.context_length as usize {
            return Err(LlamaError::ContextOverflow {
                requested_tokens: requested,
                max_context_tokens: self.context_length as usize,
            });
        }

        let ctx_size = NonZeroU32::new(self.context_length).unwrap_or(NonZeroU32::MIN);
        // `n_batch` bounds how many tokens a single `ctx.decode` call may
        // submit at once -- unrelated to `n_ctx`. The prefill below submits
        // the *entire* prompt in one `decode` call (no chunking), so
        // `n_batch` must cover the largest legal prompt, which the
        // `ContextOverflow` check above already bounds by `context_length`.
        // llama.cpp's own default (2048) is smaller than this model's
        // context window and was silently truncating/aborting on any prompt
        // longer than that (`GGML_ASSERT(n_tokens_all <= cparams.n_batch)`),
        // discovered by T14-07's real benchmark run once a fixture's prompt
        // (plus the one corrective re-prompt's doubled context) exceeded it.
        let ctx_params = LlamaContextParams::default()
            .with_n_ctx(Some(ctx_size))
            .with_n_batch(self.context_length);
        let mut ctx = self
            .model
            .new_context(&self.backend, ctx_params)
            .map_err(runtime_err)?;

        let capacity = tokens.len().max(1) + max_tokens as usize;
        let mut batch = LlamaBatch::new(capacity, 1);
        let last_index = tokens.len() as i32 - 1;
        for (i, token) in (0_i32..).zip(tokens) {
            let is_last = i == last_index;
            batch.add(token, i, &[0], is_last).map_err(runtime_err)?;
        }
        ctx.decode(&mut batch).map_err(runtime_err)?;

        let mut sampler = LlamaSampler::chain_simple([LlamaSampler::greedy()]);
        let mut decoder = encoding_rs::UTF_8.new_decoder();
        let mut output = String::new();
        let mut n_cur = batch.n_tokens();
        let mut generated: u32 = 0;

        loop {
            let sample_index = batch.n_tokens() - 1;
            let token = sampler.sample(&ctx, sample_index);
            if self.model.is_eog_token(token) {
                break;
            }

            let piece = self
                .model
                .token_to_piece(token, &mut decoder, true, None)
                .map_err(runtime_err)?;
            output.push_str(&piece);
            generated += 1;
            if generated >= max_tokens {
                return Ok(GenResponse {
                    text: output,
                    finish_reason: FinishReason::Length,
                    tokens_generated: Some(generated),
                });
            }

            batch.clear();
            batch.add(token, n_cur, &[0], true).map_err(runtime_err)?;
            n_cur += 1;
            ctx.decode(&mut batch).map_err(runtime_err)?;
        }

        Ok(GenResponse {
            text: output,
            finish_reason: FinishReason::Stop,
            tokens_generated: Some(generated),
        })
    }
}

impl Generator for LlamaGenerator {
    fn generate(&self, req: GenRequest) -> Result<GenResponse, GenError> {
        if !matches!(req.sampling, Sampling::Greedy) {
            return Err(LlamaError::UnsupportedSampling.into());
        }
        let prompt = self.build_prompt(&req.messages)?;
        self.generate_greedy(&prompt, req.max_tokens.max(1))
            .map_err(Into::into)
    }
}

/// Where the default catalog entry's GGUF file would live under `layout` —
/// convenience for callers that already checked [`LlamaGenerator::open`]'s
/// precondition themselves.
pub fn default_model_path(layout: &StoreLayout, entry: &GeneratorCatalogEntry) -> PathBuf {
    layout
        .model_dir(entry.model_id)
        .join(entry.gguf_file().relative_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temperature_sampling_is_reported_as_unsupported() {
        let err: GenError = LlamaError::UnsupportedSampling.into();
        assert!(!err.is_retryable(), "{err}");
    }
}
