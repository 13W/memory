//! T13-04 acceptance tests for the transactional observation importer (spec
//! 03 §2.5, 07 §5/§6): [`import_session_tail`] driving real on-disk LRSP
//! segments through T13-03's decoder into `state.sqlite`, and [`import_batch`]
//! directly for the resolution-boundary cases.
//!
//! Determinism: an isolated [`TempHome`], a fixed `now_ms`, and a seeded
//! [`SeqUuidV7`] (the same local `UuidSource` double `crates/index/tests/
//! reconcile.rs` uses — `test-support` is deliberately dependency-free).
//! Segments are written directly via `local_rag_core::spool::{encode_frame,
//! encode_segment_header}`; no real hook process runs.

use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};

use local_rag_core::identity::{Uuid, UuidSource, uuidv7_from};
use local_rag_core::paths::StoreLayout;
use local_rag_core::spool::{FramePayload, encode_frame, encode_segment_header};
use local_rag_store::rusqlite::Connection;
use local_rag_store::{
    DecodedObservation, DedupClass, ImportError, RequestRoot, StateDb, WorktreeKind,
    WorktreeRootFacts, create_repository, create_worktree, diagnose_spool_tail, import_batch,
    import_session_tail, observe_worktree_path,
};
use local_rag_test_support::TempHome;

fn open_state() -> (TempHome, StoreLayout, StateDb) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    (home, layout, db)
}

struct SeqUuidV7 {
    counter: AtomicU64,
    tag: u8,
}

impl SeqUuidV7 {
    fn new() -> Self {
        Self::tagged(0xAB)
    }

    /// A second (or third, ...) independent source: distinct `tag` bytes keep
    /// two sources' sequences from ever colliding.
    fn tagged(tag: u8) -> Self {
        Self {
            counter: AtomicU64::new(0),
            tag,
        }
    }
}

impl UuidSource for SeqUuidV7 {
    fn next_uuid(&self) -> Uuid {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        uuidv7_from(1000 + n, [self.tag; 10])
    }
}

fn fixture(
    event_type: &str,
    source_event_id: &str,
    dedup_key: Option<&str>,
    session_id: &str,
    captured_at: i64,
    paths: Vec<String>,
    payload: Option<&str>,
) -> FramePayload {
    FramePayload {
        format_version: 1,
        source_event_id: source_event_id.to_string(),
        dedup_key: dedup_key.map(str::to_string),
        event_type: event_type.to_string(),
        captured_at,
        session_id: session_id.to_string(),
        agent_id: None,
        turn_id: None,
        batch_id: None,
        worktree_root: None,
        commit: None,
        evidence_kind: "model_claim".to_string(),
        trust: "low".to_string(),
        paths,
        redaction_version: None,
        payload: payload.map(str::to_string),
        short_evidence_excerpt: None,
    }
}

/// Write a whole segment (header + one frame per `FramePayload`) to
/// `spool/<session_id>/<seq:06>.seg`.
fn write_segment(layout: &StoreLayout, session_id: &str, seq: u32, frames: &[FramePayload]) {
    let session_dir = layout.spool_session(session_id);
    fs::create_dir_all(&session_dir).expect("session dir");
    let mut bytes = encode_segment_header().to_vec();
    for f in frames {
        bytes.extend_from_slice(&encode_frame(f).expect("under the frame cap"));
    }
    fs::write(session_dir.join(format!("{seq:06}.seg")), bytes).expect("write segment");
}

fn envelope_count(conn: &Connection, session_id: &str) -> i64 {
    conn.query_row(
        "SELECT count(*) FROM observation_envelope WHERE session_id = ?1",
        [session_id],
        |r| r.get(0),
    )
    .unwrap()
}

fn segment_exists(layout: &StoreLayout, session_id: &str, seq: u32) -> bool {
    layout
        .spool_session(session_id)
        .join(format!("{seq:06}.seg"))
        .exists()
}

