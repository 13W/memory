//! The router's entry point (T14-07, spec 08 §4 step 3): [`route`] is the
//! `generate` closure [`local_rag_store::run_once`] is generic over —
//! composed at the daemon/`xtask` call site as
//! `|window| route(&state_db, &pool, policy, &uuids, window)`. Nothing here
//! runs inside a transaction ([`local_rag_store::memory::runner`]'s own
//! module doc requires exactly that); [`route`] only ever opens its own read
//! connection.
//!
//! Ties together, in order: [`crate::recall::candidate_conflict_set`] (pre-
//! generation recall), [`crate::prompt`] (message assembly),
//! [`local_rag_embed::GeneratorPool::generate`] (the actual call, with one
//! bounded corrective re-prompt on a structural parse failure — see
//! [`crate::parse`]'s module doc), and [`crate::guard::materialize`] (per-op
//! placement enforcement).

use std::collections::HashMap;

use std::collections::BTreeSet;

use local_rag_core::config::DataPolicy;
use local_rag_core::identity::UuidSource;
use local_rag_embed::{GenError, GenMessage, GenRequest, GenRole, GeneratorPool};
use local_rag_store::{
    ClassifiedFailure, ConsolidationWindow, GeneratedOp, StateDb, WindowObservation,
    effective_data_policy,
};

use crate::{guard, parse, prompt, recall, schema};

/// `[SPEC]` placeholder upper bound on generated tokens per router call —
/// large enough for a multi-op window's JSON array, small enough that a
/// looping/malformed generation cannot run unbounded. Revisit once Phase 5's
/// real end-to-end run measures actual response sizes.
pub const MAX_GENERATION_TOKENS: u32 = 1024;

/// D-057: `pool.generate`'s own failure classifier. A deterministic context
/// overflow (the request will never fit the model's context no matter how
/// many times this *same* window is retried) is `Mechanical`, so D-050's
/// dead-letter engages after one attempt instead of retrying every daemon
/// tick forever under `Transient` backoff; every other generator failure
/// keeps the existing `Transient` treatment.
fn classify_generate_failure(e: GenError) -> ClassifiedFailure {
    if e.is_deterministic_context_overflow() {
        ClassifiedFailure::mechanical_context_overflow(format!(
            "deterministic context overflow for this window, retrying will not help: {e}"
        ))
    } else {
        ClassifiedFailure::transient(e.to_string())
    }
}

/// Prints the generator's raw response text to stderr when
/// `LOCAL_RAG_ROUTER_DEBUG` is set — off by default, zero cost when unset.
/// Diagnostic only: `cargo xtask memory-bench` reports op-kind
/// precision/recall, not raw text, so a miss otherwise gives no visibility
/// into *what* the model actually said.
fn trace_raw_response(text: &str) {
    if std::env::var_os("LOCAL_RAG_ROUTER_DEBUG").is_some() {
        eprintln!("[router debug] raw response: {text:?}");
    }
}

/// Same gate as [`trace_raw_response`] — D-051: a partial [`parse::
/// ParseOutcome`] (a valid prefix, a trailing line dropped) is not a
/// failure, so it never reaches this crate's daemon-side caller's own
/// `tracing`-based failure logging (`daemon::resume::consolidation`/
/// `daemon::consolidation_trigger`, both of which only log `RunOutcome::
/// Failed`) — this is the one place that visibility exists, for whoever is
/// actively tuning the prompt/generation budget.
fn trace_dropped_tail(reason: &str) {
    if std::env::var_os("LOCAL_RAG_ROUTER_DEBUG").is_some() {
        eprintln!("[router debug] dropped trailing content: {reason}");
    }
}

