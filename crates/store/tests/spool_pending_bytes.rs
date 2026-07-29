//! T15-01 acceptance tests for `store_has_pending_spool_bytes` — spec 02
//! §4.3's idle-shutdown gate ("no unimported spool bytes"), built over the
//! same "fully committed" primitives `run_spool_session_sweep` (T13-05) uses.
//!
//! Deterministic: an isolated [`TempHome`], a fixed `now_ms`, no wall clock.

use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};

use local_rag_core::identity::{Uuid, UuidSource, uuidv7_from};
use local_rag_core::paths::StoreLayout;
use local_rag_core::spool::{FramePayload, encode_frame, encode_segment_header};
use local_rag_store::registry::RequestRoot;
use local_rag_store::{StateDb, import_session_tail, store_has_pending_spool_bytes};
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

fn write_spool_segment(layout: &StoreLayout, session_id: &str, seq: u32, frames: &[FramePayload]) {
    let session_dir = layout.spool_session(session_id);
    fs::create_dir_all(&session_dir).expect("session dir");
    let mut bytes = encode_segment_header().to_vec();
    for f in frames {
        bytes.extend_from_slice(&encode_frame(f).expect("under the frame cap"));
    }
    fs::write(session_dir.join(format!("{seq:06}.seg")), bytes).expect("write segment");
}

#[tokio::test]
async fn no_spool_sessions_at_all_is_not_pending() {
    let (_home, layout, db) = open_state();
    assert!(!store_has_pending_spool_bytes(&db, &layout).expect("check"));
}

/// A session directory exists but has never been imported at all (no
/// `spool_import_cursor` row yet) — must count as pending, the default
/// `(segment_seq: 1, committed_offset: 0)` case.
#[tokio::test]
async fn a_never_imported_session_is_pending() {
    let (_home, layout, db) = open_state();
    let session = "sess-never-imported";
    write_spool_segment(
        &layout,
        session,
        1,
        &[spool_fixture(session, "st:never:1", 1_000)],
    );

    assert!(store_has_pending_spool_bytes(&db, &layout).expect("check"));
}

/// A session fully imported, caught up, no further bytes written — not
/// pending.
#[tokio::test]
async fn a_fully_imported_session_is_not_pending() {
    let (_home, layout, db) = open_state();
    let uuids = SeqUuidV7::new();
    let request_root = RequestRoot::default();
    let session = "sess-caught-up";

    write_spool_segment(
        &layout,
        session,
        1,
        &[spool_fixture(session, "st:caught-up:1", 1_000)],
    );
    import_session_tail(&db, &layout, session, &request_root, &uuids, 1_000, 72)
        .await
        .expect("import");

    assert!(!store_has_pending_spool_bytes(&db, &layout).expect("check"));
}

/// A session imported once, then more bytes appended after that pass (the
/// hook wrote more, no re-import happened yet) — must count as pending, even
/// though a cursor row exists.
#[tokio::test]
async fn a_session_with_bytes_past_the_cursor_is_pending() {
    let (_home, layout, db) = open_state();
    let uuids = SeqUuidV7::new();
    let request_root = RequestRoot::default();
    let session = "sess-uncommitted-tail";

    write_spool_segment(
        &layout,
        session,
        1,
        &[spool_fixture(session, "st:uncommitted:1", 1_000)],
    );
    import_session_tail(&db, &layout, session, &request_root, &uuids, 1_000, 72)
        .await
        .expect("first import catches up");

    let second_frame = encode_frame(&spool_fixture(session, "st:uncommitted:2", 2_000))
        .expect("under the frame cap");
    let seg_path = layout.spool_session(session).join("000001.seg");
    let mut bytes = fs::read(&seg_path).unwrap();
    bytes.extend_from_slice(&second_frame);
    fs::write(&seg_path, bytes).unwrap();

    assert!(store_has_pending_spool_bytes(&db, &layout).expect("check"));
}

/// One caught-up session plus one pending session: the pending one must still
/// be found regardless of enumeration order.
#[tokio::test]
async fn one_pending_session_among_several_is_still_found() {
    let (_home, layout, db) = open_state();
    let uuids = SeqUuidV7::new();
    let request_root = RequestRoot::default();

    write_spool_segment(
        &layout,
        "sess-a-caught-up",
        1,
        &[spool_fixture("sess-a-caught-up", "st:a:1", 1_000)],
    );
    import_session_tail(
        &db,
        &layout,
        "sess-a-caught-up",
        &request_root,
        &uuids,
        1_000,
        72,
    )
    .await
    .expect("import a");

    write_spool_segment(
        &layout,
        "sess-b-pending",
        1,
        &[spool_fixture("sess-b-pending", "st:b:1", 1_000)],
    );

    assert!(store_has_pending_spool_bytes(&db, &layout).expect("check"));
}
