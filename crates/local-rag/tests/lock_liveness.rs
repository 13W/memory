//! T15-01 acceptance tests for `daemon::lock::acquire` (spec 02 §4.1 step 1):
//! live conflict, PID reuse mismatch, and stale socket/lock recovery — plus
//! D-065's and D-084's rule that the `WouldBlock` branch reclaims nothing at
//! all, and D-084's bounded handover wait that replaced the reclaim.
//!
//! All in-process: a background thread holding a real `flock` stands in for a
//! second daemon — no real second `local-rag serve` process is needed for
//! these lock-mechanics scenarios (that is `tests/serve_subprocess.rs`'s
//! job). Deterministic: no wall-clock sleeps — a `std::sync::mpsc` handshake
//! proves the background holder is ready before the foreground `acquire`
//! runs, mirroring `crates/store/tests/lock.rs`'s own channel-based idiom for
//! proving real concurrency without a race.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use local_rag::daemon::{
    StoreLockError, StoreLockFileState, StoreLockGuard, StoreLockInfo, acquire,
    read_store_lock_file,
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

/// The budget almost every test here wants: none at all. A store that is free
/// must be acquirable *now*, and a store that is held must be refused *now* —
/// waiting would only blur which of the two happened.
const NO_WAIT: Duration = Duration::ZERO;

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
/// contender) — `acquire` must report `Locked` naming that holder, never
/// silently take over.
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
    let result = acquire(&layout, "candidate-uuid", 999_999, "0.0.0", 2_000, NO_WAIT);

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
    // socket cleanup. That is also why D-084 left this test intact: the
    // reclaim it deleted lives on the `WouldBlock` branch, which this scenario
    // never reaches. Nothing here ever depended on a liveness verdict; the
    // greeter answering with a different uuid is scene-setting, not the
    // mechanism.
    let guard = acquire(&layout, "new-instance-uuid", 42, "0.0.0", 2_000, NO_WAIT)
        .expect("stale owner must be reclaimed");
    assert_eq!(guard.info().instance_uuid, "new-instance-uuid");
    assert!(
        !layout.socket_path().exists(),
        "the orphaned socket must be cleaned up"
    );

    stop_greeter();
}

