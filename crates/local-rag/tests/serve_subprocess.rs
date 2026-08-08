//! T15-01 acceptance tests driving the **real** `local-rag serve` binary as a
//! genuine second OS process — not the in-process `DaemonHandle` scenarios
//! `tests/lock_liveness.rs`/`tests/lifecycle_startup.rs` already cover.
//!
//! - **live conflict**: two real `local-rag serve` processes against one
//!   store; the second must refuse to start, the first must be unaffected.
//! - **SIGTERM at safe points** (`failpoints` feature only): a real `SIGTERM`
//!   arrives while a startup resume job is provably still in flight (a
//!   `LOCAL_RAG_TEST_RESUME_DELAY_MS`-controlled pause — see
//!   `daemon::resume::test_resume_pause`'s doc comment for why this is an
//!   env-var hand-off, not the shared in-process `Failpoints` registry); the
//!   process must still exit cleanly, having let the job finish rather than
//!   tearing it down mid-write.
//!
//! Mirrors `crates/local-rag-hook/tests/kill_matrix.rs`'s established idiom:
//! a real child via `TempHome::command`, `libc::kill` for a real signal
//! (`Child::kill` is `SIGKILL`-only on unix and cannot send `SIGTERM`).

#![cfg(unix)]

use std::io::Read;
#[cfg(feature = "failpoints")]
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use local_rag_core::paths::StoreLayout;
#[cfg(feature = "failpoints")]
use local_rag_core::spool::{FramePayload, encode_frame, encode_segment_header};
use local_rag_test_support::TempHome;

fn open_layout() -> (TempHome, StoreLayout) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    (home, layout)
}

fn spawn_serve(home: &TempHome, extra_env: &[(&str, &str)]) -> Child {
    let mut cmd = home.command(env!("CARGO_BIN_EXE_local-rag"));
    cmd.arg("serve");
    // X-004: an ambient `RUST_LOG` in the developer's own shell would
    // otherwise leak through `TempHome::command` (it only clears `HOME`)
    // and could suppress the `warn!`/`error!` lines these tests assert on.
    cmd.env("RUST_LOG", "info");
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.spawn().expect("spawn local-rag serve")
}

