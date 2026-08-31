//! The `[FIXED]` provider contract (spec 10 §1) and its typed errors.
//!
//! Spec 10 §1 pins the trait shape verbatim:
//!
//! ```text
//! trait Embedder { fn embed(&self, req: EmbedRequest) -> Result<Vec<Vector>>; fn key(&self) -> RepresentationKey; }
//! ```
//!
//! [`Embedder`] here is exactly those two methods — no third method was added.
//! In particular a provider does **not** declare its own locality: whether a
//! provider is local or remote is a property of how the pool is *assembled*
//! (spec 10 §1 gates on it before selection), so it lives on
//! [`ProviderEntry`](crate::ProviderEntry) instead. A provider that could
//! answer "am I remote?" for itself would also be a provider that could lie
//! about it to the guard.
//!
//! `EmbedRequest`/`Vector`/`GenRequest`/`GenResponse` are named but not shaped
//! by the spec, so the shapes below are `[SPEC]` decisions of this task:
//!
//! * a request is a **batch** by construction (the `[FIXED]` return type is
//!   `Vec<Vector>`), carrying the [`RepresentationKind`] the caller wants — the
//!   pool refuses a provider whose `key().kind` differs rather than silently
//!   embedding into the wrong representation;
//! * results are **positional**: `result[i]` is the vector for `texts[i]`, and
//!   `result.len() == texts.len()`. The pool enforces both (a provider that
//!   reorders or drops rows would corrupt `embedding_cache`, whose primary key
//!   is the *subject*, not the position);
//! * errors distinguish **retryable** from **permanent** (the v1 behavioral
//!   contract's central distinction, 01 §7) and carry an optional server-supplied
//!   `retry_after_ms`.
//!
//! # `Generator`/`GenRequest`/`GenResponse` (T14-07, spec 10 §1 `[FIXED]` trait
//! pinned in the same code block as `Embedder`, hence the same crate)
//!
//! Unlike `EmbedRequest`, a generation request is **not** a batch — the
//! consolidation router (spec 08 §4) makes one call per bounded window, not one
//! per text, so [`GenRequest`] carries a chat-style `messages` list (system +
//! few-shot + the window's own content, built by `local_rag_memory::prompt`)
//! rather than a `Vec<String>`. [`Generator`] has no `key()` analog: generation
//! has no [`RepresentationKey`](local_rag_store::RepresentationKey) — nothing
//! about a generated op list is cached by representation, so there is nothing
//! for a pool-level dimension/count contract to validate. [`GenResponse`]
//! carries **raw text**, never parsed JSON — the op-schema/grammar knowledge
//! belongs to the caller (`local_rag_memory`), keeping this crate model-agnostic
//! the same way it stays representation-agnostic for `Embedder`.
//! [`Sampling::Greedy`] is the shipped decode path: "same weights, same runtime
//! build, same input ⇒ same greedy token path" is this task's determinism
//! story — not a claim of cross-platform/thread-count bit-exactness, the same
//! honesty level this codebase already applies to other "byte-stable where it
//! matters" guarantees. [`GenError`] mirrors [`EmbedError`]'s shape
//! (retryable/permanent, policy-blocked, no-provider, model-assets-missing,
//! all-providers-failed) plus one generation-specific variant with no
//! `Embedder` analog: [`GenError::ContextOverflow`].

use std::fmt;

use local_rag_core::config::DataPolicy;
use local_rag_store::{RepresentationKey, RepresentationKind};

/// One embedding vector.
///
/// A newtype rather than a bare `Vec<f32>` so a raw vector cannot be confused
/// with the little-endian *bytes* stored in `embedding_cache`
/// (`local_rag_store::encode_vector_le`, spec 03 §4.2).
#[derive(Debug, Clone, PartialEq)]
pub struct Vector(Vec<f32>);

impl Vector {
    /// Wrap raw components.
    pub fn new(components: Vec<f32>) -> Self {
        Vector(components)
    }

