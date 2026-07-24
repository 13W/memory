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
                    },
                    ProviderFailure {
                        provider: "backup".to_string(),
                        attempts: 3,
                        message: "500".to_string(),
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
}
