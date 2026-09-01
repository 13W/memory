//! Recall v0, end to end (spec 08 §6 `[FIXED pipeline]`) — T14-08.
//!
//! ```text
//! scope resolution: global ∪ repository(worktree→repo) ∪ worktree
//! → candidate set: entries in recall-eligible states
//! → relevance:  RRF( FTS over memory text , brute-force cosine over embedding_cache )
//! → lifecycle filters → token budget
//! → deterministic ordering (score desc, created_at desc, memory_id)
//! → empty result ⇒ empty additionalContext (no text at all)
//! ```
//!
//! [`recall`] is that pipeline as one function, composing
//! [`super::lexical::lexical_leg`], `dense::dense_leg` and
//! [`super::fusion::rrf`] the way `local_rag_search::pipeline::SearchEngine`
//! composes its own legs — plain functions here, not a struct, matching this
//! crate's own established style ([`crate::router::route`],
//! [`crate::guard::materialize`]).
//!
//! # Two filter boxes, one predicate, two moments (spec 08 §6's own diagram)
//!
//! The diagram names candidate-set filtering *and* a later, separate
//! "lifecycle filters" step. Both are `!state.is_terminal()`
//! ([`local_rag_store::MemoryState::is_terminal`]) — applied twice because
//! code search's analogous guarantee ("no generation mixing between legs")
//! comes from holding `L2.read` across the whole pipeline, and memory recall
//! holds **no lock at all** (confirmed: no `crates/store/src/memory/*.rs`
//! file touches `WorktreeLockRegistry`; memory writes lean on
//! `state.sqlite`'s per-op transactional strictness, T14-02, not a read-side
//! lock). A concurrent `retract`/`supersede` between the initial candidate
//! read (§1 below) and formatting is therefore possible, and a stale
//! terminal entry must not leak into `additionalContext`. The second check
//! ([`local_rag_store::recall_candidate_by_id`]) re-reads fresh, immediately
//! before an entry is added to the budget — cheap, because by then the list
//! is the short, ranked, budget-bounded one, not the whole candidate set.
//!
//! # Every eligible candidate participates in the final order, not only the
//! ones a leg matched
//!
//! [`super::fusion::rrf`] only scores memory_ids a leg actually returned — a
//! termless recall (the hook's `SessionStart` case, before any prompt
//! exists) makes both legs empty, and RRF alone would then order *nothing*.
//! Spec 08 §6's own ordering step, `(score desc, created_at desc,
//! memory_id)`, is read literally here: **every** candidate participates,
//! defaulting to score `0.0` when neither leg matched it, so a termless
//! query still surfaces the scope's most-recent eligible memories — the
//! same "termless query is healthy, not empty" idiom
//! `local_rag_search`'s legs already apply to their own single-leg case,
//! generalized to the whole pipeline's tie-break.
//!
//! # Token budget: a heuristic estimate, not a real tokenizer
//!
//! No token-count utility existed anywhere in this workspace when this rule
//! was written (`T23-04` since added one, `Generator::count_prompt_tokens`,
//! for the one path that owns a real tokenizer and can afford the call —
//! this recall leg is not that path: it runs on the hot query path with no
//! generator in reach, so the heuristic stays here on purpose). The other
//! "token" constant found elsewhere, `MAX_SEQUENCE_TOKENS`, bounds an
//! unrelated ONNX subsystem, not arbitrary text. Spec 08 §6 itself only
//! fixes the number
//! (`[SPEC default 1500 tokens, config]`), not an estimation method, so
//! [`estimate_tokens`] is a plain, documented `chars / 4` heuristic — the
//! same order of magnitude every "roughly 4 characters per token" rule of
//! thumb uses for English/code-like text, adequate for a soft budget that
//! only needs to keep a prompt block bounded, not exact.

use std::collections::HashMap;

use local_rag_store::rusqlite::{self, Connection};
use local_rag_store::{
    GLOBAL_SCOPE_OWNER_ID, MemoryKind, MemoryState, RecallCandidate, RepresentationKind,
    RequestRoot, Resolution, ScopeKind, default_model_space_id,
    model_space_required_representation_ids, projection_state, recall_candidate_by_id,
    recall_candidates_for_scope, representation_key, resolve,
};

