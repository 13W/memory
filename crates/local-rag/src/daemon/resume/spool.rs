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
    ImportError, ImportOutcome, RequestRoot, StateDb, import_session_tail, known_spool_sessions,
};

use super::super::jobs::{JobKind, JobRegistry};

/// Resume every known spool session's import, one [`JobRegistry`]-tracked
/// job per session (spool 02 §4.1 step 5).
///
/// `RequestRoot::default()` (`Resolution::GlobalOnly`) is used for every
/// session: a historical spool session at startup carries no live proxy
/// connection to git-probe a `worktree_root` from at all (spec 07 §2/§3's
/// frame shape has no such field) — `local_rag_store::observation::import`'s
/// own module doc names this the structurally correct choice ("an unknown
/// root imports with NULL worktree", spec 07 §5), not a stand-in for real
/// resolution. A *live* HELLO-carried root (once T15-02 exists) is a
/// different, per-connection code path, not this startup sweep.
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
    let request_root = RequestRoot::default();
    let mut results = Vec::with_capacity(sessions.len());
    for session_id in sessions {
        let _job = jobs.begin(JobKind::SpoolImport);
        super::test_resume_pause().await;
        let outcome = import_session_tail(
            db,
            layout,
            &session_id,
            &request_root,
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
