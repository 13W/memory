//! The central policy guard and provider ordering for `GeneratorPool` (spec
//! 10 §1, 12 §1, 02 §6) — the [`GeneratorPool`] twin of `tests/policy.rs`'s
//! `ProviderPool` coverage. `GeneratorPool::generate` shares the identical
//! guard-before-selection order and redaction transform
//! (`crate::gen_pool::redact_for_transmission`) as `ProviderPool::embed`; this
//! file proves the same policy × provider × payload matrix over
//! `GenRequest`'s chat-style `messages` instead of a text batch.

mod support;

use std::sync::Arc;

use local_rag_core::config::DataPolicy;
use local_rag_embed::{GenMessage, GenRequest, GenRole, GeneratorEntry, GeneratorPool};
use local_rag_protocol::ErrorCode;
use support::{GenStep, ScriptedGenerator};

fn request() -> GenRequest {
    GenRequest::new(
        vec![GenMessage {
            role: GenRole::User,
            content: "window content".to_string(),
        }],
        64,
    )
}

/// Under `local_only` a remote generator is never invoked.
#[test]
fn local_only_never_reaches_a_remote_generator() {
    let remote_spy = Arc::new(ScriptedGenerator::persistent(
        "hosted",
        GenStep::Ok("remote answer".to_string()),
    ));
    let pool = GeneratorPool::new(vec![GeneratorEntry::remote("hosted", remote_spy.clone())]);

    let err = pool
        .generate(DataPolicy::LocalOnly, request())
        .expect_err("remote-only pool under local_only");
    assert!(
        matches!(err, local_rag_embed::GenError::PolicyBlockedRemote { .. }),
        "{err}"
    );
    assert_eq!(remote_spy.calls(), 0, "guard runs before selection");
}

/// `metadata_only_remote` blocks remote selection the same way `local_only`
/// does (T16-01's pragmatic as-built decision, `crate::policy`'s own module
/// doc).
#[test]
fn metadata_only_remote_blocks_remote_selection_like_local_only() {
    let remote_spy = Arc::new(ScriptedGenerator::persistent(
        "hosted",
        GenStep::Ok("remote answer".to_string()),
    ));
    let pool = GeneratorPool::new(vec![GeneratorEntry::remote("hosted", remote_spy.clone())]);

    let err = pool
        .generate(DataPolicy::MetadataOnlyRemote, request())
        .expect_err("remote-only pool under metadata_only_remote");
    assert!(
        matches!(err, local_rag_embed::GenError::PolicyBlockedRemote { .. }),
        "{err}"
    );
    assert_eq!(remote_spy.calls(), 0);
}

/// `allow_remote_full`/`allow_remote_with_redaction` both admit the remote
/// generator — the guard is a policy decision, not a hard-coded refusal.
#[test]
fn a_relaxed_policy_admits_the_remote_generator() {
    let remote = Arc::new(ScriptedGenerator::persistent(
        "hosted",
        GenStep::Ok("remote answer".to_string()),
    ));
    let pool = GeneratorPool::new(vec![GeneratorEntry::remote("hosted", remote.clone())]);

    let resp = pool
        .generate(DataPolicy::AllowRemoteFull, request())
        .expect("remote allowed under allow_remote_full");
    assert_eq!(resp.text, "remote answer");
    assert_eq!(remote.calls(), 1);

    assert!(pool.generate(DataPolicy::LocalOnly, request()).is_err());
}

/// Under `allow_remote_with_redaction`, a secret in the message content is
/// stripped before it ever reaches a remote generator.
#[test]
fn redaction_strips_secrets_before_a_remote_call_under_allow_remote_with_redaction() {
    let remote = Arc::new(ScriptedGenerator::persistent(
        "hosted",
        GenStep::Ok("remote answer".to_string()),
    ));
    let pool = GeneratorPool::new(vec![GeneratorEntry::remote("hosted", remote.clone())]);

    let secret = "aws_key = \"AKIAIOSFODNN7EXAMPLE\"".to_string();
    let req = GenRequest::new(
        vec![GenMessage {
            role: GenRole::User,
            content: secret.clone(),
        }],
        64,
    );
    pool.generate(DataPolicy::AllowRemoteWithRedaction, req)
        .expect("remote allowed under allow_remote_with_redaction");

    let received = remote.last_request().expect("the spy was called");
    assert_ne!(received.messages[0].content, secret);
    assert!(
        !received.messages[0]
            .content
            .contains("AKIAIOSFODNN7EXAMPLE"),
        "{}",
        received.messages[0].content
    );
}

/// Under `allow_remote_full`, the message content reaches the remote
/// generator byte-for-byte unredacted.
#[test]
fn allow_remote_full_sends_the_original_content_unredacted() {
    let remote = Arc::new(ScriptedGenerator::persistent(
        "hosted",
        GenStep::Ok("remote answer".to_string()),
    ));
    let pool = GeneratorPool::new(vec![GeneratorEntry::remote("hosted", remote.clone())]);

    let secret = "aws_key = \"AKIAIOSFODNN7EXAMPLE\"".to_string();
    let req = GenRequest::new(
        vec![GenMessage {
            role: GenRole::User,
            content: secret.clone(),
        }],
        64,
    );
    pool.generate(DataPolicy::AllowRemoteFull, req)
        .expect("remote allowed under allow_remote_full");

    let received = remote.last_request().expect("the spy was called");
    assert_eq!(received.messages[0].content, secret);
}

/// The refusal is the typed, non-retryable `POLICY_BLOCKED_REMOTE` envelope,
/// naming the refused provider — never a silent downgrade.
#[test]
fn policy_blocked_remote_diagnostic_names_the_provider() {
    let remote = Arc::new(ScriptedGenerator::persistent(
        "ollama",
        GenStep::Ok("unused".to_string()),
    ));
    let pool = GeneratorPool::new(vec![GeneratorEntry::remote("ollama", remote)]);

    let err = pool
        .generate(DataPolicy::LocalOnly, request())
        .expect_err("must be policy-blocked");
    let local_rag_embed::GenError::PolicyBlockedRemote { policy, blocked } = &err else {
        panic!("expected PolicyBlockedRemote, got {err}");
    };
    assert_eq!(*policy, DataPolicy::LocalOnly);
    assert_eq!(blocked, &vec!["ollama".to_string()]);

    let envelope = local_rag_embed::policy::envelope_for_gen(&err).expect("a canonical envelope");
    assert_eq!(envelope.code, ErrorCode::PolicyBlockedRemote);
    assert!(!envelope.retryable);
}

/// The pure guard predicate, over the full policy × locality matrix.
#[test]
fn guard_matrix_is_exhaustive() {
    use local_rag_embed::{Locality, allows};
    for policy in [
        DataPolicy::LocalOnly,
        DataPolicy::MetadataOnlyRemote,
        DataPolicy::AllowRemoteWithRedaction,
        DataPolicy::AllowRemoteFull,
    ] {
        assert!(allows(policy, Locality::Local));
        let expect_remote = matches!(
            policy,
            DataPolicy::AllowRemoteWithRedaction | DataPolicy::AllowRemoteFull
        );
        assert_eq!(allows(policy, Locality::Remote), expect_remote);
    }
}
