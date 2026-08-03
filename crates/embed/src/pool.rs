//! The ordered provider pool: policy guard → primary/fallback → retry.
//!
//! Spec 10 §1 `[FIXED]` names three behaviors and this module implements them in
//! exactly that order:
//!
//! 1. **guard first** — `allows()` filters candidates *before* selection, so a
//!    remote provider under `local_only` is never even attempted, let alone used
//!    as a fallback (spec 12 §1);
//! 2. **primary/fallback** — providers are tried in pool order; the first one to
//!    answer wins, and a permanently failing provider yields to the next;
//! 3. **retry** — a *transient* failure retries the **same** provider before the
//!    pool moves on, honoring a server-supplied delay hint when present.
//!
//! ## Where the retry semantics come from
//!
//! Spec 10 §1 says only "primary/fallback + retry semantics inherited from the
//! v1 behavioral contract `[FIXED]`" — it pins no numbers, and 01 §7 only
//! confirms the contract exists. The observable v1 behavior was imported by
//! T00-01 as `fixtures/fault/index.json`, family `fault.llm.*`: retry on 500 /
//! 503 / 429 / network error, honor `Retry-After` (and the "retry in Xs" body
//! hint) as the delay, never retry a 400, and fail after exhausting the attempt
//! budget. `crates/embed/tests/retry.rs` replays all seven cases.
//!
//! The *numbers* are therefore this task's `[SPEC]` decision:
//! [`DEFAULT_RETRY_MAX_ATTEMPTS`] = 4 is the attempt budget the v1 fixtures
//! themselves use (`max_attempts: 4`), and the exponential floor
//! ([`DEFAULT_RETRY_BASE_MS`] = 250 ms, doubling, capped at
//! [`DEFAULT_RETRY_MAX_MS`] = 4 s) reuses the shape spec 02 §4.2 already fixed
//! for the proxy handshake — chosen deliberately, not copied silently: both are
//! short, user-facing operations where a minute-scale backoff (like
//! `crates/index`'s reconcile retry) would look like a hang.

use std::sync::Arc;
use std::time::Duration;

use local_rag_core::config::DataPolicy;
use local_rag_core::redaction::Scanner;
use local_rag_store::RepresentationKind;

use crate::contract::{EmbedError, EmbedRequest, Embedder, ProviderFailure, Vector};
use crate::policy::{Locality, allows};

/// Transform `req` for transmission to a provider of `locality` under
/// `policy` (spec 12 §1's "metadata-only/redaction/full semantics",
/// `crate::policy`'s own module doc for the as-built matrix). `None` means
/// "send `req` unchanged" — the common case (every local call, and every
/// `allow_remote_full` remote call). Only `allow_remote_with_redaction`
/// against a `Remote` entry redacts: local providers never have their input
/// rewritten, and `allows()` already keeps `local_only`/`metadata_only_remote`
/// from ever reaching a remote entry at all.
fn redact_for_transmission(
    policy: DataPolicy,
    locality: Locality,
    req: &EmbedRequest,
) -> Option<EmbedRequest> {
    if locality != Locality::Remote || policy != DataPolicy::AllowRemoteWithRedaction {
        return None;
    }
    let scanner = Scanner::new();
    Some(EmbedRequest {
        kind: req.kind,
        texts: req.texts.iter().map(|t| scanner.redact(t).text).collect(),
    })
}

/// Attempts per provider before the pool falls back (`[SPEC]`, matches the v1
/// fixtures' own `max_attempts`).
pub const DEFAULT_RETRY_MAX_ATTEMPTS: u32 = 4;

/// The exponential backoff floor between attempts, in milliseconds (`[SPEC]`).
pub const DEFAULT_RETRY_BASE_MS: u64 = 250;

/// The cap on any single backoff, in milliseconds (`[SPEC]`). Also caps a
/// server-supplied `Retry-After`, so a hostile or misconfigured provider cannot
/// park a worker thread indefinitely.
pub const DEFAULT_RETRY_MAX_MS: u64 = 4_000;

/// The retry budget applied to each provider independently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Total attempts per provider (1 = no retry).
    pub max_attempts: u32,
    /// Exponential backoff base in milliseconds.
    pub base_ms: u64,
    /// Cap on any single delay in milliseconds.
    pub max_ms: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy {
            max_attempts: DEFAULT_RETRY_MAX_ATTEMPTS,
            base_ms: DEFAULT_RETRY_BASE_MS,
            max_ms: DEFAULT_RETRY_MAX_MS,
        }
    }
}

/// The delay before attempt `next_attempt` (1-based: the delay *after* the first
/// failure is `next_attempt = 2`).
///
/// A server-supplied `retry_after_ms` wins over the exponential floor (v1 honored
/// `Retry-After` and the body hint), but is still capped by `policy.max_ms`.
/// Pure and saturating: the exponent is clamped so the shift cannot overflow.
pub fn retry_delay_ms(policy: RetryPolicy, next_attempt: u32, retry_after_ms: Option<u64>) -> u64 {
    if let Some(hint) = retry_after_ms {
        return hint.min(policy.max_ms);
    }
    if next_attempt <= 1 {
        return 0;
    }
    let shift = (next_attempt - 2).min(31);
    policy
        .base_ms
        .saturating_mul(1_u64 << shift)
        .min(policy.max_ms)
}

