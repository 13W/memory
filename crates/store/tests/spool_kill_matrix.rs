//! T13-06 S3–S7 acceptance tests (spec 07 §7) for the transactional
//! observation importer.
//!
//! S3/S4/S5 need a genuine process kill: the test binary re-executes itself
//! (`std::env::current_exe()`, filtered to one `#[tokio::test]` by name, an
//! env var switching child/parent mode), mirroring `migrate_resumable.rs`'s
//! `resumable_hard_kill_via_sigabrt` exactly, for the importer's three named
//! seams (`observation.import_batch.before_commit`,
//! `observation.import_session_tail.after_commit_before_cleanup`,
//! `observation.import_session_tail.mid_cleanup`). S6/S7 need no process kill
//! — they exercise the public import API directly, end to end.
//!
//! Gated on `unix` + `failpoints` (signal inspection is POSIX-specific; the
//! three seams only exist with the feature). Run via
//! `cargo test -p local-rag-store --features failpoints`.

#![cfg(all(unix, feature = "failpoints"))]

use std::fs;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use local_rag_core::identity::{Uuid, UuidSource, uuidv7_from};
use local_rag_core::paths::StoreLayout;
use local_rag_core::spool::{FramePayload, encode_frame, encode_segment_header};
use local_rag_store::{RequestRoot, StateDb, import_session_tail};
use local_rag_test_support::{Action, TempHome, run_capturing};

const CHILD_ENV: &str = "LOCAL_RAG_T1306_CHILD";

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

fn temp_store() -> (TempHome, StoreLayout) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    (home, layout)
}

fn fixture(
    event_type: &str,
    source_event_id: &str,
    dedup_key: Option<&str>,
    session_id: &str,
    captured_at: i64,
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
        paths: vec![],
        redaction_version: None,
        payload: None,
        short_evidence_excerpt: None,
    }
}

fn write_segment(layout: &StoreLayout, session_id: &str, seq: u32, frames: &[FramePayload]) {
    let session_dir = layout.spool_session(session_id);
    fs::create_dir_all(&session_dir).expect("session dir");
    let mut bytes = encode_segment_header().to_vec();
    for f in frames {
        bytes.extend_from_slice(&encode_frame(f).expect("under the frame cap"));
    }
    fs::write(session_dir.join(format!("{seq:06}.seg")), bytes).expect("write segment");
}

fn envelope_count(db: &StateDb, session_id: &str) -> i64 {
    let read = db.open_read().expect("read conn");
    read.query_row(
        "SELECT count(*) FROM observation_envelope WHERE session_id = ?1",
        [session_id],
        |r| r.get(0),
    )
    .unwrap()
}