/// Route one consolidation window to durable-memory ops (spec 08 §4 step 3).
/// `uuids` mints every fresh `memory_id`/`candidate_id`
/// [`crate::guard::materialize`] needs — injected so tests (and, in
/// production, the daemon) control it explicitly, never a bare OS call
/// buried in this crate.
///
/// `global_policy` is the store-wide `data_policy` default; T16-01 folds it
/// with every repository the window's own observations reference
/// (`local_rag_store::effective_data_policy`, spec 02 §3.2) before the
/// generator pool ever sees it, so a repository whose stored policy is
/// stricter than the global default actually blocks a remote generator here
/// — not just in the isolated fold+pool tests. A repository can only
/// tighten, never relax, this effective value.
///
/// D-050: every failure point below is classified `Transient` or
/// `Mechanical` (`local_rag_store::memory::consolidation::FailureKind`'s own
/// doc has the full rationale) — this is what lets
/// `local_rag_store::memory::runner::run_once`'s caller dead-letter a
/// deterministically-broken window instead of retrying it every daemon tick
/// forever. Everything up through the *first* generator call, plus the
/// corrective re-prompt's own generator call, is `Transient`: a db-read
/// hiccup or a model/infra failure is not expected to reproduce identically
/// on an unchanged retry — **except** a deterministic context overflow
/// (D-057, see [`classify_generate_failure`]), which is a third case that
/// *is* about the request, not transient infra, and is folded into
/// `Mechanical` at both `pool.generate` call sites below. The two other
/// failure points that are actually *about the model's output content* —
/// the corrective-re-prompt's parse still failing, and a per-op
/// materialization rejection — are also `Mechanical`: greedy decoding makes
/// the model's response to the *same* window deterministic, so these two
/// reproduce byte-for-byte on every retry until the code (schema, prompt, or
/// generation budget) actually changes.
pub async fn route(
    state_db: &StateDb,
    pool: &GeneratorPool,
    global_policy: DataPolicy,
    uuids: &(dyn UuidSource + Send + Sync),
    window: ConsolidationWindow,
) -> Result<Vec<GeneratedOp>, ClassifiedFailure> {
    let conn = state_db
        .open_read()
        .map_err(|e| ClassifiedFailure::transient(e.to_string()))?;
    let existing = recall::candidate_conflict_set(&conn, &window.observations)
        .map_err(|e| ClassifiedFailure::transient(e.to_string()))?;

    let repo_ids: BTreeSet<&str> = window
        .observations
        .iter()
        .filter_map(|o| o.repo_id.as_deref())
        .collect();
    let repo_ids: Vec<&str> = repo_ids.into_iter().collect();
    let policy = effective_data_policy(global_policy, &conn, &repo_ids)
        .map_err(|e| ClassifiedFailure::transient(e.to_string()))?;

    let messages = prompt::initial_messages(&window, &existing);
    let request = GenRequest::new(messages.clone(), MAX_GENERATION_TOKENS)
        .with_json_schema(schema::ROUTER_OPS_JSON_SCHEMA);
    let response = pool
        .generate(policy, request)
        .map_err(classify_generate_failure)?;
    trace_raw_response(&response.text);

    // D-051: `Err` here is a tier-1 hard failure (see `parse`'s module doc) —
    // the *only* case still worth the one corrective re-prompt, since a
    // tier-2 partial recovery (`Ok(outcome)` with a `dropped_tail`) already
    // has a valid prefix worth keeping, and a live incident's own corrective
    // retry reproduced an identical truncation byte-for-byte (re-asking does
    // not fix a deterministic, greedy-decoded generation budget overrun).
    let outcome = match parse::parse_ops(&response.text) {
        Ok(outcome) => outcome,
        Err(first_error) => {
            let mut retry_messages = messages;
            retry_messages.push(GenMessage {
                role: GenRole::Assistant,
                content: response.text.clone(),
            });
            retry_messages.push(GenMessage {
                role: GenRole::User,
                content: prompt::correction_prompt(&first_error.to_string()),
            });
            let retry_request = GenRequest::new(retry_messages, MAX_GENERATION_TOKENS)
                .with_json_schema(schema::ROUTER_OPS_JSON_SCHEMA);
            let retry_response = pool
                .generate(policy, retry_request)
                .map_err(classify_generate_failure)?;
            parse::parse_ops(&retry_response.text).map_err(|e| {
                ClassifiedFailure::mechanical(format!(
                    "router output still malformed after one corrective re-prompt: {e}"
                ))
            })?
        }
    };
    if let Some(dropped) = &outcome.dropped_tail {
        trace_dropped_tail(dropped);
    }
    let raw_ops = outcome.ops;

    let by_id: HashMap<&str, &WindowObservation> = window
        .observations
        .iter()
        .map(|o| (o.observation_id.as_str(), o))
        .collect();

    let mut ops = Vec::with_capacity(raw_ops.len());
    for raw in raw_ops {
        let op = guard::materialize(&conn, &by_id, &window.observations, uuids, raw)
            .map_err(|e| ClassifiedFailure::mechanical(e.to_string()))?;
        ops.push(op);
    }
    Ok(ops)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    use local_rag_core::identity::{Uuid, uuidv7_from};
    use local_rag_core::paths::StoreLayout;
    use local_rag_embed::{FinishReason, GenError, GenResponse, Generator};
    use local_rag_store::rusqlite::params;
    use local_rag_store::{EvidenceKind, FailureKind};
    use local_rag_test_support::TempHome;

    use super::*;

    struct SeqUuidV7 {
        counter: AtomicU64,
    }

    impl SeqUuidV7 {
        fn new() -> Self {
            Self {
                counter: AtomicU64::new(0),
            }
        }
    }

    impl UuidSource for SeqUuidV7 {
        fn next_uuid(&self) -> Uuid {
            let n = self.counter.fetch_add(1, Ordering::Relaxed);
            uuidv7_from(1000 + n, [0xAA; 10])
        }
    }

    #[derive(Debug, Clone)]
    struct ScriptedGenerator {
        responses: Arc<Mutex<Vec<Result<String, String>>>>,
    }

    impl ScriptedGenerator {
        fn new(responses: Vec<&str>) -> Self {
            Self {
                responses: Arc::new(Mutex::new(
                    responses
                        .into_iter()
                        .rev()
                        .map(|s| Ok(s.to_string()))
                        .collect(),
                )),
            }
        }
    }

    impl Generator for ScriptedGenerator {
        fn generate(&self, _req: local_rag_embed::GenRequest) -> Result<GenResponse, GenError> {
            let mut responses = self.responses.lock().expect("lock");
            match responses.pop() {
                Some(Ok(text)) => Ok(GenResponse {
                    text,
                    finish_reason: FinishReason::Stop,
                    tokens_generated: None,
                }),
                Some(Err(message)) => Err(GenError::permanent(message)),
                None => Err(GenError::permanent("scripted generator exhausted")),
            }
        }
    }

    fn pool_with(responses: Vec<&str>) -> GeneratorPool {
        use local_rag_embed::GeneratorEntry;
        GeneratorPool::new(vec![GeneratorEntry::local(
            "scripted",
            Arc::new(ScriptedGenerator::new(responses)),
        )])
    }

    fn open_state() -> (TempHome, StateDb) {
        let home = TempHome::new().expect("temp home");
        let layout = StoreLayout::new(home.join("local-rag"));
        layout.ensure().expect("ensure store tree");
        let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
        (home, db)
    }

    async fn seed_observation(db: &StateDb, observation_id: &str) {
        let oid = observation_id.to_string();
        db.writer()
            .transaction(move |tx| {
                tx.execute(
                    "INSERT INTO observation_envelope \
                       (observation_id, source_event_id, payload_hash, event_type, \
                        evidence_kind, trust, session_id) \
                     VALUES (?1, 'evt-1', 'deadbeef', 'Stop', 'user_statement', 'normal', 'sess-1')",
                    params![oid],
                )
            })
            .await
            .expect("seed observation envelope");
    }

    fn window_with(observation_id: &str) -> ConsolidationWindow {
        ConsolidationWindow {
            session_id: "sess-1".to_string(),
            from_received_seq: 1,
            to_received_seq: 1,
            observations: vec![WindowObservation {
                observation_id: observation_id.to_string(),
                received_seq: 1,
                event_type: "UserPromptSubmit".to_string(),
                evidence_kind: EvidenceKind::UserStatement,
                trust: local_rag_store::TrustLevel::Normal,
                session_id: "sess-1".to_string(),
                repo_id: None,
                worktree_id: None,
                agent_id: None,
                commit_hash: None,
                short_evidence_excerpt: Some("we decided to use pnpm".to_string()),
                payload: None,
            }],
        }
    }

    #[tokio::test]
    async fn a_well_formed_response_routes_on_the_first_try() {
        let (_home, db) = open_state();
        seed_observation(&db, "o1").await;
        let pool = pool_with(vec![
            r#"{"op":"create","kind":"decision","text":"use pnpm","scope_kind":"global","confidence_signal":"high","importance_signal":"medium","cites":["o1"]}"#,
        ]);
        let uuids = SeqUuidV7::new();
        let ops = route(&db, &pool, DataPolicy::LocalOnly, &uuids, window_with("o1"))
            .await
            .expect("routes cleanly");
        assert_eq!(ops.len(), 1);
        assert!(matches!(ops[0], GeneratedOp::Materialize { .. }));
    }

    #[tokio::test]
    async fn a_malformed_first_response_recovers_via_one_corrective_reprompt() {
        let (_home, db) = open_state();
        seed_observation(&db, "o1").await;
        let pool = pool_with(vec!["not json at all", r#"{"op":"noop"}"#]);
        let uuids = SeqUuidV7::new();
        let ops = route(&db, &pool, DataPolicy::LocalOnly, &uuids, window_with("o1"))
            .await
            .expect("recovers on the second attempt");
        assert_eq!(ops, vec![GeneratedOp::Noop]);
    }

    /// D-050: a parse failure surviving the one corrective re-prompt is the
    /// exact shape the live retry-storm incident hit (`missing field
    /// confidence_signal`, `trailing characters`, `EOF while parsing a
    /// string`) — greedy decoding makes it reproduce byte-for-byte on every
    /// retry, so it must classify `Mechanical`, not `Transient` (which would
    /// keep retrying it every daemon tick forever).
    #[tokio::test]
    async fn a_response_still_malformed_after_the_reprompt_fails_the_window() {
        let (_home, db) = open_state();
        seed_observation(&db, "o1").await;
        let pool = pool_with(vec!["not json", "still not json"]);
        let uuids = SeqUuidV7::new();
        let result = route(&db, &pool, DataPolicy::LocalOnly, &uuids, window_with("o1")).await;
        let failure = result.expect_err("still malformed after the corrective re-prompt");
        assert_eq!(
            failure.kind,
            FailureKind::Mechanical,
            "reproduces identically on an unchanged retry: dead-letter it, don't retry-storm"
        );
    }

    /// D-050: a generator/infra failure (model unavailable, transport error)
    /// is not expected to reproduce on an unchanged retry the way a parse
    /// defect does — it must classify `Transient`, eligible for
    /// exponential-backoff retry rather than a fingerprint-gated dead-letter.
    #[tokio::test]
    async fn a_generator_error_surfaces_as_the_window_error() {
        let (_home, db) = open_state();
        seed_observation(&db, "o1").await;
        let pool = pool_with(vec![]);
        let uuids = SeqUuidV7::new();
        let result = route(&db, &pool, DataPolicy::LocalOnly, &uuids, window_with("o1")).await;
        let failure = result.expect_err("no provider configured for an empty pool");
        assert_eq!(failure.kind, FailureKind::Transient);
    }

    /// A generator that always fails with the live-incident shape (D-057):
    /// `state.sqlite` observed exactly `requested_tokens: 36269,
    /// max_context_tokens: 32768` for a fixed 43-observation window, retried
    /// over 1700 times under the old unconditional-`Transient` classifier.
    struct ContextOverflowGenerator;

    impl Generator for ContextOverflowGenerator {
        fn generate(&self, _req: local_rag_embed::GenRequest) -> Result<GenResponse, GenError> {
            Err(GenError::ContextOverflow {
                requested_tokens: 36_269,
                max_context_tokens: 32_768,
            })
        }
    }

    fn pool_with_context_overflow() -> GeneratorPool {
        use local_rag_embed::GeneratorEntry;
        GeneratorPool::new(vec![GeneratorEntry::local(
            "scripted",
            Arc::new(ContextOverflowGenerator),
        )])
    }

    /// D-057: the window's own content never changes between retries, so a
    /// context overflow on the *first* generator call is exactly as
    /// deterministic as the parse failures D-050 already dead-letters — it
    /// must classify `Mechanical`, not the blanket `Transient` every other
    /// generator failure gets.
    #[tokio::test]
    async fn a_context_overflow_on_the_first_call_classifies_mechanical() {
        let (_home, db) = open_state();
        seed_observation(&db, "o1").await;
        let pool = pool_with_context_overflow();
        let uuids = SeqUuidV7::new();
        let result = route(&db, &pool, DataPolicy::LocalOnly, &uuids, window_with("o1")).await;
        let failure = result.expect_err("context overflow never succeeds");
        assert_eq!(
            failure.kind,
            FailureKind::Mechanical,
            "same window, same token count, every retry — must dead-letter, not retry-storm"
        );
    }

    /// A generator that returns a malformed response once, then fails every
    /// subsequent call with a context overflow — exercises the corrective
    /// re-prompt's own `pool.generate` call site, not just the first one.
    struct MalformedThenContextOverflowGenerator {
        calls: std::sync::atomic::AtomicU32,
    }

    impl Generator for MalformedThenContextOverflowGenerator {
        fn generate(&self, _req: local_rag_embed::GenRequest) -> Result<GenResponse, GenError> {
            if self.calls.fetch_add(1, Ordering::Relaxed) == 0 {
                Ok(GenResponse {
                    text: "not json at all".to_string(),
                    finish_reason: FinishReason::Stop,
                    tokens_generated: None,
                })
            } else {
                Err(GenError::ContextOverflow {
                    requested_tokens: 36_269,
                    max_context_tokens: 32_768,
                })
            }
        }
    }

    #[tokio::test]
    async fn a_context_overflow_on_the_corrective_reprompt_also_classifies_mechanical() {
        let (_home, db) = open_state();
        seed_observation(&db, "o1").await;
        use local_rag_embed::GeneratorEntry;
        let pool = GeneratorPool::new(vec![GeneratorEntry::local(
            "scripted",
            Arc::new(MalformedThenContextOverflowGenerator {
                calls: std::sync::atomic::AtomicU32::new(0),
            }),
        )]);
        let uuids = SeqUuidV7::new();
        let result = route(&db, &pool, DataPolicy::LocalOnly, &uuids, window_with("o1")).await;
        let failure = result.expect_err("context overflow never succeeds");
        assert_eq!(failure.kind, FailureKind::Mechanical);
    }

    #[tokio::test]
    async fn an_empty_ops_array_routes_to_an_empty_batch() {
        let (_home, db) = open_state();
        seed_observation(&db, "o1").await;
        let pool = pool_with(vec![""]);
        let uuids = SeqUuidV7::new();
        let ops = route(&db, &pool, DataPolicy::LocalOnly, &uuids, window_with("o1"))
            .await
            .expect("empty is valid");
        assert!(ops.is_empty());
    }

    /// D-051's own reason for existing: a valid prefix followed by trailing
    /// garbage/truncation is accepted as-is, and — the actual live-incident
    /// motivated behavior — does **not** spend the one corrective re-prompt
    /// trying to recover it. Proven here by scripting only **one** response:
    /// if `route` called the generator a second time, `ScriptedGenerator`
    /// would return "scripted generator exhausted" and this test would fail.
    #[tokio::test]
    async fn a_dropped_trailing_line_is_accepted_without_a_wasted_reprompt() {
        let (_home, db) = open_state();
        seed_observation(&db, "o1").await;
        let pool = pool_with(vec!["{\"op\":\"noop\"}\nnot valid json at all"]);
        let uuids = SeqUuidV7::new();
        let ops = route(&db, &pool, DataPolicy::LocalOnly, &uuids, window_with("o1"))
            .await
            .expect("the valid prefix is accepted, not treated as a failure");
        assert_eq!(ops, vec![GeneratedOp::Noop]);
    }

    /// A generator that records how many times it was actually invoked — a
    /// remote-selection spy for the T16-01 wiring test below.
    #[derive(Default)]
    struct SpyGenerator {
        calls: std::sync::atomic::AtomicU32,
    }

    impl Generator for SpyGenerator {
        fn generate(&self, _req: local_rag_embed::GenRequest) -> Result<GenResponse, GenError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok(GenResponse {
                text: String::new(),
                finish_reason: FinishReason::Stop,
                tokens_generated: None,
            })
        }
    }

    /// T16-01: `route` itself — not just the isolated
    /// `effective_data_policy`+pool combo `crates/embed/tests/policy.rs`
    /// already covers — must fold in a repository's stricter stored
    /// `data_policy` before ever calling the generator pool. A lax global
    /// policy plus a tightened per-repository setting must still block a
    /// remote-only pool.
    #[tokio::test]
    async fn a_repository_can_tighten_the_router_generators_policy_for_real() {
        let (_home, db) = open_state();
        seed_observation(&db, "o1").await;

        let repo_id = "11111111-1111-7111-8111-111111111111";
        db.writer()
            .transaction(move |tx| {
                local_rag_store::create_repository(tx, repo_id, None, 1_000)?;
                local_rag_store::set_repo_data_policy(tx, repo_id, DataPolicy::LocalOnly)
            })
            .await
            .expect("seed a repository with a stricter-than-global policy");

        let mut window = window_with("o1");
        window.observations[0].repo_id = Some(repo_id.to_string());

        let remote = Arc::new(SpyGenerator::default());
        let pool = GeneratorPool::new(vec![local_rag_embed::GeneratorEntry::remote(
            "hosted",
            remote.clone(),
        )]);
        let uuids = SeqUuidV7::new();

        // Global policy is lax (would admit the remote generator on its own);
        // the repository's own stricter setting must still win.
        let result = route(&db, &pool, DataPolicy::AllowRemoteFull, &uuids, window).await;
        assert!(
            result.is_err(),
            "the tightened effective policy must block the remote-only pool"
        );
        assert_eq!(
            remote.calls.load(Ordering::Relaxed),
            0,
            "no bytes may reach the remote generator once the repository tightens the policy"
        );
    }
}
