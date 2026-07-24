//! The central policy guard and provider ordering (spec 10 §1, 12 §1, 02 §6).
//!
//! Two `[FIXED]` rules are under test:
//!
//! * "Every remote call is gated by the effective `data_policy` **before** the
//!   provider is selected; `local_only` never falls back to remote" (10 §1) —
//!   asserted the only way that is falsifiable: a remote spy that counts its
//!   invocations must stay at **zero**;
//! * "violations return `POLICY_BLOCKED_REMOTE`, never silently downgrade"
//!   (12 §1) — asserted as the typed error *and* its protocol envelope.
//!
//! The effective policy itself is computed by `local_rag_store` (T02-05); this
//! file exercises the real fold from a real `state.sqlite` in a `TempHome`, so
//! "a repository can only tighten, never relax, the global policy" is proven
//! against the store rather than restated.

mod support;

use std::sync::Arc;

use local_rag_core::config::DataPolicy;
use local_rag_core::paths::StoreLayout;
use local_rag_embed::{
    EmbedError, EmbedRequest, HashingEmbedder, Locality, ProviderEntry, ProviderPool, allows,
    policy::envelope_for,
};
use local_rag_protocol::ErrorCode;
use local_rag_store::{
    RepresentationKind, StateDb, create_repository, effective_data_policy, set_repo_data_policy,
};
use local_rag_test_support::TempHome;
use support::{RecordingSleeper, ScriptedEmbedder, Step, batch};

fn request() -> EmbedRequest {
    EmbedRequest::new(RepresentationKind::CodeRaw, batch())
}

/// Under `local_only` a remote provider is never invoked — not as primary, not
/// as fallback — even when the local provider fails first.
#[test]
fn local_only_never_reaches_a_remote_provider() {
    let remote_spy = Arc::new(ScriptedEmbedder::persistent(
        "hosted",
        Step::Ok("remote answer".to_string()),
    ));
    let failing_local = Arc::new(ScriptedEmbedder::persistent(
        "local",
        Step::Permanent("local model unavailable".to_string()),
    ));

    let pool = ProviderPool::new(vec![
        ProviderEntry::local("local", failing_local.clone()),
        ProviderEntry::remote("hosted", remote_spy.clone()),
    ])
    .with_sleeper(Arc::new(RecordingSleeper::new()));

    let err = pool
        .embed(DataPolicy::LocalOnly, request())
        .expect_err("the only local provider failed");

    // The local provider failed, so the outcome is "all allowed providers
    // failed" — crucially *not* a remote success.
    let EmbedError::AllProvidersFailed { failures } = &err else {
        panic!("expected AllProvidersFailed, got {err}");
    };
    assert_eq!(failures.len(), 1, "only the local provider was allowed");
    assert_eq!(failures[0].provider, "local");
    assert_eq!(
        remote_spy.calls(),
        0,
        "no bytes may reach a remote provider under local_only"
    );
}

/// With only remote candidates, `local_only` refuses with the typed code rather
/// than degrading to one of them.
#[test]
fn local_only_with_no_local_candidate_is_policy_blocked_remote() {
    let remote_spy = Arc::new(ScriptedEmbedder::persistent(
        "ollama",
        Step::Ok("remote answer".to_string()),
    ));
    let pool = ProviderPool::new(vec![ProviderEntry::remote("ollama", remote_spy.clone())]);

    let err = pool
        .embed(DataPolicy::LocalOnly, request())
        .expect_err("remote-only pool under local_only");

    match &err {
        EmbedError::PolicyBlockedRemote { policy, blocked } => {
            assert_eq!(*policy, DataPolicy::LocalOnly);
            assert_eq!(blocked, &vec!["ollama".to_string()]);
        }
        other => panic!("expected PolicyBlockedRemote, got {other}"),
    }
    assert_eq!(remote_spy.calls(), 0, "guard runs before selection");

    let envelope = envelope_for(&err).expect("a canonical envelope");
    assert_eq!(envelope.code, ErrorCode::PolicyBlockedRemote);
    assert_eq!(envelope.code.as_str(), "POLICY_BLOCKED_REMOTE");
    assert!(!envelope.retryable);
    assert!(
        envelope
            .details
            .as_deref()
            .is_some_and(|d| d.contains("ollama")),
        "the diagnostic must name the refused provider"
    );
}