    /// The vector's dimensionality.
    pub fn dimensions(&self) -> usize {
        self.0.len()
    }

    /// The components.
    pub fn as_slice(&self) -> &[f32] {
        &self.0
    }

    /// Consume the wrapper, yielding the components.
    pub fn into_inner(self) -> Vec<f32> {
        self.0
    }
}

/// A batch embedding request (spec 10 §1's `EmbedRequest`, shape `[SPEC]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbedRequest {
    /// Which representation the caller is filling (spec 10 §2's kinds).
    pub kind: RepresentationKind,
    /// The texts to embed, in order. `result[i]` corresponds to `texts[i]`.
    pub texts: Vec<String>,
}

impl EmbedRequest {
    /// A request for `kind` over `texts`.
    pub fn new(kind: RepresentationKind, texts: Vec<String>) -> Self {
        EmbedRequest { kind, texts }
    }

    /// Number of texts in the batch.
    pub fn len(&self) -> usize {
        self.texts.len()
    }

    /// Whether the batch is empty.
    pub fn is_empty(&self) -> bool {
        self.texts.is_empty()
    }
}

/// One provider's terminal outcome, kept for the pool's diagnostic when every
/// candidate fails (spec 02 §6: "nothing degrades silently").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderFailure {
    /// The provider's name in the pool.
    pub provider: String,
    /// How many attempts were made against it.
    pub attempts: u32,
    /// The last error message it produced.
    pub message: String,
    /// D-057: whether the last error was [`GenError::ContextOverflow`] —
    /// always `false` for `Embedder`'s own failures, which have no context-
    /// window analog. Lets [`GenError::is_deterministic_context_overflow`]
    /// see through [`GenError::AllProvidersFailed`]'s flattened `message`
    /// strings without string-sniffing `Display` text.
    pub context_overflow: bool,
}

impl fmt::Display for ProviderFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} (after {} attempt{}): {}",
            self.provider,
            self.attempts,
            if self.attempts == 1 { "" } else { "s" },
            self.message
        )
    }
}

/// A typed embedding failure.
///
/// [`EmbedError::Retryable`] vs [`EmbedError::Permanent`] is the provider's own
/// classification of *its* failure; the pool decides what to do with it
/// (retry the same provider, or move to the next one). The remaining variants
/// are produced by the pool/guard, never by a provider.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum EmbedError {
    /// A transient failure (5xx, 429, connection reset, model busy). The pool
    /// retries the same provider up to the policy's attempt budget.
    Retryable {
        /// Human-readable cause.
        message: String,
        /// A server-supplied delay hint (v1 honored `Retry-After` and the
        /// "retry in Xs" body hint), in milliseconds.
        retry_after_ms: Option<u64>,
    },
    /// A non-transient failure (4xx, malformed input, unusable model). The pool
    /// never retries it and moves to the next provider.
    Permanent {
        /// Human-readable cause.
        message: String,
    },
    /// The effective `data_policy` forbids every remaining candidate because
    /// they are remote (spec 12 §1, 02 §6 `POLICY_BLOCKED_REMOTE`).
    PolicyBlockedRemote {
        /// The effective policy that blocked selection.
        policy: DataPolicy,
        /// The remote providers that were refused, in pool order.
        blocked: Vec<String>,
    },
    /// The pool holds no provider for the requested kind at all (distinct from
    /// "all of them failed" and from "policy blocked them").
    NoProvider {
        /// The requested representation kind.
        kind: RepresentationKind,
    },
    /// A local provider's model assets are not installed yet (spec 10 §5 —
    /// `local-rag init --download-models`, T11-06).
    ModelAssetsMissing {
        /// The `model_id` whose assets are missing.
        model_id: String,
        /// Where they were expected (`<store>/models/<model_id>`).
        expected_path: String,
    },
    /// A provider returned a vector whose length contradicts its own
    /// `key().dimensions` (spec 03 §2.2).
    DimensionMismatch {
        /// The provider that produced it.
        provider: String,
        /// Dimensions declared by `key()`.
        expected: u32,
        /// Dimensions actually returned.
        actual: usize,
        /// Index within the batch.
        index: usize,
    },
    /// A provider returned a different number of vectors than texts requested.
    ResultCountMismatch {
        /// The provider that produced it.
        provider: String,
        /// Texts requested.
        expected: usize,
        /// Vectors returned.
        actual: usize,
    },
    /// Every allowed provider failed; carries each one's terminal outcome in
    /// pool order.
    AllProvidersFailed {
        /// Per-provider diagnostics, primary first.
        failures: Vec<ProviderFailure>,
    },
}