use super::dense::{DenseLegUnavailable, MemoryDenseBackend, QueryEmbedder, dense_leg};
use super::format::{RecallEntry, format_additional_context, prepare_entry_text};
use super::fusion::{RankedHit, rrf};
use super::lexical::lexical_leg;

/// The recall pipeline's cardinality guard (spec 08 §6 `[SPEC ≤ 20k
/// entries]`). A compile-time constant, unlike the token budget: the spec
/// tags only the budget `config`, not this bound.
pub const MAX_RECALL_CANDIDATES: usize = 20_000;

/// Roughly 4 characters per token — see the module doc's "heuristic, not a
/// real tokenizer" note. `div_ceil` so a non-empty remainder still counts as
/// a whole token, never rounds a budget-exceeding entry down to "free".
pub fn estimate_tokens(text: &str) -> u32 {
    (text.chars().count() as u32).div_ceil(4)
}

/// A small, fixed per-entry allowance for the numbered-line structure itself
/// (`"N. [kind|state|c=X.XX|len=NNNN] "` plus the trailing newline) — the
/// budget bounds the *whole block*, not just the concatenated entry texts.
pub(crate) const ENTRY_OVERHEAD_TOKENS: u32 = 8;

/// One recall request: the caller's already-probed root (mirrors
/// `local_rag_search::pipeline`'s own `RequestRoot` input — worktree
/// resolution happens inside [`recall`], not before it) plus the query text.
/// An absent/empty `query` is legal (see the module doc's termless-query
/// note) — it is the shape a `SessionStart` hook recall arrives in, before
/// any prompt exists to key relevance off.
pub struct RecallRequest<'a> {
    pub root: RequestRoot,
    /// The query **as both legs will read it** — already English if the caller
    /// translated it (ADR-0011 §Decision 2, `T21-15`).
    ///
    /// The decision is made above this pipeline, not inside it: translating
    /// takes a local generator and about a third of a second, and `recall` is a
    /// synchronous function holding `!Send` connections, so it could not be
    /// moved off the async worker from in here. The caller decides, then hands
    /// down both the text and what happened to it.
    pub query: &'a str,
    /// Why the query is not English, when it is not. `None` means either the
    /// query was already English or the caller translated it successfully —
    /// in both cases what the legs got is the canon's language.
    pub query_degraded: Option<QueryNotNormalized>,
}

/// Why a recall ran against a query in a language the store is not kept in.
///
/// Spec 02 §6 `[FIXED]`: nothing degrades silently. A recall in this state
/// still returns results — the dense leg is multilingual and does most of the
/// work — but the lexical leg is matching English text against a query that is
/// not, so the caller is told rather than left to wonder why the answer looks
/// thin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryNotNormalized {
    /// No generative model is installed, so nothing could translate it.
    NoGenerator,
    /// The translator was asked and refused; the reason travels for the
    /// caller's diagnostics.
    TranslationRefused(String),
}

/// One chosen recall entry with its `memory_id` attached — T15-04's MCP
/// `recall()` tool surface. Unlike [`RecallEntry`] (`format.rs`), which
/// deliberately omits `memory_id` because it is printed into the untrusted
/// `additionalContext` text block (spec 12 §4), this type is never printed
/// there — it exists only for MCP callers, per 12 §4 item 3: "provenance
/// separated from text ... available via tools only." `recall()` is one of
/// those tools.
#[derive(Debug, Clone, PartialEq)]
pub struct RecallResultEntry {
    pub memory_id: String,
    pub kind: MemoryKind,
    pub state: MemoryState,
    pub confidence: f64,
    pub text: String,
}

