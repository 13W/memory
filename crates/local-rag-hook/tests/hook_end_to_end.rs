//! CLI-level tests against the real compiled `local-rag-hook spool-write`
//! binary: malformed input, an unknown hook event, and a filesystem error all
//! fail open (exit 0, no segment written); a valid event produces a real
//! segment. A full disk-full/kill-point matrix is T13-06's `failpoints`-based
//! S1–S8 suite, out of this task's scope — these are the deterministic,
//! portably-reproducible failure modes this card's own bullet asks for.

use std::io::Write;
use std::process::{Output, Stdio};

use local_rag_test_support::TempHome;

fn run_spool_write(home: &TempHome, stdin_input: &[u8]) -> Output {
    let mut child = home
        .command(env!("CARGO_BIN_EXE_local-rag-hook"))
        .arg("spool-write")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn local-rag-hook");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(stdin_input)
        .expect("write stdin");
    child.wait_with_output().expect("wait for local-rag-hook")
}

fn spool_dir(home: &TempHome) -> std::path::PathBuf {
    home.join("local-rag").join("spool")
}

#[test]
fn malformed_json_exits_zero_and_writes_nothing() {
    let home = TempHome::new().expect("temp home");
    std::fs::create_dir_all(spool_dir(&home)).unwrap();

    let output = run_spool_write(&home, b"not json at all");
    assert!(output.status.success(), "hook must always exit 0");
    assert_eq!(
        std::fs::read_dir(spool_dir(&home)).unwrap().count(),
        0,
        "no session directory created"
    );
}

#[test]
fn unknown_hook_event_name_fails_open() {
    let home = TempHome::new().expect("temp home");
    std::fs::create_dir_all(spool_dir(&home)).unwrap();

    let input = br#"{"session_id":"s","hook_event_name":"PreCompact"}"#;
    let output = run_spool_write(&home, input);
    assert!(output.status.success());
    assert_eq!(std::fs::read_dir(spool_dir(&home)).unwrap().count(), 0);
}

#[test]
fn a_valid_event_produces_a_real_segment() {
    let home = TempHome::new().expect("temp home");
    std::fs::create_dir_all(spool_dir(&home)).unwrap();

    let input =
        br#"{"session_id":"sess-e2e","hook_event_name":"Stop","last_assistant_message":"done"}"#;
    let output = run_spool_write(&home, input);
    assert!(output.status.success());

    let seg = spool_dir(&home).join("sess-e2e").join("000001.seg");
    assert!(seg.exists(), "segment file must exist");
    assert!(
        fs_metadata_len(&seg) > 16,
        "segment holds more than a bare header"
    );
}

fn fs_metadata_len(path: &std::path::Path) -> u64 {
    std::fs::metadata(path).expect("segment metadata").len()
}

/// spec 12 §2 `[FIXED]` ("Secret redaction runs before anything is written
/// to the spool"), the on-disk half T16-04 closes: `crates/local-rag-hook/
/// tests/payload.rs`'s own tests already prove the raw secret never reaches
/// `PreparedPayload::bytes` in memory; this reads the real, on-disk `.seg`
/// file the compiled binary wrote and confirms the raw secret is absent
/// there too, not just in the in-memory value the earlier tests inspect
/// (`adversarial.hook.secret-redacted-on-disk`). `main.rs::
/// spool_write_pipeline` scans the *entire* raw stdin JSON
/// (`String::from_utf8_lossy(raw)`), so a secret anywhere in the event
/// lands in the redacted `FramePayload.payload`, encoded uncompressed
/// (`local_rag_core::spool::encode_frame`) — plain, directly-searchable
/// bytes on disk.
#[test]
fn a_secret_in_the_payload_is_redacted_on_disk_not_just_in_memory() {
    let home = TempHome::new().expect("temp home");
    std::fs::create_dir_all(spool_dir(&home)).unwrap();

    let secret = "AKIAIOSFODNN7EXAMPLE";
    let input = format!(
        r#"{{"session_id":"sess-secret","hook_event_name":"Stop","last_assistant_message":"token = {secret}"}}"#
    );
    let output = run_spool_write(&home, input.as_bytes());
    assert!(output.status.success(), "{output:?}");

    let seg = spool_dir(&home).join("sess-secret").join("000001.seg");
    let bytes = std::fs::read(&seg).expect("read segment");
    let text = String::from_utf8_lossy(&bytes);
    assert!(!text.contains(secret), "raw secret must never reach disk");
    assert!(
        text.contains("[REDACTED]"),
        "the redaction marker must be present"
    );
}

#[cfg(unix)]
#[test]
fn a_permission_denied_session_dir_fails_open_without_a_panic() {
    use std::os::unix::fs::PermissionsExt;

    let home = TempHome::new().expect("temp home");
    let spool = spool_dir(&home);
    std::fs::create_dir_all(&spool).unwrap();
    // Read-only spool/: the hook cannot create the session subdirectory.
    std::fs::set_permissions(&spool, std::fs::Permissions::from_mode(0o500)).unwrap();

    let input = br#"{"session_id":"sess-denied","hook_event_name":"Stop"}"#;
    let output = run_spool_write(&home, input);

    // Restore write access so `TempHome`'s `Drop` can clean up.
    std::fs::set_permissions(&spool, std::fs::Permissions::from_mode(0o700)).unwrap();

    assert!(
        output.status.success(),
        "hook must always exit 0 even on a filesystem error"
    );
    assert!(!spool.join("sess-denied").exists());
}