impl EmbedError {
    /// A transient failure with no server-supplied delay hint.
    pub fn retryable(message: impl Into<String>) -> Self {
        EmbedError::Retryable {
            message: message.into(),
            retry_after_ms: None,
        }
    }

    /// A transient failure with a server-supplied delay hint.
    pub fn retryable_after(message: impl Into<String>, retry_after_ms: u64) -> Self {
        EmbedError::Retryable {
            message: message.into(),
            retry_after_ms: Some(retry_after_ms),
        }
    }

    /// A non-transient failure.
    pub fn permanent(message: impl Into<String>) -> Self {
        EmbedError::Permanent {
            message: message.into(),
        }
    }

    /// Whether the pool may retry the *same* provider for this error.
    pub fn is_retryable(&self) -> bool {
        matches!(self, EmbedError::Retryable { .. })
    }
}

impl fmt::Display for EmbedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EmbedError::Retryable {
                message,
                retry_after_ms,
            } => match retry_after_ms {
                Some(ms) => write!(
                    f,
                    "transient embedding failure (retry after {ms} ms): {message}"
                ),
                None => write!(f, "transient embedding failure: {message}"),
            },
            EmbedError::Permanent { message } => {
                write!(f, "permanent embedding failure: {message}")
            }
            EmbedError::PolicyBlockedRemote { policy, blocked } => write!(
                f,
                "data_policy {} forbids remote providers [{}]",
                policy.as_str(),
                blocked.join(", ")
            ),
            EmbedError::NoProvider { kind } => {
                write!(f, "no embedding provider for kind {}", kind.as_str())
            }
            EmbedError::ModelAssetsMissing {
                model_id,
                expected_path,
            } => write!(
                f,
                "model assets for {model_id} are not installed at {expected_path}"
            ),
            EmbedError::DimensionMismatch {
                provider,
                expected,
                actual,
                index,
            } => write!(
                f,
                "{provider} returned {actual} dimensions at index {index}, its key declares {expected}"
            ),
            EmbedError::ResultCountMismatch {
                provider,
                expected,
                actual,
            } => write!(
                f,
                "{provider} returned {actual} vectors for {expected} texts"
            ),
            EmbedError::AllProvidersFailed { failures } => {
                write!(f, "all embedding providers failed: ")?;
                for (i, failure) in failures.iter().enumerate() {
                    if i > 0 {
                        write!(f, "; ")?;
                    }
                    write!(f, "{failure}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for EmbedError {}

/// The `[FIXED]` embedding provider contract (spec 10 §1).
///
/// Implementations must be positional (`result[i]` embeds `texts[i]`) and must
/// return vectors of exactly `key().dimensions` components; the pool verifies
/// both and turns a violation into [`EmbedError::ResultCountMismatch`] /
/// [`EmbedError::DimensionMismatch`] rather than letting a malformed vector
/// reach `embedding_cache`.
///
/// `embed` is synchronous, matching the spec signature. Providers that block
/// (local inference, network) are expected to be driven from a blocking worker
/// (T11-04's backfill), not from inside an async reactor.
pub trait Embedder: Send + Sync {
    /// Embed a batch.
    fn embed(&self, req: EmbedRequest) -> Result<Vec<Vector>, EmbedError>;

    /// The canonical six-field representation key this provider produces
    /// (spec 03 §2.2 / 10 §2).
    fn key(&self) -> RepresentationKey;
}

// ---------------------------------------------------------------------------
// Generator / GenRequest / GenResponse (T14-07, spec 10 §1 `[FIXED]`). See the
// module doc's "Generator/GenRequest/GenResponse" section for the shape
// rationale.
// ---------------------------------------------------------------------------

/// One message in a [`GenRequest`]'s chat-style input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenMessage {
    pub role: GenRole,
    pub content: String,
}

/// Who a [`GenMessage`] is attributed to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenRole {
    /// Instructions/placement rules/few-shot framing (`local_rag_memory::prompt`).
    System,
    /// The window's own content — what the router is asking to be classified.
    User,
    /// A prior turn's output, for multi-turn corrective re-prompting
    /// (`local_rag_memory::parse`'s bounded "that wasn't valid JSON" retry).
    Assistant,
}

/// How [`Generator::generate`] decodes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Sampling {
    /// Deterministic (greedy) decoding — the shipped path; see the module
    /// doc's determinism note.
    Greedy,
    /// Sampled decoding with an explicit seed. Not the default: a router that
    /// must be reproducible for the memory-quality benchmark (spec 08 §7)
    /// has no reason to sample.
    Temperature { temperature: f32, seed: u64 },
}

