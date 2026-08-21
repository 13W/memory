//! Recall: turning stored `memory_entry` rows into what a caller actually
//! reads. Two independent consumers, two independent shapes, one module:
//!
//! - **This file** — the router's pre-generation recall (T14-07, spec 08 §4
//!   step 3: "Input: window observations + recall of plausibly related
//!   existing entries (candidate conflict set)") and post-generation target
//!   resolution ([`resolve_target`]). Unscored by design — see below.
//! - **[`pipeline`]** (T14-08, spec 08 §6's `[FIXED pipeline]`) — the real,
//!   scored recall pipeline behind the `recall`/hook-injection surface:
//!   scope union → cardinality guard → lexical (`lexical` submodule) + dense
//!   (`dense` submodule) legs → fusion (`fusion` submodule) → lifecycle
//!   recheck → token budget → deterministic order → `format` submodule's
//!   byte-exact `additionalContext` text.
//!
//! # Why the router's own recall ([`candidate_conflict_set`]) stays unscored
//!
//! `local_rag_store::memory::runner`'s own module doc reserves the *name* "scored
//! relevance pipeline" for [`pipeline`] and keeps
//! [`local_rag_store::ConsolidationWindow`] free of any such field. What the
//! router receives is still an **unscored set**, not a ranked top-K a small
//! local model would have no reliable way to ask for "the next one" beyond:
//! every recall-eligible entry in every scope the window's own observations
//! actually touch (global, always; each distinct `repo_id`/`worktree_id` seen
//! among the window's [`WindowObservation`]s). [`crate::prompt`] shows that
//! set to the model so it can target an existing entry by `memory_id` (see
//! [`crate::schema`]'s module doc for why `memory_id`, never `canonical_key`,
//! is the addressing key).
//!
//! What this doc used to claim, and D-080 had to take back: that the router
//! "already shows the model every relevant entry (bounded, not ranked)". It
//! did not. The set was sorted by `memory_id` — UUIDv7, so time-ascending —
//! and truncated at [`MAX_PROMPT_CANDIDATES`], which means a scope holding
//! more than the cap showed the model only its **oldest** entries and hid
//! everything recent, including what the router itself had written a window
//! earlier. The reasoning above was sound about the *shape* of the answer and
//! silently wrong about its *contents*; the paragraph discussed only whether
//! the truncation was deterministic, never what the chosen order threw away.
//! That blind spot is the mechanism behind D-078's 136 copies of one
//! sentence, and on the owner's store it was live: 68 eligible entries
//! against a cap of 50, the 18 newest discarded on every run.
//!
//! So ranking now enters — narrowly, and only where the prompt overflows.
//! When the union fits, nothing changed. When it does not,
//! [`candidate_conflict_set`] keeps the entries the window is lexically
//! related to (spec 08 §4 step 3's own word: "plausibly related") and then
//! fills what is left newest-first — and shows them in that order, because
//! putting the entry the window is about at the bottom of a fifty-item list
//! spends the fix on nothing. This is not [`pipeline`] duplicated:
//! only its lexical leg is reused, and that leg is a pure synchronous
//! function over an already-fetched list backed by an ephemeral in-memory
//! FTS5 table — no embedder, no persistence, nothing async. "A consolidation
//! window is not a recall request" still holds for what the router *gets*;
//! it never justified choosing what to drop by age.
//!
//! [`resolve_target`] is the second, independent use of recall: **after**
//! generation, re-resolving whatever `target_memory_id` the model echoed back
//! against a fresh read — never trusting a model-echoed `entry_version`.
//! Small models are unreliable at echoing exact integers, and a stale value
//! would just be rejected by [`local_rag_store::apply_reinforce`]'s
//! optimistic-concurrency check anyway (aborting the whole consolidation
//! batch, per `runner`'s atomicity guarantee) — re-resolving fresh is
//! strictly better and avoids burning a whole attempt on a stale echo.

mod dense;
mod format;
mod fusion;
mod lexical;
pub mod pipeline;