/// A less restrictive policy admits the remote provider — the guard is a policy
/// decision, not a hard-coded refusal.
#[test]
fn a_relaxed_policy_admits_the_remote_provider() {
    let remote = Arc::new(ScriptedEmbedder::persistent(
        "hosted",
        Step::Ok("remote answer".to_string()),
    ));
    let pool = ProviderPool::new(vec![ProviderEntry::remote("hosted", remote.clone())]);

    let vectors = pool
        .embed(DataPolicy::AllowRemoteFull, request())
        .expect("remote allowed under allow_remote_full");
    assert_eq!(vectors.len(), batch().len());
    assert_eq!(remote.calls(), 1);

    // ... and the same pool refuses under local_only, so the difference is the
    // policy and nothing else.
    assert!(pool.embed(DataPolicy::LocalOnly, request()).is_err());
}

/// Providers are tried in pool order; the first that answers wins.
#[test]
fn fallback_follows_pool_order() {
    let primary = Arc::new(ScriptedEmbedder::persistent(
        "primary",
        Step::Permanent("400".to_string()),
    ));
    let secondary = Arc::new(ScriptedEmbedder::persistent(
        "secondary",
        Step::Permanent("400".to_string()),
    ));
    let tertiary = Arc::new(ScriptedEmbedder::new(
        "tertiary",
        vec![Step::Ok("third".to_string())],
    ));

    let pool = ProviderPool::new(vec![
        ProviderEntry::local("primary", primary.clone()),
        ProviderEntry::local("secondary", secondary.clone()),
        ProviderEntry::local("tertiary", tertiary.clone()),
    ])
    .with_sleeper(Arc::new(RecordingSleeper::new()));

    let vectors = pool
        .embed(DataPolicy::LocalOnly, request())
        .expect("the third provider answers");
    assert_eq!(vectors.len(), batch().len());

    assert_eq!(primary.calls(), 1, "a permanent failure is not retried");
    assert_eq!(secondary.calls(), 1);
    assert_eq!(tertiary.calls(), 1);
}

/// Every failure is reported, in order, when no provider answers.
#[test]
fn all_failures_are_reported_in_pool_order() {
    let pool = ProviderPool::new(vec![
        ProviderEntry::local(
            "first",
            Arc::new(ScriptedEmbedder::persistent(
                "first",
                Step::Permanent("assets missing".to_string()),
            )),
        ),
        ProviderEntry::local(
            "second",
            Arc::new(ScriptedEmbedder::persistent(
                "second",
                Step::Retryable("500".to_string(), None),
            )),
        ),
    ])
    .with_sleeper(Arc::new(RecordingSleeper::new()));

    let err = pool
        .embed(DataPolicy::LocalOnly, request())
        .expect_err("both providers fail");
    let EmbedError::AllProvidersFailed { failures } = &err else {
        panic!("expected AllProvidersFailed, got {err}");
    };

    assert_eq!(failures.len(), 2);
    assert_eq!(failures[0].provider, "first");
    assert_eq!(failures[0].attempts, 1, "permanent failure: one attempt");
    assert!(failures[0].message.contains("assets missing"));
    assert_eq!(failures[1].provider, "second");
    assert_eq!(failures[1].attempts, 4, "transient failure: full budget");
    assert!(failures[1].message.contains("500"));
}

/// A pool with no provider for the requested kind is distinct from a policy
/// refusal and from a provider failure.
#[test]
fn a_missing_kind_is_its_own_error() {
    let pool = ProviderPool::new(vec![ProviderEntry::local(
        "code",
        Arc::new(HashingEmbedder::new(RepresentationKind::CodeRaw)),
    )]);

    let err = pool
        .embed(
            DataPolicy::LocalOnly,
            EmbedRequest::new(RepresentationKind::Memory, batch()),
        )
        .expect_err("no memory provider");
    assert!(
        matches!(
            err,
            EmbedError::NoProvider {
                kind: RepresentationKind::Memory
            }
        ),
        "{err}"
    );
}

