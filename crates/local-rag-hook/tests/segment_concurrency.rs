//! Concurrent append: multiple writers, each with their own `File` handle,
//! contend on one session's current segment via `std::fs::File::lock()`'s
//! per-open-file-description semantics — the same mechanism
//! `crates/store/src/migrate/lock.rs`'s `MigrationLock` already relies on, so
//! genuine contention needs only OS threads, not real subprocesses.

use std::fs;
use std::sync::Arc;
use std::thread;

use local_rag_core::paths::StoreLayout;
use local_rag_core::spool::{crc32c, encode_frame_bytes, encode_segment_header};
use local_rag_hook::segment::{DEFAULT_ROTATE_THRESHOLD_BYTES, append_frame};
use local_rag_test_support::TempHome;

fn layout(home: &TempHome) -> StoreLayout {
    let l = StoreLayout::new(home.join("local-rag"));
    fs::create_dir_all(l.spool_dir()).expect("spool dir");
    l
}

#[test]
fn concurrent_appends_land_intact_with_no_interleaving() {
    let home = TempHome::new().expect("temp home");
    let layout = Arc::new(layout(&home));
    const WRITERS: usize = 16;

    let handles: Vec<_> = (0..WRITERS)
        .map(|i| {
            let layout = Arc::clone(&layout);
            thread::spawn(move || {
                let payload = format!("{{\"writer\":{i}}}");
                let frame = encode_frame_bytes(payload.as_bytes()).expect("under cap");
                append_frame(
                    &layout,
                    "sess-concurrent",
                    &frame,
                    DEFAULT_ROTATE_THRESHOLD_BYTES,
                )
                .expect("append from thread");
            })
        })
        .collect();
    for h in handles {
        h.join().expect("writer thread panicked");
    }

    let session_dir = layout.spool_session("sess-concurrent");
    let bytes = fs::read(session_dir.join("000001.seg")).expect("single segment (under threshold)");
    assert_eq!(&bytes[..16], &encode_segment_header());
    assert!(
        !session_dir.join("000002.seg").exists(),
        "16 small frames stay well under the 8 MiB threshold"
    );

    // Walk every frame by hand (no shared decoder — that's T13-03's design
    // surface): each frame's CRC must match and its payload must be intact,
    // unambiguous JSON, proving no writer's bytes were torn or interleaved
    // with another's.
    let mut offset = 16;
    let mut seen = Vec::new();
    while offset < bytes.len() {
        let len = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
        let crc = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap());
        let payload = &bytes[offset + 8..offset + 8 + len];
        assert_eq!(
            crc32c(payload),
            crc,
            "frame at offset {offset} has a valid crc"
        );
        let value: serde_json::Value =
            serde_json::from_slice(payload).expect("frame payload is intact json");
        seen.push(value["writer"].as_i64().expect("writer field present"));
        offset += 8 + len;
    }
    assert_eq!(
        offset,
        bytes.len(),
        "no trailing garbage after the last frame"
    );
    assert_eq!(seen.len(), WRITERS, "exactly one frame per writer");
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(
        seen.len(),
        WRITERS,
        "every writer's frame is distinct, none lost or duplicated"
    );
}