/// **D-084's reproduction.** A live owner that has stopped answering — this is
/// a daemon in shutdown, which unlinks its socket in step 1 (`daemon::shutdown`,
/// D-077) and releases the lock last, so for the whole length of its drain it
/// is alive, still writing, and unreachable. Its record parses, names a live
/// pid, and says `ready: true`; nothing can answer on the socket.
///
/// Before D-084 that combination was read as "the owner is dead" and the lock
/// was reclaimed: the file unlinked, a fresh one created, and the incumbent's
/// real `flock` — which lives on its open file description, not on the path —
/// left untouched. Two daemons, one canonical store. Live capture:
/// `daemon stopping` 11:53:47, a second `store lock acquired` 11:54:02, the
/// first `daemon stopped` 11:54:14.
///
/// The inode assertion is the sharp one, exactly as in D-065's tests: a
/// `Locked` result with a *changed* inode would still be the defect.
#[test]
fn a_live_owner_that_stopped_answering_is_never_reclaimed() {
    use std::os::unix::fs::MetadataExt;

    let (_home, layout) = open_layout();
    let (ready_tx, ready_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let lock_path = layout.store_lock();

    // A well-formed, `ready: true` record naming a genuinely live pid (ours),
    // written by the holder through the same handle that holds the `flock` —
    // exactly the state a daemon leaves behind when it enters shutdown.
    let owner_json = serde_json::json!({
        "instance_uuid": "draining-owner",
        "pid": std::process::id(),
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
    });
    ready_rx.recv().expect("holder ready");

    // Deliberately no `spawn_greeter`: the socket the record names does not
    // exist, which is the whole scenario.
    let ino_before = std::fs::metadata(layout.store_lock())
        .expect("lock file exists before acquire")
        .ino();
    let result = acquire(&layout, "candidate-uuid", 999_999, "0.0.0", 2_000, NO_WAIT);
    let ino_after = std::fs::metadata(layout.store_lock())
        .expect("lock file must still exist — a reclaim would have unlinked it")
        .ino();

    release_tx.send(()).expect("release holder");
    holder.join().expect("holder thread");

    match result {
        Err(StoreLockError::Locked { owner }) => {
            assert_eq!(owner.instance_uuid, "draining-owner");
            assert_eq!(owner.pid, std::process::id());
        }
        other => panic!(
            "expected Locked (a live owner that stopped answering must never be reclaimed), \
             got {other:?}"
        ),
    }
    assert_eq!(
        ino_before, ino_after,
        "the lock file was unlinked and recreated — this is the reclaim that leaves two \
         daemons each owning the store"
    );
}

/// The budget is a real wait, not a formality: a lock held for its whole
/// length is refused *after* it, not immediately. Asserting a lower bound on
/// the elapsed time is safe — an upper bound is what would flake — and it is
/// the only thing that distinguishes "retried until the budget ran out" from
/// "returned on the first `WouldBlock`".
#[test]
fn a_lock_held_past_the_budget_is_refused_after_waiting_it_out() {
    use std::os::unix::fs::MetadataExt;

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

    let budget = Duration::from_millis(200);
    let ino_before = std::fs::metadata(layout.store_lock())
        .expect("lock file exists")
        .ino();
    let started = Instant::now();
    let result = acquire(&layout, "candidate-uuid", 999_999, "0.0.0", 2_000, budget);
    let elapsed = started.elapsed();
    let ino_after = std::fs::metadata(layout.store_lock())
        .expect("lock file must still exist")
        .ino();

    release_tx.send(()).expect("release holder");
    holder.join().expect("holder thread");

    assert!(
        matches!(result, Err(StoreLockError::Locked { .. })),
        "a lock held for the whole budget must be refused: {result:?}"
    );
    assert!(
        elapsed >= budget,
        "the budget must be spent waiting, not skipped: {elapsed:?} < {budget:?}"
    );
    assert_eq!(ino_before, ino_after, "waiting must not reclaim either");
}

/// The other half of the budget: a store released while a contender is waiting
/// is handed over, not refused. This is the case the reclaim used to "solve"
/// by stealing — a daemon spawned during someone else's drain.
///
/// The zero-budget probe first proves the lock is genuinely held, so the
/// budgeted call that follows is known to start against a real incumbent. The
/// handover itself is asserted on the outcome, never on timing.
#[test]
fn an_incumbent_that_releases_is_handed_over_to_within_the_budget() {
    let (_home, layout) = open_layout();
    let (ready_tx, ready_rx) = mpsc::channel::<()>();
    let (blocked_tx, blocked_rx) = mpsc::channel::<()>();
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
        // `file` drops here: the flock goes, exactly as an exiting daemon's
        // would.
    });
    ready_rx.recv().expect("holder ready");

    let contender_layout = layout.clone();
    let contender = std::thread::spawn(move || {
        let refused = acquire(
            &contender_layout,
            "contender",
            424_242,
            "0.0.0",
            3_000,
            NO_WAIT,
        );
        assert!(
            matches!(refused, Err(StoreLockError::Locked { .. })),
            "the incumbent must genuinely hold the lock first: {refused:?}"
        );
        blocked_tx.send(()).expect("signal blocked");
        acquire(
            &contender_layout,
            "contender",
            424_242,
            "0.0.0",
            3_000,
            Duration::from_secs(5),
        )
    });

    blocked_rx.recv().expect("contender blocked once");
    release_tx.send(()).expect("release holder");
    holder.join().expect("holder thread");

    let guard = contender
        .join()
        .expect("contender thread")
        .expect("a released lock must be handed over within the budget");
    assert_eq!(guard.info().instance_uuid, "contender");
}

