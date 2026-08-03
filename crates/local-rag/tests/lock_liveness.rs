//! T15-01 acceptance tests for `daemon::lock::acquire` (spec 02 §4.1 step 1):
//! live conflict, PID reuse mismatch, and stale socket/lock recovery.
//!
//! All in-process: a background thread (for the genuine live-holder case) or
//! a hand-rolled `UnixListener` (for the liveness probe) stand in for a
//! second daemon — no real second `local-rag serve` process is needed for
//! these lock-mechanics scenarios (that is `tests/serve_subprocess.rs`'s
//! job). Deterministic: no wall-clock sleeps — a `std::sync::mpsc` handshake
//! proves the background holder is ready before the foreground `acquire`
//! runs, mirroring `crates/store/tests/lock.rs`'s own channel-based idiom for
//! proving real concurrency without a race.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::sync::mpsc;

use local_rag::daemon::probe::{LivenessOutcome, LivenessProbe};
use local_rag::daemon::{
    SocketLivenessProbe, StoreLockError, StoreLockFileState, acquire, read_store_lock_file,
};
use local_rag_core::paths::StoreLayout;
use local_rag_protocol::{Message, PROTO_VERSION, Welcome, encode_message};
use local_rag_test_support::TempHome;

fn open_layout() -> (TempHome, StoreLayout) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    (home, layout)
}

/// A probe that always answers the same fixed outcome, for tests that don't
/// want to stand up a real socket.
struct FixedProbe(LivenessOutcome);

impl LivenessProbe for FixedProbe {
    fn check(&self, _pid: u32, _expected_instance_uuid: &str) -> LivenessOutcome {
        self.0
    }
}

/// Bind a `UnixListener` at `socket_path` that answers every connection with
/// the real HELLO/WELCOME handshake — reads (and discards) one HELLO line,
/// then replies with one WELCOME line carrying `store_instance_uuid` — until
/// `stop` fires.
fn spawn_greeter(socket_path: std::path::PathBuf, store_instance_uuid: String) -> impl FnOnce() {
    let listener = UnixListener::bind(&socket_path).expect("bind fake daemon socket");
    listener.set_nonblocking(true).expect("nonblocking");
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let handle = std::thread::spawn(move || {
        loop {
            if stop_rx.try_recv().is_ok() {
                break;
            }
            match listener.accept() {
                Ok((stream, _)) => {
                    stream
                        .set_nonblocking(false)
                        .expect("blocking for this connection");
                    let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
                    let mut line = String::new();
                    let _ = reader.read_line(&mut line); // consume HELLO, content unused
                    let welcome = Message::Welcome(Welcome {
                        proto: PROTO_VERSION,
                        daemon_version: "0.0.0".to_string(),
                        store_instance_uuid: store_instance_uuid.clone(),
                        capabilities: Vec::new(),
                        mcp_passthrough_version: local_rag_protocol::MCP_PASSTHROUGH_VERSION,
                        spool_max_format_version: local_rag_core::spool::FORMAT_VERSION,
                        mode: "normal".to_string(),
                    });
                    let bytes = encode_message(&welcome).unwrap();
                    let mut stream = stream;
                    let _ = stream.write_all(&bytes);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::yield_now();
                }
                Err(_) => break,
            }
        }
    });
    move || {
        let _ = stop_tx.send(());
        let _ = handle.join();
    }
}

