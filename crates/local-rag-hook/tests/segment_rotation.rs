//! Rotation: a small injected threshold forces real rotation deterministically
//! instead of writing 8+ MiB per test run (spec 07 §2: "writer opens a new
//! segment when the current one exceeds 8 MiB").

use std::fs;

use local_rag_core::paths::StoreLayout;
use local_rag_hook::frame::encode_frame_bytes;
use local_rag_hook::segment::append_frame;
use local_rag_test_support::TempHome;

fn layout(home: &TempHome) -> StoreLayout {
    let l = StoreLayout::new(home.join("local-rag"));
    fs::create_dir_all(l.spool_dir()).expect("spool dir");
    l
}

#[test]
fn a_small_threshold_forces_rotation_across_several_writes() {
    let home = TempHome::new().expect("temp home");
    let l = layout(&home);
    let session_dir = l.spool_session("sess-rotate");
    let frame = encode_frame_bytes(b"{\"x\":1}").expect("under cap");
    // Just below "header + exactly one frame": the *second* write to a
    // segment already at that size must rotate; the *first* write into an
    // empty (len == 0) segment always proceeds regardless of the threshold.
    let threshold = (16 + frame.len() as u64) - 1;

    append_frame(&l, "sess-rotate", &frame, threshold).expect("write 1");
    assert!(session_dir.join("000001.seg").exists());
    assert!(!session_dir.join("000002.seg").exists(), "no rotation yet");

    append_frame(&l, "sess-rotate", &frame, threshold).expect("write 2");
    assert!(session_dir.join("000002.seg").exists(), "rotated to seg 2");

    append_frame(&l, "sess-rotate", &frame, threshold).expect("write 3");
    assert!(session_dir.join("000003.seg").exists(), "rotated to seg 3");
    assert!(
        !session_dir.join("000004.seg").exists(),
        "seq only advances as far as needed"
    );

    // Each segment holds its header plus exactly one frame — no writer ever
    // appended to a segment that was already over threshold when it acquired
    // the lock, and none is missing a write either.
    for seq in 1..=3 {
        let bytes = fs::read(session_dir.join(format!("{seq:06}.seg"))).unwrap();
        assert_eq!(bytes.len(), 16 + frame.len(), "segment {seq}");
    }
}

#[test]
fn seq_never_decreases_across_many_rotating_writes() {
    let home = TempHome::new().expect("temp home");
    let l = layout(&home);
    let frame = encode_frame_bytes(b"{}").expect("under cap");
    // A threshold of 0 forces rotation on every write after the first (the
    // first write into an empty segment always proceeds; every following
    // write sees a non-empty segment whose size is always > 0).
    let threshold = 0u64;

    for _ in 0..5 {
        append_frame(&l, "sess-many", &frame, threshold).expect("write");
    }

    let session_dir = l.spool_session("sess-many");
    for seq in 1..=5 {
        assert!(
            session_dir.join(format!("{seq:06}.seg")).exists(),
            "segment {seq} must exist"
        );
    }
    assert!(!session_dir.join("000006.seg").exists());
}