pub use dense::{
    BruteForceCosine, DenseLegUnavailable, DenseRecallHit, MemoryDenseBackend, QueryEmbedError,
    QueryEmbedder, UnavailableEmbedder, dense_leg,
};
pub use format::{
    RECALL_ENTRY_CAP_BYTES, RecallEntry, format_additional_context, prepare_entry_text,
};
pub use fusion::{FusedRecallHit, rrf};
pub use lexical::lexical_leg;
pub use pipeline::{
    MAX_RECALL_CANDIDATES, QueryNotNormalized, RecallOutcome, RecallRequest, RecallResultEntry,
    recall, scopes_for,
};

use std::collections::{BTreeMap, BTreeSet};

use local_rag_store::rusqlite;
use local_rag_store::rusqlite::Connection;
use local_rag_store::{
    GLOBAL_SCOPE_OWNER_ID, MemoryEntrySummary, ScopeKind, WindowObservation,
    active_entries_for_scope, memory_entry_summary,
};

/// `[SPEC]` placeholder cap on how many existing entries [`candidate_conflict_set`]
/// puts in front of the model — bounds prompt size for a small local model's
/// context window.
///
/// **Which** entries survive it is D-080's rule, not a byproduct of the sort:
/// entries lexically related to the window first, then the rest newest-first
/// (see [`candidate_conflict_set`]). Until D-080 the set was simply sorted by
/// `memory_id` — time-ascending, since ids are UUIDv7 — and truncated, so a
/// scope larger than the cap showed the model only its **oldest** entries and
/// hid everything recent, including what the router itself had written a
/// window earlier.
pub const MAX_PROMPT_CANDIDATES: usize = 50;

/// Every recall-eligible entry in a scope the window's observations touch:
/// the global scope, plus one lookup per distinct `repo_id`/`worktree_id`
/// seen, deduplicated.
///
/// At most [`MAX_PROMPT_CANDIDATES`] of them reach the model. When the union
/// fits, every entry goes, ordered by `memory_id` — byte-identical to what
/// this function did before D-080. When it does not fit, the survivors are
/// chosen (D-080), not truncated off the tail:
///
/// 1. entries the window is lexically related to, best match first — this is
///    spec 08 §4 step 3's own word, "plausibly related";
/// 2. then the remaining entries **newest first**, filling the rest of the
///    budget, because the entries the router most often needs to reinforce,
///    supersede or retract are the ones it wrote most recently.
///
/// Above the cap the set is also **presented** in that order, most related
/// first. Below it, order stays `memory_id` ascending as it always was —
/// where nothing was dropped, the order carries no signal to pass on. That
/// asymmetry was measured, not assumed: presenting the selection in
/// `memory_id` order left the one entry the window was about sitting at
/// position 49 of 50, and the model answered `noop`; moving it to the front
/// changed the answer (D-080's evidence has the full three-way run).
///
/// A window with no excerpt text at all yields no query terms, so selection
/// falls back to rule 2 alone and no SQL is issued — the same treatment
/// [`lexical_leg`] gives a termless query.
pub fn candidate_conflict_set(
    conn: &Connection,
    observations: &[WindowObservation],
) -> rusqlite::Result<Vec<MemoryEntrySummary>> {
    let mut repo_ids: BTreeSet<&str> = BTreeSet::new();
    let mut worktree_ids: BTreeSet<&str> = BTreeSet::new();
    for o in observations {
        if let Some(r) = o.repo_id.as_deref() {
            repo_ids.insert(r);
        }
        if let Some(w) = o.worktree_id.as_deref() {
            worktree_ids.insert(w);
        }
    }

    let mut out = active_entries_for_scope(conn, ScopeKind::Global, GLOBAL_SCOPE_OWNER_ID, None)?;
    for repo_id in repo_ids {
        out.extend(active_entries_for_scope(
            conn,
            ScopeKind::Repository,
            repo_id,
            None,
        )?);
    }
    for worktree_id in worktree_ids {
        out.extend(active_entries_for_scope(
            conn,
            ScopeKind::Worktree,
            worktree_id,
            None,
        )?);
    }

    out.sort_by(|a, b| a.memory_id.cmp(&b.memory_id));
    out.dedup_by(|a, b| a.memory_id == b.memory_id);
    if out.len() > MAX_PROMPT_CANDIDATES {
        out = select_prompt_candidates(out, observations)?;
    }
    Ok(out)
}