#[tokio::test]
async fn stable_duplicate_across_segments_is_imported_once() {
    let (_home, layout, db) = open_state();
    let uuids = SeqUuidV7::new();
    let request_root = RequestRoot::default();
    let session = "sess-1";

    write_segment(
        &layout,
        session,
        1,
        &[fixture(
            "PostToolUse",
            "pt:sess-1:t1:ok",
            Some("pt:sess-1:t1:ok"),
            session,
            1_000,
            vec![],
            None,
        )],
    );

    let outcome = import_session_tail(&db, &layout, session, &request_root, &uuids, 5_000, 72)
        .await
        .expect("first pass imports");
    assert_eq!(outcome.report.imported, 1);
    assert_eq!(outcome.report.exact_duplicates, 0);
    assert_eq!(outcome.final_segment_seq, 1);
    assert!(outcome.stalled_on.is_none());

    // A rotation happens and the hook's retry of the same stable event lands
    // in the new segment.
    write_segment(
        &layout,
        session,
        2,
        &[fixture(
            "PostToolUse",
            "pt:sess-1:t1:ok",
            Some("pt:sess-1:t1:ok"),
            session,
            2_000,
            vec![],
            None,
        )],
    );

    let outcome2 = import_session_tail(&db, &layout, session, &request_root, &uuids, 6_000, 72)
        .await
        .expect("second pass sees the duplicate");
    assert_eq!(outcome2.report.imported, 0);
    assert_eq!(outcome2.report.exact_duplicates, 1);
    assert_eq!(outcome2.final_segment_seq, 2);

    let read = db.open_read().expect("read conn");
    let count: i64 = read
        .query_row(
            "SELECT count(*) FROM observation_envelope WHERE dedup_key = 'pt:sess-1:t1:ok'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        count, 1,
        "the duplicate across segments never inserts twice"
    );
}

#[tokio::test]
async fn rollback_before_commit_leaves_no_partial_state() {
    let (_home, layout, db) = open_state();
    let uuids = SeqUuidV7::new();
    let request_root = RequestRoot::default();
    let session = "sess-1";

    let good = fixture("Stop", "st:sess-1:a:1", None, session, 1_000, vec![], None);
    let mut bad = fixture("Stop", "st:sess-1:b:2", None, session, 2_000, vec![], None);
    bad.evidence_kind = "bogus".to_string(); // violates the CHECK domain.

    write_segment(&layout, session, 1, &[good, bad]);

    let err = import_session_tail(&db, &layout, session, &request_root, &uuids, 5_000, 72)
        .await
        .expect_err("an invalid evidence_kind must fail the whole batch");
    assert!(matches!(err, ImportError::Write(_)), "{err:?}");

    let read = db.open_read().expect("read conn");
    assert_eq!(
        envelope_count(&read, session),
        0,
        "the good frame before the bad one must not survive the rollback either",
    );
    let cursor_rows: i64 = read
        .query_row("SELECT count(*) FROM spool_import_cursor", [], |r| r.get(0))
        .unwrap();
    assert_eq!(cursor_rows, 0, "the cursor is not advanced on rollback");
}

#[tokio::test]
async fn restart_after_commit_persists_and_reimport_is_idempotent() {
    let (home, layout, db) = open_state();
    let uuids = SeqUuidV7::new();
    let request_root = RequestRoot::default();
    let session = "sess-1";

    write_segment(
        &layout,
        session,
        1,
        &[fixture(
            "Stop",
            "st:sess-1:a:1",
            None,
            session,
            1_000,
            vec![],
            None,
        )],
    );
    import_session_tail(&db, &layout, session, &request_root, &uuids, 5_000, 72)
        .await
        .expect("first import");
    drop(db);

    // "Restart": reopen the same store from the same home.
    let layout2 = StoreLayout::new(home.join("local-rag"));
    let db2 = StateDb::open(layout2.state_db()).expect("reopen state.sqlite");
    let read = db2.open_read().expect("read conn");
    assert_eq!(envelope_count(&read, session), 1, "data survives a restart");
    let (seg, off): (i64, i64) = read
        .query_row(
            "SELECT segment_seq, committed_offset FROM spool_import_cursor WHERE session_id = ?1",
            [session],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(seg, 1);
    drop(read);

    // Re-running against the same, unchanged segment file imports nothing new.
    let outcome = import_session_tail(&db2, &layout2, session, &request_root, &uuids, 6_000, 72)
        .await
        .expect("idempotent re-import");
    assert_eq!(outcome.report.imported, 0);
    assert_eq!(outcome.report.exact_duplicates, 0);
    assert_eq!(outcome.final_segment_seq, seg as u32);
    assert_eq!(outcome.final_committed_offset, off as u64);

    let read = db2.open_read().expect("read conn");
    assert_eq!(
        envelope_count(&read, session),
        1,
        "no duplicate row appeared"
    );
}

#[tokio::test]
async fn monotone_seq_never_decreases_or_repeats_across_batches() {
    let (_home, layout, db) = open_state();
    let uuids = SeqUuidV7::new();
    let request_root = RequestRoot::default();

    for (session, n) in [("sess-a", 3), ("sess-b", 2)] {
        let frames: Vec<FramePayload> = (0..n)
            .map(|i| {
                fixture(
                    "Stop",
                    &format!("st:{session}:{i}"),
                    None,
                    session,
                    1_000 + i,
                    vec![],
                    None,
                )
            })
            .collect();
        write_segment(&layout, session, 1, &frames);
        import_session_tail(&db, &layout, session, &request_root, &uuids, 5_000, 72)
            .await
            .expect("import");
    }

    let read = db.open_read().expect("read conn");
    let mut stmt = read
        .prepare("SELECT received_seq FROM observation_envelope ORDER BY received_seq")
        .unwrap();
    let seqs: Vec<i64> = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(seqs.len(), 5);
    let mut sorted = seqs.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        seqs, sorted,
        "received_seq is strictly increasing, never repeated"
    );
}