/// The guard's selection is inspectable without performing an embedding.
#[test]
fn allowed_selection_reflects_policy_and_kind() {
    let pool = ProviderPool::new(vec![
        ProviderEntry::local(
            "local-code",
            Arc::new(HashingEmbedder::new(RepresentationKind::CodeRaw)),
        ),
        ProviderEntry::remote(
            "remote-code",
            Arc::new(HashingEmbedder::new(RepresentationKind::CodeRaw)),
        ),
        ProviderEntry::local(
            "local-memory",
            Arc::new(HashingEmbedder::new(RepresentationKind::Memory)),
        ),
    ]);

    let local_only: Vec<&str> = pool
        .allowed_for(DataPolicy::LocalOnly, RepresentationKind::CodeRaw)
        .iter()
        .map(|e| e.name())
        .collect();
    assert_eq!(local_only, vec!["local-code"]);

    let relaxed: Vec<&str> = pool
        .allowed_for(
            DataPolicy::AllowRemoteWithRedaction,
            RepresentationKind::CodeRaw,
        )
        .iter()
        .map(|e| e.name())
        .collect();
    assert_eq!(relaxed, vec!["local-code", "remote-code"]);

    assert_eq!(
        pool.allowed_for(DataPolicy::LocalOnly, RepresentationKind::Memory)
            .len(),
        1
    );
}

/// The effective policy the pool is handed comes from the store's fold, and a
/// repository can only tighten it (spec 02 §3.2).
#[tokio::test(flavor = "multi_thread")]
async fn a_repository_can_tighten_but_never_relax_the_global_policy() {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");

    let repo_id = "11111111-1111-7111-8111-111111111111";
    db.writer()
        .transaction(move |tx| {
            create_repository(tx, repo_id, None, 1_700_000_000_000)?;
            // A repository asking for a *stricter* policy than the global one.
            set_repo_data_policy(tx, repo_id, DataPolicy::LocalOnly)?;
            Ok(())
        })
        .await
        .expect("seed repository");

    let conn = db.open_read().expect("read connection");
    let effective = effective_data_policy(DataPolicy::AllowRemoteFull, &conn, &[repo_id])
        .expect("effective policy");
    assert_eq!(
        effective,
        DataPolicy::LocalOnly,
        "the stricter repository setting must win"
    );

    let remote_spy = Arc::new(ScriptedEmbedder::persistent(
        "hosted",
        Step::Ok("remote answer".to_string()),
    ));
    let pool = ProviderPool::new(vec![ProviderEntry::remote("hosted", remote_spy.clone())]);
    let err = pool
        .embed(effective, request())
        .expect_err("tightened policy blocks the remote provider");
    assert!(
        matches!(err, EmbedError::PolicyBlockedRemote { .. }),
        "{err}"
    );
    assert_eq!(remote_spy.calls(), 0);

    // The converse: a repository asking for a laxer policy cannot relax the
    // global one.
    db.writer()
        .transaction(move |tx| {
            set_repo_data_policy(tx, repo_id, DataPolicy::AllowRemoteFull)?;
            Ok(())
        })
        .await
        .expect("relax attempt");
    let conn = db.open_read().expect("read connection");
    assert_eq!(
        effective_data_policy(DataPolicy::LocalOnly, &conn, &[repo_id]).expect("effective policy"),
        DataPolicy::LocalOnly,
        "a repository must not be able to relax the global policy"
    );
}

/// The pure guard predicate, over the full policy × locality matrix.
#[test]
fn guard_matrix_is_exhaustive() {
    for policy in [
        DataPolicy::LocalOnly,
        DataPolicy::MetadataOnlyRemote,
        DataPolicy::AllowRemoteWithRedaction,
        DataPolicy::AllowRemoteFull,
    ] {
        assert!(allows(policy, Locality::Local));
        assert_eq!(
            allows(policy, Locality::Remote),
            policy != DataPolicy::LocalOnly
        );
    }
}
