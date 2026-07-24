//! The v1 provider retry contract, replayed from the imported fixtures.
//!
//! Spec 10 §1 `[FIXED]`: "Primary/fallback + retry semantics inherited from the
//! v1 behavioral contract". T00-01 imported that contract as
//! `fixtures/fault/index.json`, family `fault.llm.*` (provenance:
//! v1's `src/llm-client.test.ts`). This file translates each of those seven
//! cases into the embedding contract — an HTTP status becomes a typed
//! [`EmbedError`], a response body becomes the vectors a successful provider
//! returns — and asserts the pool reproduces the imported behavior: which
//! failures retry, how long it waits, and how many attempts happen.
//!
//! Deterministic: the pool's `Sleeper` seam records delays instead of sleeping.

mod support;

use std::sync::Arc;

use local_rag_core::config::DataPolicy;
use local_rag_embed::{EmbedError, EmbedRequest, ProviderEntry, ProviderPool, RetryPolicy};
use local_rag_store::RepresentationKind;
use local_rag_test_support::fixtures::read_fixture;
use serde::Deserialize;
use support::{RecordingSleeper, ScriptedEmbedder, Step, batch, tagged_vectors};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FaultFamily {
    #[allow(dead_code)]
    family: String,
    #[allow(dead_code)]
    version: String,
    #[allow(dead_code)]
    description: String,
    cases: Vec<Case>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    id: String,
    title: String,
    status: String,
    #[allow(dead_code)]
    provenance: serde_json::Value,
    input: serde_json::Value,
    expected: serde_json::Value,
}

/// The `responses` field is either a script or a single persistent response.
fn script(input: &serde_json::Value) -> (Vec<Step>, Option<Step>) {
    match &input["responses"] {
        serde_json::Value::Array(items) => (
            items
                .iter()
                .map(|v| step_from(v.as_str().expect("response is a string")))
                .collect(),
            None,
        ),
        serde_json::Value::String(one) => {
            let raw = one.strip_prefix("persistent ").unwrap_or(one);
            (Vec::new(), Some(step_from(raw)))
        }
        other => panic!("unsupported `responses` shape: {other}"),
    }
}

/// Translate one v1 response token into a provider outcome.
///
/// The mapping is the whole point of the translation: v1's transport-level
/// distinctions (5xx/429/network vs 4xx) are exactly this crate's
/// `Retryable`/`Permanent` split, and v1's `Retry-After` / "retry in Xs" body
/// hint is `retry_after_ms`.
fn step_from(raw: &str) -> Step {
    if let Some(rest) = raw.strip_prefix("error:") {
        // v1: `TypeError: fetch failed` — a transport error, retried.
        return Step::Retryable(format!("NetworkError: {rest}"), None);
    }
    let status: u16 = raw
        .strip_prefix("status:")
        .and_then(|s| s.split_whitespace().next())
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("unrecognized response token: {raw}"));
    match status {
        200 => {
            let body = raw.split("body:").nth(1).unwrap_or("ok").trim().to_string();
            Step::Ok(body)
        }
        // `retry-after:<seconds>` (v1 honored the header verbatim).
        503 => {
            let after = raw
                .split("retry-after:")
                .nth(1)
                .and_then(|s| s.split_whitespace().next())
                .and_then(|s| s.parse::<f64>().ok())
                .map(|secs| (secs * 1000.0) as u64);
            Step::Retryable("503".to_string(), after)
        }
        // `body:{"error":"please retry in 0.1s"}` — the body hint fallback.
        429 => {
            let after = raw
                .split("retry in ")
                .nth(1)
                .and_then(|s| s.split('s').next())
                .and_then(|s| s.parse::<f64>().ok())
                .map(|secs| (secs * 1000.0) as u64);
            Step::Retryable("429".to_string(), after)
        }
        400 => Step::Permanent("400".to_string()),
        other => Step::Retryable(other.to_string(), None),
    }
}

fn llm_cases() -> Vec<Case> {
    let raw = read_fixture("fault/index.json").expect("read fixtures/fault/index.json");
    let family: FaultFamily = serde_json::from_str(&raw).expect("typed fault fixtures");
    family
        .cases
        .into_iter()
        .filter(|c| c.id.starts_with("fault.llm."))
        .collect()
}