/// A generation request (spec 10 §1's `GenRequest`, shape `[SPEC]`).
#[derive(Debug, Clone, PartialEq)]
pub struct GenRequest {
    /// Chat-style input, in order.
    pub messages: Vec<GenMessage>,
    /// Upper bound on generated tokens (a malformed/looping model must not run
    /// unbounded).
    pub max_tokens: u32,
    pub sampling: Sampling,
    /// A JSON Schema the caller wants the output constrained to. A runtime
    /// that supports grammar-constrained decoding compiles this into its own
    /// grammar; a runtime that doesn't ignores it — the field is advisory, not
    /// part of the `[FIXED]` contract, so every `Generator` impl stays valid
    /// whether or not it honors it.
    pub json_schema: Option<String>,
}

impl GenRequest {
    /// A request over `messages`, greedy-decoded, with no schema constraint.
    pub fn new(messages: Vec<GenMessage>, max_tokens: u32) -> Self {
        GenRequest {
            messages,
            max_tokens,
            sampling: Sampling::Greedy,
            json_schema: None,
        }
    }

    /// Constrain output to `json_schema` (builder-style).
    pub fn with_json_schema(mut self, json_schema: impl Into<String>) -> Self {
        self.json_schema = Some(json_schema.into());
        self
    }
}

/// Why generation stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinishReason {
    /// The model emitted its own end-of-turn signal.
    Stop,
    /// `max_tokens` was reached before the model finished.
    Length,
    /// A runtime-specific stop condition with no `[FIXED]` name of its own.
    Other(String),
}

/// A generation response (spec 10 §1's `GenResponse`, shape `[SPEC]`).
///
/// Carries **raw text** — this crate does not parse it. Structured-output
/// parsing/validation is `local_rag_memory::parse`'s concern, keeping the
/// op-schema knowledge in exactly one place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenResponse {
    pub text: String,
    pub finish_reason: FinishReason,
    /// `None` when the provider does not report a token count.
    pub tokens_generated: Option<u32>,
}