/// D-084's second half: an owner on its way out must never unlink a record
/// that is no longer its own. `flock` lives on the open file description and
/// the record lives at a path; anything that recreates the file while a guard
/// is alive pulls them apart. Before this check, the exiting daemon deleted
/// its successor's record, and the daemon after *that* found no file at all
/// and acquired cleanly alongside both — the third process in the live
/// capture, 57 seconds after the second.
#[test]
fn a_departing_owner_never_unlinks_a_record_it_no_longer_owns() {
    use std::os::unix::fs::MetadataExt;

    let (_home, layout) = open_layout();
    let guard = acquire(&layout, "departing", 111, "0.0.0", 1_000, NO_WAIT).expect("acquire");

    // A successor takes the path: unlink and recreate, exactly what a reclaim
    // (or any recreate-in-place) does to it.
    std::fs::remove_file(layout.store_lock()).expect("unlink the departing owner's record");
    local_rag_core::paths::ensure_file_0600(&layout.store_lock()).expect("successor's record");
    std::fs::write(layout.store_lock(), b"successor's record").expect("seed successor content");
    let successor_ino = std::fs::metadata(layout.store_lock())
        .expect("successor record exists")
        .ino();

    guard.release(&layout);

    let after = std::fs::metadata(layout.store_lock())
        .expect("the successor's record must survive the departing owner's release");
    assert_eq!(
        after.ino(),
        successor_ino,
        "release unlinked a record that was not its own"
    );
    assert_eq!(
        std::fs::read(layout.store_lock()).expect("read"),
        b"successor's record",
        "the successor's content must be untouched"
    );
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
    let guard = acquire(&layout, "fresh-instance", 7, "0.0.0", 2_000, NO_WAIT)
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
    let guard = acquire(&layout, "fresh-instance", 7, "0.0.0", 2_000, NO_WAIT)
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
    let guard = acquire(&layout, "fresh-instance", 7, "0.0.0", 2_000, NO_WAIT)
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
    let mut guard = acquire(
        &layout,
        "instance-a",
        std::process::id(),
        "0.0.0",
        1_000,
        NO_WAIT,
    )
    .expect("acquire");

    let contender = acquire(&layout, "instance-b", 200, "0.0.0", 1_000, NO_WAIT);
    assert!(matches!(contender, Err(StoreLockError::Locked { .. })));

    guard
        .mark_ready(1_500, &layout.socket_path())
        .expect("mark ready");
    assert!(guard.info().ready);

    let contender_after = acquire(&layout, "instance-c", 300, "0.0.0", 1_600, NO_WAIT);
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
    let guard = acquire(&layout, "instance-a", 100, "0.0.0", 1_000, NO_WAIT).expect("acquire");
    guard.release(&layout);
    assert!(!layout.store_lock().exists());

    let second = acquire(&layout, "instance-b", 200, "0.0.0", 2_000, NO_WAIT);
    assert!(second.is_ok(), "release must free the lock: {second:?}");
}

/// A live owner that has not finished starting up yet (`ready: false` — most
/// commonly, still running a migration, spec 02 §4.1 step 2) has bound no
/// socket at all. A naive probe would see "no listener" and misclassify it
/// as dead, wrongly reclaiming the lock out from under a genuinely live
/// daemon (whose real OS `flock` on its own open file descriptor is never
/// actually released by that reclaim — this file's `daemon::lock` module doc).
/// `acquire` must report `Locked`, not silently steal the store from a daemon
/// that is still migrating. Since D-084 that follows from the branch rule
/// rather than from a `ready`-specific exception, and the test is unchanged:
/// the property it guards is the one that survived.
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
    // has bound no socket yet, so anything asking the socket would see
    // "connection refused".
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
    let result = acquire(&layout, "candidate-uuid", 999_999, "0.0.0", 2_000, NO_WAIT);

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
/// mid-migration) — this must still be recovered from. A dead process cannot
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
    let guard = acquire(&layout, "fresh-instance", 7, "0.0.0", 2_000, NO_WAIT)
        .expect("a dead not-yet-ready owner must be reclaimed");
    assert_eq!(guard.info().instance_uuid, "fresh-instance");
}

// ---------------------------------------------------------------------------
// D-065: an *unreadable* record on the `WouldBlock` branch is a live conflict,
// never grounds for a reclaim. Reaching that branch already proves a live
// holder (POSIX releases `flock` on exit), so the record can only belong to an
// owner that has not written it yet or is rewriting it — the two windows these
// tests stand in for. Neither existing unparseable-content test covers this:
// both write their garbage with no `flock` held at all, which is the success
// branch. Production hit this window on 2026-08-18 and ended up with two
// daemons each believing it owned the store.
// ---------------------------------------------------------------------------