/// D-080's selection, applied only when the union overflows
/// [`MAX_PROMPT_CANDIDATES`]: lexical matches against the window's own text
/// first, then the rest newest-first. `entries` arrives sorted by `memory_id`
/// ascending and deduplicated.
fn select_prompt_candidates(
    entries: Vec<MemoryEntrySummary>,
    observations: &[WindowObservation],
) -> rusqlite::Result<Vec<MemoryEntrySummary>> {
    let query = window_query(observations);
    let docs: Vec<(&str, &str)> = entries
        .iter()
        .map(|e| (e.memory_id.as_str(), e.text.as_str()))
        .collect();
    let ranked = lexical::rank_by_lexical(&query, &docs, MAX_PROMPT_CANDIDATES)?;

    let mut by_id: BTreeMap<&str, &MemoryEntrySummary> =
        entries.iter().map(|e| (e.memory_id.as_str(), e)).collect();

    let mut chosen: Vec<MemoryEntrySummary> = Vec::with_capacity(MAX_PROMPT_CANDIDATES);
    for (memory_id, _) in &ranked {
        if let Some(entry) = by_id.remove(memory_id.as_str()) {
            chosen.push(entry.clone());
        }
    }
    // `by_id` is a BTreeMap keyed by `memory_id`, so iterating it in reverse
    // is newest-first: ids are UUIDv7, and lexicographic order on them is
    // chronological.
    for (_, entry) in by_id.iter().rev() {
        if chosen.len() >= MAX_PROMPT_CANDIDATES {
            break;
        }
        chosen.push((*entry).clone());
    }
    Ok(chosen)
}

