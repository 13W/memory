//! Startup spool-import catch-up (spec 02 §4.1 step 5: "Resume: pending
//! spool import (07 §6)") — T15-01.
//!
//! `known_spool_sessions`/`import_session_tail`
//! (`local_rag_store::observation`, T13-04) are the ready-made primitives;
//! this module is the daemon-side driver that walks every session on disk at
//! startup — a driver `import_session_tail`'s own doc comment names as
//! belonging to "a background worker, group 15's daemon lifecycle." This is
//! a **startup catch-up pass**, not a continuous watcher — no filesystem
//! watching is wired here (see `daemon` module's own scope note).

use local_rag_core::identity::UuidSource;
use local_rag_core::paths::StoreLayout;
use local_rag_store::{
    ImportError, ImportOutcome, StateDb, import_session_tail, known_spool_sessions,
};

use super::super::gitroot::ProbingRootResolver;
use super::super::jobs::{JobKind, JobRegistry};

/// Resume every known spool session's import, one [`JobRegistry`]-tracked
/// job per session (spool 02 §4.1 step 5).
///
/// Root resolution does not need a live connection: every frame carries the
/// hook's own `cwd` (spec 07 §3's `worktree_root`), so this sweep git-probes
/// it through one shared [`ProbingRootResolver`] and each session's envelopes
/// get their real `repo_id`/`worktree_id` (spec 07 §5, D-063). Before D-063
/// this passed a fixed `RequestRoot::default()` on the incorrect premise that
/// the frame shape has no such field. A root that no longer exists on disk, or
/// one belonging to a worktree this store never registered, still resolves to
/// `GlobalOnly` — spec 07 §5's "an unknown root imports with NULL worktree".
///
/// A session directory that cannot even be enumerated (`known_spool_sessions`
/// failing, e.g. the `spool/` directory itself is unreadable) yields an empty
/// result rather than propagating — startup must not fail outright over spool
/// housekeeping; the next resume pass (a later daemon start, or the periodic
/// trigger a later task wires) tries again.
pub async fn resume_spool_import(
    db: &StateDb,
    layout: &StoreLayout,
    uuids: &(dyn UuidSource + Send + Sync),
    jobs: &JobRegistry,
    now_ms: i64,
    payload_ttl_hours: u64,
) -> Vec<(String, Result<ImportOutcome, ImportError>)> {
    let sessions = match known_spool_sessions(layout) {
        Ok(sessions) => sessions,
        Err(_) => return Vec::new(),
    };
    let root_resolver = ProbingRootResolver::default();
    let mut results = Vec::with_capacity(sessions.len());
    for session_id in sessions {
        let _job = jobs.begin(JobKind::SpoolImport);
        super::test_resume_pause().await;
        super::test_resume_blocking_stall();
        let outcome = import_session_tail(
            db,
            layout,
            &session_id,
            &root_resolver,
            uuids,
            now_ms,
            payload_ttl_hours,
        )
        .await;
        results.push((session_id, outcome));
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use local_rag_core::identity::{Uuid, uuidv7_from};
    use local_rag_core::spool::{FramePayload, encode_frame, encode_segment_header};
    use local_rag_test_support::TempHome;
    use std::sync::atomic::{AtomicU64, Ordering};

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
            uuidv7_from(1000 + n, [0xAB; 10])
        }
    }

    fn open_state() -> (TempHome, StoreLayout, StateDb) {
        let home = TempHome::new().expect("temp home");
        let layout = StoreLayout::new(home.join("local-rag"));
        layout.ensure().expect("ensure store tree");
        let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
        (home, layout, db)
    }

    fn spool_fixture(session_id: &str, source_event_id: &str, captured_at: i64) -> FramePayload {
        FramePayload {
            format_version: 1,
            source_event_id: source_event_id.to_string(),
            dedup_key: None,
            event_type: "Stop".to_string(),
            captured_at,
            session_id: session_id.to_string(),
            agent_id: None,
            turn_id: None,
            batch_id: None,
            worktree_root: None,
            commit: None,
            evidence_kind: "model_claim".to_string(),
            trust: "low".to_string(),
            paths: vec![],
            redaction_version: None,
            payload: None,
            short_evidence_excerpt: None,
        }
    }

    fn write_spool_segment(layout: &StoreLayout, session_id: &str, seq: u32, frame: &FramePayload) {
        let session_dir = layout.spool_session(session_id);
        std::fs::create_dir_all(&session_dir).expect("session dir");
        let mut bytes = encode_segment_header().to_vec();
        bytes.extend_from_slice(&encode_frame(frame).expect("under the frame cap"));
        std::fs::write(session_dir.join(format!("{seq:06}.seg")), bytes).expect("write segment");
    }

    #[tokio::test]
    async fn no_sessions_on_disk_resumes_nothing() {
        let (_home, layout, db) = open_state();
        let uuids = SeqUuidV7::new();
        let jobs = JobRegistry::new();
        let results = resume_spool_import(&db, &layout, &uuids, &jobs, 1_000, 72).await;
        assert!(results.is_empty());
        assert!(jobs.is_empty(), "no job should be left tracked afterward");
    }

    #[tokio::test]
    async fn every_known_session_is_resumed_and_tracked_as_a_job() {
        let (_home, layout, db) = open_state();
        let uuids = SeqUuidV7::new();
        let jobs = JobRegistry::new();

        write_spool_segment(
            &layout,
            "sess-a",
            1,
            &spool_fixture("sess-a", "st:a:1", 1_000),
        );
        write_spool_segment(
            &layout,
            "sess-b",
            1,
            &spool_fixture("sess-b", "st:b:1", 1_000),
        );

        let results = resume_spool_import(&db, &layout, &uuids, &jobs, 1_000, 72).await;
        let mut ids: Vec<String> = results.iter().map(|(id, _)| id.clone()).collect();
        ids.sort();
        assert_eq!(ids, vec!["sess-a".to_string(), "sess-b".to_string()]);
        for (_, outcome) in &results {
            outcome.as_ref().expect("import must succeed");
        }
        assert!(
            jobs.is_empty(),
            "every job guard must be released once its session finishes"
        );
    }

    /// D-063, end to end: a session whose frames carry a `cwd` inside a
    /// registered worktree imports **attributed** — the resolved
    /// `repo_id`/`worktree_id` land on every envelope, which is what lets the
    /// memory router place a `repository`-scoped entry at all. Before D-063
    /// this same fixture imported with NULL ids.
    #[tokio::test]
    async fn a_registered_worktrees_own_cwd_attributes_its_envelopes() {
        if !git_available() {
            eprintln!("skip: git not on PATH");
            return;
        }
        let (home, layout, db) = open_state();
        let uuids = SeqUuidV7::new();
        let jobs = JobRegistry::new();

        let repo = home.join("repo");
        std::fs::create_dir_all(&repo).expect("create repo dir");
        git_init(&repo);
        // Register it exactly as a real `local-rag index` would: the probe's
        // own canonical path and fingerprint, not a hand-written string.
        let facts = crate::daemon::gitroot::probe(&repo).expect("probe the temp repo");
        let repo_id = "11111111-1111-7111-8111-111111111111".to_string();
        let worktree_id = "22222222-2222-7222-8222-222222222222".to_string();
        {
            let (r, w, f) = (repo_id.clone(), worktree_id.clone(), facts.clone());
            db.writer()
                .transaction(move |tx| {
                    local_rag_store::create_repository(tx, &r, None, 1_000)?;
                    local_rag_store::create_worktree(
                        tx,
                        &w,
                        &r,
                        local_rag_store::WorktreeKind::Main,
                        1_000,
                    )?;
                    local_rag_store::observe_worktree_path(
                        tx,
                        &w,
                        &f.observed_canonical_path,
                        &f.display_path,
                        &f.path_fingerprint,
                        1_000,
                    )
                })
                .await
                .expect("register the worktree");
        }

        let mut frame = spool_fixture("sess-root", "st:root:1", 1_000);
        frame.worktree_root = Some(repo.to_str().expect("utf-8 path").to_string());
        write_spool_segment(&layout, "sess-root", 1, &frame);

        let results = resume_spool_import(&db, &layout, &uuids, &jobs, 1_000, 72).await;
        assert_eq!(results.len(), 1);
        results[0].1.as_ref().expect("import must succeed");

        let read = db.open_read().expect("read conn");
        let (got_repo, got_worktree): (Option<String>, Option<String>) = read
            .query_row(
                "SELECT repo_id, worktree_id FROM observation_envelope WHERE session_id = 'sess-root'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("the envelope was imported");
        assert_eq!(got_repo, Some(repo_id));
        assert_eq!(got_worktree, Some(worktree_id));
    }

    /// The negative half: the same frame pointing at a directory this store
    /// never registered still imports, with NULL ids (spec 07 §5's "an unknown
    /// root imports with NULL worktree").
    #[tokio::test]
    async fn an_unregistered_cwd_still_imports_with_null_ids() {
        let (home, layout, db) = open_state();
        let uuids = SeqUuidV7::new();
        let jobs = JobRegistry::new();

        let elsewhere = home.join("never-registered");
        std::fs::create_dir_all(&elsewhere).expect("create dir");
        let mut frame = spool_fixture("sess-unknown", "st:unknown:1", 1_000);
        frame.worktree_root = Some(elsewhere.to_str().expect("utf-8 path").to_string());
        write_spool_segment(&layout, "sess-unknown", 1, &frame);

        let results = resume_spool_import(&db, &layout, &uuids, &jobs, 1_000, 72).await;
        results[0].1.as_ref().expect("import must succeed");

        let read = db.open_read().expect("read conn");
        let (got_repo, got_worktree): (Option<String>, Option<String>) = read
            .query_row(
                "SELECT repo_id, worktree_id FROM observation_envelope
                  WHERE session_id = 'sess-unknown'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("the envelope was imported");
        assert!(got_repo.is_none());
        assert!(got_worktree.is_none());
    }

    fn git_available() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn git_init(dir: &std::path::Path) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["init", "-q"])
            .status()
            .expect("run git init");
        assert!(status.success(), "git init failed");
    }

    #[tokio::test]
    async fn resuming_twice_is_idempotent() {
        let (_home, layout, db) = open_state();
        let uuids = SeqUuidV7::new();
        let jobs = JobRegistry::new();
        write_spool_segment(
            &layout,
            "sess-a",
            1,
            &spool_fixture("sess-a", "st:a:1", 1_000),
        );

        let first = resume_spool_import(&db, &layout, &uuids, &jobs, 1_000, 72).await;
        assert_eq!(first.len(), 1);
        let first_outcome = first[0].1.as_ref().expect("first import succeeds");
        assert_eq!(first_outcome.report.imported, 1);

        let second = resume_spool_import(&db, &layout, &uuids, &jobs, 2_000, 72).await;
        assert_eq!(second.len(), 1);
        let second_outcome = second[0].1.as_ref().expect("second import succeeds too");
        assert_eq!(
            second_outcome.report.imported, 0,
            "nothing new to import the second time"
        );
        assert_eq!(
            second_outcome.final_segment_seq,
            first_outcome.final_segment_seq
        );
        assert_eq!(
            second_outcome.final_committed_offset,
            first_outcome.final_committed_offset
        );
    }
}
