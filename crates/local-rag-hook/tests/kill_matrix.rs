//! T13-06 S1/S2 acceptance tests (spec 07 §7): a **real**, genuinely killed
//! `local-rag-hook spool-write` process, not a hand-truncated fixture.
//!
//! # Scope decision: genuine byte-level torn writes are not reproduced here
//!
//! POSIX gives no portable way to force a short `write()` of a payload well
//! under the 1 MiB frame cap to a local regular file without kernel-level
//! fault injection, so this suite does not attempt to manufacture one.
//! `local_rag_core::spool`/`local_rag_store::spool`'s own tests (T13-03,
//! T13-04) already exhaustively prove correct recovery for a torn frame of
//! **any** truncation length via direct byte manipulation — that claim needs
//! no further proof from a real process. What a real kill *uniquely* adds:
//!
//! - the one realistic single-syscall interruption point — killed right after
//!   the write lands in the OS page cache but before `fdatasync` confirms
//!   it — still leaves a complete, durable, exactly-once-imported frame
//!   (`s2_...`): a process kill is not power loss, so page-cache-visible
//!   bytes survive it regardless of `fdatasync`;
//! - "segment remains valid" is demonstrated for real: a **second**, real,
//!   unarmed hook invocation appends validly to the same session afterward
//!   (`s1_...`), which is the empirically-checkable half of S1's claim.
//!
//! Gated on `unix` + `failpoints` (signal inspection is POSIX-specific; the
//! `LOCAL_RAG_HOOK_FAILPOINT` seam only exists with the feature). Run via
//! `cargo test -p local-rag-hook --features failpoints`.

#![cfg(all(unix, feature = "failpoints"))]

use std::io::Write;
use std::os::unix::process::ExitStatusExt;
use std::process::{Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use local_rag_core::identity::{Uuid, UuidSource, uuidv7_from};
use local_rag_core::paths::StoreLayout;
use local_rag_core::spool::HEADER_LEN;
use local_rag_store::{RequestRoot, StateDb, StopReason, decode_segment, import_session_tail};
use local_rag_test_support::TempHome;

const FAILPOINT_ENV: &str = "LOCAL_RAG_HOOK_FAILPOINT";
const WRITE_BEFORE_FDATASYNC: &str = "hook.segment.after_write_before_fdatasync";

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

fn spool_dir(home: &TempHome) -> std::path::PathBuf {
    home.join("local-rag").join("spool")
}

/// Run `local-rag-hook spool-write` for real, optionally with a named
/// failpoint armed via the environment (see `arm_failpoint_from_env` in
/// `src/main.rs`).
fn run_spool_write(home: &TempHome, stdin_input: &[u8], failpoint: Option<&str>) -> Output {
    let mut cmd = home.command(env!("CARGO_BIN_EXE_local-rag-hook"));
    cmd.arg("spool-write")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(name) = failpoint {
        cmd.env(FAILPOINT_ENV, name);
    }
    let mut child = cmd.spawn().expect("spawn local-rag-hook");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(stdin_input)
        .expect("write stdin");
    child.wait_with_output().expect("wait for local-rag-hook")
}

fn stop_event(session_id: &str, message: &str) -> Vec<u8> {
    format!(
        r#"{{"session_id":"{session_id}","hook_event_name":"Stop","last_assistant_message":"{message}"}}"#
    )
    .into_bytes()
}

/// S2: killed after the write lands but before `fdatasync` — the frame is
/// still complete on disk (no torn tail) and imports exactly once.
#[tokio::test]
async fn s2_hook_killed_after_write_before_fdatasync_is_durable_and_imported_once() {
    let home = TempHome::new().expect("temp home");
    std::fs::create_dir_all(spool_dir(&home)).unwrap();
    let session = "sess-s2";

    let output = run_spool_write(
        &home,
        &stop_event(session, "done"),
        Some(WRITE_BEFORE_FDATASYNC),
    );
    assert_eq!(
        output.status.signal(),
        Some(6),
        "hook must die with SIGABRT; status={:?}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );

    let seg_path = spool_dir(&home).join(session).join("000001.seg");
    let bytes = std::fs::read(&seg_path).expect("segment file exists");
    assert!(bytes.len() > HEADER_LEN, "the frame's bytes landed");

    let decoded = decode_segment(&bytes).expect("valid header");
    assert_eq!(decoded.frames.len(), 1, "one complete frame, not torn");
    assert!(
        matches!(decoded.stop_reason, StopReason::EndOfInput),
        "no torn tail: {:?}",
        decoded.stop_reason
    );

    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    let uuids = SeqUuidV7::new();
    let request_root = RequestRoot::default();

    let first = import_session_tail(&db, &layout, session, &request_root, &uuids, 5_000, 72)
        .await
        .expect("first import");
    assert_eq!(first.report.imported, 1);

    let second = import_session_tail(&db, &layout, session, &request_root, &uuids, 6_000, 72)
        .await
        .expect("second import");
    assert_eq!(second.report.imported, 0, "already imported, idempotent");
}

/// S1's empirically-checkable half: after a hook is killed mid-append, the
/// segment "remains valid" — a second, real, unarmed hook invocation still
/// appends correctly to the same session, and both frames end up imported.
#[tokio::test]
async fn s1_segment_remains_valid_for_the_next_hook_after_a_kill() {
    let home = TempHome::new().expect("temp home");
    std::fs::create_dir_all(spool_dir(&home)).unwrap();
    let session = "sess-s1";

    let killed = run_spool_write(
        &home,
        &stop_event(session, "done"),
        Some(WRITE_BEFORE_FDATASYNC),
    );
    assert_eq!(killed.status.signal(), Some(6), "the first hook must die");

    // A second, real, unarmed invocation for the same session, with genuinely
    // distinct content (a different `source_event_id`, not a retry of the same
    // logical event — that scenario is S6, not S1).
    let ok = run_spool_write(&home, &stop_event(session, "done2"), None);
    assert!(
        ok.status.success(),
        "a fresh hook must append cleanly after the prior one was killed"
    );

    let seg_path = spool_dir(&home).join(session).join("000001.seg");
    let bytes = std::fs::read(&seg_path).unwrap();
    let decoded = decode_segment(&bytes).expect("valid header");
    assert_eq!(
        decoded.frames.len(),
        2,
        "both the killed process's frame and the next hook's frame decode cleanly"
    );
    assert!(matches!(decoded.stop_reason, StopReason::EndOfInput));

    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    let uuids = SeqUuidV7::new();
    let request_root = RequestRoot::default();

    let outcome = import_session_tail(&db, &layout, session, &request_root, &uuids, 5_000, 72)
        .await
        .expect("import both frames");
    assert_eq!(outcome.report.imported, 2);
}