/// The query D-080's selection ranks against: the window's own excerpt text,
/// which is exactly what [`crate::prompt`] shows the model. An observation
/// with no excerpt contributes nothing (it has no content to be related to).
fn window_query(observations: &[WindowObservation]) -> String {
    observations
        .iter()
        .filter_map(|o| o.short_evidence_excerpt.as_deref())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Fresh-resolve `target_memory_id` (see the module doc's "never trust an
/// echoed `entry_version`"). `None` covers both "no such id" and "that id is
/// not recall-eligible for a plain existing-entry citation" — callers
/// ([`crate::guard`]) that need to see terminal entries too (e.g. a
/// `supersede` target legitimately transitions `active`/`confirmed` only, so
/// this restriction is actually always correct there too) use this
/// uniformly; there is no case in this crate that needs
/// [`local_rag_store::memory_entry_summary`]'s terminal-inclusive behavior
/// directly.
pub fn resolve_target(
    conn: &Connection,
    target_memory_id: &str,
) -> rusqlite::Result<Option<MemoryEntrySummary>> {
    let Some(summary) = memory_entry_summary(conn, target_memory_id)? else {
        return Ok(None);
    };
    if summary.state.is_terminal() {
        return Ok(None);
    }
    Ok(Some(summary))
}

#[cfg(test)]
mod tests {
    use local_rag_core::identity::uuidv7_from;
    use local_rag_core::paths::StoreLayout;
    use local_rag_store::{
        EvidenceKind, MemoryKind, MemoryState, NewMemoryEntry, StateDb, TrustLevel,
        create_memory_entry, transition_memory_entry,
    };
    use local_rag_test_support::TempHome;

    use super::*;

    fn uuid(seed: u8) -> String {
        let mut rand = [0u8; 10];
        rand[9] = seed;
        uuidv7_from(1000, rand).to_string()
    }

    /// Ids that stay chronologically ordered past 256 entries: `uuidv7_from`
    /// pins the timestamp, so ordering comes from the random tail, and the
    /// last two bytes ascending means the id string ascends too.
    fn uuid_at(i: u16) -> String {
        let mut rand = [0u8; 10];
        rand[8] = (i >> 8) as u8;
        rand[9] = (i & 0xff) as u8;
        uuidv7_from(1000, rand).to_string()
    }

    fn open_state() -> (TempHome, StateDb) {
        let home = TempHome::new().expect("temp home");
        let layout = StoreLayout::new(home.join("local-rag"));
        layout.ensure().expect("ensure store tree");
        let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
        (home, db)
    }

    async fn create_memory(
        db: &StateDb,
        memory_id: &str,
        kind: MemoryKind,
        scope_kind: ScopeKind,
        scope_owner_id: &str,
    ) {
        create_memory_with_text(
            db,
            memory_id,
            kind,
            scope_kind,
            scope_owner_id,
            "some durable text",
        )
        .await;
    }

    async fn create_memory_with_text(
        db: &StateDb,
        memory_id: &str,
        kind: MemoryKind,
        scope_kind: ScopeKind,
        scope_owner_id: &str,
        text: &str,
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
                        confidence: 0.5,
                        importance: 0.5,
                        valid_from_tree: None,
                        last_verified_tree: None,
                        supersedes_id: None,
                    },
                    1000,
                )
            })
            .await
            .expect("create memory tx")
            .expect("create memory domain");
    }

    fn window_observation(repo_id: Option<&str>, worktree_id: Option<&str>) -> WindowObservation {
        WindowObservation {
            observation_id: uuid(1),
            received_seq: 1,
            event_type: "Stop".to_string(),
            evidence_kind: EvidenceKind::UserStatement,
            trust: TrustLevel::Normal,
            session_id: "sess-1".to_string(),
            repo_id: repo_id.map(str::to_string),
            worktree_id: worktree_id.map(str::to_string),
            agent_id: None,
            commit_hash: None,
            short_evidence_excerpt: None,
            payload: None,
        }
    }

    #[tokio::test]
    async fn candidate_conflict_set_includes_global_and_touched_scopes_only() {
        let (_home, db) = open_state();
        let repo_id = uuid(10);
        let other_repo_id = uuid(11);

        create_memory(
            &db,
            &uuid(20),
            MemoryKind::Convention,
            ScopeKind::Global,
            GLOBAL_SCOPE_OWNER_ID,
        )
        .await;
        create_memory(
            &db,
            &uuid(21),
            MemoryKind::Fact,
            ScopeKind::Repository,
            &repo_id,
        )
        .await;
        create_memory(
            &db,
            &uuid(22),
            MemoryKind::Fact,
            ScopeKind::Repository,
            &other_repo_id,
        )
        .await;

        let read = db.open_read().expect("read conn");
        let observations = vec![window_observation(Some(&repo_id), None)];
        let found = candidate_conflict_set(&read, &observations).expect("query");
        let ids: Vec<&str> = found.iter().map(|e| e.memory_id.as_str()).collect();
        assert!(
            ids.contains(&uuid(20).as_str()),
            "global is always included"
        );
        assert!(
            ids.contains(&uuid(21).as_str()),
            "the touched repo is included"
        );
        assert!(
            !ids.contains(&uuid(22).as_str()),
            "an untouched repo must not leak into the prompt"
        );
    }

    #[tokio::test]
    async fn candidate_conflict_set_is_empty_when_the_window_touches_nothing_and_no_globals_exist()
    {
        let (_home, db) = open_state();
        let read = db.open_read().expect("read conn");
        let found = candidate_conflict_set(&read, &[]).expect("query");
        assert!(found.is_empty());
    }

    // -----------------------------------------------------------------
    // D-080: which entries survive MAX_PROMPT_CANDIDATES
    //
    // Every test here seeds a global scope *larger* than the cap, because
    // that is the only situation in which the selection rule runs at all —
    // and, until D-080, the only situation in which the router went blind.
    // -----------------------------------------------------------------

    /// Filler text with no term in common with [`WINDOW_TEXT`], so the
    /// lexical half of the rule cannot match it and the recency half decides.
    fn filler_text(i: u16) -> String {
        format!("Unrelated durable note number {i} about build tooling.")
    }

    const WINDOW_TEXT: &str =
        "We changed our mind: the internal API will use gRPC instead of REST from now on.";

    /// `n` global entries, ids ascending with `i` (so higher `i` is newer).
    /// `related` gets text that overlaps [`WINDOW_TEXT`]; everything else gets
    /// [`filler_text`].
    async fn seed_global_entries(db: &StateDb, n: u16, related: &[u16]) {
        for i in 0..n {
            let text = if related.contains(&i) {
                format!("Use REST for the internal API ({i}).")
            } else {
                filler_text(i)
            };
            create_memory_with_text(
                db,
                &uuid_at(i),
                MemoryKind::Decision,
                ScopeKind::Global,
                GLOBAL_SCOPE_OWNER_ID,
                &text,
            )
            .await;
        }
    }

    fn window_with_text(text: Option<&str>) -> Vec<WindowObservation> {
        let mut o = window_observation(None, None);
        o.short_evidence_excerpt = text.map(str::to_string);
        vec![o]
    }

    /// The defect D-080 exists for: the entry the window is actually about was
    /// created most recently, so the old "sort by id, truncate" rule dropped
    /// it — the router could not reinforce, supersede or retract what it had
    /// just written.
    #[tokio::test]
    async fn a_recent_related_entry_survives_a_store_larger_than_the_cap() {
        let (_home, db) = open_state();
        let n = (MAX_PROMPT_CANDIDATES + 10) as u16;
        let newest = n - 1;
        seed_global_entries(&db, n, &[newest]).await;

        let read = db.open_read().expect("read conn");
        let found =
            candidate_conflict_set(&read, &window_with_text(Some(WINDOW_TEXT))).expect("query");

        assert_eq!(found.len(), MAX_PROMPT_CANDIDATES);
        let ids: Vec<&str> = found.iter().map(|e| e.memory_id.as_str()).collect();
        assert!(
            ids.contains(&uuid_at(newest).as_str()),
            "the entry this window is about must reach the model",
        );
    }

    /// The other half of the rule: whatever budget the lexical matches leave
    /// goes to the newest entries, not the oldest. Before D-080 this was
    /// exactly inverted.
    #[tokio::test]
    async fn the_rest_of_the_budget_goes_to_the_newest_entries() {
        let (_home, db) = open_state();
        let n = (MAX_PROMPT_CANDIDATES + 10) as u16;
        seed_global_entries(&db, n, &[0]).await;

        let read = db.open_read().expect("read conn");
        let found =
            candidate_conflict_set(&read, &window_with_text(Some(WINDOW_TEXT))).expect("query");
        let ids: Vec<&str> = found.iter().map(|e| e.memory_id.as_str()).collect();

        assert!(
            ids.contains(&uuid_at(0).as_str()),
            "the one lexical match is kept even though it is the oldest entry",
        );
        assert!(
            ids.contains(&uuid_at(n - 1).as_str()),
            "the newest entry fills the budget",
        );
        assert!(
            !ids.contains(&uuid_at(1).as_str()),
            "an old, unrelated entry is what gets dropped: {ids:?}",
        );
    }

    /// A window with nothing quotable yields no query terms, so no SQL runs
    /// and recency alone decides — still the newest, never the oldest.
    #[tokio::test]
    async fn a_window_with_no_excerpt_keeps_the_newest_entries() {
        let (_home, db) = open_state();
        let n = (MAX_PROMPT_CANDIDATES + 10) as u16;
        seed_global_entries(&db, n, &[]).await;

        let read = db.open_read().expect("read conn");
        let found = candidate_conflict_set(&read, &window_with_text(None)).expect("query");
        let ids: Vec<&str> = found.iter().map(|e| e.memory_id.as_str()).collect();

        assert_eq!(found.len(), MAX_PROMPT_CANDIDATES);
        assert!(ids.contains(&uuid_at(n - 1).as_str()), "{ids:?}");
        assert!(!ids.contains(&uuid_at(0).as_str()), "{ids:?}");
    }

    /// The guard on everything that already worked: a union within the cap is
    /// returned whole, in `memory_id` order, whether or not the window matches
    /// it. All 42 `memory.router.op.*` fixtures live in this branch (their
    /// conflict sets are at most one entry), as do the two scope tests above.
    #[tokio::test]
    async fn a_union_within_the_cap_is_returned_whole_and_ordered_by_id() {
        let (_home, db) = open_state();
        seed_global_entries(&db, 10, &[3]).await;

        let read = db.open_read().expect("read conn");
        let found =
            candidate_conflict_set(&read, &window_with_text(Some(WINDOW_TEXT))).expect("query");

        let ids: Vec<String> = found.into_iter().map(|e| e.memory_id).collect();
        let expected: Vec<String> = (0..10u16).map(uuid_at).collect();
        assert_eq!(ids, expected, "no reordering, no dropping below the cap");
    }

    /// Above the cap the survivors are also *shown* related-first. Measured,
    /// not assumed: with the same selection presented in `memory_id` order
    /// the one entry the window was about sat at position 49 of 50 and the
    /// model answered `noop`.
    #[tokio::test]
    async fn above_the_cap_the_most_related_entry_is_shown_first() {
        let (_home, db) = open_state();
        let n = (MAX_PROMPT_CANDIDATES + 10) as u16;
        let newest = n - 1;
        seed_global_entries(&db, n, &[newest]).await;

        let read = db.open_read().expect("read conn");
        let found =
            candidate_conflict_set(&read, &window_with_text(Some(WINDOW_TEXT))).expect("query");

        assert_eq!(
            found[0].memory_id,
            uuid_at(newest),
            "the only lexical match must lead the list, not trail it",
        );
    }

    /// Selection must stay golden-testable — the property the pre-D-080 sort
    /// was chosen for, and the one a ranked rule could most easily lose.
    #[tokio::test]
    async fn selection_is_deterministic_across_calls() {
        let (_home, db) = open_state();
        let n = (MAX_PROMPT_CANDIDATES + 10) as u16;
        seed_global_entries(&db, n, &[7, 9, 11]).await;

        let read = db.open_read().expect("read conn");
        let window = window_with_text(Some(WINDOW_TEXT));
        let first = candidate_conflict_set(&read, &window).expect("query");
        let second = candidate_conflict_set(&read, &window).expect("query");
        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn resolve_target_finds_an_active_entry() {
        let (_home, db) = open_state();
        let id = uuid(30);
        create_memory(
            &db,
            &id,
            MemoryKind::Fact,
            ScopeKind::Global,
            GLOBAL_SCOPE_OWNER_ID,
        )
        .await;

        let read = db.open_read().expect("read conn");
        let found = resolve_target(&read, &id)
            .expect("query")
            .expect("active entry found");
        assert_eq!(found.memory_id, id);
    }

    #[tokio::test]
    async fn resolve_target_is_none_for_a_terminal_entry() {
        let (_home, db) = open_state();
        let id = uuid(31);
        create_memory(
            &db,
            &id,
            MemoryKind::Task,
            ScopeKind::Global,
            GLOBAL_SCOPE_OWNER_ID,
        )
        .await;
        let (mid, _owner) = (id.clone(), ());
        db.writer()
            .transaction(move |tx| transition_memory_entry(tx, &mid, MemoryState::Resolved))
            .await
            .expect("transition tx")
            .expect("transition domain");

        let read = db.open_read().expect("read conn");
        assert_eq!(resolve_target(&read, &id).expect("query"), None);
    }

    #[tokio::test]
    async fn resolve_target_is_none_for_an_unknown_id() {
        let (_home, db) = open_state();
        let read = db.open_read().expect("read conn");
        assert_eq!(resolve_target(&read, &uuid(32)).expect("query"), None);
    }
}