/// Poll `store.lock` until it parses with `ready: true`, or panic after
/// `timeout`. Real, bounded wall-clock waiting is inherent to driving a real
/// child process across a real OS scheduling boundary — the same class of
/// wait `local-rag-hook`'s own real-subprocess tests already accept; nothing
/// here asserts on the *duration*, only on eventual readiness.
fn wait_until_ready(layout: &StoreLayout, timeout: Duration) -> serde_json::Value {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(bytes) = std::fs::read(layout.store_lock())
            && let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes)
            && json.get("ready").and_then(|v| v.as_bool()) == Some(true)
        {
            return json;
        }
        if Instant::now() >= deadline {
            panic!("store.lock did not become ready within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_exit(child: &mut Child, timeout: Duration) -> std::process::ExitStatus {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("child did not exit within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(feature = "failpoints")]
fn write_spool_segment(layout: &StoreLayout, session_id: &str) {
    let frame = FramePayload {
        format_version: 1,
        source_event_id: format!("st:{session_id}:1"),
        dedup_key: None,
        event_type: "Stop".to_string(),
        captured_at: 1_000,
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
    };
    let session_dir = layout.spool_session(session_id);
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let mut bytes = encode_segment_header().to_vec();
    bytes.extend_from_slice(&encode_frame(&frame).expect("under the frame cap"));
    std::fs::write(session_dir.join("000001.seg"), bytes).expect("write segment");
}

/// D-030 / T17-04: a genuinely corrupt spool segment on disk before startup
/// (spec 11 §4 `[FIXED concern]`: "a newer hook binary writing a newer
/// format... is a reportable incompatibility, not silent loss") is reported
/// on the real daemon's stderr by its startup resume pass, not silently
/// dropped — before this fix, `spawn_spool_resume`'s result was discarded
/// unread (`daemon/lifecycle.rs`). `DaemonHandle::shutdown` awaits every
/// `resume_handles` entry before exiting (see the `sigterm_during_a_resume_
/// job...` test above for the same guarantee), so sending `SIGTERM` right
/// after readiness and then reading stderr after a clean exit reliably
/// observes whatever the resume pass reported.
#[test]
fn a_stalled_spool_session_is_reported_on_stderr_not_silently_dropped() {
    let (home, layout) = open_layout();

    let stalled_dir = layout.spool_session("stalled-session");
    std::fs::create_dir_all(&stalled_dir).expect("mkdir stalled session");
    // 16 zero bytes: exactly HEADER_LEN (never `Truncated`), but the magic
    // does not match — genuine corruption, not a normal in-progress write.
    std::fs::write(stalled_dir.join("000001.seg"), [0u8; 16]).expect("write corrupt header");

    let mut child = spawn_serve(&home, &[]);
    wait_until_ready(&layout, Duration::from_secs(20));

    send_sigterm(child.id());
    let status = wait_for_exit(&mut child, Duration::from_secs(20));
    assert!(status.success(), "must exit cleanly: {status:?}");

    let mut stderr_buf = Vec::new();
    child
        .stderr
        .take()
        .expect("stderr was piped")
        .read_to_end(&mut stderr_buf)
        .expect("read stderr");
    let stderr = String::from_utf8_lossy(&stderr_buf);
    assert!(
        stderr.contains("stalled-session") && stderr.contains("stalled on import"),
        "stderr must report the stall, not silently drop it: {stderr}"
    );
}

/// Send a real `SIGTERM` (not `SIGKILL` — `std::process::Child::kill` is
/// unix-`SIGKILL`-only and cannot express this).
fn send_sigterm(pid: u32) {
    // SAFETY: `kill` with a valid pid and signal number is a plain,
    // side-effect-documented syscall; no memory is read or written.
    #[allow(unsafe_code)]
    let rc = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    assert_eq!(
        rc,
        0,
        "kill(SIGTERM) failed: {}",
        std::io::Error::last_os_error()
    );
}

#[test]
fn live_conflict_the_second_daemon_refuses_and_the_first_is_unaffected() {
    let (home, layout) = open_layout();

    let mut first = spawn_serve(&home, &[]);
    let owner = wait_until_ready(&layout, Duration::from_secs(20));
    let owner_pid = owner["pid"].as_u64().expect("pid field");

    let mut second = spawn_serve(&home, &[]);
    let second_status = wait_for_exit(&mut second, Duration::from_secs(20));
    let second_output = second.wait_with_output().unwrap_or_else(|_| {
        // `wait_for_exit` already reaped the status; if `wait_with_output`
        // can't re-collect (already waited), fall back to empty streams —
        // the exit-code assertion below is what matters most.
        std::process::Output {
            status: second_status,
            stdout: Vec::new(),
            stderr: Vec::new(),
        }
    });

    assert!(
        !second_status.success(),
        "a second daemon must not start against a locked store"
    );
    let stderr = String::from_utf8_lossy(&second_output.stderr);
    assert!(
        stderr.contains("STORE_LOCKED") || stderr.contains(&owner_pid.to_string()),
        "stderr should name the conflict: {stderr}"
    );

    // The first daemon must still be alive and unaffected throughout.
    assert!(
        first.try_wait().expect("try_wait").is_none(),
        "the first daemon must still be running"
    );

    send_sigterm(first.id());
    let first_status = wait_for_exit(&mut first, Duration::from_secs(20));
    assert!(
        first_status.success(),
        "the first daemon must exit cleanly on SIGTERM: {first_status:?}"
    );
    assert!(!layout.store_lock().exists());
}

/// A real `SIGTERM` arrives while a startup resume job is provably still in
/// flight (via `LOCAL_RAG_TEST_RESUME_DELAY_MS`): the daemon must still let
/// the job finish (spec 02 §4.3 "cancel... at the next safe point") rather
/// than tearing it down — proven by the spool segment being **fully
/// imported** afterward, not merely "not corrupted."
#[test]
#[cfg(feature = "failpoints")]
fn sigterm_during_a_resume_job_lets_it_finish_before_exiting() {
    let (home, layout) = open_layout();
    write_spool_segment(&layout, "sess-mid-resume");

    let mut child = spawn_serve(&home, &[("LOCAL_RAG_TEST_RESUME_DELAY_MS", "3000")]);
    wait_until_ready(&layout, Duration::from_secs(20));

    // The resume pass starts right after readiness; sending SIGTERM
    // immediately gives a wide margin before the 3s artificial pause would
    // naturally elapse on its own.
    send_sigterm(child.id());

    let status = wait_for_exit(&mut child, Duration::from_secs(20));
    assert!(
        status.success(),
        "must exit 0, not be torn down mid-job: {status:?} (signal: {:?})",
        status.signal()
    );
    assert!(
        !layout.store_lock().exists(),
        "shutdown must release the lock even when a job was in flight when the signal arrived"
    );

    // Reopen fresh and confirm the paused job actually completed (not just
    // "didn't corrupt anything") — the segment must be fully imported.
    let state_db =
        local_rag_store::StateDb::open(layout.state_db()).expect("reopen state.sqlite cleanly");
    let pending = local_rag_store::store_has_pending_spool_bytes(&state_db, &layout)
        .expect("check pending spool bytes");
    assert!(
        !pending,
        "the in-flight import must have been allowed to finish before shutdown"
    );
}