/// A typed generation failure — mirrors [`EmbedError`]'s shape (see the module
/// doc); [`GenError::ContextOverflow`] is the one variant with no `Embedder`
/// analog.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum GenError {
    /// A transient failure. The pool retries the same provider up to the
    /// policy's attempt budget.
    Retryable {
        message: String,
        retry_after_ms: Option<u64>,
    },
    /// A non-transient failure. The pool never retries it and moves to the
    /// next provider.
    Permanent { message: String },
    /// The effective `data_policy` forbids every remaining candidate because
    /// they are remote (spec 12 §1, 02 §6 `POLICY_BLOCKED_REMOTE`).
    PolicyBlockedRemote {
        policy: DataPolicy,
        blocked: Vec<String>,
    },
    /// The pool holds no generator at all.
    NoProvider,
    /// A local provider's model assets are not installed yet (spec 10 §5).
    ModelAssetsMissing {
        model_id: String,
        expected_path: String,
    },
    /// The request's `messages` (plus expected output) exceed the model's
    /// context window — a failure mode with no `Embedder` analog, since an
    /// embedding batch has no comparable single-sequence limit.
    ContextOverflow {
        requested_tokens: usize,
        max_context_tokens: usize,
    },
    /// Every allowed provider failed; carries each one's terminal outcome in
    /// pool order.
    AllProvidersFailed { failures: Vec<ProviderFailure> },
}

impl GenError {
    /// A transient failure with no server-supplied delay hint.
    pub fn retryable(message: impl Into<String>) -> Self {
        GenError::Retryable {
            message: message.into(),
            retry_after_ms: None,
        }
    }

    /// A non-transient failure.
    pub fn permanent(message: impl Into<String>) -> Self {
        GenError::Permanent {
            message: message.into(),
        }
    }

    /// Whether the pool may retry the *same* provider for this error.
    pub fn is_retryable(&self) -> bool {
        matches!(self, GenError::Retryable { .. })
    }

    /// D-057: whether this failure is guaranteed to reproduce byte-for-byte
    /// on an unchanged retry — the request's token count does not change
    /// between attempts, so a context overflow can never be resolved by
    /// waiting and trying again (unlike every other [`GenError`] variant,
    /// which may reflect transient infra/network conditions).
    ///
    /// `true` for the bare [`GenError::ContextOverflow`] variant, and for
    /// [`GenError::AllProvidersFailed`] only when *every* contained
    /// [`ProviderFailure`] is itself tagged `context_overflow` — a provider
    /// that failed for a different, possibly-transient reason must not be
    /// swallowed into "deterministic" just because another provider in the
    /// same pool happened to overflow.
    pub fn is_deterministic_context_overflow(&self) -> bool {
        match self {
            GenError::ContextOverflow { .. } => true,
            GenError::AllProvidersFailed { failures } => {
                !failures.is_empty() && failures.iter().all(|f| f.context_overflow)
            }
            _ => false,
        }
    }
}