/// Everything one `recall()` call observed, beyond the wire text itself —
/// for daemon-side logging/diagnostics, never printed into
/// `additionalContext` (spec 12 §4: provenance/diagnostics stay out of the
/// untrusted block).
#[derive(Debug, Clone, PartialEq)]
pub struct RecallOutcome {
    /// The exact `additionalContext` bytes (spec 11 §5) — `""` for an empty
    /// result.
    pub additional_context: String,
    /// The scope descriptor also embedded in `additional_context`'s own
    /// header (`global` or `repo:<repo_id>`) — surfaced as its own field so
    /// an MCP caller does not have to parse it back out of the text block.
    pub scope_label: String,
    /// The same entries `additional_context` renders, with `memory_id`
    /// attached (T15-04, `[SPEC]`: `recall(query?, limit?)`'s own `limit?`
    /// param is only meaningful against a countable list). Ordered
    /// identically to the text block; a caller wanting fewer entries slices
    /// this list — `additional_context` itself is never re-rendered for a
    /// smaller `limit`.
    pub entries: Vec<RecallResultEntry>,
    /// How many candidates were actually scored, after the guard.
    pub candidate_count: usize,
    /// Whether [`MAX_RECALL_CANDIDATES`] truncated the candidate set.
    pub truncated: bool,
    /// Why the dense leg produced nothing, if it didn't — `None` means the
    /// dense leg ran normally (whether or not it found any hits).
    pub dense_degraded: Option<DenseLegUnavailable>,
    /// Why the query was not in the store's language, if it wasn't — copied
    /// through from [`RecallRequest::query_degraded`] so a caller reading the
    /// outcome does not have to keep the request around to know (`T21-15`).
    pub query_degraded: Option<QueryNotNormalized>,
}

