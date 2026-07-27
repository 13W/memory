//! Permissions: the session directory and each segment file get the store's
//! strict modes (spec 12 §2/§6: dirs 0700, files 0600), mirroring
//! `crates/core/src/paths/perms.rs`'s own permission tests.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;

use local_rag_core::paths::StoreLayout;
use local_rag_hook::frame::encode_frame_bytes;
use local_rag_hook::segment::{DEFAULT_ROTATE_THRESHOLD_BYTES, append_frame};
use local_rag_test_support::TempHome;

#[test]
fn session_dir_and_segment_file_get_strict_permissions() {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    fs::create_dir_all(layout.spool_dir()).expect("spool dir");

    let frame = encode_frame_bytes(b"{}").expect("under cap");
    append_frame(
        &layout,
        "sess-perms",
        &frame,
        DEFAULT_ROTATE_THRESHOLD_BYTES,
    )
    .expect("append");

    let session_dir = layout.spool_session("sess-perms");
    let dir_mode = fs::metadata(&session_dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(dir_mode, 0o700, "session dir is 0700");

    let seg_mode = fs::metadata(session_dir.join("000001.seg"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(seg_mode, 0o600, "segment file is 0600");
}