/// Replay every imported `fault.llm.*` case against the pool.
#[test]
fn v1_llm_retry_contract_is_reproduced_case_by_case() {
    let cases = llm_cases();
    assert_eq!(
        cases.len(),
        7,
        "expected the seven imported v1 llm-client cases, got {:?}",
        cases.iter().map(|c| &c.id).collect::<Vec<_>>()
    );

    for case in cases {
        assert_eq!(case.status, "active", "{}: fixture is not active", case.id);
        let (steps, persistent) = script(&case.input);
        let max_attempts = case.input["max_attempts"].as_u64().expect("max_attempts") as u32;

        let provider = match persistent {
            Some(step) => Arc::new(ScriptedEmbedder::persistent("scripted", step)),
            None => Arc::new(ScriptedEmbedder::new("scripted", steps)),
        };
        let sleeper = Arc::new(RecordingSleeper::new());
        let pool = ProviderPool::new(vec![ProviderEntry::local("scripted", provider.clone())])
            .with_retry_policy(RetryPolicy {
                max_attempts,
                ..RetryPolicy::default()
            })
            .with_sleeper(sleeper.clone());

        let req = EmbedRequest::new(RepresentationKind::CodeRaw, batch());
        let outcome = pool.embed(DataPolicy::LocalOnly, req.clone());

        let expected = &case.expected;
        if expected["throws"].as_bool().unwrap_or(false) {
            let err = outcome.expect_err(&format!("{}: expected failure", case.id));
            let EmbedError::AllProvidersFailed { failures } = &err else {
                panic!("{}: expected AllProvidersFailed, got {err}", case.id);
            };
            let needle = expected["error_matches"].as_str().expect("error_matches");
            assert!(
                failures[0].message.contains(needle),
                "{} ({}): message {:?} does not mention {needle}",
                case.id,
                case.title,
                failures[0].message
            );
        } else {
            let vectors =
                outcome.unwrap_or_else(|e| panic!("{}: expected success, got {e}", case.id));
            let body = expected["result"].as_str().expect("result body");
            assert_eq!(
                vectors,
                tagged_vectors(body, &req),
                "{}: the winning response body must be the one that answered",
                case.id
            );
        }

        let attempts = expected["attempts"].as_u64().expect("attempts") as usize;
        assert_eq!(
            provider.calls(),
            attempts,
            "{} ({}): attempt count",
            case.id,
            case.title
        );

        // `retried: false` (the 400 case) must mean no delay was ever waited.
        if expected["retried"].as_bool() == Some(false) {
            assert!(
                sleeper.delays().is_empty(),
                "{}: a non-retryable failure must not sleep",
                case.id
            );
        }
        // `Retry-After: 0` / "retry in 0.1s" must be honored verbatim.
        if expected["honors_retry_after"].as_bool() == Some(true) {
            assert_eq!(
                sleeper.delays(),
                vec![0],
                "{}: Retry-After honored",
                case.id
            );
        }
        if expected["uses_body_hint"].as_bool() == Some(true) {
            assert_eq!(
                sleeper.delays(),
                vec![100],
                "{}: body hint honored",
                case.id
            );
        }
    }
}

/// Without a server hint the pool waits the documented exponential floor.
#[test]
fn exhausting_a_provider_walks_the_exponential_backoff() {
    let provider = Arc::new(ScriptedEmbedder::persistent(
        "flaky",
        Step::Retryable("500".to_string(), None),
    ));
    let sleeper = Arc::new(RecordingSleeper::new());
    let pool = ProviderPool::new(vec![ProviderEntry::local("flaky", provider.clone())])
        .with_sleeper(sleeper.clone());

    let err = pool
        .embed(
            DataPolicy::LocalOnly,
            EmbedRequest::new(RepresentationKind::CodeRaw, batch()),
        )
        .expect_err("persistent 500 must fail");

    assert!(
        matches!(err, EmbedError::AllProvidersFailed { .. }),
        "{err}"
    );
    assert_eq!(provider.calls(), 4, "default budget is four attempts");
    // Three waits between four attempts: 250, 500, 1000 — never after the last.
    assert_eq!(sleeper.delays(), vec![250, 500, 1_000]);
}

/// A batch is answered positionally: `result[i]` embeds `texts[i]`.
#[test]
fn batch_results_stay_positional() {
    let provider = Arc::new(ScriptedEmbedder::new(
        "ok",
        vec![Step::Ok("answer".to_string())],
    ));
    let pool = ProviderPool::new(vec![ProviderEntry::local("ok", provider)])
        .with_sleeper(Arc::new(RecordingSleeper::new()));

    let req = EmbedRequest::new(RepresentationKind::CodeRaw, batch());
    let vectors = pool
        .embed(DataPolicy::LocalOnly, req.clone())
        .expect("embed");

    assert_eq!(vectors.len(), req.texts.len());
    assert_eq!(vectors, tagged_vectors("answer", &req));
    // Distinct inputs must not collapse onto one vector.
    assert_ne!(vectors[0], vectors[1]);
    assert_ne!(vectors[1], vectors[2]);
}

/// An empty batch needs no provider and no policy decision.
#[test]
fn an_empty_batch_is_answered_without_calling_a_provider() {
    let provider = Arc::new(ScriptedEmbedder::persistent(
        "unused",
        Step::Permanent("must not be called".to_string()),
    ));
    let pool = ProviderPool::new(vec![ProviderEntry::local("unused", provider.clone())]);

    let vectors = pool
        .embed(
            DataPolicy::LocalOnly,
            EmbedRequest::new(RepresentationKind::CodeRaw, Vec::new()),
        )
        .expect("empty batch");

    assert!(vectors.is_empty());
    assert_eq!(provider.calls(), 0);
}