/// Run the full recall pipeline and render its result.
///
/// `state_read`/`cache_read` are plain read connections (recall takes no
/// lock — see the module doc); `embedder`/`dense_backend` are the injected
/// seams the `dense` submodule defines, defaulting to
/// [`super::dense::UnavailableEmbedder`]/[`super::dense::BruteForceCosine`]
/// at the caller's discretion. `token_budget` is the caller-resolved
/// `local_rag_core::config::MemoryConfig::recall_token_budget` (config
/// plumbing is the daemon's job, not this pipeline's).
pub fn recall(
    state_read: &Connection,
    cache_read: &Connection,
    embedder: &dyn QueryEmbedder,
    dense_backend: &dyn MemoryDenseBackend,
    request: &RecallRequest<'_>,
    token_budget: u32,
) -> rusqlite::Result<RecallOutcome> {
    // 1. Scope resolution: global ∪ repository(worktree→repo) ∪ worktree.
    let resolution = resolve(state_read, &request.root)?;
    let (scope_label, scopes) = scopes_for(&resolution);

    // 2. Candidate set: recall-eligible entries in every resolved scope,
    //    unioned, then the cardinality guard.
    let mut candidates: Vec<RecallCandidate> = Vec::new();
    for (kind, owner) in &scopes {
        candidates.extend(recall_candidates_for_scope(state_read, *kind, owner)?);
    }
    candidates.sort_by(|a, b| a.memory_id.cmp(&b.memory_id));
    let truncated = candidates.len() > MAX_RECALL_CANDIDATES;
    candidates.truncate(MAX_RECALL_CANDIDATES);
    let candidate_count = candidates.len();

    if candidates.is_empty() {
        return Ok(RecallOutcome {
            query_degraded: request.query_degraded.clone(),
            additional_context: String::new(),
            scope_label,
            entries: Vec::new(),
            candidate_count: 0,
            truncated: false,
            dense_degraded: None,
        });
    }

    // 3. Relevance: FTS leg + bounded brute-force cosine leg, behind the
    //    relevance-backend trait.
    let depth = candidates.len();
    let lexical_hits = lexical_leg(request.query, &candidates, depth)?;

    let (dense_hits, dense_degraded) = match resolve_memory_representation(state_read, &resolution)?
    {
        Some((key, representation_id)) => match dense_leg(
            cache_read,
            request.query,
            &key,
            &representation_id,
            embedder,
            dense_backend,
            &candidates,
            depth,
        ) {
            Ok(hits) => (hits, None),
            Err(e) => (Vec::new(), Some(e)),
        },
        None => (Vec::new(), Some(DenseLegUnavailable::NoRepresentation)),
    };

    let lexical_ranked: Vec<RankedHit<'_>> = lexical_hits
        .iter()
        .map(|h| RankedHit {
            memory_id: h.memory_id.as_str(),
            rank: h.rank,
        })
        .collect();
    let dense_ranked: Vec<RankedHit<'_>> = dense_hits
        .iter()
        .map(|h| RankedHit {
            memory_id: h.memory_id.as_str(),
            rank: h.rank,
        })
        .collect();
    let fused = rrf(&lexical_ranked, &dense_ranked, candidates.len());
    let score_by_id: HashMap<&str, f64> = fused
        .iter()
        .map(|f| (f.memory_id.as_str(), f.score))
        .collect();

    // 5. Deterministic ordering (score desc, created_at desc, memory_id) —
    //    over every eligible candidate, not only the ones a leg matched
    //    (see the module doc's "termless query" note).
    let mut ordered: Vec<&RecallCandidate> = candidates.iter().collect();
    ordered.sort_by(|a, b| {
        let sa = score_by_id
            .get(a.memory_id.as_str())
            .copied()
            .unwrap_or(0.0);
        let sb = score_by_id
            .get(b.memory_id.as_str())
            .copied()
            .unwrap_or(0.0);
        sb.partial_cmp(&sa)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.created_at.cmp(&a.created_at))
            .then_with(|| a.memory_id.cmp(&b.memory_id))
    });

    // 4. Lifecycle re-check + token budget, walked together in the final
    //    order: a stale (now-terminal) entry is skipped; the first entry
    //    that would overflow the budget stops the walk (a ranked prefix).
    let mut chosen: Vec<RecallEntry> = Vec::new();
    let mut entries: Vec<RecallResultEntry> = Vec::new();
    let mut used_tokens: u32 = 0;
    for candidate in ordered {
        let Some(fresh) = recall_candidate_by_id(state_read, &candidate.memory_id)? else {
            continue; // purged/gone since the candidate read — skip, not an error.
        };
        if fresh.state.is_terminal() {
            continue;
        }
        let prepared = prepare_entry_text(&fresh.text);
        let cost = estimate_tokens(&prepared) + ENTRY_OVERHEAD_TOKENS;
        if used_tokens + cost > token_budget {
            break;
        }
        used_tokens += cost;
        entries.push(RecallResultEntry {
            memory_id: fresh.memory_id.clone(),
            kind: fresh.kind,
            state: fresh.state,
            confidence: fresh.confidence,
            text: fresh.text.clone(),
        });
        chosen.push(RecallEntry {
            kind: fresh.kind,
            state: fresh.state,
            confidence: fresh.confidence,
            text: fresh.text,
        });
    }

    let additional_context = format_additional_context(&scope_label, &chosen);
    Ok(RecallOutcome {
        query_degraded: request.query_degraded.clone(),
        additional_context,
        scope_label,
        entries,
        candidate_count,
        truncated,
        dense_degraded,
    })
}

/// The scope descriptor (for the `additionalContext` header) and the list of
/// `(scope_kind, scope_owner_id)` pairs to union, for a resolved request
/// root. `Ambiguous` degrades exactly like `GlobalOnly` — neither yields a
/// `repo_id`/`worktree_id` recall can safely scope to, and spec 02 §6's own
/// table already establishes the principle this generalizes: "Worktree
/// unknown / never indexed | … memory tools work in repo/global scope".
///
/// `pub` (T15-04): `list_memory`/`stats` (`local_rag::daemon::mcp::memory`)
/// need the identical scope-union logic — reusing this instead of
/// re-deriving it in the daemon crate keeps the "memory tools degrade to
/// global scope" rule defined in exactly one place.
pub fn scopes_for(resolution: &Resolution) -> (String, Vec<(ScopeKind, String)>) {
    match resolution {
        Resolution::Resolved {
            repo_id,
            worktree_id,
        } => (
            format!("repo:{repo_id}"),
            vec![
                (ScopeKind::Global, GLOBAL_SCOPE_OWNER_ID.to_string()),
                (ScopeKind::Repository, repo_id.clone()),
                (ScopeKind::Worktree, worktree_id.clone()),
            ],
        ),
        Resolution::GlobalOnly | Resolution::Ambiguous { .. } => (
            "global".to_string(),
            vec![(ScopeKind::Global, GLOBAL_SCOPE_OWNER_ID.to_string())],
        ),
    }
}

