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
//! global value with every involved repository's stricter setting — wired into
//! the real call sites by T16-01: `local_rag_memory::router::route` folds over
//! a consolidation window's involved repositories; `cli::index::project_
//! generation` folds over the worktree being indexed), and this guard only
//! answers "may a provider of this locality be selected under this effective
//! policy?".
//!
//! # The as-built policy × selection matrix (T16-01)
//!
//! | Policy | Remote selectable? | What a *selected* remote provider receives |
//! | --- | --- | --- |
//! | `local_only` | no | — |
//! | `metadata_only_remote` | **no** (see below) | — |
//! | `allow_remote_with_redaction` | yes | text run through `local_rag_core::redaction::Scanner::redact` first (`crate::pool`/`crate::gen_pool`) |
//! | `allow_remote_full` | yes | the original, unredacted text |
//!
//! **`metadata_only_remote` is a pragmatic as-built decision, not a spec
//! reading.** Spec 12 §1 names it but never defines what "metadata only" means
//! for an `Embedder`/`Generator` call — both of this workspace's only two
//! provider contracts fundamentally require real body text to produce anything
//! useful (`EmbedRequest.texts`, `GenRequest.messages[].content`); neither has a
//! metadata-only request variant to distinguish this policy from `local_only`.
//! Rather than invent a lossy placeholder-payload mode nothing asks for, this
//! policy is treated identically to `local_only` for both contracts — a
//! deliberate, documented limitation (spec 12 §1's own as-built note), not an
//! oversight. A future group that adds an operation genuinely separable into
//! "metadata" vs. "content" (e.g. a remote capability/model-listing call) would
//! be the first real consumer of a distinct `metadata_only_remote` behavior.

use local_rag_core::config::DataPolicy;
use local_rag_protocol::{ErrorCode, ErrorEnvelope};

use crate::{EmbedError, GenError};

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
/// Local providers are always admitted. A remote provider is admitted only
/// under `allow_remote_with_redaction`/`allow_remote_full` — `local_only` and
/// `metadata_only_remote` both refuse remote selection (see this module's own
/// doc for why `metadata_only_remote` is not distinct from `local_only` here).
pub fn allows(policy: DataPolicy, locality: Locality) -> bool {
    match locality {
        Locality::Local => true,
        Locality::Remote => matches!(
            policy,
            DataPolicy::AllowRemoteWithRedaction | DataPolicy::AllowRemoteFull
        ),
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

/// [`envelope_for`]'s [`GenError`] twin (T16-01: "blocked call typed" applies
/// equally to `Embedder` and `Generator` remote selections).
pub fn envelope_for_gen(error: &GenError) -> Option<ErrorEnvelope> {
    match error {
        GenError::PolicyBlockedRemote { policy, blocked } => {
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
    fn local_only_and_metadata_only_remote_both_block_remote_selection() {
        for policy in [DataPolicy::LocalOnly, DataPolicy::MetadataOnlyRemote] {
            assert!(
                !allows(policy, Locality::Remote),
                "{} must block remote provider selection",
                policy.as_str()
            );
        }
        for policy in [
            DataPolicy::AllowRemoteWithRedaction,
            DataPolicy::AllowRemoteFull,
        ] {
            assert!(
                allows(policy, Locality::Remote),
                "{} must admit remote provider selection",
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

    #[test]
    fn only_the_policy_gen_error_maps_to_a_canonical_code() {
        let blocked = GenError::PolicyBlockedRemote {
            policy: DataPolicy::LocalOnly,
            blocked: vec!["hosted".to_string()],
        };
        assert_eq!(
            envelope_for_gen(&blocked).map(|e| e.code),
            Some(ErrorCode::PolicyBlockedRemote)
        );
        assert!(envelope_for_gen(&GenError::permanent("400")).is_none());
        assert!(envelope_for_gen(&GenError::retryable("500")).is_none());
    }
}