#[tokio::test]
async fn concurrent_sessions_import_without_cross_contamination() {
    let (_home, layout, db) = open_state();
    let uuids_a = SeqUuidV7::tagged(0xAB);
    let uuids_b = SeqUuidV7::tagged(0xCD);
    let request_root = RequestRoot::default();

    write_segment(
        &layout,
        "sess-a",
        1,
        &[fixture(
            "Stop",
            "st:sess-a:1",
            None,
            "sess-a",
            1_000,
            vec![],
            None,
        )],
    );
    write_segment(
        &layout,
        "sess-b",
        1,
        &[fixture(
            "Stop",
            "st:sess-b:1",
            None,
            "sess-b",
            1_000,
            vec![],
            None,
        )],
    );

    let (a, b) = tokio::join!(
        import_session_tail(&db, &layout, "sess-a", &request_root, &uuids_a, 5_000, 72),
        import_session_tail(&db, &layout, "sess-b", &request_root, &uuids_b, 5_000, 72),
    );
    assert_eq!(a.unwrap().report.imported, 1);
    assert_eq!(b.unwrap().report.imported, 1);

    let read = db.open_read().expect("read conn");
    assert_eq!(envelope_count(&read, "sess-a"), 1);
    assert_eq!(envelope_count(&read, "sess-b"), 1);
}

#[tokio::test]
async fn bytes_deleted_only_up_to_the_new_cursor() {
    let (_home, layout, db) = open_state();
    let uuids = SeqUuidV7::new();
    let request_root = RequestRoot::default();
    let session = "sess-1";

    for seq in 1..=3u32 {
        write_segment(
            &layout,
            session,
            seq,
            &[fixture(
                "Stop",
                &format!("st:sess-1:{seq}"),
                None,
                session,
                1_000 + seq as i64,
                vec![],
                None,
            )],
        );
    }

    let outcome = import_session_tail(&db, &layout, session, &request_root, &uuids, 5_000, 72)
        .await
        .expect("import across three segments");
    assert_eq!(outcome.report.imported, 3);
    assert_eq!(
        outcome.final_segment_seq, 3,
        "walked through to the last segment"
    );

    assert!(
        !segment_exists(&layout, session, 1),
        "segment 1 is fully behind the cursor"
    );
    assert!(
        !segment_exists(&layout, session, 2),
        "segment 2 is fully behind the cursor"
    );
    assert!(
        segment_exists(&layout, session, 3),
        "the current segment is never deleted"
    );
    assert!(
        !segment_exists(&layout, session, 4),
        "no future segment was created"
    );
}