/// The `memory`-kind representation to search the dense leg under (spec 08
/// §6: "same `representation_id` as the active memory representation").
///
/// Spec 08 §6 also states "model-space migration covers the memory
/// representation exactly like code" — so this mirrors how code search
/// resolves *its* active representation: the resolved worktree's own
/// `active_model_space_id` when one exists, falling back to the store's
/// `default_model_space_id` (spec 05 §8: "an offline/dormant worktree
/// migrates to the default space at its next open") for a `GlobalOnly`/
/// `Ambiguous` resolution, or a worktree that has never opened a projection
/// tuple. `None` — no representation resolvable at all — is not an error:
/// the dense leg simply degrades (see [`DenseLegUnavailable::
/// NoRepresentation`]).
fn resolve_memory_representation(
    conn: &Connection,
    resolution: &Resolution,
) -> rusqlite::Result<Option<(local_rag_store::RepresentationKey, String)>> {
    let model_space_id = match resolution {
        Resolution::Resolved { worktree_id, .. } => {
            match projection_state(conn, worktree_id)?.and_then(|row| row.active_model_space_id) {
                Some(id) => id,
                None => match default_model_space_id(conn)? {
                    Some(id) => id,
                    None => return Ok(None),
                },
            }
        }
        Resolution::GlobalOnly | Resolution::Ambiguous { .. } => {
            match default_model_space_id(conn)? {
                Some(id) => id,
                None => return Ok(None),
            }
        }
    };

    let representations = model_space_required_representation_ids(conn, &model_space_id)?;
    let Some((_, representation_id)) = representations
        .into_iter()
        .find(|(kind, _)| *kind == RepresentationKind::Memory)
    else {
        return Ok(None);
    };
    let Some(key) = representation_key(conn, &representation_id)? else {
        return Ok(None);
    };
    Ok(Some((key, representation_id)))
}

#[cfg(test)]
mod tests {
    use local_rag_core::identity::uuidv7_from;
    use local_rag_core::paths::StoreLayout;
    use local_rag_store::{
        CacheDb, MemoryKind, MemoryState, NewMemoryEntry, StateDb, create_memory_entry,
        transition_memory_entry,
    };
    use local_rag_test_support::TempHome;

    use super::super::dense::{BruteForceCosine, UnavailableEmbedder};
    use super::*;

    fn uuid(seed: u8) -> String {
        let mut rand = [0u8; 10];
        rand[9] = seed;
        uuidv7_from(1000, rand).to_string()
    }

    fn open_both() -> (TempHome, StateDb, CacheDb) {
        let home = TempHome::new().expect("temp home");
        let layout = StoreLayout::new(home.join("local-rag"));
        layout.ensure().expect("ensure store tree");
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        let cache = CacheDb::open(layout.cache_db(), "store-uuid").expect("open cache.sqlite");
        (home, state, cache)
    }

    #[allow(clippy::too_many_arguments)]
    async fn seed_memory(
        db: &StateDb,
        memory_id: &str,
        kind: MemoryKind,
        scope_kind: ScopeKind,
        scope_owner_id: &str,
        text: &str,
        confidence: f64,
        now_ms: i64,
    ) {
        let (id, owner, text) = (
            memory_id.to_string(),
            scope_owner_id.to_string(),
            text.to_string(),
        );
        db.writer()
            .transaction(move |tx| {
                create_memory_entry(
                    tx,
                    &NewMemoryEntry {
                        memory_id: &id,
                        kind,
                        text: &text,
                        canonical_key: None,
                        scope_kind,
                        scope_owner_id: &owner,
                        confidence,
                        importance: 0.5,
                        valid_from_tree: None,
                        last_verified_tree: None,
                        supersedes_id: None,
                    },
                    now_ms,
                )
            })
            .await
            .expect("create memory tx")
            .expect("create memory domain");
    }