/// How the pool waits between attempts.
///
/// A seam, not a convenience: the Definition of Done forbids wall-clock sleeps
/// in tests, so tests install a recording sleeper and assert the *computed*
/// delays instead of living through them.
pub trait Sleeper: Send + Sync {
    /// Block the current thread for `ms` milliseconds.
    fn sleep_ms(&self, ms: u64);
}

/// The production sleeper: `std::thread::sleep`.
#[derive(Debug, Default, Clone, Copy)]
pub struct ThreadSleeper;

impl Sleeper for ThreadSleeper {
    fn sleep_ms(&self, ms: u64) {
        if ms > 0 {
            std::thread::sleep(Duration::from_millis(ms));
        }
    }
}

/// One provider in the pool, with the locality the guard reads.
#[derive(Clone)]
pub struct ProviderEntry {
    name: String,
    locality: Locality,
    provider: Arc<dyn Embedder>,
}

impl ProviderEntry {
    /// A local (in-process) provider — spec 10 §1's working default.
    pub fn local(name: impl Into<String>, provider: Arc<dyn Embedder>) -> Self {
        ProviderEntry {
            name: name.into(),
            locality: Locality::Local,
            provider,
        }
    }

    /// A remote provider (hosted API or external daemon such as Ollama) —
    /// "strictly optional" per spec 10 §1, and gated by the policy guard.
    pub fn remote(name: impl Into<String>, provider: Arc<dyn Embedder>) -> Self {
        ProviderEntry {
            name: name.into(),
            locality: Locality::Remote,
            provider,
        }
    }

    /// The provider's pool name (diagnostics only, never an identity).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Where this provider runs.
    pub fn locality(&self) -> Locality {
        self.locality
    }
}

impl std::fmt::Debug for ProviderEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderEntry")
            .field("name", &self.name)
            .field("locality", &self.locality)
            .field("key", &self.provider.key())
            .finish()
    }
}

/// An ordered, local-first embedding provider pool (spec 10 §1).
pub struct ProviderPool {
    entries: Vec<ProviderEntry>,
    retry: RetryPolicy,
    sleeper: Arc<dyn Sleeper>,
}

impl ProviderPool {
    /// A pool over `entries`, tried in the given order (primary first).
    pub fn new(entries: Vec<ProviderEntry>) -> Self {
        ProviderPool {
            entries,
            retry: RetryPolicy::default(),
            sleeper: Arc::new(ThreadSleeper),
        }
    }

