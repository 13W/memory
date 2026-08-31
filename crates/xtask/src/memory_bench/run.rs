//! Runs the memory-router benchmark end to end (spec 08 §7) — T14-07.
//!
//! Deliberately not a test: it needs the installed GGUF weights (~490 MiB)
//! and the `llama-cpp-2`/`cmake`/`libclang` toolchain (ADR-0006), neither of
//! which the repository ships or `cargo xtask ci` requires. It is invoked as
//! `cargo xtask memory-bench`.
//!
//! # One throwaway `state.sqlite` per case, not one for the whole run
//!
//! [`local_rag_memory::router::route`] itself never mutates `state.sqlite`
//! (it only reads — see that function's own module doc), so the *router's*
//! own output can never leak between cases. What could still leak is this
//! module's own [`seed_existing_entries`] step: two cases whose fixture
//! authors happened to reuse the same `canonical_key` text would collide if
//! they shared one database. Rather than depend on every fixture author
//! remembering a global naming convention, each case gets its own fresh,
//! empty `state.sqlite` — the safe default, and cheap next to a real
//! generation call.
//!
//! # Every `existing_entries` seed is `scope_kind = "global"`
//!
//! See `crate::memory_bench::corpus`'s module doc: this benchmark measures
//! router *quality*, not scope-owner resolution (already covered by
//! `local_rag_memory::guard`'s own mocked unit tests), so fixture-seeded
//! entries stay global. [`seed_existing_entries`] rejects any other
//! `scope_kind` outright rather than silently mis-seeding it. This does
//! *not* mean the router is nudged toward `global` for its own output —
//! [`build_window`] stamps a synthetic `repo_id` on every observation (see
//! its own doc) precisely so a repository-scoped `create` (the system
//! prompt's own few-shot convention) still resolves cleanly; scoring only
//! checks op *kind*, so either scope choice is scoreable.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use local_rag_core::config::DataPolicy;
use local_rag_core::identity::{SystemUuidV7, UuidSource};
use local_rag_core::paths::StoreLayout;
use local_rag_embed::{GeneratorEntry, GeneratorPool};
use local_rag_generate::{DEFAULT_MODEL_ID, HttpFetcher, LlamaGenerator, find, install_model};
use local_rag_memory::router;
use local_rag_store::{
    ConsolidationWindow, CreateMemoryEntryError, EvidenceKind, MemoryKind, NewMemoryEntry,
    ScopeKind, StateDb, TrustLevel, WindowObservation, create_memory_entry,
};

use crate::git::git_short_head;
use crate::memory_bench::corpus::{RouterCase, load_router_cases};
use crate::memory_bench::report::{CaseResult, Latency, MemoryBenchReport, Provenance};
use crate::memory_bench::score::{CaseTally, aggregate, op_kind, score_case};
use crate::stats::percentile;

/// A fixed, synthetic `repo_id` stamped on every benchmark window's
/// observations (see [`build_window`]). Never a real `repository` row.
const BENCH_REPO_ID: &str = "00000000-0000-7000-8000-0000000b4e17";

pub struct Options {
    pub case_index_path: PathBuf,
    /// A catalog `model_id` to run instead of
    /// [`local_rag_generate::DEFAULT_MODEL_ID`] — how ADR-0006's size
    /// comparison (spec 10 §1's `[OPEN]` half T14-07 closes) actually gets
    /// measured: the same corpus, the same harness, a different catalog
    /// entry.
    pub model_id: Option<String>,
}