    fn global_only_request(query: &'static str) -> RecallRequest<'static> {
        RecallRequest {
            query_degraded: None,
            root: RequestRoot {
                worktree_root: None,
                repo_hint: None,
            },
            query,
        }
    }

    #[tokio::test]
    async fn empty_store_yields_empty_additional_context() {
        let (_home, state, cache) = open_both();
        let state_read = state.open_read().expect("state read");
        let cache_read = cache.open_read().expect("cache read");
        let request = global_only_request("anything");
        let outcome = recall(
            &state_read,
            &cache_read,
            &UnavailableEmbedder,
            &BruteForceCosine,
            &request,
            1500,
        )
        .expect("recall");
        assert_eq!(outcome.additional_context, "");
        assert_eq!(outcome.candidate_count, 0);
    }

    #[tokio::test]
    async fn a_termless_query_still_surfaces_eligible_memories_in_recency_order() {
        let (_home, state, cache) = open_both();
        seed_memory(
            &state,
            &uuid(1),
            MemoryKind::Fact,
            ScopeKind::Global,
            GLOBAL_SCOPE_OWNER_ID,
            "older fact",
            0.5,
            1_000,
        )
        .await;
        seed_memory(
            &state,
            &uuid(2),
            MemoryKind::Fact,
            ScopeKind::Global,
            GLOBAL_SCOPE_OWNER_ID,
            "newer fact",
            0.5,
            2_000,
        )
        .await;

        let state_read = state.open_read().expect("state read");
        let cache_read = cache.open_read().expect("cache read");
        let request = global_only_request("");
        let outcome = recall(
            &state_read,
            &cache_read,
            &UnavailableEmbedder,
            &BruteForceCosine,
            &request,
            1500,
        )
        .expect("recall");
        assert!(!outcome.additional_context.is_empty());
        let newer_pos = outcome.additional_context.find("newer fact").unwrap();
        let older_pos = outcome.additional_context.find("older fact").unwrap();
        assert!(newer_pos < older_pos, "the more recent entry ranks first");
    }

    #[tokio::test]
    async fn global_only_resolution_never_leaks_a_worktree_scoped_entry() {
        let (_home, state, cache) = open_both();
        seed_memory(
            &state,
            &uuid(10),
            MemoryKind::Fact,
            ScopeKind::Global,
            GLOBAL_SCOPE_OWNER_ID,
            "global fact",
            0.5,
            1_000,
        )
        .await;
        seed_memory(
            &state,
            &uuid(11),
            MemoryKind::Fact,
            ScopeKind::Worktree,
            &uuid(12),
            "worktree-only fact",
            0.5,
            1_000,
        )
        .await;

        let state_read = state.open_read().expect("state read");
        let cache_read = cache.open_read().expect("cache read");
        let request = global_only_request("fact");
        let outcome = recall(
            &state_read,
            &cache_read,
            &UnavailableEmbedder,
            &BruteForceCosine,
            &request,
            1500,
        )
        .expect("recall");
        assert!(outcome.additional_context.contains("global fact"));
        assert!(!outcome.additional_context.contains("worktree-only fact"));
    }

    #[tokio::test]
    async fn a_terminal_entry_is_excluded() {
        let (_home, state, cache) = open_both();
        let id = uuid(20);
        seed_memory(
            &state,
            &id,
            MemoryKind::Task,
            ScopeKind::Global,
            GLOBAL_SCOPE_OWNER_ID,
            "a resolved task",
            0.5,
            1_000,
        )
        .await;
        let mid = id.clone();
        state
            .writer()
            .transaction(move |tx| transition_memory_entry(tx, &mid, MemoryState::Resolved))
            .await
            .expect("transition tx")
            .expect("transition domain");

        let state_read = state.open_read().expect("state read");
        let cache_read = cache.open_read().expect("cache read");
        let request = global_only_request("task");
        let outcome = recall(
            &state_read,
            &cache_read,
            &UnavailableEmbedder,
            &BruteForceCosine,
            &request,
            1500,
        )
        .expect("recall");
        assert_eq!(outcome.additional_context, "");
    }

    #[tokio::test]
    async fn zero_token_budget_yields_empty_additional_context() {
        let (_home, state, cache) = open_both();
        seed_memory(
            &state,
            &uuid(30),
            MemoryKind::Fact,
            ScopeKind::Global,
            GLOBAL_SCOPE_OWNER_ID,
            "some fact",
            0.5,
            1_000,
        )
        .await;

        let state_read = state.open_read().expect("state read");
        let cache_read = cache.open_read().expect("cache read");
        let request = global_only_request("fact");
        let outcome = recall(
            &state_read,
            &cache_read,
            &UnavailableEmbedder,
            &BruteForceCosine,
            &request,
            0,
        )
        .expect("recall");
        assert_eq!(outcome.additional_context, "");
    }

    #[test]
    fn estimate_tokens_rounds_up() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
    }

