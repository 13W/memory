//! `D-125` / `T23-04`: what a router prompt really costs, measured with the
//! model's own tokenizer instead of the four-characters-per-token estimate.
//!
//! Every test here is `#[ignore]`d and reads two things the repository does
//! not ship: an installed GGUF model and a real store. Both come from
//! `LOCAL_RAG_LIVE_ROOT`, which must point at a store root (the directory
//! holding `state.sqlite` and `models/`). Without it these tests do not run,
//! so nothing in CI depends on a home directory, a network, or a clock —
//! `CLAUDE.md`'s determinism rules are kept by exclusion, not by exception.
//!
//! The store is opened **read only, through a URI**, and never through
//! `StateDb`: this is meant to be pointed at a live store with a running
//! daemon, and opening one writably would run migrations against it.
//!
//! Reproduce:
//!
//! ```text
//! LOCAL_RAG_LIVE_ROOT=~/.local/share/local-rag \
//!   cargo test -p local-rag --test prompt_budget_live -- --ignored --nocapture
//! ```

use std::path::PathBuf;

use local_rag_core::paths::StoreLayout;
use local_rag_embed::{FinishReason, GenMessage, GenRequest, GenRole, Generator};
use local_rag_generate::{DEFAULT_MODEL_ID, LlamaGenerator};
use local_rag_store::{ConsolidationWindow, EvidenceKind, TrustLevel, WindowObservation};
use rusqlite::{Connection, OpenFlags};

fn live_root() -> Option<PathBuf> {
    std::env::var_os("LOCAL_RAG_LIVE_ROOT").map(PathBuf::from)
}

fn open_live_read_only(root: &std::path::Path) -> Connection {
    let layout = StoreLayout::new(root.to_path_buf());
    let uri = format!("file:{}?mode=ro", layout.state_db().display());
    Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .expect("open the live store read-only")
}

fn open_model(root: &std::path::Path) -> LlamaGenerator {
    let layout = StoreLayout::new(root.to_path_buf());
    let entry = local_rag_generate::find(DEFAULT_MODEL_ID).expect("default model in the catalog");
    LlamaGenerator::open(&layout, entry).expect("the default model is installed under the root")
}

/// Prompt tokens of a one-user-message request carrying `text`.
fn prompt_tokens(model: &LlamaGenerator, text: &str) -> usize {
    let req = GenRequest::new(
        vec![GenMessage {
            role: GenRole::User,
            content: text.to_string(),
        }],
        1,
    );
    model
        .count_prompt_tokens(&req)
        .expect("the local model can always count")
}