pub async fn run(options: &Options) -> Result<MemoryBenchReport, String> {
    let (corpus_version, cases) =
        load_router_cases(&options.case_index_path).map_err(|e| e.to_string())?;

    let model_home_dir = model_home()?;
    let model_layout = StoreLayout::new(model_home_dir.join("local-rag"));
    model_layout
        .ensure()
        .map_err(|e| format!("model layout: {e}"))?;
    let model_id = options.model_id.as_deref().unwrap_or(DEFAULT_MODEL_ID);
    let entry =
        find(model_id).ok_or_else(|| format!("{model_id:?} is not in the generator catalog"))?;

    eprintln!(
        "[memory-bench] model {} in {}",
        entry.model_id,
        model_layout.model_dir(entry.model_id).display()
    );
    let install_start = Instant::now();
    install_model(
        &model_layout,
        entry,
        &HttpFetcher::default(),
        &mut std::io::stderr(),
    )
    .map_err(|e| format!("install {}: {e}", entry.model_id))?;
    let install_ms = install_start.elapsed().as_millis() as u64;

    let load_start = Instant::now();
    let generator = LlamaGenerator::open(&model_layout, entry)
        .map_err(|e| format!("open {}: {e}", entry.model_id))?;
    let load_ms = load_start.elapsed().as_millis() as u64;

    let pool = GeneratorPool::new(vec![GeneratorEntry::local("llama", Arc::new(generator))]);
    let uuids = SystemUuidV7;

    let mut per_case = Vec::with_capacity(cases.len());
    let mut tallies = Vec::with_capacity(cases.len());
    let mut durations_ms = Vec::with_capacity(cases.len());

    for (i, case) in cases.iter().enumerate() {
        let state_dir = fresh_case_state_dir(i)?;
        let state_layout = StoreLayout::new(state_dir.join("local-rag"));
        state_layout
            .ensure()
            .map_err(|e| format!("state layout: {e}"))?;
        let state_db = StateDb::open(state_layout.state_db())
            .map_err(|e| format!("open state.sqlite: {e}"))?;

        seed_existing_entries(&state_db, case, &uuids).await?;
        let window = build_window(case);

        let start = Instant::now();
        // D-095: the bench measures the shipped router, so it must show the model
        // the same amount of existing memory production does.
        let outcome = router::route(
            &state_db,
            &pool,
            DataPolicy::LocalOnly,
            &uuids,
            window,
            local_rag_core::config::MemoryConfig::default().router_conflict_token_budget,
            // T23-04: and the same prompt budget, derived from the same
            // catalog entry the pool above was built from — a bench that
            // measured an unbudgeted router would stop measuring the shipped
            // one.
            local_rag_memory::budget::PromptBudget::derive(entry.context_length),
        )
        .await;
        durations_ms.push(start.elapsed().as_secs_f64() * 1000.0);

        let (result, tally) = match outcome {
            Ok(ops) => {
                let predicted: Vec<String> = ops.iter().map(|op| op_kind(op).to_string()).collect();
                let tally = score_case(&case.expected.op_kinds, &predicted);
                (
                    CaseResult {
                        id: case.id.clone(),
                        tags: case.tags.clone(),
                        expected: case.expected.op_kinds.clone(),
                        predicted,
                        correct: tally.exact_match,
                        error: None,
                    },
                    tally,
                )
            }
            Err(e) => (
                CaseResult {
                    id: case.id.clone(),
                    tags: case.tags.clone(),
                    expected: case.expected.op_kinds.clone(),
                    predicted: Vec::new(),
                    correct: false,
                    error: Some(e.reason),
                },
                CaseTally {
                    true_positive: 0,
                    false_positive: 0,
                    false_negative: case.expected.op_kinds.len(),
                    exact_match: false,
                },
            ),
        };
        eprintln!(
            "[memory-bench] {} expected={:?} predicted={:?} {}",
            result.id,
            result.expected,
            result.predicted,
            if result.correct { "OK" } else { "MISS" }
        );
        per_case.push(result);
        tallies.push(tally);
    }

    let metrics = aggregate(&tallies);
    let mut samples = durations_ms.clone();
    let route_p50_ms = percentile(&mut samples, 0.50);
    let route_p95_ms = percentile(&mut samples, 0.95);

    let provenance = Provenance {
        commit: git_short_head(&std::env::current_dir().unwrap_or_default())
            .unwrap_or_else(|| "unknown".to_string()),
        corpus_path: "fixtures/memory/index.json".to_string(),
        corpus_version,
        case_count: cases.len(),
        model_id: entry.model_id.to_string(),
        sampling: "greedy".to_string(),
        router_version: "v0".to_string(),
        host: std::env::consts::ARCH.to_string() + "-" + std::env::consts::OS,
    };
    let latency = Latency {
        install_ms,
        load_ms,
        route_p50_ms,
        route_p95_ms,
    };

    Ok(MemoryBenchReport::new(
        provenance, metrics, per_case, latency,
    ))
}

/// Seed `case`'s `existing_entries` (see the module doc) so
/// `local_rag_memory::recall::candidate_conflict_set` finds them before the
/// generator is even called.
async fn seed_existing_entries(
    state_db: &StateDb,
    case: &RouterCase,
    uuids: &dyn UuidSource,
) -> Result<(), String> {
    for existing in &case.input.existing_entries {
        let kind = MemoryKind::from_db(&existing.kind).ok_or_else(|| {
            format!(
                "{}: unknown existing_entries.kind {:?}",
                case.id, existing.kind
            )
        })?;
        let scope_kind = ScopeKind::from_db(&existing.scope_kind).ok_or_else(|| {
            format!(
                "{}: unknown existing_entries.scope_kind {:?}",
                case.id, existing.scope_kind
            )
        })?;
        if scope_kind != ScopeKind::Global {
            return Err(format!(
                "{}: only scope_kind=\"global\" existing_entries are supported (see this \
                 module's doc)",
                case.id
            ));
        }
        let memory_id = uuids.next_uuid().to_string();
        let (text, canonical_key) = (existing.text.clone(), existing.canonical_key.clone());
        let case_id = case.id.clone();
        state_db
            .writer()
            .transaction(move |tx| {
                create_memory_entry(
                    tx,
                    &NewMemoryEntry {
                        memory_id: &memory_id,
                        kind,
                        text: &text,
                        canonical_key: canonical_key.as_deref(),
                        scope_kind,
                        scope_owner_id: local_rag_store::GLOBAL_SCOPE_OWNER_ID,
                        confidence: 0.7,
                        importance: 0.5,
                        valid_from_tree: None,
                        last_verified_tree: None,
                        supersedes_id: None,
                    },
                    1_700_000_000_000,
                )
            })
            .await
            .map_err(|e| format!("{case_id}: seeding an existing entry: {e}"))?
            .map_err(|e: CreateMemoryEntryError| {
                format!("{case_id}: seeding an existing entry: {e}")
            })?;
    }
    Ok(())
}

