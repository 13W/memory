//! The central remote-policy guard (spec 12 §1, 10 §1, 02 §6).
//!
//! Spec 12 §1 `[FIXED]`: "Enforced **centrally in the provider pool before
//! provider selection**; violations return `POLICY_BLOCKED_REMOTE`, never
//! silently downgrade." Spec 10 §1 `[FIXED]` adds the direction of the gate:
//! "Every remote call is gated by the effective `data_policy` (02 §3.2)
//! *before* the provider is selected; `local_only` never falls back to remote."
//!
//! This module is deliberately tiny and *pure*: the effective policy is computed
//! elsewhere (`local_rag_store::effective_data_policy`, T02-05, which folds the
//! global value with every involved repository's stricter setting), and this
//! guard only answers "may a provider of this locality be selected under this
//! effective policy?".
//!
//! Scope boundary: the three non-`local_only` policies differ in *what may be
//! sent* (metadata only / redacted / full payload) rather than in *which
//! provider may be selected*. Those payload semantics — plus redaction before
//! transmission and the full policy × provider matrix — are **T16-01**'s card
//! ("all Embedder/Generator remote selections pass one effective-policy guard;
//! metadata-only/redaction/full semantics explicit"). T11-03 ships the seam and
//! the one rule that is `[FIXED]` and testable today: under `local_only` a
//! remote provider is never selected, not even as a fallback.

use local_rag_core::config::DataPolicy;
use local_rag_protocol::{ErrorCode, ErrorEnvelope};

use crate::EmbedError;

/// Where a provider runs.
///
/// Lives on the pool's provider entry rather than on the [`Embedder`](crate::Embedder)
/// trait, so the guard's input cannot be supplied by the provider it is
/// guarding (and so the `[FIXED]` two-method trait shape stays untouched).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Locality {
    /// Runs in-process on this machine; no network, no external daemon.
    Local,
    /// Leaves the machine (hosted API) or depends on an external daemon
    /// (e.g. Ollama) — "strictly optional" per spec 10 §1.
    Remote,
}

impl Locality {
    /// The canonical string used in diagnostics.
    pub fn as_str(self) -> &'static str {
        match self {
            Locality::Local => "local",
            Locality::Remote => "remote",
        }
    }
}

/// Whether `locality` may be selected under the effective `policy`.
///
/// `local_only` admits local providers only. Every less restrictive policy
/// admits both localities — they constrain the *payload*, not the provider
/// (T16-01).
pub fn allows(policy: DataPolicy, locality: Locality) -> bool {
    match locality {
        Locality::Local => true,
        Locality::Remote => policy != DataPolicy::LocalOnly,
    }
}

/// The wire envelope for a refusal (spec 02 §6's `POLICY_BLOCKED_REMOTE` row:
/// "operation refused, local fallback if defined").
///
/// Not retryable: retrying the same request under the same policy is refused
/// identically. The blocked provider names go into `details` so a diagnostic can
/// state *which* providers were refused (spec 02 §6: nothing degrades silently).
pub fn policy_blocked_remote(policy: DataPolicy, blocked: &[String]) -> ErrorEnvelope {
    ErrorEnvelope {
        code: ErrorCode::PolicyBlockedRemote,
        message: format!(
            "effective data_policy {} forbids remote providers",
            policy.as_str()
        ),
        retryable: false,
        details: Some(format!("blocked providers: {}", blocked.join(", "))),
    }
}

/// Map an [`EmbedError`] onto the protocol envelope where a canonical code
/// exists for it (spec 02 §6). Errors without a taxonomy row of their own
/// return `None` — the caller decides how to surface them, rather than this
/// crate inventing a code.
pub fn envelope_for(error: &EmbedError) -> Option<ErrorEnvelope> {
    match error {
        EmbedError::PolicyBlockedRemote { policy, blocked } => {
            Some(policy_blocked_remote(*policy, blocked))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [DataPolicy; 4] = [
        DataPolicy::LocalOnly,
        DataPolicy::MetadataOnlyRemote,
        DataPolicy::AllowRemoteWithRedaction,
        DataPolicy::AllowRemoteFull,
    ];

    #[test]
    fn local_providers_are_allowed_under_every_policy() {
        for policy in ALL {
            assert!(
                allows(policy, Locality::Local),
                "local provider must be selectable under {}",
                policy.as_str()
            );
        }
    }

    #[test]
    fn local_only_is_the_one_policy_that_blocks_remote() {
        assert!(!allows(DataPolicy::LocalOnly, Locality::Remote));
        for policy in ALL.into_iter().filter(|p| *p != DataPolicy::LocalOnly) {
            assert!(
                allows(policy, Locality::Remote),
                "{} must not block provider selection (payload semantics are T16-01)",
                policy.as_str()
            );
        }
    }

    #[test]
    fn refusal_envelope_is_typed_and_not_retryable() {
        let env = policy_blocked_remote(DataPolicy::LocalOnly, &["ollama".to_string()]);
        assert_eq!(env.code, ErrorCode::PolicyBlockedRemote);
        assert_eq!(env.code.as_str(), "POLICY_BLOCKED_REMOTE");
        assert!(!env.retryable);
        assert_eq!(env.details.as_deref(), Some("blocked providers: ollama"));
    }

    #[test]
    fn only_the_policy_error_maps_to_a_canonical_code() {
        let blocked = EmbedError::PolicyBlockedRemote {
            policy: DataPolicy::LocalOnly,
            blocked: vec!["hosted".to_string()],
        };
        assert_eq!(
            envelope_for(&blocked).map(|e| e.code),
            Some(ErrorCode::PolicyBlockedRemote)
        );
        assert!(envelope_for(&EmbedError::permanent("400")).is_none());
        assert!(envelope_for(&EmbedError::retryable("500")).is_none());
    }
}
