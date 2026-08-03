//! The ordered generator pool: policy guard → primary/fallback → retry.
//!
//! Mirrors [`crate::pool::ProviderPool`] exactly, minus the parts that only
//! make sense for a *batch* embedding contract: there is no per-kind filter
//! (a [`Generator`] has no `RepresentationKind` analog) and no post-hoc
//! positional/dimensional [`validate`](crate::pool)-style check (a
//! [`GenResponse`] carries raw text, not a shape the pool can verify — see
//! `contract.rs`'s module doc). Reuses [`RetryPolicy`]/[`Sleeper`]/
//! [`ThreadSleeper`]/[`retry_delay_ms`] verbatim from [`crate::pool`] — none of
//! those were ever `Embedder`-specific — and [`Locality`]/[`allows`] from
//! [`crate::policy`], the same guard-before-selection order spec 10 §1/12 §1
//! fix for `Embedder`.

use std::sync::Arc;

use local_rag_core::config::DataPolicy;
use local_rag_core::redaction::Scanner;

use crate::contract::{GenError, GenMessage, GenRequest, GenResponse, Generator, ProviderFailure};
use crate::policy::{Locality, allows};
use crate::pool::{RetryPolicy, Sleeper, ThreadSleeper, retry_delay_ms};

/// Transform `req` for transmission to a provider of `locality` under
/// `policy` — the [`crate::pool`]'s `redact_for_transmission` twin for chat
/// messages instead of a text batch (spec 12 §1, `crate::policy`'s own module
/// doc for the as-built matrix). `None` means "send `req` unchanged".
fn redact_for_transmission(
    policy: DataPolicy,
    locality: Locality,
    req: &GenRequest,
) -> Option<GenRequest> {
    if locality != Locality::Remote || policy != DataPolicy::AllowRemoteWithRedaction {
        return None;
    }
    let scanner = Scanner::new();
    Some(GenRequest {
        messages: req
            .messages
            .iter()
            .map(|m| GenMessage {
                role: m.role,
                content: scanner.redact(&m.content).text,
            })
            .collect(),
        max_tokens: req.max_tokens,
        sampling: req.sampling,
        json_schema: req.json_schema.clone(),
    })
}

/// One generator in the pool, with the locality the guard reads.
#[derive(Clone)]
pub struct GeneratorEntry {
    name: String,
    locality: Locality,
    provider: Arc<dyn Generator>,
}

impl GeneratorEntry {
    /// A local (in-process) provider — spec 10 §1's working default.
    pub fn local(name: impl Into<String>, provider: Arc<dyn Generator>) -> Self {
        GeneratorEntry {
            name: name.into(),
            locality: Locality::Local,
            provider,
        }
    }