    /// Override the retry budget.
    pub fn with_retry_policy(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Override how the pool waits between attempts (tests inject a recorder).
    pub fn with_sleeper(mut self, sleeper: Arc<dyn Sleeper>) -> Self {
        self.sleeper = sleeper;
        self
    }

    /// The retry budget in force.
    pub fn retry_policy(&self) -> RetryPolicy {
        self.retry
    }

    /// Provider names in pool order.
    pub fn provider_names(&self) -> Vec<&str> {
        self.entries.iter().map(|e| e.name.as_str()).collect()
    }

    /// Providers allowed under `policy` for `kind`, in pool order — the guard's
    /// decision, exposed so callers (and tests) can inspect selection without
    /// performing an embedding.
    pub fn allowed_for(&self, policy: DataPolicy, kind: RepresentationKind) -> Vec<&ProviderEntry> {
        self.entries
            .iter()
            .filter(|e| e.provider.key().kind == kind)
            .filter(|e| allows(policy, e.locality))
            .collect()
    }

    /// Embed a batch under the **effective** `data_policy` (02 §3.2; compute it
    /// with `local_rag_store::effective_data_policy`, which folds in every
    /// involved repository's stricter setting — this pool never relaxes it).
    ///
    /// Order of operations is `[FIXED]` (spec 10 §1, 12 §1): the policy guard
    /// runs *before* provider selection, so a refused remote provider is never
    /// invoked.
    pub fn embed(&self, policy: DataPolicy, req: EmbedRequest) -> Result<Vec<Vector>, EmbedError> {
        // An empty batch cannot send anything anywhere; answering it without
        // selecting a provider keeps callers from having to special-case it.
        if req.is_empty() {
            return Ok(Vec::new());
        }

        let for_kind: Vec<&ProviderEntry> = self
            .entries
            .iter()
            .filter(|e| e.provider.key().kind == req.kind)
            .collect();
        if for_kind.is_empty() {
            return Err(EmbedError::NoProvider { kind: req.kind });
        }

        // Guard before selection (spec 10 §1 / 12 §1).
        let (allowed, blocked): (Vec<&ProviderEntry>, Vec<&ProviderEntry>) = for_kind
            .into_iter()
            .partition(|e| allows(policy, e.locality));
        if allowed.is_empty() {
            return Err(EmbedError::PolicyBlockedRemote {
                policy,
                blocked: blocked.iter().map(|e| e.name.clone()).collect(),
            });
        }

        let mut failures = Vec::new();
        for entry in allowed {
            let redacted = redact_for_transmission(policy, entry.locality, &req);
            let req_for_entry = redacted.as_ref().unwrap_or(&req);
            match self.try_provider(entry, req_for_entry) {
                Ok(vectors) => return Ok(vectors),
                Err(ProviderOutcome::Contract(err)) => return Err(err),
                Err(ProviderOutcome::Failed(failure)) => failures.push(failure),
            }
        }
        Err(EmbedError::AllProvidersFailed { failures })
    }

    /// Attempt one provider up to the retry budget.
    fn try_provider(
        &self,
        entry: &ProviderEntry,
        req: &EmbedRequest,
    ) -> Result<Vec<Vector>, ProviderOutcome> {
        let mut attempts = 0;
        let mut last = String::new();
        while attempts < self.retry.max_attempts {
            attempts += 1;
            match entry.provider.embed(req.clone()) {
                Ok(vectors) => {
                    return validate(entry, req, vectors).map_err(ProviderOutcome::Contract);
                }
                Err(err) => {
                    last = err.to_string();
                    let retry_after = match &err {
                        EmbedError::Retryable { retry_after_ms, .. } => *retry_after_ms,
                        // Not retryable: fall back to the next provider now.
                        _ => {
                            return Err(ProviderOutcome::Failed(ProviderFailure {
                                provider: entry.name.clone(),
                                attempts,
                                message: last,
                            }));
                        }
                    };
                    if attempts < self.retry.max_attempts {
                        self.sleeper.sleep_ms(retry_delay_ms(
                            self.retry,
                            attempts + 1,
                            retry_after,
                        ));
                    }
                }
            }
        }
        Err(ProviderOutcome::Failed(ProviderFailure {
            provider: entry.name.clone(),
            attempts,
            message: last,
        }))
    }
}

impl std::fmt::Debug for ProviderPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderPool")
            .field("entries", &self.entries)
            .field("retry", &self.retry)
            .finish_non_exhaustive()
    }
}

/// Why a provider attempt ended.
enum ProviderOutcome {
    /// The provider broke its own contract (wrong count/dimensions). Surfaced
    /// immediately rather than falling back: a silent fallback would hide a
    /// defect that corrupts `embedding_cache` (spec 02 §6, "nothing degrades
    /// silently").
    Contract(EmbedError),
    /// The provider failed; try the next one.
    Failed(ProviderFailure),
}

/// Enforce the positional/dimensional contract a provider promises via `key()`.
fn validate(
    entry: &ProviderEntry,
    req: &EmbedRequest,
    vectors: Vec<Vector>,
) -> Result<Vec<Vector>, EmbedError> {
    if vectors.len() != req.texts.len() {
        return Err(EmbedError::ResultCountMismatch {
            provider: entry.name.clone(),
            expected: req.texts.len(),
            actual: vectors.len(),
        });
    }
    let expected = entry.provider.key().dimensions;
    for (index, vector) in vectors.iter().enumerate() {
        if vector.dimensions() != expected as usize {
            return Err(EmbedError::DimensionMismatch {
                provider: entry.name.clone(),
                expected,
                actual: vector.dimensions(),
                index,
            });
        }
    }
    Ok(vectors)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_from_the_base_and_caps() {
        let p = RetryPolicy::default();
        assert_eq!(retry_delay_ms(p, 1, None), 0, "no delay before attempt 1");
        assert_eq!(retry_delay_ms(p, 2, None), 250);
        assert_eq!(retry_delay_ms(p, 3, None), 500);
        assert_eq!(retry_delay_ms(p, 4, None), 1_000);
        assert_eq!(retry_delay_ms(p, 5, None), 2_000);
        assert_eq!(retry_delay_ms(p, 6, None), 4_000);
        assert_eq!(retry_delay_ms(p, 7, None), 4_000, "capped");
        assert_eq!(retry_delay_ms(p, u32::MAX, None), 4_000, "no overflow");
    }

    #[test]
    fn a_server_hint_wins_over_the_floor_but_is_capped() {
        let p = RetryPolicy::default();
        // v1 fixture `fault.llm.retry-503-retry-after` honors `retry-after: 0`.
        assert_eq!(retry_delay_ms(p, 2, Some(0)), 0);
        assert_eq!(retry_delay_ms(p, 2, Some(100)), 100);
        // A hostile hint cannot park the worker beyond the cap.
        assert_eq!(retry_delay_ms(p, 2, Some(600_000)), p.max_ms);
    }

    #[test]
    fn defaults_are_the_documented_spec_values() {
        let p = RetryPolicy::default();
        assert_eq!(p.max_attempts, 4);
        assert_eq!(p.base_ms, 250);
        assert_eq!(p.max_ms, 4_000);
    }
}