impl fmt::Display for GenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GenError::Retryable {
                message,
                retry_after_ms,
            } => match retry_after_ms {
                Some(ms) => write!(
                    f,
                    "transient generation failure (retry after {ms} ms): {message}"
                ),
                None => write!(f, "transient generation failure: {message}"),
            },
            GenError::Permanent { message } => {
                write!(f, "permanent generation failure: {message}")
            }
            GenError::PolicyBlockedRemote { policy, blocked } => write!(
                f,
                "data_policy {} forbids remote providers [{}]",
                policy.as_str(),
                blocked.join(", ")
            ),
            GenError::NoProvider => write!(f, "no generation provider configured"),
            GenError::ModelAssetsMissing {
                model_id,
                expected_path,
            } => write!(
                f,
                "model assets for {model_id} are not installed at {expected_path}"
            ),
            GenError::ContextOverflow {
                requested_tokens,
                max_context_tokens,
            } => write!(
                f,
                "request needs {requested_tokens} tokens, model context is {max_context_tokens}"
            ),
            GenError::AllProvidersFailed { failures } => {
                write!(f, "all generation providers failed: ")?;
                for (i, failure) in failures.iter().enumerate() {
                    if i > 0 {
                        write!(f, "; ")?;
                    }
                    write!(f, "{failure}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for GenError {}

/// The `[FIXED]` generation provider contract (spec 10 §1).
///
/// `generate` is synchronous, matching the spec signature and mirroring
/// [`Embedder::embed`] — a provider that blocks (local inference) is expected
/// to be driven from a blocking worker
/// (`local_rag_store::memory::runner::run_once`'s generator closure runs
/// outside any transaction for exactly this reason), not from inside an async
/// reactor.
pub trait Generator: Send + Sync {
    fn generate(&self, req: GenRequest) -> Result<GenResponse, GenError>;

    /// How many tokens this provider's own tokenizer makes of `req`'s prompt,
    /// when it can say (`D-125`).
    ///
    /// The question is asked about a whole [`GenRequest`], not about a string,
    /// so the answer counts what the provider will really submit — chat
    /// template, role markers, BOS and all. Counting the raw message texts
    /// instead would leave the template's own tokens unaccounted, which is the
    /// same "close enough" reasoning that produced the defect below.
    /// `max_tokens` is *not* included: it is the caller's reserve to add.
    ///
    /// Defaulted to `None` so the `[FIXED]` contract above is unchanged for
    /// every existing provider: a remote endpoint that bills in tokens it
    /// never reveals, and every test double, keep answering "I cannot tell
    /// you". A caller that needs a bound must stay conservative on `None`
    /// rather than assume the prompt fits.
    ///
    /// The point of asking at all is that the estimate everything else uses
    /// (`local_rag_memory::recall::pipeline::estimate_tokens`, four characters
    /// per token) was measured to undercount real prompt text by roughly half,
    /// which is how a window of one observation came to ask for 41 720 tokens
    /// of a 32 768-token context. A provider that owns a tokenizer can answer
    /// exactly, and `local_rag_memory::router` cuts the conflict set to what
    /// actually fits instead of to what was guessed.
    fn count_prompt_tokens(&self, req: &GenRequest) -> Option<usize> {
        let _ = req;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vector_reports_its_own_dimensions() {
        let v = Vector::new(vec![0.5, -0.5, 0.0]);
        assert_eq!(v.dimensions(), 3);
        assert_eq!(v.as_slice()[1], -0.5);
        assert_eq!(v.clone().into_inner(), vec![0.5, -0.5, 0.0]);
    }

    #[test]
    fn only_the_retryable_variant_is_retryable() {
        assert!(EmbedError::retryable("500").is_retryable());
        assert!(EmbedError::retryable_after("503", 250).is_retryable());
        assert!(!EmbedError::permanent("400").is_retryable());
        assert!(
            !EmbedError::NoProvider {
                kind: RepresentationKind::CodeRaw
            }
            .is_retryable()
        );
    }

    #[test]
    fn errors_render_their_cause() {
        assert_eq!(
            EmbedError::retryable_after("upstream busy", 1_000).to_string(),
            "transient embedding failure (retry after 1000 ms): upstream busy"
        );
        assert_eq!(
            EmbedError::PolicyBlockedRemote {
                policy: DataPolicy::LocalOnly,
                blocked: vec!["ollama".to_string(), "hosted".to_string()],
            }
            .to_string(),
            "data_policy local_only forbids remote providers [ollama, hosted]"
        );
        assert_eq!(
            EmbedError::AllProvidersFailed {
                failures: vec![
                    ProviderFailure {
                        provider: "local".to_string(),
                        attempts: 1,
                        message: "assets missing".to_string(),
                        context_overflow: false,
                    },
                    ProviderFailure {
                        provider: "backup".to_string(),
                        attempts: 3,
                        message: "500".to_string(),
                        context_overflow: false,
                    },
                ],
            }
            .to_string(),
            "all embedding providers failed: local (after 1 attempt): assets missing; \
             backup (after 3 attempts): 500"
        );
    }

    #[test]
    fn request_reports_batch_size() {
        let req = EmbedRequest::new(RepresentationKind::CodeRaw, vec!["a".into(), "b".into()]);
        assert_eq!(req.len(), 2);
        assert!(!req.is_empty());
        assert!(EmbedRequest::new(RepresentationKind::Memory, Vec::new()).is_empty());
    }

    #[test]
    fn gen_request_defaults_to_greedy_with_no_schema() {
        let req = GenRequest::new(
            vec![GenMessage {
                role: GenRole::User,
                content: "classify this".to_string(),
            }],
            64,
        );
        assert_eq!(req.sampling, Sampling::Greedy);
        assert_eq!(req.json_schema, None);
        assert_eq!(req.max_tokens, 64);
    }

    #[test]
    fn gen_request_with_json_schema_sets_the_schema() {
        let req = GenRequest::new(Vec::new(), 32).with_json_schema("{\"type\":\"array\"}");
        assert_eq!(req.json_schema.as_deref(), Some("{\"type\":\"array\"}"));
    }

    #[test]
    fn only_the_retryable_gen_error_variant_is_retryable() {
        assert!(GenError::retryable("500").is_retryable());
        assert!(!GenError::permanent("400").is_retryable());
        assert!(!GenError::NoProvider.is_retryable());
    }

    #[test]
    fn gen_errors_render_their_cause() {
        assert_eq!(
            GenError::retryable("upstream busy").to_string(),
            "transient generation failure: upstream busy"
        );
        assert_eq!(
            GenError::PolicyBlockedRemote {
                policy: DataPolicy::LocalOnly,
                blocked: vec!["hosted-llm".to_string()],
            }
            .to_string(),
            "data_policy local_only forbids remote providers [hosted-llm]"
        );
        assert_eq!(
            GenError::ContextOverflow {
                requested_tokens: 5_000,
                max_context_tokens: 4_096,
            }
            .to_string(),
            "request needs 5000 tokens, model context is 4096"
        );
        assert_eq!(
            GenError::AllProvidersFailed {
                failures: vec![ProviderFailure {
                    provider: "llama-local".to_string(),
                    attempts: 2,
                    message: "context overflow".to_string(),
                    context_overflow: true,
                }],
            }
            .to_string(),
            "all generation providers failed: llama-local (after 2 attempts): context overflow"
        );
    }

    #[test]
    fn bare_context_overflow_is_deterministic() {
        assert!(
            GenError::ContextOverflow {
                requested_tokens: 36_269,
                max_context_tokens: 32_768,
            }
            .is_deterministic_context_overflow()
        );
    }

    #[test]
    fn all_providers_failed_is_deterministic_only_when_every_failure_overflowed() {
        let overflow = |provider: &str| ProviderFailure {
            provider: provider.to_string(),
            attempts: 1,
            message: "request needs 36269 tokens, model context is 32768".to_string(),
            context_overflow: true,
        };
        let other = |provider: &str| ProviderFailure {
            provider: provider.to_string(),
            attempts: 1,
            message: "500".to_string(),
            context_overflow: false,
        };

        assert!(
            GenError::AllProvidersFailed {
                failures: vec![overflow("a"), overflow("b")],
            }
            .is_deterministic_context_overflow(),
            "every provider overflowed"
        );
        assert!(
            !GenError::AllProvidersFailed {
                failures: vec![overflow("a"), other("b")],
            }
            .is_deterministic_context_overflow(),
            "a non-overflow failure might still succeed on retry — must not be swallowed"
        );
        assert!(
            !GenError::AllProvidersFailed { failures: vec![] }.is_deterministic_context_overflow(),
            "never vacuously true on an empty list"
        );
    }

    #[test]
    fn every_other_gen_error_variant_is_not_deterministic_context_overflow() {
        assert!(!GenError::NoProvider.is_deterministic_context_overflow());
        assert!(!GenError::retryable("busy").is_deterministic_context_overflow());
        assert!(!GenError::permanent("400").is_deterministic_context_overflow());
        assert!(
            !GenError::ModelAssetsMissing {
                model_id: "gemma".to_string(),
                expected_path: "/models/gemma".to_string(),
            }
            .is_deterministic_context_overflow()
        );
        assert!(
            !GenError::PolicyBlockedRemote {
                policy: DataPolicy::LocalOnly,
                blocked: vec!["hosted".to_string()],
            }
            .is_deterministic_context_overflow()
        );
    }
}