    /// A remote provider — "strictly optional" per spec 10 §1, gated by the
    /// policy guard.
    pub fn remote(name: impl Into<String>, provider: Arc<dyn Generator>) -> Self {
        GeneratorEntry {
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

impl std::fmt::Debug for GeneratorEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GeneratorEntry")
            .field("name", &self.name)
            .field("locality", &self.locality)
            .finish_non_exhaustive()
    }
}

/// An ordered, local-first generation provider pool (spec 10 §1).
pub struct GeneratorPool {
    entries: Vec<GeneratorEntry>,
    retry: RetryPolicy,
    sleeper: Arc<dyn Sleeper>,
}

impl GeneratorPool {
    /// A pool over `entries`, tried in the given order (primary first).
    pub fn new(entries: Vec<GeneratorEntry>) -> Self {
        GeneratorPool {
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

    /// Providers allowed under `policy` — the guard's decision, exposed so
    /// callers (and tests) can inspect selection without generating.
    pub fn allowed(&self, policy: DataPolicy) -> Vec<&GeneratorEntry> {
        self.entries
            .iter()
            .filter(|e| allows(policy, e.locality))
            .collect()
    }

    /// Generate under the **effective** `data_policy` (02 §3.2; compute it with
    /// `local_rag_store::effective_data_policy`, which folds in every involved
    /// repository's stricter setting — this pool never relaxes it).
    ///
    /// Order of operations is `[FIXED]` (spec 10 §1, 12 §1): the policy guard
    /// runs *before* provider selection, so a refused remote provider is never
    /// invoked.
    pub fn generate(&self, policy: DataPolicy, req: GenRequest) -> Result<GenResponse, GenError> {
        if self.entries.is_empty() {
            return Err(GenError::NoProvider);
        }

        // Guard before selection (spec 10 §1 / 12 §1).
        let (allowed, blocked): (Vec<&GeneratorEntry>, Vec<&GeneratorEntry>) = self
            .entries
            .iter()
            .partition(|e| allows(policy, e.locality));
        if allowed.is_empty() {
            return Err(GenError::PolicyBlockedRemote {
                policy,
                blocked: blocked.iter().map(|e| e.name.clone()).collect(),
            });
        }

        let mut failures = Vec::new();
        for entry in allowed {
            let redacted = redact_for_transmission(policy, entry.locality, &req);
            let req_for_entry = redacted.as_ref().unwrap_or(&req);
            match self.try_provider(entry, req_for_entry) {
                Ok(resp) => return Ok(resp),
                Err(failure) => failures.push(failure),
            }
        }
        Err(GenError::AllProvidersFailed { failures })
    }

    /// Attempt one provider up to the retry budget.
    fn try_provider(
        &self,
        entry: &GeneratorEntry,
        req: &GenRequest,
    ) -> Result<GenResponse, ProviderFailure> {
        let mut attempts = 0;
        let mut last = String::new();
        while attempts < self.retry.max_attempts {
            attempts += 1;
            match entry.provider.generate(req.clone()) {
                Ok(resp) => return Ok(resp),
                Err(err) => {
                    last = err.to_string();
                    let retry_after = match &err {
                        GenError::Retryable { retry_after_ms, .. } => *retry_after_ms,
                        // Not retryable: fall back to the next provider now.
                        _ => {
                            return Err(ProviderFailure {
                                provider: entry.name.clone(),
                                attempts,
                                message: last,
                            });
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
        Err(ProviderFailure {
            provider: entry.name.clone(),
            attempts,
            message: last,
        })
    }
}

impl std::fmt::Debug for GeneratorPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GeneratorPool")
            .field("entries", &self.entries)
            .field("retry", &self.retry)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::contract::{GenMessage, GenRole};

    /// A scripted mock generator: returns queued responses/errors in order.
    struct ScriptedGenerator {
        script: Mutex<Vec<Result<GenResponse, GenError>>>,
        calls: Mutex<u32>,
    }

    impl ScriptedGenerator {
        fn new(script: Vec<Result<GenResponse, GenError>>) -> Self {
            ScriptedGenerator {
                script: Mutex::new(script),
                calls: Mutex::new(0),
            }
        }

        fn calls(&self) -> u32 {
            *self.calls.lock().expect("calls mutex poisoned")
        }
    }

    impl Generator for ScriptedGenerator {
        fn generate(&self, _req: GenRequest) -> Result<GenResponse, GenError> {
            *self.calls.lock().expect("calls mutex poisoned") += 1;
            self.script.lock().expect("script mutex poisoned").remove(0)
        }
    }

    /// Records requested delays instead of sleeping, mirroring `pool.rs`'s own
    /// test convention (Definition of Done forbids wall-clock sleeps).
    #[derive(Default)]
    struct RecordingSleeper {
        delays: Mutex<Vec<u64>>,
    }

    impl Sleeper for RecordingSleeper {
        fn sleep_ms(&self, ms: u64) {
            self.delays.lock().expect("delays mutex poisoned").push(ms);
        }
    }

    fn ok_response(text: &str) -> Result<GenResponse, GenError> {
        Ok(GenResponse {
            text: text.to_string(),
            finish_reason: crate::contract::FinishReason::Stop,
            tokens_generated: None,
        })
    }

    fn req() -> GenRequest {
        GenRequest::new(
            vec![GenMessage {
                role: GenRole::User,
                content: "window".to_string(),
            }],
            64,
        )
    }

    #[test]
    fn local_only_never_selects_a_remote_generator() {
        let local = Arc::new(ScriptedGenerator::new(vec![ok_response("[]")]));
        let remote = Arc::new(ScriptedGenerator::new(vec![ok_response("should-not-run")]));
        let pool = GeneratorPool::new(vec![
            GeneratorEntry::remote("remote", remote.clone()),
            GeneratorEntry::local("local", local.clone()),
        ]);

        let resp = pool
            .generate(DataPolicy::LocalOnly, req())
            .expect("local generator answers");
        assert_eq!(resp.text, "[]");
        assert_eq!(remote.calls(), 0, "remote must never be attempted");
        assert_eq!(local.calls(), 1);
    }

    #[test]
    fn empty_pool_is_no_provider() {
        let pool = GeneratorPool::new(Vec::new());
        assert_eq!(
            pool.generate(DataPolicy::LocalOnly, req()),
            Err(GenError::NoProvider)
        );
    }

    #[test]
    fn policy_blocked_when_only_remote_entries_exist_under_local_only() {
        let remote = Arc::new(ScriptedGenerator::new(vec![ok_response("unused")]));
        let pool = GeneratorPool::new(vec![GeneratorEntry::remote("hosted-llm", remote)]);
        let err = pool
            .generate(DataPolicy::LocalOnly, req())
            .expect_err("must be policy-blocked");
        assert_eq!(
            err,
            GenError::PolicyBlockedRemote {
                policy: DataPolicy::LocalOnly,
                blocked: vec!["hosted-llm".to_string()],
            }
        );
    }

    #[test]
    fn permanent_failure_falls_back_to_the_next_provider_without_retry() {
        let primary = Arc::new(ScriptedGenerator::new(vec![Err(GenError::permanent(
            "bad request",
        ))]));
        let fallback = Arc::new(ScriptedGenerator::new(vec![ok_response("[]")]));
        let pool = GeneratorPool::new(vec![
            GeneratorEntry::local("primary", primary.clone()),
            GeneratorEntry::local("fallback", fallback.clone()),
        ]);

        let resp = pool
            .generate(DataPolicy::LocalOnly, req())
            .expect("fallback answers");
        assert_eq!(resp.text, "[]");
        assert_eq!(primary.calls(), 1, "no retry for a permanent failure");
        assert_eq!(fallback.calls(), 1);
    }

    #[test]
    fn transient_failure_retries_the_same_provider_before_falling_back() {
        let primary = Arc::new(ScriptedGenerator::new(vec![
            Err(GenError::retryable("busy")),
            ok_response("[]"),
        ]));
        let sleeper = Arc::new(RecordingSleeper::default());
        let pool = GeneratorPool::new(vec![GeneratorEntry::local("primary", primary.clone())])
            .with_sleeper(sleeper.clone());

        let resp = pool
            .generate(DataPolicy::LocalOnly, req())
            .expect("retry succeeds");
        assert_eq!(resp.text, "[]");
        assert_eq!(primary.calls(), 2, "one retry on the same provider");
        assert_eq!(
            *sleeper.delays.lock().expect("delays mutex poisoned"),
            vec![250]
        );
    }

    #[test]
    fn all_providers_failed_carries_every_diagnostic() {
        let a = Arc::new(ScriptedGenerator::new(vec![Err(GenError::permanent(
            "400",
        ))]));
        let b = Arc::new(ScriptedGenerator::new(vec![Err(GenError::permanent(
            "400",
        ))]));
        let pool = GeneratorPool::new(vec![
            GeneratorEntry::local("a", a),
            GeneratorEntry::local("b", b),
        ]);

        let err = pool
            .generate(DataPolicy::LocalOnly, req())
            .expect_err("both fail");
        match err {
            GenError::AllProvidersFailed { failures } => {
                assert_eq!(failures.len(), 2);
                assert_eq!(failures[0].provider, "a");
                assert_eq!(failures[1].provider, "b");
            }
            other => panic!("expected AllProvidersFailed, got {other:?}"),
        }
    }

    #[test]
    fn allowed_reflects_the_guard_without_generating() {
        let local = Arc::new(ScriptedGenerator::new(Vec::new()));
        let remote = Arc::new(ScriptedGenerator::new(Vec::new()));
        let pool = GeneratorPool::new(vec![
            GeneratorEntry::local("local", local.clone()),
            GeneratorEntry::remote("remote", remote.clone()),
        ]);

        assert_eq!(
            pool.allowed(DataPolicy::LocalOnly)
                .iter()
                .map(|e| e.name())
                .collect::<Vec<_>>(),
            vec!["local"]
        );
        assert_eq!(local.calls(), 0);
        assert_eq!(remote.calls(), 0);
        assert_eq!(pool.provider_names(), vec!["local", "remote"]);
    }
}