/// Percentile of an already-sorted slice, nearest-rank.
fn pct(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((p / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn report(label: &str, samples: &[(usize, usize)]) {
    // samples: (chars, tokens)
    let mut ratios: Vec<f64> = samples
        .iter()
        .filter(|(c, t)| *c > 0 && *t > 0)
        .map(|(c, t)| *c as f64 / *t as f64)
        .collect();
    ratios.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
    let total_chars: usize = samples.iter().map(|(c, _)| c).sum();
    let total_tokens: usize = samples.iter().map(|(_, t)| t).sum();
    println!(
        "{label}: n={} chars={total_chars} tokens={total_tokens} \
         aggregate={:.3} chars/token | min={:.3} p1={:.3} p5={:.3} p50={:.3} p95={:.3} max={:.3}",
        samples.len(),
        total_chars as f64 / total_tokens.max(1) as f64,
        ratios.first().copied().unwrap_or(0.0),
        pct(&ratios, 1.0),
        pct(&ratios, 5.0),
        pct(&ratios, 50.0),
        pct(&ratios, 95.0),
        ratios.last().copied().unwrap_or(0.0),
    );
}

/// The measurement `T23-04`'s constants are derived from. Prints; asserts only
/// that the estimate is the direction the defect claims it is.
#[test]
#[ignore = "needs LOCAL_RAG_LIVE_ROOT: an installed model and a real store"]
fn measure_real_tokens_per_character() {
    let Some(root) = live_root() else {
        panic!("set LOCAL_RAG_LIVE_ROOT to a store root");
    };
    let conn = open_live_read_only(&root);
    let model = open_model(&root);

    // The chat template's own cost, so every other figure is text-only.
    let empty = prompt_tokens(&model, "");
    println!("chat template overhead: {empty} tokens for an empty user message");

    let system = local_rag_memory::prompt::system_prompt();
    let system_tokens = prompt_tokens(&model, &system).saturating_sub(empty);
    println!(
        "system prompt: {} chars, {system_tokens} real tokens, estimate says {}",
        system.chars().count(),
        system.chars().count().div_ceil(4),
    );

    let mut excerpts = Vec::new();
    let mut stmt = conn
        .prepare(
            "SELECT short_evidence_excerpt FROM observation_envelope \
             WHERE short_evidence_excerpt IS NOT NULL \
             ORDER BY received_seq DESC LIMIT 400",
        )
        .expect("prepare excerpts");
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .expect("query excerpts");
    for row in rows {
        let text = row.expect("excerpt row");
        let chars = text.chars().count();
        let tokens = prompt_tokens(&model, &text).saturating_sub(empty);
        excerpts.push((chars, tokens));
    }
    report("observation excerpts", &excerpts);

    let mut entries = Vec::new();
    let mut stmt = conn
        .prepare("SELECT text FROM memory_entry WHERE state = 'active'")
        .expect("prepare entries");
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .expect("query entries");
    for row in rows {
        let text = row.expect("entry row");
        let chars = text.chars().count();
        let tokens = prompt_tokens(&model, &text).saturating_sub(empty);
        entries.push((chars, tokens));
    }
    report("active memory entries", &entries);

    let excerpt_tokens: usize = excerpts.iter().map(|(_, t)| t).sum();
    let excerpt_estimate: usize = excerpts.iter().map(|(c, _)| c.div_ceil(4)).sum();
    let entry_tokens: usize = entries.iter().map(|(_, t)| t).sum();
    let entry_estimate: usize = entries.iter().map(|(c, _)| c.div_ceil(4)).sum();
    println!(
        "estimate/real: excerpts {excerpt_estimate}/{excerpt_tokens} = {:.3}, \
         entries {entry_estimate}/{entry_tokens} = {:.3}",
        excerpt_estimate as f64 / excerpt_tokens.max(1) as f64,
        entry_estimate as f64 / entry_tokens.max(1) as f64,
    );
}

/// Reads one real window out of the live store, exactly as
/// `local_rag_store::memory::observation::envelopes_in_range` would.
fn load_window(conn: &Connection, session_id: &str, from: i64, to: i64) -> ConsolidationWindow {
    let mut stmt = conn
        .prepare(
            "SELECT received_seq, observation_id, event_type, evidence_kind, trust, \
                    repo_id, worktree_id, agent_id, commit_hash, short_evidence_excerpt \
               FROM observation_envelope \
              WHERE session_id = ?1 AND received_seq BETWEEN ?2 AND ?3 \
              ORDER BY received_seq",
        )
        .expect("prepare window");
    let observations = stmt
        .query_map(rusqlite::params![session_id, from, to], |r| {
            Ok(WindowObservation {
                received_seq: r.get(0)?,
                observation_id: r.get(1)?,
                event_type: r.get(2)?,
                evidence_kind: EvidenceKind::from_db(&r.get::<_, String>(3)?)
                    .expect("known evidence kind"),
                trust: TrustLevel::from_db(&r.get::<_, String>(4)?).expect("known trust level"),
                session_id: session_id.to_string(),
                repo_id: r.get(5)?,
                worktree_id: r.get(6)?,
                agent_id: r.get(7)?,
                commit_hash: r.get(8)?,
                short_evidence_excerpt: r.get(9)?,
                payload: None,
            })
        })
        .expect("query window")
        .map(|o| o.expect("window row"))
        .collect();
    ConsolidationWindow {
        session_id: session_id.to_string(),
        from_received_seq: from,
        to_received_seq: to,
        observations,
    }
}

/// The number the budget must be derived from: what a real assembled prompt
/// really costs, JSON structure and escaping included.
#[test]
#[ignore = "needs LOCAL_RAG_LIVE_ROOT: an installed model and a real store"]
fn measure_real_prompt_cost() {
    let Some(root) = live_root() else {
        panic!("set LOCAL_RAG_LIVE_ROOT to a store root");
    };
    let conn = open_live_read_only(&root);
    let model = open_model(&root);

    // Every window a run actually failed on with a context overflow, newest
    // first: the exact inputs the daemon could not fit.
    let mut stmt = conn
        .prepare(
            "SELECT session_id, from_received_seq, to_received_seq, last_failure_reason \
               FROM consolidation_run \
              WHERE last_failure_context_overflow = 1 \
              ORDER BY created_at DESC LIMIT 6",
        )
        .expect("prepare overflow runs");
    let runs: Vec<(String, i64, i64, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .expect("query overflow runs")
        .map(|r| r.expect("run row"))
        .collect();

    for (session_id, from, to, reason) in runs {
        let window = load_window(&conn, &session_id, from, to);
        if window.observations.is_empty() {
            continue;
        }
        let conflicts =
            local_rag_memory::recall::candidate_conflict_set(&conn, &window.observations, 12_000)
                .expect("conflict set");

        let cost = |messages: Vec<GenMessage>| -> usize {
            let req = GenRequest::new(messages, 1);
            model.count_prompt_tokens(&req).expect("local model counts")
        };

        let full = cost(local_rag_memory::prompt::initial_messages(
            &window, &conflicts,
        ));
        let window_only = cost(local_rag_memory::prompt::initial_messages(&window, &[]));
        let system_only = cost(vec![GenMessage {
            role: GenRole::System,
            content: local_rag_memory::prompt::system_prompt(),
        }]);
        let window_chars: usize = window
            .observations
            .iter()
            .map(|o| {
                o.short_evidence_excerpt
                    .as_deref()
                    .unwrap_or("(no excerpt)")
                    .chars()
                    .count()
            })
            .sum();
        let conflict_chars: usize = conflicts.iter().map(|e| e.text.chars().count()).sum();
        let reported: Option<usize> = reason
            .split("request needs ")
            .nth(1)
            .and_then(|t| t.split(' ').next())
            .and_then(|t| t.parse().ok());

        println!(
            "session {} [{from}..={to}] rows={} window_chars={window_chars} \
             conflicts={} conflict_chars={conflict_chars}\n  \
             system_only={system_only} system+window={window_only} full={full} \
             (window costs {} tokens, {:.3} chars/token; conflict set costs {} tokens, {:.3} chars/token)\n  \
             daemon reported {:?} for this run; full + 1024 answer = {}",
            &session_id[..8],
            window.observations.len(),
            conflicts.len(),
            window_only - system_only,
            window_chars as f64 / (window_only - system_only).max(1) as f64,
            full - window_only,
            conflict_chars as f64 / (full - window_only).max(1) as f64,
            reported,
            full + 1024,
        );
    }
}

/// `T23-06`/`D-122`/`D-130`: the uncensored real-answer distribution
/// `ANSWER_RESERVE_TOKENS` is chosen from.
///
/// Everything the store already holds is CENSORED at the old `MAX_GENERATION_
/// TOKENS = 1024` -- a proposal the model would have made past that point was
/// never written, so it cannot be read back out of `memory_entry`/`pending_
/// memory_candidate` and used to size the very budget that cut it off. The
/// only honest measurement re-generates real answers with the cap raised far
/// out of the way (8192) and records what the model actually stops at on its
/// own (`FinishReason::Stop`) versus where the raised cap itself had to
/// intervene (`FinishReason::Length`).
///
/// **The two are not the same population, and this test does not pretend
/// they are.** A hand inspection of one `Length` case (kept in this card's
/// evidence, not reproduced by this test) found verbatim repetition -- the
/// same op, the same text, a fresh fabricated citation id each time -- which
/// is a known greedy-decoding degeneracy (`D-130`), not a legitimately long
/// answer. No finite reserve fixes that class; a larger one only burns more
/// tokens and wall-clock before the cap steps in. So the percentiles below
/// are computed over `Stop`-finished samples only, `truncated_at_cap` is
/// reported as its own count rather than folded into them, and the constant
/// this measurement justifies is chosen to cover the `Stop` distribution
/// generously -- not to chase covering `Length` samples that a bigger number
/// cannot actually help.
///
/// Greedy decoding (`GenRequest::new`'s default `Sampling::Greedy`) makes one
/// exact prompt's answer deterministic, but this is a live, continuously
/// consolidating store: the conflict set for a given window can differ
/// between two calls minutes apart, so re-running this test does not
/// reproduce byte-identical numbers, only the same shape of distribution.
///
/// Slow by design: a real generation per window, not a token count -- budget
/// tens of minutes, not seconds, for the live default sample size, more when
/// a `Length` sample spends its whole 8192-token cap.
#[test]
#[ignore = "needs LOCAL_RAG_LIVE_ROOT: an installed model and a real store; slow (real generation)"]
fn measure_real_answer_tokens() {
    let Some(root) = live_root() else {
        panic!("set LOCAL_RAG_LIVE_ROOT to a store root");
    };
    let conn = open_live_read_only(&root);
    let model = open_model(&root);
    let entry = local_rag_generate::find(DEFAULT_MODEL_ID).expect("default model in the catalog");
    let conflict_budget =
        local_rag_core::config::MemoryConfig::default().router_conflict_token_budget;

    const OVERSIZED_ANSWER_CAP: u32 = 8_192;
    let sample_limit: usize = std::env::var("LOCAL_RAG_ANSWER_SAMPLE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60);

    // The current backlog is a handful of sessions on a store this far along
    // (T23-04/T23-05 already drained most of it) -- nowhere near enough for a
    // distribution. The real source is history: every window that has ever
    // actually applied still has its exact `[from, to]` in `consolidation_run`,
    // and replaying it with the cap raised measures what the model would have
    // written without the very ceiling under design. Stratified across row
    // counts (1, 5, 10, 20 -- the shapes `D-058`'s ladder and `open_window`'s
    // batch cap actually produce) rather than newest-first, so the sample
    // is not dominated by whatever shape happens to be running today.
    let mut stmt = conn
        .prepare(
            "WITH sized AS ( \
               SELECT run_id, session_id, from_received_seq, to_received_seq, \
                      (to_received_seq - from_received_seq + 1) AS seq_span, \
                      CASE \
                        WHEN (to_received_seq - from_received_seq + 1) <= 2 THEN 1 \
                        WHEN (to_received_seq - from_received_seq + 1) <= 6 THEN 5 \
                        WHEN (to_received_seq - from_received_seq + 1) <= 12 THEN 10 \
                        ELSE 20 \
                      END AS bucket \
                 FROM consolidation_run WHERE state = 'applied' \
             ), \
             ranked AS ( \
               SELECT run_id, session_id, from_received_seq, to_received_seq, bucket, \
                      ROW_NUMBER() OVER (PARTITION BY bucket ORDER BY run_id) AS rn \
                 FROM sized WHERE seq_span <= 400 \
             ) \
             SELECT run_id, session_id, from_received_seq, to_received_seq \
               FROM ranked \
              WHERE rn <= ?1 \
              ORDER BY bucket, run_id",
        )
        .expect("prepare applied runs");
    // Per-bucket cap, not a total: `ORDER BY bucket LIMIT n` would return
    // only the smallest bucket if it alone has more than `n` rows -- which
    // it does here (595 one/two-row windows against a sample of 60) -- so
    // every previous run of this test silently measured only the smallest
    // window shape. `ROW_NUMBER() OVER (PARTITION BY bucket ...)` above is
    // what makes this an actual stratified sample across row counts (1, 5,
    // 10, 20) rather than a sort order that happens to look like one.
    let per_bucket = (sample_limit / 4).max(1);
    let runs: Vec<(String, String, i64, i64)> = stmt
        .query_map(rusqlite::params![per_bucket as i64], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .expect("query applied runs")
        .map(|r| r.expect("run row"))
        .collect();

    // Only `Stop`-finished samples feed the percentiles -- see the module
    // doc's `D-130` note for why a `Length` sample is a different population
    // (degenerate repetition) that a larger reserve does not fix.
    let mut stopped_tokens: Vec<u32> = Vec::new();
    let mut truncated_at_cap = 0usize;
    let mut checked = 0usize;

    for (run_id, session_id, from, to) in runs {
        // The exact window that already applied -- not re-derived, so the
        // sample is real consolidation history, not a hypothetical reshaping
        // of it under today's window budget (`T23-04` postdates most of this
        // history, so re-deriving would silently narrow older, wider windows).
        let window = load_window(&conn, &session_id, from, to);
        if window.observations.is_empty() {
            continue;
        }
        let conflicts = local_rag_memory::recall::candidate_conflict_set(
            &conn,
            &window.observations,
            conflict_budget,
        )
        .expect("conflict set");
        let messages = local_rag_memory::prompt::initial_messages(&window, &conflicts);
        let prompt_tokens_used = model
            .count_prompt_tokens(&GenRequest::new(messages.clone(), 1))
            .expect("local model counts");
        // Leave real headroom so the oversized cap itself cannot be the
        // reason for a ContextOverflow -- that would measure the model's
        // context, not its answer.
        if prompt_tokens_used + OVERSIZED_ANSWER_CAP as usize + 64 > entry.context_length as usize {
            println!(
                "run {} session {} [{from}..={to}] skipped: prompt alone is \
                 {prompt_tokens_used} tokens, no room for an {OVERSIZED_ANSWER_CAP}-token answer",
                &run_id[..8],
                &session_id[..8]
            );
            continue;
        }

        let window_chars: usize = window
            .observations
            .iter()
            .map(|o| {
                o.short_evidence_excerpt
                    .as_deref()
                    .unwrap_or("(no excerpt)")
                    .chars()
                    .count()
            })
            .sum();
        let request = GenRequest::new(messages, OVERSIZED_ANSWER_CAP)
            .with_json_schema(local_rag_memory::schema::ROUTER_OPS_JSON_SCHEMA);
        let response = model.generate(request).expect("local generation succeeds");
        let tokens = response.tokens_generated.unwrap_or(0);
        let chars = response.text.chars().count();
        checked += 1;
        if matches!(response.finish_reason, FinishReason::Length) {
            truncated_at_cap += 1;
        } else {
            stopped_tokens.push(tokens);
        }
        println!(
            "run {} session {} [{from}..={to}] window_rows={} window_chars={window_chars} \
             answer_tokens={tokens} answer_chars={chars} finish={:?}",
            &run_id[..8],
            &session_id[..8],
            window.observations.len(),
            response.finish_reason,
        );
    }

    stopped_tokens.sort_unstable();
    let pct_u32 = |p: f64| -> u32 {
        if stopped_tokens.is_empty() {
            return 0;
        }
        let idx = ((p / 100.0) * (stopped_tokens.len() as f64 - 1.0)).round() as usize;
        stopped_tokens[idx.min(stopped_tokens.len() - 1)]
    };
    println!(
        "checked {checked} windows; {} finished on their own (`Stop`), {truncated_at_cap} were \
         still writing at {OVERSIZED_ANSWER_CAP} tokens (`Length`, excluded from the \
         percentiles below -- see the module doc's `D-130` note)\n\
         Stop-finished answer tokens: min={:?} p50={} p90={} p95={} p99={} max={:?}",
        stopped_tokens.len(),
        stopped_tokens.first(),
        pct_u32(50.0),
        pct_u32(90.0),
        pct_u32(95.0),
        pct_u32(99.0),
        stopped_tokens.last(),
    );
    assert!(checked > 0, "the store produced no window to measure");
    assert!(
        !stopped_tokens.is_empty(),
        "every sample hit the cap -- no `Stop`-finished answer to size a reserve from"
    );
}

/// `T23-04`'s acceptance, offline: with the window budget applied, no window
/// this store can produce overflows the model's context.
///
/// It asserts the strong form deliberately — a budgeted window plus the
/// **whole** conflict set `D-095` selected, without the exact cut
/// `router::route` also applies. If that fits, the shipped path fits with room
/// to spare, and the assertion does not depend on a private function.
#[test]
#[ignore = "needs LOCAL_RAG_LIVE_ROOT: an installed model and a real store"]
fn a_budgeted_window_never_overflows_the_context() {
    let Some(root) = live_root() else {
        panic!("set LOCAL_RAG_LIVE_ROOT to a store root");
    };
    let conn = open_live_read_only(&root);
    let model = open_model(&root);

    let entry = local_rag_generate::find(DEFAULT_MODEL_ID).expect("default model in the catalog");
    let budget = local_rag_memory::budget::PromptBudget::derive(entry.context_length);
    let conflict_budget =
        local_rag_core::config::MemoryConfig::default().router_conflict_token_budget;
    println!(
        "budget: {budget:?}, window_chars = {}",
        budget.window_chars()
    );

    // Every window a run has ever failed on with an overflow, plus the next
    // window each backlogged session would open right now.
    let mut cases: Vec<(String, i64, i64)> = Vec::new();
    let mut stmt = conn
        .prepare(
            "SELECT session_id, from_received_seq, to_received_seq FROM consolidation_run \
              WHERE last_failure_context_overflow = 1",
        )
        .expect("prepare overflow runs");
    cases.extend(
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .expect("query overflow runs")
            .map(|r| r.expect("run row")),
    );
    let mut stmt = conn
        .prepare(
            "SELECT e.session_id, COALESCE(c.last_consolidated_received_seq, 0) + 1, \
                    MAX(e.received_seq) \
               FROM observation_envelope e \
               LEFT JOIN processing_cursor c ON c.session_id = e.session_id \
              WHERE e.received_seq > COALESCE(c.last_consolidated_received_seq, 0) \
              GROUP BY e.session_id",
        )
        .expect("prepare backlogged sessions");
    cases.extend(
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .expect("query backlogged sessions")
            .map(|r| r.expect("session row")),
    );

    let mut worst = 0usize;
    let mut checked = 0usize;
    for (session_id, from, to) in cases {
        let full = load_window(&conn, &session_id, from, to);
        if full.observations.is_empty() {
            continue;
        }
        // What `open_window` would now choose: at most `consolidation_batch_size`
        // rows, and at most `window_chars` characters, never fewer than one.
        let mut spent = 0i64;
        let mut kept = Vec::new();
        for o in full.observations.into_iter().take(20) {
            let cost = o
                .short_evidence_excerpt
                .as_deref()
                .unwrap_or("(no excerpt)")
                .chars()
                .count() as i64;
            if !kept.is_empty() && spent + cost > budget.window_chars() {
                break;
            }
            spent += cost;
            kept.push(o);
        }
        let window = ConsolidationWindow {
            session_id: session_id.clone(),
            from_received_seq: from,
            to_received_seq: kept.last().expect("at least one").received_seq,
            observations: kept,
        };
        let conflicts = local_rag_memory::recall::candidate_conflict_set(
            &conn,
            &window.observations,
            conflict_budget,
        )
        .expect("conflict set");
        let req = GenRequest::new(
            local_rag_memory::prompt::initial_messages(&window, &conflicts),
            budget.answer_reserve_tokens,
        );
        let needed = model.count_prompt_tokens(&req).expect("local model counts")
            + budget.answer_reserve_tokens as usize;
        worst = worst.max(needed);
        checked += 1;
        assert!(
            needed <= entry.context_length as usize,
            "session {} [{from}..] needs {needed} tokens of {} after budgeting \
             ({} observations, {spent} characters)",
            &session_id[..8],
            entry.context_length,
            window.observations.len(),
        );
    }
    println!(
        "checked {checked} windows; worst needed {worst} of {} tokens",
        entry.context_length
    );
    assert!(checked > 0, "the store produced no window to check");
}
