//! The router's pre-generation recall (T14-07, spec 08 §4 step 3: "Input:
//! window observations + recall of plausibly related existing entries
//! (candidate conflict set)").
//!
//! This is deliberately **not** T14-08's scored relevance pipeline —
//! [`local_rag_store::runner`]'s own module doc already reserves that name
//! for a later task and keeps [`local_rag_store::ConsolidationWindow`] free
//! of any such field. Without it, "plausibly related" is approximated the
//! only honest way available pre-T14-08: every recall-eligible entry in
//! every scope the window's own observations actually touch (global,
//! always; each distinct `repo_id`/`worktree_id` seen among the window's
//! [`WindowObservation`]s) — a real, unscored candidate set, not a fabricated
//! relevance ranking. [`crate::prompt`] shows this set to the model so it can
//! target an existing entry by `memory_id` (see [`crate::schema`]'s module
//! doc for why `memory_id`, never `canonical_key`, is the addressing key).
//!
//! [`resolve_target`] is the second, independent use of recall: **after**
//! generation, re-resolving whatever `target_memory_id` the model echoed back
//! against a fresh read — never trusting a model-echoed `entry_version`.
//! Small models are unreliable at echoing exact integers, and a stale value
//! would just be rejected by [`local_rag_store::apply_reinforce`]'s
//! optimistic-concurrency check anyway (aborting the whole consolidation
//! batch, per `runner`'s atomicity guarantee) — re-resolving fresh is
//! strictly better and avoids burning a whole attempt on a stale echo.

use std::collections::BTreeSet;

use local_rag_store::rusqlite;
use local_rag_store::rusqlite::Connection;
use local_rag_store::{
    GLOBAL_SCOPE_OWNER_ID, MemoryEntrySummary, ScopeKind, WindowObservation,
    active_entries_for_scope, memory_entry_summary,
};

/// `[SPEC]` placeholder cap on how many existing entries [`candidate_conflict_set`]
/// puts in front of the model — bounds prompt size for a small local model's
/// context window. Entries are ordered by `memory_id` (time-ordered, since
/// ids are UUIDv7) before truncating, so which entries survive the cap is
/// deterministic and golden-testable, not a function of scope-iteration
/// order.
pub const MAX_PROMPT_CANDIDATES: usize = 50;

/// Every recall-eligible entry in a scope the window's observations touch:
/// the global scope, plus one lookup per distinct `repo_id`/`worktree_id`
/// seen. Deduplicated and ordered by `memory_id`, truncated to
/// [`MAX_PROMPT_CANDIDATES`].
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
    out.truncate(MAX_PROMPT_CANDIDATES);
    Ok(out)
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
        let (id, owner) = (memory_id.to_string(), scope_owner_id.to_string());
        db.writer()
            .transaction(move |tx| {
                create_memory_entry(
                    tx,
                    &NewMemoryEntry {
                        memory_id: &id,
                        kind,
                        text: "some durable text",
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