fn build_window(case: &RouterCase) -> ConsolidationWindow {
    let observations: Vec<WindowObservation> = case
        .input
        .observations
        .iter()
        .enumerate()
        .map(|(i, o)| WindowObservation {
            observation_id: o.id.clone(),
            received_seq: (i + 1) as i64,
            event_type: o.event_type.clone(),
            evidence_kind: EvidenceKind::from_db(&o.evidence_kind)
                .unwrap_or(EvidenceKind::ModelClaim),
            trust: TrustLevel::from_db(&o.trust).unwrap_or(TrustLevel::Normal),
            session_id: "memory-bench".to_string(),
            // A fixed, synthetic repo_id (no real `repository` row -- see
            // the module doc: `memory_entry.scope_owner_id` has no FK, so
            // this is a legitimate plain string) so a model that reasonably
            // prefers `scope_kind: "repository"` (the system prompt's own
            // few-shot examples use it) can still resolve a scope owner.
            // Scoring only checks op *kind*, never scope, so either choice
            // is scoreable -- this is not a nudge toward one answer.
            repo_id: Some(BENCH_REPO_ID.to_string()),
            worktree_id: None,
            agent_id: None,
            commit_hash: None,
            short_evidence_excerpt: Some(o.text.clone()),
            payload: None,
        })
        .collect();
    let to_received_seq = observations.len() as i64;
    ConsolidationWindow {
        session_id: "memory-bench".to_string(),
        from_received_seq: 1,
        to_received_seq,
        observations,
    }
}

/// Where model weights are kept **between** runs — reuses
/// `crate::bench::run`'s own `LOCAL_RAG_BENCH_MODEL_HOME` env var and cache
/// root: `StoreLayout::model_dir` already namespaces by `model_id`, so the
/// ONNX embedder and this GGUF generator share one root without collision.
fn model_home() -> Result<PathBuf, String> {
    if let Some(explicit) = std::env::var_os("LOCAL_RAG_BENCH_MODEL_HOME") {
        return Ok(PathBuf::from(explicit));
    }
    let home = std::env::var_os("HOME").ok_or_else(|| "HOME is unset".to_string())?;
    Ok(PathBuf::from(home).join(".local/share/local-rag-bench"))
}

fn fresh_case_state_dir(case_ord: usize) -> Result<PathBuf, String> {
    let base = std::env::temp_dir().join(format!(
        "local-rag-memory-bench-{}-{case_ord}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).map_err(|e| format!("temp dir: {e}"))?;
    Ok(base)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_bench::corpus::{CaseExpected, CaseInput, CaseObservation};

    #[test]
    fn build_window_maps_every_observation_field() {
        let case = RouterCase {
            id: "c1".to_string(),
            tags: vec![],
            input: CaseInput {
                existing_entries: vec![],
                observations: vec![CaseObservation {
                    id: "o1".to_string(),
                    event_type: "UserPromptSubmit".to_string(),
                    evidence_kind: "user_statement".to_string(),
                    trust: "high".to_string(),
                    text: "we decided X".to_string(),
                }],
            },
            expected: CaseExpected {
                op_kinds: vec!["create".to_string()],
            },
        };
        let window = build_window(&case);
        assert_eq!(window.observations.len(), 1);
        let o = &window.observations[0];
        assert_eq!(o.observation_id, "o1");
        assert_eq!(o.evidence_kind, EvidenceKind::UserStatement);
        assert_eq!(o.trust, TrustLevel::High);
        assert_eq!(o.short_evidence_excerpt.as_deref(), Some("we decided X"));
        assert_eq!(window.to_received_seq, 1);
    }

    #[test]
    fn an_unknown_evidence_kind_string_falls_back_to_model_claim_not_a_panic() {
        let case = RouterCase {
            id: "c1".to_string(),
            tags: vec![],
            input: CaseInput {
                existing_entries: vec![],
                observations: vec![CaseObservation {
                    id: "o1".to_string(),
                    event_type: "UserPromptSubmit".to_string(),
                    evidence_kind: "not-a-real-kind".to_string(),
                    trust: "normal".to_string(),
                    text: "x".to_string(),
                }],
            },
            expected: CaseExpected {
                op_kinds: vec!["noop".to_string()],
            },
        };
        let window = build_window(&case);
        assert_eq!(
            window.observations[0].evidence_kind,
            EvidenceKind::ModelClaim
        );
    }
}