    #[tokio::test]
    async fn recall_entries_carry_memory_id_in_the_same_order_as_additional_context() {
        let (_home, state, cache) = open_both();
        let older_id = uuid(40);
        seed_memory(
            &state,
            &older_id,
            MemoryKind::Fact,
            ScopeKind::Global,
            GLOBAL_SCOPE_OWNER_ID,
            "older fact",
            0.5,
            1_000,
        )
        .await;
        let newer_id = uuid(41);
        seed_memory(
            &state,
            &newer_id,
            MemoryKind::Fact,
            ScopeKind::Global,
            GLOBAL_SCOPE_OWNER_ID,
            "newer fact",
            0.5,
            2_000,
        )
        .await;

        let state_read = state.open_read().expect("state read");
        let cache_read = cache.open_read().expect("cache read");
        let request = global_only_request("");
        let outcome = recall(
            &state_read,
            &cache_read,
            &UnavailableEmbedder,
            &BruteForceCosine,
            &request,
            1500,
        )
        .expect("recall");

        assert_eq!(
            outcome
                .entries
                .iter()
                .map(|e| e.memory_id.as_str())
                .collect::<Vec<_>>(),
            vec![newer_id.as_str(), older_id.as_str()],
            "entries mirror additional_context's own recency order"
        );
        assert_eq!(outcome.entries[0].text, "newer fact");
        assert_eq!(outcome.entries[0].kind, MemoryKind::Fact);
        assert_eq!(outcome.entries[0].state, MemoryState::Active);
        assert_eq!(outcome.entries[0].confidence, 0.5);
    }

    #[tokio::test]
    async fn recall_scope_label_reflects_global_only_resolution() {
        let (_home, state, cache) = open_both();
        let state_read = state.open_read().expect("state read");
        let cache_read = cache.open_read().expect("cache read");
        let request = global_only_request("anything");
        let outcome = recall(
            &state_read,
            &cache_read,
            &UnavailableEmbedder,
            &BruteForceCosine,
            &request,
            1500,
        )
        .expect("recall");
        assert_eq!(outcome.scope_label, "global");
    }

    #[tokio::test]
    async fn recall_entries_is_empty_when_candidates_exist_but_none_fit_the_budget() {
        let (_home, state, cache) = open_both();
        seed_memory(
            &state,
            &uuid(42),
            MemoryKind::Fact,
            ScopeKind::Global,
            GLOBAL_SCOPE_OWNER_ID,
            "some fact",
            0.5,
            1_000,
        )
        .await;

        let state_read = state.open_read().expect("state read");
        let cache_read = cache.open_read().expect("cache read");
        let request = global_only_request("fact");
        let outcome = recall(
            &state_read,
            &cache_read,
            &UnavailableEmbedder,
            &BruteForceCosine,
            &request,
            0,
        )
        .expect("recall");
        assert_eq!(outcome.additional_context, "");
        assert!(outcome.entries.is_empty());
    }
}