#[tokio::test]
async fn unknown_root_imports_with_null_repo_and_worktree() {
    let (_home, _layout, db) = open_state();
    let payload = fixture("Stop", "st:sess-1:1", None, "sess-1", 1_000, vec![], None);
    let decoded = DecodedObservation {
        payload,
        classification: DedupClass::BestEffort,
        frame_offset: 0,
        frame_len: 0,
    };
    let ids = vec!["obs-1".to_string()];
    let request_root = RequestRoot::default(); // worktree_root: None

    db.writer()
        .transaction(move |tx| {
            import_batch(
                tx,
                "sess-1",
                std::slice::from_ref(&decoded),
                &ids,
                &request_root,
                5_000,
                72,
                1,
                100,
            )
        })
        .await
        .expect("import_batch commits");

    let read = db.open_read().expect("read conn");
    let (repo_id, worktree_id): (Option<String>, Option<String>) = read
        .query_row(
            "SELECT repo_id, worktree_id FROM observation_envelope WHERE observation_id = 'obs-1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert!(repo_id.is_none());
    assert!(worktree_id.is_none());
}

#[tokio::test]
async fn resolved_root_populates_repo_and_worktree_ids() {
    let (_home, _layout, db) = open_state();
    let repo_id = "11111111-1111-7111-8111-111111111111".to_string();
    let worktree_id = "22222222-2222-7222-8222-222222222222".to_string();
    let canonical = "/repo/root".to_string();

    {
        let repo_id = repo_id.clone();
        let worktree_id = worktree_id.clone();
        let canonical = canonical.clone();
        db.writer()
            .transaction(move |tx| {
                create_repository(tx, &repo_id, None, 1_000)?;
                create_worktree(tx, &worktree_id, &repo_id, WorktreeKind::Main, 1_000)?;
                observe_worktree_path(tx, &worktree_id, &canonical, &canonical, "fp-1", 1_000)
            })
            .await
            .expect("seed registry");
    }

    let payload = fixture("Stop", "st:sess-1:1", None, "sess-1", 1_000, vec![], None);
    let decoded = DecodedObservation {
        payload,
        classification: DedupClass::BestEffort,
        frame_offset: 0,
        frame_len: 0,
    };
    let ids = vec!["obs-1".to_string()];
    let request_root = RequestRoot {
        worktree_root: Some(WorktreeRootFacts {
            observed_canonical_path: canonical.clone(),
            display_path: canonical.clone(),
            path_fingerprint: "fp-1".to_string(),
            kind: WorktreeKind::Main,
            common_dir_fingerprint: None,
            remote_fingerprint: None,
        }),
        repo_hint: None,
    };

    db.writer()
        .transaction(move |tx| {
            import_batch(
                tx,
                "sess-1",
                std::slice::from_ref(&decoded),
                &ids,
                &request_root,
                5_000,
                72,
                1,
                100,
            )
        })
        .await
        .expect("import_batch commits");

    let read = db.open_read().expect("read conn");
    let (got_repo, got_worktree): (Option<String>, Option<String>) = read
        .query_row(
            "SELECT repo_id, worktree_id FROM observation_envelope WHERE observation_id = 'obs-1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(got_repo.as_deref(), Some(repo_id.as_str()));
    assert_eq!(got_worktree.as_deref(), Some(worktree_id.as_str()));
}

#[tokio::test]
async fn a_torn_tail_stops_cleanly_and_is_picked_up_after_the_writer_finishes() {
    let (_home, layout, db) = open_state();
    let uuids = SeqUuidV7::new();
    let request_root = RequestRoot::default();
    let session = "sess-1";

    let frame = fixture("Stop", "st:sess-1:1", None, session, 1_000, vec![], None);
    let full = {
        let mut bytes = encode_segment_header().to_vec();
        bytes.extend_from_slice(&encode_frame(&frame).unwrap());
        bytes
    };
    // Torn: only half the frame's bytes made it to disk (a crash mid-write).
    let torn_len = full.len() - (full.len() - 16) / 2;
    let session_dir = layout.spool_session(session);
    fs::create_dir_all(&session_dir).unwrap();
    fs::write(session_dir.join("000001.seg"), &full[..torn_len]).unwrap();

    let outcome = import_session_tail(&db, &layout, session, &request_root, &uuids, 5_000, 72)
        .await
        .expect("torn tail is not an error");
    assert_eq!(outcome.report.imported, 0);
    assert!(
        outcome.stalled_on.is_none(),
        "a torn tail is not corruption"
    );
    assert_eq!(outcome.final_segment_seq, 1);
    // The 16-byte header itself was intact and validated, so the cursor
    // advances past it even though the (torn) frame after it did not import.
    assert_eq!(outcome.final_committed_offset, 16);

    // The writer "finishes": the full frame lands.
    fs::write(session_dir.join("000001.seg"), &full).unwrap();
    let outcome2 = import_session_tail(&db, &layout, session, &request_root, &uuids, 6_000, 72)
        .await
        .expect("retry succeeds once the frame is complete");
    assert_eq!(outcome2.report.imported, 1);
}

/// D-019: `import_session_tail` carries a frame's `redaction_version` through
/// to `observation_envelope.redaction_version`, and an envelope-only frame
/// (no scanner ever ran) stores `NULL`, not `0` or some other stand-in.
#[tokio::test]
async fn import_stores_redaction_version_and_null_for_envelope_only() {
    let (_home, layout, db) = open_state();
    let uuids = SeqUuidV7::new();
    let request_root = RequestRoot::default();
    let session = "sess-1";

    let mut scanned = fixture(
        "Stop",
        "st:sess-1:scanned:1",
        None,
        session,
        1_000,
        vec![],
        Some("{\"x\":1}"),
    );
    scanned.redaction_version = Some(1);
    let envelope_only = fixture(
        "Stop",
        "st:sess-1:denied:2",
        None,
        session,
        2_000,
        vec![],
        None,
    );

    write_segment(&layout, session, 1, &[scanned, envelope_only]);

    let outcome = import_session_tail(&db, &layout, session, &request_root, &uuids, 5_000, 72)
        .await
        .expect("import commits");
    assert_eq!(outcome.report.imported, 2);

    let read = db.open_read().expect("read conn");
    let scanned_version: Option<i64> = read
        .query_row(
            "SELECT redaction_version FROM observation_envelope WHERE source_event_id = 'st:sess-1:scanned:1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(scanned_version, Some(1));
    let denied_version: Option<i64> = read
        .query_row(
            "SELECT redaction_version FROM observation_envelope WHERE source_event_id = 'st:sess-1:denied:2'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        denied_version, None,
        "an envelope-only event's payload was never scanned"
    );
}

// ---------------------------------------------------------------------------
// D-024: ImportBatchReport::{saw_stop, saw_session_end}
// ---------------------------------------------------------------------------

#[tokio::test]
async fn import_batch_flags_saw_stop_when_newly_imported() {
    let (_home, _layout, db) = open_state();
    let payload = fixture(
        "Stop",
        "st:sess-1:1",
        Some("stop-1"),
        "sess-1",
        1_000,
        vec![],
        None,
    );
    let decoded = DecodedObservation {
        payload,
        classification: DedupClass::Stable {
            dedup_key: "stop-1".to_string(),
        },
        frame_offset: 0,
        frame_len: 0,
    };
    let ids = vec!["obs-1".to_string()];
    let request_root = RequestRoot::default();

    let report = db
        .writer()
        .transaction(move |tx| {
            import_batch(
                tx,
                "sess-1",
                std::slice::from_ref(&decoded),
                &ids,
                &request_root,
                5_000,
                72,
                1,
                100,
            )
        })
        .await
        .expect("import_batch commits");

    assert_eq!(report.imported, 1);
    assert!(report.saw_stop, "a newly-imported Stop row sets saw_stop");
    assert!(!report.saw_session_end);
}

#[tokio::test]
async fn import_batch_does_not_flag_saw_stop_for_a_deduplicated_redelivered_stop() {
    let (_home, _layout, db) = open_state();
    let payload = fixture(
        "Stop",
        "st:sess-1:1",
        Some("stop-1"),
        "sess-1",
        1_000,
        vec![],
        None,
    );
    let decoded = DecodedObservation {
        payload,
        classification: DedupClass::Stable {
            dedup_key: "stop-1".to_string(),
        },
        frame_offset: 0,
        frame_len: 0,
    };
    let ids = vec!["obs-1".to_string()];
    let request_root = RequestRoot::default();

    // First delivery: genuinely imported.
    {
        let decoded = decoded.clone();
        let ids = ids.clone();
        let request_root = request_root.clone();
        db.writer()
            .transaction(move |tx| {
                import_batch(
                    tx,
                    "sess-1",
                    std::slice::from_ref(&decoded),
                    &ids,
                    &request_root,
                    5_000,
                    72,
                    1,
                    100,
                )
            })
            .await
            .expect("first import commits");
    }

    // Second delivery of the very same dedup_key: exact-duplicate skip.
    let ids2 = vec!["obs-2".to_string()];
    let report = db
        .writer()
        .transaction(move |tx| {
            import_batch(
                tx,
                "sess-1",
                std::slice::from_ref(&decoded),
                &ids2,
                &request_root,
                5_000,
                72,
                2,
                200,
            )
        })
        .await
        .expect("second import commits");

    assert_eq!(report.imported, 0);
    assert_eq!(report.exact_duplicates, 1);
    assert!(
        !report.saw_stop,
        "a deduplicated-away redelivery must not trigger a fresh checkpoint"
    );
}

#[tokio::test]
async fn import_batch_flags_saw_session_end_independently() {
    let (_home, _layout, db) = open_state();
    let payload = fixture(
        "SessionEnd",
        "se:sess-1:1",
        Some("end-1"),
        "sess-1",
        1_000,
        vec![],
        None,
    );
    let decoded = DecodedObservation {
        payload,
        classification: DedupClass::Stable {
            dedup_key: "end-1".to_string(),
        },
        frame_offset: 0,
        frame_len: 0,
    };
    let ids = vec!["obs-1".to_string()];
    let request_root = RequestRoot::default();

    let report = db
        .writer()
        .transaction(move |tx| {
            import_batch(
                tx,
                "sess-1",
                std::slice::from_ref(&decoded),
                &ids,
                &request_root,
                5_000,
                72,
                1,
                100,
            )
        })
        .await
        .expect("import_batch commits");

    assert!(report.saw_session_end);
    assert!(!report.saw_stop);
}

#[tokio::test]
async fn import_batch_flags_neither_for_an_ordinary_batch() {
    let (_home, _layout, db) = open_state();
    let payload = fixture(
        "PreToolUse",
        "pt:sess-1:1",
        Some("pt-1"),
        "sess-1",
        1_000,
        vec![],
        None,
    );
    let decoded = DecodedObservation {
        payload,
        classification: DedupClass::Stable {
            dedup_key: "pt-1".to_string(),
        },
        frame_offset: 0,
        frame_len: 0,
    };
    let ids = vec!["obs-1".to_string()];
    let request_root = RequestRoot::default();

    let report = db
        .writer()
        .transaction(move |tx| {
            import_batch(
                tx,
                "sess-1",
                std::slice::from_ref(&decoded),
                &ids,
                &request_root,
                5_000,
                72,
                1,
                100,
            )
        })
        .await
        .expect("import_batch commits");

    assert!(!report.saw_stop);
    assert!(!report.saw_session_end);
}

/// D-030: [`diagnose_spool_tail`] must report exactly the same `stalled_on`
/// signal a real [`import_session_tail`] pass would, without ever creating a
/// `spool_import_cursor` row of its own — the whole point of the read-only
/// diagnostic is that it can be called freely (e.g. from `local-rag doctor`)
/// without racing or interfering with a real import pass.
#[tokio::test]
async fn diagnose_spool_tail_matches_import_session_tail_without_advancing_the_cursor() {
    let (_home, layout, db) = open_state();
    let session_dir = layout.spool_session("corrupt-session");
    fs::create_dir_all(&session_dir).expect("session dir");
    // 16 zero bytes: exactly HEADER_LEN (never `Truncated`), but the magic
    // does not match — genuine corruption, not a normal in-progress write.
    fs::write(session_dir.join("000001.seg"), [0u8; 16]).expect("write corrupt header");

    let uuids = SeqUuidV7::new();
    let request_root = RequestRoot::default();
    let outcome = import_session_tail(
        &db,
        &layout,
        "corrupt-session",
        &request_root,
        &uuids,
        1_000,
        72,
    )
    .await
    .expect("a stall is reported, not a hard error");
    let real_stalled_on = outcome
        .stalled_on
        .expect("a bad-magic header stalls the real importer");
    assert!(real_stalled_on.contains("magic"), "{real_stalled_on}");

    let read = db.open_read().expect("read conn");
    let diagnosed = diagnose_spool_tail(&read, &layout, "corrupt-session")
        .expect("diagnosis runs cleanly")
        .expect("diagnosis agrees the session is stalled");
    assert_eq!(
        diagnosed, real_stalled_on,
        "diagnose_spool_tail must report exactly what the real importer found"
    );

    // Read-only: repeated diagnosis never creates a cursor row for a session
    // whose only pass so far stalled before decoding anything.
    diagnose_spool_tail(&read, &layout, "corrupt-session").expect("diagnose again");
    let cursor_rows: i64 = read
        .query_row(
            "SELECT count(*) FROM spool_import_cursor WHERE session_id = 'corrupt-session'",
            [],
            |r| r.get(0),
        )
        .expect("query spool_import_cursor");
    assert_eq!(
        cursor_rows, 0,
        "a session that stalled before decoding anything must have no cursor row"
    );
}
