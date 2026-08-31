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
    ClassifiedFailure, ConsolidationWindow, GeneratedOp, MemoryEntrySummary, StateDb,
    WindowObservation, effective_data_policy,
};

use crate::budget::PromptBudget;
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
    conflict_token_budget: u32,
    budget: PromptBudget,
) -> Result<Vec<GeneratedOp>, ClassifiedFailure> {
    let conn = state_db
        .open_read()
        .map_err(|e| ClassifiedFailure::transient(e.to_string()))?;
    let existing =
        recall::candidate_conflict_set(&conn, &window.observations, conflict_token_budget)
            .map_err(|e| ClassifiedFailure::transient(e.to_string()))?;

    let repo_ids: BTreeSet<&str> = window
        .observations
        .iter()
        .filter_map(|o| o.repo_id.as_deref())
        .collect();
    let repo_ids: Vec<&str> = repo_ids.into_iter().collect();
    let policy = effective_data_policy(global_policy, &conn, &repo_ids)
        .map_err(|e| ClassifiedFailure::transient(e.to_string()))?;

    let existing = fit_conflict_set(pool, policy, &window, existing, &budget)?;

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

/// Cut the conflict set to what the model's own tokenizer says will fit
/// (`T23-04`/`D-125`).
///
/// `D-095` already bounds this set by an *estimated* budget, and that bound
/// stays: it is what decides which entries are worth showing at all. This is
/// the second, exact bound underneath it, and it exists because the estimate
/// is the wrong unit for the other half of the prompt. Measured on six real
/// windows, the observations cost 2.02 to 2.78 characters per token where the
/// estimator assumes four, so a prompt the estimator called comfortable was
/// 33 401 to 37 254 tokens against a 32 768-token context.
///
/// The conflict set is the term that yields, and that is not arbitrary: the
/// window is a promise to the cursor (`apply_run` advances it to
/// `to_received_seq` regardless of what the router read), while `D-095`
/// settled that showing fewer entries is legal — "the router can route with
/// no conflict set, but not with a prompt that does not fit". `T23-04` bounds
/// the window in the store so this cut has something to give back.
///
/// Binary search rather than a per-entry sum: tokenizers are not additive
/// across a JSON boundary, so summing each entry's own count would be an
/// estimate again. Each probe assembles the real prompt, and there are
/// `log2(n)` of them — six for a set of fifty.
///
/// A provider that cannot count (every remote endpoint, every test double)
/// answers `None`, and the set is left exactly as `D-095` cut it — this
/// function is then invisible, which is why the pre-existing tests still
/// assert what they always asserted.
fn fit_conflict_set(
    pool: &GeneratorPool,
    policy: DataPolicy,
    window: &ConsolidationWindow,
    existing: Vec<MemoryEntrySummary>,
    budget: &PromptBudget,
) -> Result<Vec<MemoryEntrySummary>, ClassifiedFailure> {
    let ceiling = budget.prompt_ceiling_tokens() as usize;
    let cost = |entries: &[MemoryEntrySummary]| -> Option<usize> {
        let req = GenRequest::new(
            prompt::initial_messages(window, entries),
            MAX_GENERATION_TOKENS,
        )
        .with_json_schema(schema::ROUTER_OPS_JSON_SCHEMA);
        pool.count_prompt_tokens(policy, &req)
    };

    let Some(full) = cost(&existing) else {
        return Ok(existing);
    };
    if full <= ceiling {
        return Ok(existing);
    }
    // Even with nothing to compare against, this window does not fit. Say so
    // without spending a generation on it: the failure is the same
    // deterministic context overflow `llama.cpp` would report, and
    // `open_next_run`'s `D-058` ladder narrows the window on the next tick
    // exactly as it does today.
    if cost(&[]).is_none_or(|empty| empty > ceiling) {
        return Err(ClassifiedFailure::mechanical_context_overflow(format!(
            "deterministic context overflow for this window, retrying will not help: \
             the window alone needs more than the {ceiling} prompt tokens this model's \
             {} of context leaves after the answer and one corrective re-prompt",
            budget.context_tokens
        )));
    }

    // Cost is monotonic in the prefix length (entries are appended to one
    // JSON array), so the largest prefix that fits is a binary search.
    let (mut lo, mut hi) = (0usize, existing.len());
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        match cost(&existing[..mid]) {
            Some(tokens) if tokens <= ceiling => lo = mid,
            _ => hi = mid - 1,
        }
    }
    let mut existing = existing;
    existing.truncate(lo);
    Ok(existing)
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

    /// A budget no fixture here can reach — these tests predate `D-095` and are
    /// about routing, not about how much memory the prompt may carry.
    const NO_BUDGET_LIMIT: u32 = u32::MAX;

    /// The same idiom for `T23-04`'s prompt budget. Every generator in this
    /// module is a scripted double that cannot count tokens, so
    /// `fit_conflict_set` is invisible to these tests either way; passing an
    /// unbounded budget says that on purpose rather than by accident.
    const NO_PROMPT_LIMIT: PromptBudget = PromptBudget {
        context_tokens: u32::MAX,
        answer_reserve_tokens: 0,
        retry_reserve_tokens: 0,
        system_tokens: 0,
        conflict_floor_tokens: 0,
        window_tokens: u32::MAX,
    };

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

    /// `T23-04`: a scripted generator that also owns a tokenizer, so
    /// `fit_conflict_set` has something to ask. One token per four characters
    /// of every message — the point of these tests is the cut, not the
    /// tokenizer, and a deterministic counter makes the expected prefix
    /// arithmetic rather than a guess. Counts calls, so a test can assert the
    /// generator was never reached.
    #[derive(Debug, Clone)]
    struct CountingGenerator {
        inner: ScriptedGenerator,
        calls: Arc<AtomicU64>,
        /// The user message of the last prompt actually submitted. Without it
        /// a test asserting "the set was cut" would be asserting nothing: a
        /// scripted generator answers the same whatever it is shown.
        last_prompt: Arc<Mutex<Option<String>>>,
    }

    impl CountingGenerator {
        fn new(responses: Vec<&str>) -> Self {
            Self {
                inner: ScriptedGenerator::new(responses),
                calls: Arc::new(AtomicU64::new(0)),
                last_prompt: Arc::new(Mutex::new(None)),
            }
        }
    }

    /// How many existing entries a submitted user prompt carries.
    fn entries_shown(prompt: &Option<String>) -> usize {
        let Some(text) = prompt else { return 0 };
        let value: serde_json::Value = serde_json::from_str(text).expect("the prompt is JSON");
        value["existing_entries"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0)
    }

    impl Generator for CountingGenerator {
        fn generate(&self, req: local_rag_embed::GenRequest) -> Result<GenResponse, GenError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if let Some(user) = req.messages.iter().find(|m| m.role == GenRole::User) {
                *self.last_prompt.lock().expect("lock") = Some(user.content.clone());
            }
            self.inner.generate(req)
        }

        fn count_prompt_tokens(&self, req: &local_rag_embed::GenRequest) -> Option<usize> {
            Some(
                req.messages
                    .iter()
                    .map(|m| m.content.chars().count().div_ceil(4))
                    .sum(),
            )
        }
    }

    /// A pool whose provider can count, plus the call counter.
    fn counting_pool_with(
        responses: Vec<&str>,
    ) -> (GeneratorPool, Arc<AtomicU64>, Arc<Mutex<Option<String>>>) {
        use local_rag_embed::GeneratorEntry;
        let generator = CountingGenerator::new(responses);
        let calls = Arc::clone(&generator.calls);
        let last_prompt = Arc::clone(&generator.last_prompt);
        (
            GeneratorPool::new(vec![GeneratorEntry::local("counting", Arc::new(generator))]),
            calls,
            last_prompt,
        )
    }

    /// The same recording double, minus the tokenizer: a provider that
    /// answers and cannot count, which is what every remote endpoint and every
    /// other double in this module is.
    #[derive(Debug, Clone)]
    struct BlindGenerator(CountingGenerator);

    impl Generator for BlindGenerator {
        fn generate(&self, req: local_rag_embed::GenRequest) -> Result<GenResponse, GenError> {
            self.0.generate(req)
        }
    }

    fn blind_pool_with(
        responses: Vec<&str>,
    ) -> (GeneratorPool, Arc<AtomicU64>, Arc<Mutex<Option<String>>>) {
        use local_rag_embed::GeneratorEntry;
        let inner = CountingGenerator::new(responses);
        let calls = Arc::clone(&inner.calls);
        let last_prompt = Arc::clone(&inner.last_prompt);
        (
            GeneratorPool::new(vec![GeneratorEntry::local(
                "blind",
                Arc::new(BlindGenerator(inner)),
            )]),
            calls,
            last_prompt,
        )
    }

    /// A budget whose ceiling is exactly `tokens`, with no reserves in the
    /// way — the tests below are about the cut, not about the derivation,
    /// which `budget`'s own tests assert.
    /// A stable UUIDv7 per seed, ordered by seed — the same helper
    /// `recall`'s tests use, so entries come back in a predictable order.
    fn uuid(seed: u8) -> String {
        let mut rand = [0u8; 10];
        rand[9] = seed;
        uuidv7_from(1_000 + u64::from(seed), rand).to_string()
    }

    fn budget_with_ceiling(tokens: u32) -> PromptBudget {
        PromptBudget {
            context_tokens: tokens,
            answer_reserve_tokens: 0,
            retry_reserve_tokens: 0,
            system_tokens: 0,
            conflict_floor_tokens: 0,
            window_tokens: tokens,
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
        let ops = route(
            &db,
            &pool,
            DataPolicy::LocalOnly,
            &uuids,
            window_with("o1"),
            NO_BUDGET_LIMIT,
            NO_PROMPT_LIMIT,
        )
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
        let ops = route(
            &db,
            &pool,
            DataPolicy::LocalOnly,
            &uuids,
            window_with("o1"),
            NO_BUDGET_LIMIT,
            NO_PROMPT_LIMIT,
        )
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
        let result = route(
            &db,
            &pool,
            DataPolicy::LocalOnly,
            &uuids,
            window_with("o1"),
            NO_BUDGET_LIMIT,
            NO_PROMPT_LIMIT,
        )
        .await;
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
    async fn seed_global_entry(db: &StateDb, memory_id: &str, text: &str) {
        let (id, text) = (memory_id.to_string(), text.to_string());
        db.writer()
            .transaction(move |tx| {
                local_rag_store::create_memory_entry(
                    tx,
                    &local_rag_store::NewMemoryEntry {
                        memory_id: &id,
                        kind: local_rag_store::MemoryKind::Fact,
                        text: &text,
                        canonical_key: None,
                        scope_kind: local_rag_store::ScopeKind::Global,
                        scope_owner_id: local_rag_store::GLOBAL_SCOPE_OWNER_ID,
                        confidence: 0.5,
                        importance: 0.5,
                        valid_from_tree: None,
                        last_verified_tree: None,
                        supersedes_id: None,
                    },
                    1_000,
                )
            })
            .await
            .expect("create memory tx")
            .expect("create memory domain");
    }

    /// How many entries the router actually showed the model, read back out of
    /// the prompt it built.
    fn entries_in_prompt(db: &StateDb, window: &ConsolidationWindow, budget: u32) -> usize {
        let conn = db.open_read().expect("read conn");
        recall::candidate_conflict_set(&conn, &window.observations, budget)
            .expect("conflict set")
            .len()
    }

    /// `T23-04`/`D-125`: when the assembled prompt does not fit, the conflict
    /// set is cut to the prefix that does — not the window, which the cursor
    /// has already been promised.
    #[tokio::test]
    async fn a_conflict_set_that_does_not_fit_is_cut_to_the_prefix_that_does() {
        let (_home, db) = open_state();
        seed_observation(&db, "o1").await;
        for i in 0..6u8 {
            seed_global_entry(&db, &uuid(60 + i), &"e".repeat(400)).await;
        }
        let window = window_with("o1");
        let unbounded = entries_in_prompt(&db, &window, NO_BUDGET_LIMIT);
        assert_eq!(unbounded, 6, "all six are worth showing before any budget");

        let (pool, calls, last_prompt) = counting_pool_with(vec!["{\"op\":\"noop\"}"]);
        let uuids = SeqUuidV7::new();
        // The system prompt and window cost whatever they cost; give the whole
        // prompt room for three of the 400-character entries on top, and not a
        // fourth. Derived from the same counter the provider uses, so the
        // expected prefix is arithmetic rather than a number to re-tune when
        // the prompt text changes.
        let system_and_window = prompt::initial_messages(&window, &[])
            .iter()
            .map(|m| m.content.chars().count().div_ceil(4))
            .sum::<usize>() as u32;
        let ceiling = system_and_window + 3 * 400_u32.div_ceil(4);

        let ops = route(
            &db,
            &pool,
            DataPolicy::LocalOnly,
            &uuids,
            window.clone(),
            NO_BUDGET_LIMIT,
            budget_with_ceiling(ceiling),
        )
        .await
        .expect("routes with a cut conflict set");
        assert_eq!(ops.len(), 1, "the window still routes: {ops:?}");
        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "exactly one generation, on a prompt that fits"
        );
        let shown = entries_shown(&last_prompt.lock().expect("lock"));
        assert!(
            shown < unbounded,
            "the prompt the model actually received must be shorter than the set \
             `D-095` selected: showed {shown} of {unbounded}"
        );
        assert!(
            shown > 0,
            "and the cut is a prefix, not a purge: showed {shown}"
        );
    }

    /// The window alone does not fit, so no generation is spent finding that
    /// out: the failure is the deterministic overflow `D-058`'s ladder already
    /// knows how to narrow, reported without a local inference run.
    #[tokio::test]
    async fn a_window_that_cannot_fit_alone_fails_without_calling_the_generator() {
        let (_home, db) = open_state();
        seed_observation(&db, "o1").await;
        let (pool, calls, _last_prompt) = counting_pool_with(vec!["{\"op\":\"noop\"}"]);
        let uuids = SeqUuidV7::new();

        let failure = route(
            &db,
            &pool,
            DataPolicy::LocalOnly,
            &uuids,
            window_with("o1"),
            NO_BUDGET_LIMIT,
            budget_with_ceiling(1),
        )
        .await
        .expect_err("a one-token ceiling cannot hold the system prompt");
        assert_eq!(failure.kind, FailureKind::Mechanical);
        assert!(
            failure.context_overflow,
            "classified so `open_next_run`'s shrink ladder still applies: {failure:?}"
        );
        assert_eq!(
            calls.load(Ordering::Relaxed),
            0,
            "the generator is never asked to prove what the tokenizer already said"
        );
    }

    /// A provider that owns no tokenizer — every remote endpoint, every double
    /// in this module — leaves the set exactly as `D-095` cut it.
    #[tokio::test]
    async fn a_provider_that_cannot_count_leaves_the_conflict_set_alone() {
        let (_home, db) = open_state();
        seed_observation(&db, "o1").await;
        for i in 0..6u8 {
            seed_global_entry(&db, &uuid(70 + i), &"e".repeat(400)).await;
        }
        // The scripted generator of every other test in this module: it
        // answers, and it cannot count. Wrapped so the prompt is still
        // observable, which is the whole assertion.
        let (pool, _calls, last_prompt) = blind_pool_with(vec!["{\"op\":\"noop\"}"]);
        let uuids = SeqUuidV7::new();

        let ops = route(
            &db,
            &pool,
            DataPolicy::LocalOnly,
            &uuids,
            window_with("o1"),
            NO_BUDGET_LIMIT,
            // A ceiling far below what six 400-character entries cost: it is
            // ignored, because nobody can say what they cost.
            budget_with_ceiling(1),
        )
        .await
        .expect("routes unchanged when the provider cannot count");
        assert_eq!(ops.len(), 1);
        assert_eq!(
            entries_shown(&last_prompt.lock().expect("lock")),
            6,
            "all six still reach the model: an unanswerable budget must not \
             silently shrink the prompt"
        );
    }

    #[tokio::test]
    async fn a_generator_error_surfaces_as_the_window_error() {
        let (_home, db) = open_state();
        seed_observation(&db, "o1").await;
        let pool = pool_with(vec![]);
        let uuids = SeqUuidV7::new();
        let result = route(
            &db,
            &pool,
            DataPolicy::LocalOnly,
            &uuids,
            window_with("o1"),
            NO_BUDGET_LIMIT,
            NO_PROMPT_LIMIT,
        )
        .await;
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
        let result = route(
            &db,
            &pool,
            DataPolicy::LocalOnly,
            &uuids,
            window_with("o1"),
            NO_BUDGET_LIMIT,
            NO_PROMPT_LIMIT,
        )
        .await;
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
        let result = route(
            &db,
            &pool,
            DataPolicy::LocalOnly,
            &uuids,
            window_with("o1"),
            NO_BUDGET_LIMIT,
            NO_PROMPT_LIMIT,
        )
        .await;
        let failure = result.expect_err("context overflow never succeeds");
        assert_eq!(failure.kind, FailureKind::Mechanical);
    }

    #[tokio::test]
    async fn an_empty_ops_array_routes_to_an_empty_batch() {
        let (_home, db) = open_state();
        seed_observation(&db, "o1").await;
        let pool = pool_with(vec![""]);
        let uuids = SeqUuidV7::new();
        let ops = route(
            &db,
            &pool,
            DataPolicy::LocalOnly,
            &uuids,
            window_with("o1"),
            NO_BUDGET_LIMIT,
            NO_PROMPT_LIMIT,
        )
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
        let ops = route(
            &db,
            &pool,
            DataPolicy::LocalOnly,
            &uuids,
            window_with("o1"),
            NO_BUDGET_LIMIT,
            NO_PROMPT_LIMIT,
        )
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
        let result = route(
            &db,
            &pool,
            DataPolicy::AllowRemoteFull,
            &uuids,
            window,
            NO_BUDGET_LIMIT,
            NO_PROMPT_LIMIT,
        )
        .await;
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