/// Hold a real `flock` on `store.lock`, seed `content`, and run `acquire`
/// against it. Returns the result plus the lock file's inode before and after
/// — a changed inode is precisely the reclaim this must never perform.
///
/// The `mpsc` handshake (this file's own idiom) proves the holder is in place
/// before the foreground `acquire` runs, so there is no wall-clock dependency.
fn acquire_against_a_live_holder(
    layout: &StoreLayout,
    content: Option<&'static [u8]>,
) -> (Result<StoreLockGuard, StoreLockError>, u64, u64) {
    use std::os::unix::fs::MetadataExt;

    let (ready_tx, ready_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let lock_path = layout.store_lock();

    let holder = std::thread::spawn(move || {
        local_rag_core::paths::ensure_file_0600(&lock_path).expect("ensure lock file");
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .expect("open lock file");
        file.lock().expect("blocking lock");
        if let Some(bytes) = content {
            file.write_all(bytes).expect("seed content");
            file.flush().expect("flush");
        }
        ready_tx.send(()).expect("signal ready");
        release_rx.recv().expect("wait for release signal");
    });
    ready_rx.recv().expect("holder ready");

    let ino_before = std::fs::metadata(layout.store_lock())
        .expect("lock file exists before acquire")
        .ino();

    // Under the pre-D-065 code an unreadable record drove the reclaim all by
    // itself; since D-084 nothing on this branch can drive one at all.
    let result = acquire(layout, "candidate-uuid", 999_999, "0.0.0", 2_000, NO_WAIT);

    let ino_after = std::fs::metadata(layout.store_lock())
        .expect("lock file must still exist — a reclaim would have unlinked it")
        .ino();

    release_tx.send(()).expect("release holder");
    holder.join().expect("holder thread");

    (result, ino_before, ino_after)
}

fn assert_locked_by_an_unnamed_owner(
    result: Result<StoreLockGuard, StoreLockError>,
    ino_before: u64,
    ino_after: u64,
) {
    match result {
        Err(StoreLockError::Locked { owner }) => {
            assert_eq!(owner.pid, 0, "an unreadable record cannot name a pid");
            assert!(!owner.ready, "an owner mid-startup is not ready");
        }
        other => panic!(
            "expected Locked (a live holder whose record cannot be read must never be \
             reclaimed), got {other:?}"
        ),
    }
    assert_eq!(
        ino_before, ino_after,
        "the lock file was unlinked and recreated — this is the reclaim that leaves two \
         daemons each owning the store"
    );
}

/// The owner took the `flock` microseconds ago and has not written its record
/// yet: the file `ensure_file_0600` created is still empty.
#[test]
fn an_unwritten_record_under_a_live_flock_is_locked_not_reclaimed() {
    let (_home, layout) = open_layout();
    let (result, before, after) = acquire_against_a_live_holder(&layout, None);
    assert_locked_by_an_unnamed_owner(result, before, after);
}

/// The owner is rewriting its record right now (`mark_ready` through
/// `write_info`) and a reader caught a partial one.
#[test]
fn a_torn_record_under_a_live_flock_is_locked_not_reclaimed() {
    let (_home, layout) = open_layout();
    let (result, before, after) =
        acquire_against_a_live_holder(&layout, Some(b"{\"instance_uuid\":\"half-writ"));
    assert_locked_by_an_unnamed_owner(result, before, after);
}

/// `write_info`'s padding covers the previous record only while the write is
/// in flight — the file left on disk is exactly the new record, with no
/// trailing spaces for the next reader to puzzle over.
#[test]
fn a_shrinking_rewrite_leaves_no_padding_on_disk() {
    let (_home, layout) = open_layout();

    // A long record first: `ready: true` carries `ready_at` and `socket_path`.
    let mut guard = acquire(&layout, "long-instance-uuid", 4242, "0.0.0", 1_000, NO_WAIT)
        .expect("first acquire");
    guard
        .mark_ready(1_500, &layout.socket_path())
        .expect("mark ready");
    let long_len = std::fs::metadata(layout.store_lock())
        .expect("metadata")
        .len();
    drop(guard); // releases the `flock`, leaves the content behind

    // Now a shorter one over it: fresh records carry neither field.
    let _guard = acquire(&layout, "short", 7, "0.0.0", 2_000, NO_WAIT).expect("second acquire");

    let bytes = std::fs::read(layout.store_lock()).expect("read lock file");
    assert!(
        (bytes.len() as u64) < long_len,
        "the shorter record must not keep the old one's length: {} vs {long_len}",
        bytes.len()
    );
    assert!(
        !bytes.ends_with(b" "),
        "padding must be truncated away: {:?}",
        String::from_utf8_lossy(&bytes)
    );
    let parsed: StoreLockInfo = serde_json::from_slice(&bytes).expect("parses");
    assert_eq!(parsed.instance_uuid, "short");
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
    let guard = acquire(&layout, "instance-a", 42, "0.0.0", 1_000, NO_WAIT).expect("acquire");
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
    // read (or one that tried to `try_lock`) would either fail or need to
    // wait; `read_store_lock_file` must do neither.
    let guard = acquire(&layout, "instance-b", 99, "0.0.0", 1_000, NO_WAIT).expect("acquire");

    match read_store_lock_file(&layout) {
        StoreLockFileState::Parsed(info) => assert_eq!(info.instance_uuid, "instance-b"),
        other => panic!("expected Parsed, got {other:?}"),
    }

    drop(guard);
}