fn cursor_row(db: &StateDb, session_id: &str) -> Option<(i64, i64)> {
    let read = db.open_read().expect("read conn");
    read.query_row(
        "SELECT segment_seq, committed_offset FROM spool_import_cursor WHERE session_id = ?1",
        [session_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .ok()
}

/// Arm `name` to abort, then run `import_session_tail`. If the seam did not
/// fire, exit loudly with a distinct, non-signal code rather than silently
/// reporting a false pass.
async fn child_arm_and_import(root: &Path, session_id: &str, seam: &str) -> ! {
    let layout = StoreLayout::new(root.to_path_buf());
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    let uuids = SeqUuidV7::new();
    let request_root = RequestRoot::default();

    let fp = local_rag_test_support::failpoint::global();
    fp.register(seam);
    fp.arm(seam, Action::Abort).expect("arm abort");

    let _ = import_session_tail(&db, &layout, session_id, &request_root, &uuids, 5_000, 72).await;
    // Reaching here means the seam did not fire — fail loudly (not a signal).
    std::process::exit(97);
}

/// Re-exec this test binary filtered to `test_name`, with `CHILD_ENV` set to
/// `root`; asserts the child died with `SIGABRT`.
fn run_as_child(test_name: &str, root: &Path) -> local_rag_test_support::RunOutcome {
    let mut cmd = Command::new(std::env::current_exe().expect("current exe"));
    cmd.arg(test_name)
        .arg("--exact")
        .arg("--nocapture")
        .env(CHILD_ENV, root);
    let outcome = run_capturing(cmd, test_name).expect("spawn child");
    assert_eq!(
        outcome.status.signal(),
        Some(6),
        "child must die with SIGABRT; status={:?}\nstderr:\n{}",
        outcome.status,
        outcome.stderr_lossy()
    );
    outcome
}

/// S3: a daemon killed after reading frames but before the importing
/// transaction commits loses nothing — the transaction never committed, so a
/// fresh pass simply (re-)imports from the unchanged cursor.
#[tokio::test]
async fn s3_daemon_killed_before_commit_loses_nothing() {
    const SEAM: &str = "observation.import_batch.before_commit";
    let session = "sess-s3";

    if let Ok(root) = std::env::var(CHILD_ENV) {
        child_arm_and_import(Path::new(&root), session, SEAM).await;
    }

    let (_home, layout) = temp_store();
    write_segment(
        &layout,
        session,
        1,
        &[fixture(
            "PostToolUse",
            "pt:sess-s3:t1:ok",
            Some("pt:sess-s3:t1:ok"),
            session,
            1_000,
        )],
    );

    run_as_child(
        "s3_daemon_killed_before_commit_loses_nothing",
        layout.root(),
    );

    let db = StateDb::open(layout.state_db()).expect("reopen state.sqlite");
    assert_eq!(envelope_count(&db, session), 0, "nothing committed");
    assert_eq!(cursor_row(&db, session), None, "cursor never advanced");

    let uuids = SeqUuidV7::new();
    let request_root = RequestRoot::default();
    let outcome = import_session_tail(&db, &layout, session, &request_root, &uuids, 6_000, 72)
        .await
        .expect("fresh import succeeds");
    assert_eq!(outcome.report.imported, 1, "the event is not lost");
}

/// S4: a daemon killed after the transaction commits but before segment
/// cleanup — a re-scan skips the already-committed frames (nothing is
/// re-imported) and finishes the deferred cleanup.
#[tokio::test]
async fn s4_daemon_killed_after_commit_before_cleanup_is_safe_to_resume() {
    const SEAM: &str = "observation.import_session_tail.after_commit_before_cleanup";
    let session = "sess-s4";

    if let Ok(root) = std::env::var(CHILD_ENV) {
        child_arm_and_import(Path::new(&root), session, SEAM).await;
    }

    let (_home, layout) = temp_store();
    write_segment(
        &layout,
        session,
        1,
        &[fixture("Stop", "st:sess-s4:1", None, session, 1_000)],
    );
    write_segment(
        &layout,
        session,
        2,
        &[fixture("Stop", "st:sess-s4:2", None, session, 2_000)],
    );

    run_as_child(
        "s4_daemon_killed_after_commit_before_cleanup_is_safe_to_resume",
        layout.root(),
    );

    let db = StateDb::open(layout.state_db()).expect("reopen state.sqlite");
    assert_eq!(envelope_count(&db, session), 2, "both frames committed");
    assert_eq!(
        cursor_row(&db, session),
        Some((2, /* full seg 2 */ {
            let bytes = fs::read(layout.spool_session(session).join("000002.seg")).unwrap();
            bytes.len() as i64
        }))
    );
    assert!(
        layout.spool_session(session).join("000001.seg").exists(),
        "cleanup never ran — segment 1 is still on disk"
    );

    let uuids = SeqUuidV7::new();
    let request_root = RequestRoot::default();
    let outcome = import_session_tail(&db, &layout, session, &request_root, &uuids, 6_000, 72)
        .await
        .expect("fresh pass finishes cleanup");
    assert_eq!(outcome.report.imported, 0, "nothing new to decode");
    assert!(
        !layout.spool_session(session).join("000001.seg").exists(),
        "the deferred cleanup finally ran"
    );
}

/// S5: a daemon killed mid-cleanup (after deleting one prior segment, before
/// the next) leaves a consistent segment set — the DB state (already
/// committed before any cleanup started) is unaffected, and a fresh pass
/// finishes the deletion without re-importing anything.
#[tokio::test]
async fn s5_daemon_killed_mid_cleanup_leaves_a_consistent_segment_set() {
    const SEAM: &str = "observation.import_session_tail.mid_cleanup";
    let session = "sess-s5";

    if let Ok(root) = std::env::var(CHILD_ENV) {
        child_arm_and_import(Path::new(&root), session, SEAM).await;
    }

    let (_home, layout) = temp_store();
    for (seq, ts) in [(1, 1_000), (2, 2_000), (3, 3_000)] {
        write_segment(
            &layout,
            session,
            seq,
            &[fixture(
                "Stop",
                &format!("st:sess-s5:{seq}"),
                None,
                session,
                ts,
            )],
        );
    }

    run_as_child(
        "s5_daemon_killed_mid_cleanup_leaves_a_consistent_segment_set",
        layout.root(),
    );

    let db = StateDb::open(layout.state_db()).expect("reopen state.sqlite");
    assert_eq!(
        envelope_count(&db, session),
        3,
        "all three already committed"
    );
    assert_eq!(cursor_row(&db, session).map(|(seg, _)| seg), Some(3));
    let session_dir = layout.spool_session(session);
    assert!(!session_dir.join("000001.seg").exists(), "first cleaned up");
    assert!(
        session_dir.join("000002.seg").exists(),
        "cleanup died before reaching segment 2"
    );
    assert!(
        session_dir.join("000003.seg").exists(),
        "current segment retained"
    );

    let uuids = SeqUuidV7::new();
    let request_root = RequestRoot::default();
    let outcome = import_session_tail(&db, &layout, session, &request_root, &uuids, 6_000, 72)
        .await
        .expect("fresh pass finishes cleanup");
    assert_eq!(outcome.report.imported, 0, "nothing new to decode");
    assert!(!session_dir.join("000002.seg").exists(), "cleanup finished");
    assert!(
        session_dir.join("000003.seg").exists(),
        "current segment still retained"
    );
}

/// S6: the same stable event, retried by the hook and landing in a later
/// segment, imports exactly once (`UNIQUE(dedup_key)`).
#[tokio::test]
async fn s6_duplicate_stable_event_across_segments_yields_exactly_one_envelope() {
    let (_home, layout) = temp_store();
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    let uuids = SeqUuidV7::new();
    let request_root = RequestRoot::default();
    let session = "sess-s6";

    write_segment(
        &layout,
        session,
        1,
        &[fixture(
            "PostToolUse",
            "pt:sess-s6:t1:ok",
            Some("pt:sess-s6:t1:ok"),
            session,
            1_000,
        )],
    );
    let first = import_session_tail(&db, &layout, session, &request_root, &uuids, 5_000, 72)
        .await
        .expect("first pass");
    assert_eq!(first.report.imported, 1);

    write_segment(
        &layout,
        session,
        2,
        &[fixture(
            "PostToolUse",
            "pt:sess-s6:t1:ok",
            Some("pt:sess-s6:t1:ok"),
            session,
            2_000,
        )],
    );
    let second = import_session_tail(&db, &layout, session, &request_root, &uuids, 6_000, 72)
        .await
        .expect("second pass sees the retry");
    assert_eq!(second.report.imported, 0);
    assert_eq!(second.report.exact_duplicates, 1);

    assert_eq!(envelope_count(&db, session), 1);
}

/// S7: a duplicate best-effort event within the bounded window dedups to one
/// envelope; outside the window (both by time and by count — the union of
/// spec 07 §5's two bounds), it does not, and two envelopes result.
#[tokio::test]
async fn s7_best_effort_duplicate_within_window_dedups_outside_window_does_not() {
    let (_home, layout) = temp_store();
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    let uuids = SeqUuidV7::new();
    let request_root = RequestRoot::default();

    // Part (a): within window — same source_event_id, close in time, few
    // total envelopes in the session.
    let within = "sess-s7-within";
    write_segment(
        &layout,
        within,
        1,
        &[
            fixture(
                "UserPromptSubmit",
                "up:sess-s7-within:x:1",
                None,
                within,
                1_000,
            ),
            fixture(
                "UserPromptSubmit",
                "up:sess-s7-within:x:1",
                None,
                within,
                1_500,
            ),
        ],
    );
    let outcome = import_session_tail(&db, &layout, within, &request_root, &uuids, 5_000, 72)
        .await
        .expect("within-window pass");
    assert_eq!(outcome.report.imported, 1);
    assert_eq!(outcome.report.window_duplicates, 1);
    assert_eq!(envelope_count(&db, within), 1);

    // Part (b): outside window — 511 filler envelopes push the original past
    // the last-512 count bound, and the repeat's own captured_at is well past
    // the 10-minute time bound too, so neither side of the union fires.
    let outside = "sess-s7-outside";
    let mut frames = vec![fixture(
        "UserPromptSubmit",
        "up:sess-s7-outside:dup:1",
        None,
        outside,
        0,
    )];
    // 512 fillers (not 511): with only 511, the original would be *exactly*
    // the 512th-most-recent row when the repeat is checked — the inclusive
    // boundary, still a match (proven separately by `observation::mod::tests::
    // window_dedup_count_boundary_is_inclusive_at_512_and_exclusive_at_513`).
    // 512 fillers push it to 513th-most-recent, genuinely outside.
    for i in 0..512 {
        frames.push(fixture(
            "UserPromptSubmit",
            &format!("up:sess-s7-outside:filler-{i}"),
            None,
            outside,
            0,
        ));
    }
    frames.push(fixture(
        "UserPromptSubmit",
        "up:sess-s7-outside:dup:1",
        None,
        outside,
        1_000_000_000,
    ));
    write_segment(&layout, outside, 1, &frames);

    let outcome = import_session_tail(&db, &layout, outside, &request_root, &uuids, 5_000, 72)
        .await
        .expect("outside-window pass");
    assert_eq!(
        outcome.report.imported, 514,
        "all 514 frames are distinct envelopes"
    );
    assert_eq!(outcome.report.window_duplicates, 0);

    let read = db.open_read().unwrap();
    let dup_count: i64 = read
        .query_row(
            "SELECT count(*) FROM observation_envelope WHERE source_event_id = ?1",
            ["up:sess-s7-outside:dup:1"],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        dup_count, 2,
        "outside the window, both survive as distinct envelopes"
    );
}