/// A background thread genuinely holds `store.lock` (a real, live `flock`
/// contender) and answers the liveness probe correctly — `acquire` must
/// report `Locked` naming that holder, never silently take over.
#[test]
fn live_conflict_is_reported_and_names_the_owner() {
    let (_home, layout) = open_layout();
    let owner_uuid = "owner-instance-uuid";
    let owner_pid = std::process::id();

    let stop_greeter = spawn_greeter(layout.socket_path(), owner_uuid.to_string());

    let (ready_tx, ready_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let lock_path = layout.store_lock();
    let owner_json = serde_json::json!({
        "instance_uuid": owner_uuid,
        "pid": owner_pid,
        "daemon_version": "0.0.0",
        "started_at": 1_000,
        "ready": true,
        "ready_at": 1_000,
        "socket_path": layout.socket_path().display().to_string(),
    })
    .to_string();

    let holder = std::thread::spawn(move || {
        local_rag_core::paths::ensure_file_0600(&lock_path).expect("ensure lock file");
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .expect("open lock file");
        file.lock().expect("blocking lock");
        file.write_all(owner_json.as_bytes()).expect("write info");
        file.flush().expect("flush");
        ready_tx.send(()).expect("signal ready");
        release_rx.recv().expect("wait for release signal");
        // `file` drops here, releasing the flock.
    });

    ready_rx.recv().expect("holder ready");

    let probe = SocketLivenessProbe::new(layout.socket_path());
    let result = acquire(&layout, "candidate-uuid", 999_999, "0.0.0", 2_000, &probe);

    release_tx.send(()).expect("release holder");
    holder.join().expect("holder thread");
    stop_greeter();

    match result {
        Err(StoreLockError::Locked { owner }) => {
            assert_eq!(owner.instance_uuid, owner_uuid);
            assert_eq!(owner.pid, owner_pid);
        }
        other => panic!("expected Locked, got {other:?}"),
    }
}

/// The lock file names a PID that is genuinely alive (our own test process),
/// but the socket answers with a *different* `instance_uuid` — a different
/// process entirely happens to have been assigned that PID since. `acquire`
/// must classify the owner as stale and successfully reclaim, not treat
/// "PID exists" alone as proof of ownership.
#[test]
fn pid_reuse_mismatch_is_reclaimed() {
    let (_home, layout) = open_layout();

    let stop_greeter = spawn_greeter(
        layout.socket_path(),
        "a-totally-different-daemon".to_string(),
    );

    let stale_json = serde_json::json!({
        "instance_uuid": "stale-uuid-nobody-answers-for",
        "pid": std::process::id(),
        "daemon_version": "0.0.0",
        "started_at": 1_000,
        "ready": true,
        "ready_at": 1_000,
        "socket_path": layout.socket_path().display().to_string(),
    })
    .to_string();
    local_rag_core::paths::ensure_file_0600(&layout.store_lock()).expect("ensure lock file");
    std::fs::write(layout.store_lock(), stale_json).expect("seed stale lock file");

    // No real flock held on the file — a fresh `try_lock` succeeds
    // immediately, so this specifically exercises the *success-path* stale
    // socket cleanup, not the WouldBlock recovery branch (see
    // `wouldblock_recovery_reclaims_after_probe_reports_stale` below for that
    // one, and `daemon::probe`'s own unit tests for the probe logic itself).
    let probe = SocketLivenessProbe::new(layout.socket_path());
    let guard = acquire(&layout, "new-instance-uuid", 42, "0.0.0", 2_000, &probe)
        .expect("stale owner must be reclaimed");
    assert_eq!(guard.info().instance_uuid, "new-instance-uuid");
    assert!(
        !layout.socket_path().exists(),
        "the orphaned socket must be cleaned up"
    );

    stop_greeter();
}

/// `WouldBlock` recovery specifically (the file genuinely was `flock`'d by a
/// holder that has since become unreachable — probe reports `Stale`):
/// `acquire` must reclaim on the single retry.
#[test]
fn wouldblock_recovery_reclaims_after_probe_reports_stale() {
    let (_home, layout) = open_layout();
    let (ready_tx, ready_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let lock_path = layout.store_lock();

    let holder = std::thread::spawn(move || {
        local_rag_core::paths::ensure_file_0600(&lock_path).expect("ensure lock file");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .expect("open lock file");
        file.lock().expect("blocking lock");
        ready_tx.send(()).expect("signal ready");
        release_rx.recv().expect("wait for release signal");
    });
    ready_rx.recv().expect("holder ready");

    // No greeting: the probe's `pid_exists` check for our own live test
    // process would pass, but with no listener at all the socket half of the
    // handshake fails — `Stale` (see `daemon::probe`'s own
    // `no_listener_at_all_is_stale`). Note: the lock file has no parseable
    // JSON in it in this scenario (nothing wrote one), so `acquire` takes the
    // "unparseable ⇒ treat as stale" path, not a probe-driven one — proven
    // separately by `stale_unparseable_lock_file_is_reclaimed` below. This
    // test instead seeds a well-formed-but-mismatched record so the probe
    // itself is exercised on the WouldBlock path.
    let stale_json = serde_json::json!({
        "instance_uuid": "unreachable-owner",
        "pid": std::process::id(),
        "daemon_version": "0.0.0",
        "started_at": 1_000,
        "ready": true,
        "ready_at": 1_000,
        "socket_path": layout.socket_path().display().to_string(),
    })
    .to_string();
    // Written by the holder before signaling ready would race the `file`
    // handle above; instead write it via a second, independent open — still
    // valid, since `flock` is advisory and this is a plain content write, not
    // a second lock attempt.
    std::fs::write(layout.store_lock(), stale_json).expect("seed owner json");

    let probe = FixedProbe(LivenessOutcome::Stale);
    let result = acquire(&layout, "candidate-uuid", 1, "0.0.0", 2_000, &probe);

    release_tx.send(()).expect("release holder");
    holder.join().expect("holder thread");

    let guard = result.expect("stale WouldBlock owner must be reclaimed on retry");
    assert_eq!(guard.info().instance_uuid, "candidate-uuid");
}

/// A `store.lock` naming a **definitely dead** PID (spawned and reaped
/// before the test even runs `acquire`) — the "stale lock" half of the
/// card's "stale socket/lock" scenario, taken via the success-path branch
/// (no live flock held at all, so `try_lock` succeeds immediately and the
/// stale *content* is simply overwritten).
#[test]
fn stale_lock_content_from_a_dead_owner_is_overwritten() {
    let (_home, layout) = open_layout();

    let mut child = std::process::Command::new("true")
        .spawn()
        .expect("spawn trivial child");
    let dead_pid = child.id();
    child.wait().expect("reap child");

    let stale_json = serde_json::json!({
        "instance_uuid": "long-dead-instance",
        "pid": dead_pid,
        "daemon_version": "0.0.0",
        "started_at": 1_000,
        "ready": true,
        "ready_at": 1_000,
        "socket_path": layout.socket_path().display().to_string(),
    })
    .to_string();
    local_rag_core::paths::ensure_file_0600(&layout.store_lock()).expect("ensure lock file");
    std::fs::write(layout.store_lock(), stale_json).expect("seed dead-owner lock file");

    let probe = FixedProbe(LivenessOutcome::Stale);
    let guard = acquire(&layout, "fresh-instance", 7, "0.0.0", 2_000, &probe)
        .expect("dead owner must not block acquisition");
    assert_eq!(guard.info().instance_uuid, "fresh-instance");
}

/// An orphaned socket file (no real listener behind it — the crashed prior
/// daemon never got to clean it up) does not stop a fresh `acquire`: since
/// `try_lock` succeeds immediately (no live holder), the socket is simply
/// removed as garbage before the caller binds a real listener there.
#[test]
fn orphaned_socket_file_does_not_block_acquisition_and_is_removed() {
    let (_home, layout) = open_layout();
    std::fs::write(layout.socket_path(), b"not a real socket").expect("seed orphan socket file");
    assert!(layout.socket_path().exists());

    let probe = FixedProbe(LivenessOutcome::Stale);
    let guard = acquire(&layout, "fresh-instance", 7, "0.0.0", 2_000, &probe)
        .expect("orphan socket must not block acquisition");
    assert_eq!(guard.info().instance_uuid, "fresh-instance");
    assert!(
        !layout.socket_path().exists(),
        "the orphaned socket file must be removed"
    );
}

/// A completely unparseable `store.lock` (a torn write from a crash) is
/// treated the same as "stale, unknown owner" — never a hard error.
#[test]
fn unparseable_lock_content_is_treated_as_stale() {
    let (_home, layout) = open_layout();
    local_rag_core::paths::ensure_file_0600(&layout.store_lock()).expect("ensure lock file");
    std::fs::write(layout.store_lock(), b"{not valid json at all").expect("seed torn write");

    let probe = FixedProbe(LivenessOutcome::Alive); // must not even be consulted
    let guard = acquire(&layout, "fresh-instance", 7, "0.0.0", 2_000, &probe)
        .expect("unparseable content must not block acquisition");
    assert_eq!(guard.info().instance_uuid, "fresh-instance");
}

/// `mark_ready` rewrites the JSON through the same handle without ever
/// releasing the lock mid-swap: a concurrent `acquire` attempt still sees
/// `Locked` immediately before and immediately after the call.
///
/// Uses our own real, live pid for `guard`'s owner (exactly what a real
/// daemon does): the *first* contender check runs while `guard` is still
/// `ready: false` (before `mark_ready`), which — since `daemon::lock`
/// correctly trusts PID alone for a not-yet-ready owner (see
/// `a_not_yet_ready_live_owner_is_never_mistaken_for_dead`) — needs a
/// genuinely alive pid to be classified `Locked` rather than reclaimed; a
/// placeholder pid like `100` would not exercise that path honestly.
#[test]
fn mark_ready_never_drops_the_lock() {
    let (_home, layout) = open_layout();
    let probe = FixedProbe(LivenessOutcome::Stale);
    let mut guard = acquire(
        &layout,
        "instance-a",
        std::process::id(),
        "0.0.0",
        1_000,
        &probe,
    )
    .expect("acquire");

    let contender = acquire(
        &layout,
        "instance-b",
        200,
        "0.0.0",
        1_000,
        &FixedProbe(LivenessOutcome::Alive),
    );
    assert!(matches!(contender, Err(StoreLockError::Locked { .. })));

    guard
        .mark_ready(1_500, &layout.socket_path())
        .expect("mark ready");
    assert!(guard.info().ready);

    let contender_after = acquire(
        &layout,
        "instance-c",
        300,
        "0.0.0",
        1_600,
        &FixedProbe(LivenessOutcome::Alive),
    );
    assert!(matches!(
        contender_after,
        Err(StoreLockError::Locked { .. })
    ));
}

/// After an explicit `release`, a fresh `acquire` succeeds immediately — no
/// stale-recovery branch needed at all. This is the reliable proof that
/// `release` actually released the OS lock (not merely that the process
/// happened to exit), used again by the shutdown/checkpoint tests.
#[test]
fn release_lets_a_fresh_acquire_succeed_without_recovery() {
    let (_home, layout) = open_layout();
    let probe = FixedProbe(LivenessOutcome::Alive); // must not be consulted at all
    let guard = acquire(&layout, "instance-a", 100, "0.0.0", 1_000, &probe).expect("acquire");
    guard.release(&layout);
    assert!(!layout.store_lock().exists());

    let second = acquire(&layout, "instance-b", 200, "0.0.0", 2_000, &probe);
    assert!(second.is_ok(), "release must free the lock: {second:?}");
}

/// A live owner that has not finished starting up yet (`ready: false` — most
/// commonly, still running a migration, spec 02 §4.1 step 2) has bound no
/// socket at all. A naive probe would see "no listener" and misclassify it
/// as dead, wrongly reclaiming the lock out from under a genuinely live
/// daemon (whose real OS `flock` on its own open file descriptor is never
/// actually released by that reclaim — see `daemon::lock::is_owner_alive`'s
/// doc comment). `acquire` must instead trust the PID alone while
/// `ready == false`, and correctly report `Locked`, not silently steal the
/// store from a daemon that is still migrating.
#[test]
fn a_not_yet_ready_live_owner_is_never_mistaken_for_dead() {
    let (_home, layout) = open_layout();
    // A real background holder — genuinely holding the OS `flock`, exactly
    // as a real still-migrating daemon would — not just a content write:
    // `acquire` must actually take the `WouldBlock` recovery path here, not
    // the (structurally different) success-path content overwrite.
    let (ready_tx, ready_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let lock_path = layout.store_lock();
    // Deliberately no `spawn_greeter`: this is the whole point — the owner
    // has bound no socket yet, so a real `SocketLivenessProbe` would see
    // "connection refused" if consulted at all.
    let owner_json = serde_json::json!({
        "instance_uuid": "still-migrating-instance",
        "pid": std::process::id(),
        "daemon_version": "0.0.0",
        "started_at": 1_000,
        "ready": false,
        "ready_at": null,
        "socket_path": null,
    })
    .to_string();

    let holder = std::thread::spawn(move || {
        local_rag_core::paths::ensure_file_0600(&lock_path).expect("ensure lock file");
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .expect("open lock file");
        file.lock().expect("blocking lock");
        file.write_all(owner_json.as_bytes()).expect("write info");
        file.flush().expect("flush");
        ready_tx.send(()).expect("signal ready");
        release_rx.recv().expect("wait for release signal");
    });
    ready_rx.recv().expect("holder ready");

    let probe = SocketLivenessProbe::new(layout.socket_path());
    let result = acquire(&layout, "candidate-uuid", 999_999, "0.0.0", 2_000, &probe);

    release_tx.send(()).expect("release holder");
    holder.join().expect("holder thread");

    match result {
        Err(StoreLockError::Locked { owner }) => {
            assert_eq!(owner.instance_uuid, "still-migrating-instance");
            assert!(!owner.ready);
        }
        other => panic!(
            "expected Locked (a still-starting live owner must never be reclaimed), got {other:?}"
        ),
    }
}

/// The same not-yet-ready record, but the PID is genuinely dead (a crash
/// mid-migration) — this must still be reclaimed. A dead process cannot
/// realistically hold a real `flock` either (POSIX releases it on exit, see
/// this module's own doc comment), so — unlike the live sibling test above —
/// this one takes the success-path content overwrite, not the `WouldBlock`
/// recovery branch; that is the realistic shape of this scenario, not a gap
/// in coverage.
#[test]
fn a_not_yet_ready_owner_with_a_dead_pid_is_reclaimed() {
    let (_home, layout) = open_layout();
    let mut child = std::process::Command::new("true")
        .spawn()
        .expect("spawn trivial child");
    let dead_pid = child.id();
    child.wait().expect("reap child");

    let owner_json = serde_json::json!({
        "instance_uuid": "crashed-mid-migration",
        "pid": dead_pid,
        "daemon_version": "0.0.0",
        "started_at": 1_000,
        "ready": false,
        "ready_at": null,
        "socket_path": null,
    })
    .to_string();
    local_rag_core::paths::ensure_file_0600(&layout.store_lock()).expect("ensure lock file");
    std::fs::write(layout.store_lock(), owner_json).expect("seed dead not-yet-ready lock file");

    let probe = SocketLivenessProbe::new(layout.socket_path());
    let guard = acquire(&layout, "fresh-instance", 7, "0.0.0", 2_000, &probe)
        .expect("a dead not-yet-ready owner must be reclaimed");
    assert_eq!(guard.info().instance_uuid, "fresh-instance");
}

// ---------------------------------------------------------------------------
// T16-03: `read_store_lock_file` — pure read, no `flock` contention, no
// mutation; distinguishes absent from corrupt, unlike `acquire`'s own
// best-effort `read_lock_info` (which deliberately collapses both).
// ---------------------------------------------------------------------------

#[test]
fn read_store_lock_file_is_absent_when_no_lock_file_exists() {
    let (_home, layout) = open_layout();
    assert_eq!(read_store_lock_file(&layout), StoreLockFileState::Absent);
}

#[test]
fn read_store_lock_file_is_corrupt_on_unparseable_content() {
    let (_home, layout) = open_layout();
    local_rag_core::paths::ensure_file_0600(&layout.store_lock()).expect("ensure lock file");
    std::fs::write(layout.store_lock(), b"{not valid json at all").expect("seed torn write");

    assert_eq!(read_store_lock_file(&layout), StoreLockFileState::Corrupt);
}

#[test]
fn read_store_lock_file_parses_a_real_lock_written_by_acquire() {
    let (_home, layout) = open_layout();
    let probe = FixedProbe(LivenessOutcome::Alive); // never consulted -- fresh acquire
    let guard = acquire(&layout, "instance-a", 42, "0.0.0", 1_000, &probe).expect("acquire");
    assert_eq!(guard.info().pid, 42);

    match read_store_lock_file(&layout) {
        StoreLockFileState::Parsed(info) => {
            assert_eq!(info.instance_uuid, "instance-a");
            assert_eq!(info.pid, 42);
            assert!(!info.ready, "mark_ready was never called");
        }
        other => panic!("expected Parsed, got {other:?}"),
    }
}

#[test]
fn read_store_lock_file_never_contends_with_a_live_flock() {
    let (_home, layout) = open_layout();
    let probe = FixedProbe(LivenessOutcome::Alive);
    // Hold the real flock via a live guard for the whole test -- a mutating
    // read (or one that tried to `try_lock`) would either fail or need to
    // wait; `read_store_lock_file` must do neither.
    let guard = acquire(&layout, "instance-b", 99, "0.0.0", 1_000, &probe).expect("acquire");

    match read_store_lock_file(&layout) {
        StoreLockFileState::Parsed(info) => assert_eq!(info.instance_uuid, "instance-b"),
        other => panic!("expected Parsed, got {other:?}"),
    }

    drop(guard);
}
